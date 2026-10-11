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
    Artifact, ArtifactEncoding, ArtifactKind, FetchRecord, RequestRecord, Scope, SessionId,
};
use kage_core::ids::{ArtifactId, RequestId};
use kage_store::ArtifactError;
use tokio::io::AsyncReadExt;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::fetch::request::{FetchRequest, Include, Included};
use crate::fetch::result::{
    Bodies, FetchAccepted, FetchResult, Viewer, from_stored, parse_artifact,
};
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

/// The stored document and body of a result, as text.
async fn bodies(state: &ApiState, record: &FetchRecord, include: &Included) -> ApiResult<Bodies> {
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
            Ok(Some((_, bytes))) => String::from_utf8(bytes).ok(),
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

/// The result of a fetch, by the request's id.
///
/// `200` with the result once the fetch has ended; `202` while it runs.
/// What only the synchronous answer has (`pacing`, `behavior`, `console`,
/// `side_effects`) is null here.
#[utoipa::path(
    get,
    path = "/v1/results/{id}",
    tag = "fetch",
    operation_id = "get_result",
    params(("id" = String, Path, format = Uuid, description = "The request's id, as in `X-Request-Id`.")),
    responses(
        (status = 200, description = "The result.", body = FetchResult),
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
    let Some(record) = record else {
        // A request a job has still to finish is running however long it
        // takes: the job waits for a paused site, for its visitor, for a
        // restart of the server.
        let item = match request.job_id {
            Some(_) => state
                .store()
                .job_items()
                .find_by_request(id)
                .await
                .map_err(|error| ApiError::internal(&error))?,
            None => None,
        };
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
        &record,
        &attempts,
        bodies,
        &include,
        request.session_id.as_ref().map(SessionId::as_str),
        &viewer,
    );
    Ok(Json(result).into_response())
}

/// Whether a client takes a gzip body.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| {
            let mut parts = coding.split(';');
            let name = parts.next().unwrap_or_default().trim();
            let refused = parts.any(|parameter| {
                let parameter = parameter.trim().to_ascii_lowercase();
                parameter == "q=0" || parameter == "q=0.0" || parameter == "q=0.00"
            });
            (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
        })
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
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", artifact.content_hash)) {
        headers.insert(ETAG, etag);
    }
    headers
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
        (status = 200, description = "The artifact's bytes, in its own media type.", body = Vec<u8>, content_type = "application/octet-stream"),
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
    if artifact.encoding == ArtifactEncoding::Gzip {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok((headers, stream_file(file)).into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::http::header::{ACCEPT_ENCODING, HeaderValue};
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    use super::{CHUNK_BYTES, accepts_gzip, stream_file};

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
