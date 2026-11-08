//! Stored results and their artifacts (spec §16.3).
//!
//! `GET /v1/results/{id}` returns the result of a fetch again, by the
//! request's id: for a fetch that outlasted its wall time (`202` while it
//! runs), and for anyone who kept only the id. `GET /v1/artifacts/{id}`
//! streams what was kept of a page: the document, the raw body, a
//! screenshot, a PDF, the body of a captured XHR response.
//!
//! A key reads what it fetched and nothing else: another key's result is
//! `404`, as if it were not there. A key with the `admin` scope reads all.
//! Results and artifacts are kept for `artifacts.retention_days`.

use std::io;

use axum::Json;
use axum::body::Body as HttpBody;
use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_SECURITY_POLICY,
    CONTENT_TYPE, ETAG, HeaderValue, RETRY_AFTER, VARY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::Duration as Span;
use kage_core::domain::{
    Artifact, ArtifactEncoding, ArtifactKind, FetchRecord, JobItem, JobItemState, RequestRecord,
    Scope, SessionId, Visit,
};
use kage_core::ids::{ArtifactId, RequestId, ResultId};
use kage_core::outcome::Outcome;
use kage_store::ArtifactError;
use tokio::io::AsyncReadExt;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::fetch::request::{FetchRequest, Include, Included};
use crate::fetch::result::{
    Bodies, FetchAccepted, FetchResult, Viewer, from_stored, parse_artifact,
};
use crate::shared::Binary;
use crate::state::ApiState;

/// How long a fetch may be "running" before its result is given up on: the
/// server stopped while it ran. Longer than any fetch can take (three
/// attempts at the hard cap of five minutes each, and their pauses).
const LOST_AFTER_MINUTES: i64 = 60;

/// How many bytes of an artifact are read from disk at a time.
const CHUNK_BYTES: usize = 64 * 1024;

fn not_found(code: ErrorCode) -> ApiError {
    ApiError::new(
        code,
        "there is no such thing among what this key fetched; it may have expired",
    )
}

/// The request with this id, when the caller may read it.
async fn owned_request(
    state: &ApiState,
    caller: &Caller,
    id: RequestId,
    code: ErrorCode,
) -> ApiResult<RequestRecord> {
    let request = state
        .store()
        .requests()
        .get(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| not_found(code))?;
    // Another key's request is not there, as far as this key can tell.
    if request.api_key_id == Some(caller.key.id) || caller.has(Scope::Admin) {
        Ok(request)
    } else {
        Err(not_found(code))
    }
}

/// What a stored request asked to have returned. A request that was not
/// made through the API (`kage fetch`) asked for nothing in particular and
/// gets everything that was kept.
fn included(request: &RequestRecord) -> Included {
    serde_json::from_value::<FetchRequest>(request.spec.clone())
        .ok()
        .and_then(|spec| Included::of(&spec))
        .unwrap_or_else(Included::everything)
}

/// The `Content-Type` of a stored result's main response.
fn content_type_of(record: &FetchRecord) -> Option<&str> {
    record
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.as_str())
}

/// The stored document and body of a result, as text.
pub(crate) async fn bodies(
    state: &ApiState,
    record: &FetchRecord,
    include: &Included,
) -> ApiResult<Bodies> {
    let mut bodies = Bodies::default();
    for (kind, wanted) in [
        (ArtifactKind::DomHtml, include.has(Include::Dom)),
        (
            ArtifactKind::RawBody,
            include.has(Include::Raw) || include.has(Include::Dom),
        ),
    ] {
        let Some(id) = record.artifact(kind).filter(|_| wanted) else {
            continue;
        };
        let text = match state.artifacts().read(id).await {
            // The document a browser serialized is UTF-8; the body is read
            // in the encoding a browser would take for it (F100).
            Ok(Some((_, bytes))) if kind == ArtifactKind::DomHtml => String::from_utf8(bytes).ok(),
            // The page's address is the detector's hint for an undeclared
            // body, as on the fresh answer (F160).
            // A body cut at `max_body_bytes` ends at its last whole
            // character (QA round three's extract-08).
            Ok(Some((_, bytes))) => kage_http::charset::text_of_cut_from(
                record
                    .final_url
                    .as_deref()
                    .and_then(|url| url::Url::parse(url).ok())
                    .as_ref(),
                content_type_of(record),
                &bytes,
                record.truncated,
            ),
            // Expired, or its blob is gone: the result is still a result.
            Ok(None) | Err(ArtifactError::Missing { .. }) => None,
            Err(error) => return Err(ApiError::internal(&error)),
        };
        match kind {
            ArtifactKind::DomHtml => bodies.dom_html = text,
            _ => bodies.raw_body = text,
        }
    }
    Ok(bodies)
}

/// What a request whose job item ended without a page and without a
/// stored result came to: the item's end, with nothing of a page; `None`
/// for an item that is still open or succeeded, whose result is still to
/// be stored. The worker ends such a request with the item; this answers
/// one it could not (a server that was killed between the two, or a
/// request of a release before F109). Its id is the item's, so that every
/// read names the same result: this endpoint's and the job's list of items
/// (F155).
pub(crate) fn ended_with(item: &JobItem, request: RequestId) -> Option<FetchRecord> {
    if !item.state.is_terminal() || item.state == JobItemState::Succeeded {
        return None;
    }
    Some(FetchRecord {
        id: ResultId::from_uuid(item.id.as_uuid()),
        request_id: request,
        outcome: item.outcome.unwrap_or(Outcome::ProviderError),
        status: None,
        headers: Vec::new(),
        final_url: None,
        redirects: Vec::new(),
        truncated: false,
        extract: None,
        reading: None,
        xhr: None,
        artifacts: Vec::new(),
        persona: None,
        block: None,
        robots: None,
        cached: false,
        cached_at: None,
        reason: item.error.clone(),
        visit: Visit::default(),
        created_at: item
            .finished_at
            .or(item.started_at)
            .unwrap_or(item.created_at),
    })
}

/// The result of a fetch, by the request's id.
///
/// `200` with the result once the fetch has ended; `202` while it runs.
/// A request whose job ended without it (cancelled, expired, interrupted)
/// has ended too: `200` with what ended it.
/// What the visit did (`pacing`, `behavior`, `console`, `side_effects`) is
/// kept with the result and said here as the synchronous answer said it
/// (F36); `console` when the request asked for it.
#[utoipa::path(
    get,
    path = "/v1/results/{id}",
    tag = "fetch",
    operation_id = "get_result",
    params(("id" = String, Path, format = Uuid, description = "The request's id, as in `X-Request-Id`.")),
    responses(
        (status = 200, description = "The result; for a request whose job ended without it (cancelled, expired, interrupted), `provider_error` with the item's reason.", body = FetchResult),
        (status = 202, description = "The fetch is still under way.", body = FetchAccepted,
            headers(("Retry-After" = u64, description = "Seconds to wait before asking again."))),
        (status = 404, description = "`result_not_found`: no such request of this key, or it expired.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["fetch"]))
)]
pub(crate) async fn result(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    caller.require(Scope::Fetch)?;
    let id = id
        .parse::<RequestId>()
        .map_err(|_| not_found(ErrorCode::ResultNotFound))?;
    let request = owned_request(&state, &caller, id, ErrorCode::ResultNotFound).await?;
    let record = state
        .store()
        .results()
        .get_by_request(id)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    // The job's item of a request that has no result yet.
    let item = match (&record, request.job_id) {
        (None, Some(_)) => state
            .store()
            .job_items()
            .find_by_request(id)
            .await
            .map_err(|error| ApiError::internal(&error))?,
        _ => None,
    };
    // A request whose item ended without a result has ended with it
    // (F109): it is not running.
    let record = record.or_else(|| item.as_ref().and_then(|item| ended_with(item, id)));
    let Some(record) = record else {
        // A request a job has still to finish is running however long it
        // takes: the job waits for a paused site, for its visitor, for a
        // restart of the server.
        let carried = item.as_ref().filter(|item| !item.state.is_terminal());
        let age = state.clock().now() - request.created_at;
        if carried.is_none() && age > Span::minutes(LOST_AFTER_MINUTES) {
            return Err(ApiError::new(
                ErrorCode::ResultNotFound,
                "the fetch never finished: the server stopped while it ran",
            ));
        }
        let accepted = match carried {
            Some(item) => FetchAccepted::carried(id, item.job_id),
            None => FetchAccepted::running(id),
        };
        let mut response = (StatusCode::ACCEPTED, Json(accepted)).into_response();
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(5_u64));
        return Ok(response);
    };
    let attempts = state
        .store()
        .attempts()
        .list_for_request(id)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let include = included(&request);
    let bodies = bodies(&state, &record, &include).await?;
    let viewer = Viewer {
        key: caller.key.id,
        admin: caller.has(Scope::Admin),
        handles: state.handles(),
    };
    let result = from_stored(
        (&record, request.level),
        &attempts,
        bodies,
        &include,
        request.session_id.as_ref().map(SessionId::as_str),
        &viewer,
    );
    Ok(Json(result).into_response())
}

/// The weight of a qvalue (RFC 9110 §12.4.2): `0` to `1` with up to
/// three decimals, in thousandths; `None` for one that is no qvalue.
fn qvalue(written: &str) -> Option<u16> {
    let (whole, fraction) = written.split_once('.').unwrap_or((written, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let thousandths = format!("{fraction:0<3}").parse::<u16>().ok()?;
    match whole {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// Whether a client takes a gzip body (RFC 9110 §12.5.3): the weight of
/// `gzip` (or `x-gzip`) when the client names it, else that of `*`;
/// a weight of 0 in any of its spellings refuses it, and a client that
/// names neither does not take it. An element whose weight is no qvalue
/// says nothing.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let (mut named, mut any): (Option<u16>, Option<u16>) = (None, None);
    for coding in headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let mut parts = coding.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let mut weight = Some(1000);
        for parameter in parts {
            let Some((key, value)) = parameter.split_once('=') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("q") {
                weight = qvalue(value.trim());
            }
        }
        let Some(weight) = weight else {
            continue;
        };
        let slot = if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") {
            &mut named
        } else if name == "*" {
            &mut any
        } else {
            continue;
        };
        // Of two elements for one coding the higher weight stands.
        *slot = Some(slot.map_or(weight, |known| known.max(weight)));
    }
    named.or(any).is_some_and(|weight| weight > 0)
}

/// A file as a stream of chunks, read as the client takes them.
fn stream_file(file: tokio::fs::File) -> HttpBody {
    let chunks = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut chunk = vec![0_u8; CHUNK_BYTES];
        match file.read(&mut chunk).await {
            Ok(0) => None,
            Ok(read) => {
                chunk.truncate(read);
                Some((Ok::<Bytes, io::Error>(Bytes::from(chunk)), Some(file)))
            }
            // The error ends the body; the client sees a transfer cut short.
            Err(error) => Some((Err(error), None)),
        }
    });
    HttpBody::from_stream(chunks)
}

/// The headers every artifact is sent with. What a site served is never
/// handed to a browser as a page of this origin: no script runs, no type
/// is guessed.
fn artifact_headers(artifact: &Artifact) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(&artifact.content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600, immutable"),
    );
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    headers
}

/// The entity tag of one representation of an artifact: the stored
/// content's hash, with `-gzip` for the compressed one. A strong tag names
/// one representation, so the gzip and the unpacked bytes never share one
/// (RFC 9110 §8.8.3, QA round three's extract-03).
fn entity_tag(artifact: &Artifact, gzip: bool) -> Option<HeaderValue> {
    let suffix = if gzip { "-gzip" } else { "" };
    HeaderValue::from_str(&format!("\"{}{suffix}\"", artifact.content_hash)).ok()
}

/// The artifact with this id, when the caller may read it.
async fn owned_artifact(state: &ApiState, caller: &Caller, id: ArtifactId) -> ApiResult<Artifact> {
    let missing = || not_found(ErrorCode::ArtifactNotFound);
    let artifact = state
        .artifacts()
        .get(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .filter(|artifact| artifact.expires_at > state.clock().now())
        .ok_or_else(missing)?;
    if caller.has(Scope::Admin) {
        return Ok(artifact);
    }
    // An artifact without a request is Kage's own (a self-test's evidence).
    let request = artifact.request_id.ok_or_else(missing)?;
    owned_request(state, caller, request, ErrorCode::ArtifactNotFound).await?;
    Ok(artifact)
}

/// What was kept of a page, streamed.
///
/// The media type is the stored one. A text body that is kept compressed
/// is sent compressed (`Content-Encoding: gzip`) to a client that takes
/// it, and unpacked for one that does not.
#[utoipa::path(
    get,
    path = "/v1/artifacts/{id}",
    tag = "fetch",
    operation_id = "get_artifact",
    params(("id" = String, Path, description = "The artifact's id, as a result names it (`art_...`).")),
    responses(
        (status = 200, description = "The artifact's bytes, in its own media type (`Content-Type`): an HTML document, a body as the site sent it, a WebP, PNG or JPEG image, a PDF. Read it as bytes, not as JSON.", body = inline(Binary), content_type = "*/*"),
        (status = 404, description = "`artifact_not_found`: no such artifact of this key, or it expired.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["fetch"]))
)]
pub(crate) async fn artifact(
    State(state): State<ApiState>,
    caller: Caller,
    request_headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    caller.require(Scope::Fetch)?;
    let id = parse_artifact(&id).ok_or_else(|| not_found(ErrorCode::ArtifactNotFound))?;
    let artifact = owned_artifact(&state, &caller, id).await?;
    let mut headers = artifact_headers(&artifact);

    // A client that does not take gzip gets the content unpacked, which
    // also checks it against its hash.
    if artifact.encoding == ArtifactEncoding::Gzip && !accepts_gzip(&request_headers) {
        let bytes = match state.artifacts().read(id).await {
            Ok(Some((_, bytes))) => bytes,
            Ok(None) | Err(ArtifactError::Missing { .. }) => {
                return Err(not_found(ErrorCode::ArtifactNotFound));
            }
            Err(error) => return Err(ApiError::internal(&error)),
        };
        headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        if let Some(etag) = entity_tag(&artifact, false) {
            headers.insert(ETAG, etag);
        }
        return Ok((headers, bytes).into_response());
    }

    let path = state
        .artifacts()
        .blob_path(&artifact.content_hash, artifact.encoding);
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(artifact = %id, "an artifact's blob is gone");
            return Err(not_found(ErrorCode::ArtifactNotFound));
        }
        Err(error) => return Err(ApiError::internal(&error)),
    };
    headers.insert(CONTENT_LENGTH, HeaderValue::from(artifact.stored_bytes));
    let gzip = artifact.encoding == ArtifactEncoding::Gzip;
    if gzip {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    if let Some(etag) = entity_tag(&artifact, gzip) {
        headers.insert(ETAG, etag);
    }
    Ok((headers, stream_file(file)).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::http::header::{ACCEPT_ENCODING, HeaderValue};
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    use super::{CHUNK_BYTES, accepts_gzip, qvalue, stream_file};

    #[test]
    fn gzip_is_sent_only_to_a_client_that_takes_it() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT_ENCODING, HeaderValue::from_str(value).unwrap());
            accepts_gzip(&headers)
        };

        assert!(with("gzip"));
        assert!(with("br, GZIP;q=0.8, deflate"));
        assert!(with("*"));
        assert!(!with("br, deflate"));
        assert!(!with("gzip;q=0"));
        assert!(!with("identity"));
        // QA round one's extract-09 (F142): a weight of 0 in any of its
        // spellings refuses, and the named coding wins over `*`.
        for refused in [
            "gzip;q=0.000",
            "gzip; Q=0.00",
            "gzip;q=0, *",
            "*, gzip;q=0",
            "*;q=0",
        ] {
            assert!(!with(refused), "{refused}");
        }
        for taken in [
            "gzip;q=0.001",
            "gzip;q=1.000",
            "x-gzip",
            "*;q=0.5, br",
            "gzip;q=0, gzip;q=0.5",
        ] {
            assert!(with(taken), "{taken}");
        }
        // A weight that is no qvalue says nothing.
        assert!(!with("gzip;q=2"));
        assert!(!with("gzip;q=0.0001"));
        assert!(with("gzip;q=x, *"));
        assert_eq!(
            (qvalue("1"), qvalue("0.5"), qvalue("0."), qvalue("1.001")),
            (Some(1000), Some(500), Some(0), None)
        );
        assert!(!accepts_gzip(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn a_file_is_streamed_whole_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let content: Vec<u8> = (0..CHUNK_BYTES * 2 + 123)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(&content).await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let body = stream_file(tokio::fs::File::open(&path).await.unwrap());
        let read = body.collect().await.unwrap().to_bytes();

        assert_eq!(read.len(), content.len());
        assert_eq!(read.as_ref(), content.as_slice());

        // An empty file is an empty body.
        let empty = dir.path().join("empty");
        tokio::fs::write(&empty, b"").await.unwrap();
        let body = stream_file(tokio::fs::File::open(&empty).await.unwrap());
        assert!(body.collect().await.unwrap().to_bytes().is_empty());
    }
}
