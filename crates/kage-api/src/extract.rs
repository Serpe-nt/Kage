//! Extractors whose refusals are problem documents (spec §16.1, §19).
//!
//! axum's own `Json` and `Query` answer a body or a query they cannot read
//! with a line of plain text, take any `+json` media type and let the last
//! of two equal keys win. These read a body in four steps, each with its
//! own answer:
//!
//! 1. **The media type** is `application/json` (parameters such as
//!    `charset` aside), or the request is `415 unsupported_media_type`.
//! 2. **The bytes** arrive within `server.body_read_timeout` (`408
//!    request_timeout`) and are at most `server.max_body_bytes` (`413
//!    payload_too_large`).
//! 3. **The document** is well-formed JSON (`400 invalid_request`), names
//!    no member twice and holds no NUL character in a string or a name
//!    (`422 validation_failed` naming the member as `field`): a value with a
//!    NUL can be neither kept (Postgres' `jsonb` takes none) nor sent.
//! 4. **The type** is what the endpoint takes: an unknown member, a wrong
//!    type, a null where a value is needed, a value outside an enumeration
//!    are `422 validation_failed` with the member's path as `field`.
//!
//! A query string is read in one step: `422 validation_failed` with `in:
//! "query"`.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use serde_json::error::Category;

use crate::error::{ApiError, ErrorCode};

/// How long the body of a request may take to arrive. The outermost layer
/// puts it into the request's extensions; without it there is no deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyDeadline(pub(crate) Duration);

/// Whether a request says that its body is `application/json`. A `+json`
/// type is another type: a problem document, a vendor's format.
fn says_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

fn unsupported() -> ApiError {
    ApiError::new(
        ErrorCode::UnsupportedMediaType,
        "the body must be sent with Content-Type: application/json",
    )
}

/// The bytes of a body, within the deadline and the limit.
async fn bytes_of<S: Send + Sync>(request: Request, state: &S) -> Result<Bytes, ApiError> {
    let deadline = request.extensions().get::<BodyDeadline>().copied();
    let reading = Bytes::from_request(request, state);
    let read = match deadline {
        Some(BodyDeadline(within)) => {
            tokio::time::timeout(within, reading).await.map_err(|_| {
                ApiError::new(
                    ErrorCode::RequestTimeout,
                    "the body of the request did not arrive in time",
                )
            })?
        }
        None => reading.await,
    };
    read.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                ErrorCode::PayloadTooLarge,
                "the body of the request is larger than server.max_body_bytes",
            )
        } else {
            ApiError::new(
                ErrorCode::InvalidRequest,
                "the body of the request could not be read",
            )
        }
    })
}

/// One step of the way to a member of a document.
#[derive(Debug, Clone)]
enum Step {
    Member(String),
    Index(usize),
}

/// The way to a member, as a refusal names it: `actions[2].selector`.
fn path_of(steps: &[Step]) -> String {
    let mut path = String::new();
    for step in steps {
        match step {
            Step::Member(name) => {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(name);
            }
            Step::Index(index) => {
                path.push('[');
                path.push_str(&index.to_string());
                path.push(']');
            }
        }
    }
    path
}

/// What the walk over a document found wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Flaw {
    field: String,
    detail: &'static str,
}

/// A walk over a JSON document that keeps nothing: it only looks for a
/// member named twice and for a NUL character.
struct Walk<'a> {
    steps: &'a mut Vec<Step>,
    flaw: &'a mut Option<Flaw>,
}

impl Walk<'_> {
    fn found<E: de::Error>(self, detail: &'static str) -> E {
        *self.flaw = Some(Flaw {
            field: path_of(self.steps),
            detail,
        });
        E::custom(detail)
    }
}

const NUL: &str = "holds a NUL character, which can be neither kept nor sent";
const TWICE: &str = "is given twice; give every member once";

impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        if text.contains('\0') {
            return Err(self.found(NUL));
        }
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0;
        loop {
            self.steps.push(Step::Index(index));
            let next = seq.next_element_seed(Walk {
                steps: &mut *self.steps,
                flaw: &mut *self.flaw,
            });
            self.steps.pop();
            if next?.is_none() {
                return Ok(());
            }
            index += 1;
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(name) = map.next_key::<String>()? {
            self.steps.push(Step::Member(name.clone()));
            let flaw = if name.contains('\0') {
                Some(NUL)
            } else if !seen.insert(name) {
                Some(TWICE)
            } else {
                None
            };
            if let Some(detail) = flaw {
                let error = Walk {
                    steps: &mut *self.steps,
                    flaw: &mut *self.flaw,
                }
                .found(detail);
                self.steps.pop();
                return Err(error);
            }
            let value = map.next_value_seed(Walk {
                steps: &mut *self.steps,
                flaw: &mut *self.flaw,
            });
            self.steps.pop();
            value?;
        }
        Ok(())
    }
}

/// A `serde_json` message without the position it adds: the position is
/// the parser's, the path names the member.
fn message_of(error: &serde_json::Error) -> String {
    let text = error.to_string();
    let position = format!(" at line {} column {}", error.line(), error.column());
    text.strip_suffix(&position).unwrap_or(&text).to_owned()
}

/// Checks a body's document: well-formed JSON (`invalid_request`), no
/// member given twice and no NUL in a string or a name (`validation_failed`
/// naming the member).
///
/// # Errors
///
/// The refusal, as above.
pub(crate) fn check_document(bytes: &[u8]) -> Result<(), ApiError> {
    let mut steps = Vec::new();
    let mut flaw = None;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let walked = Walk {
        steps: &mut steps,
        flaw: &mut flaw,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    if let Some(Flaw { field, detail }) = flaw {
        let detail = if field.is_empty() {
            format!("the body {detail}")
        } else {
            format!("{field}: {detail}")
        };
        let error = ApiError::new(ErrorCode::ValidationFailed, detail);
        return Err(if field.is_empty() {
            error
        } else {
            error.with("field", field)
        });
    }
    walked.map_err(|error| {
        ApiError::new(
            ErrorCode::InvalidRequest,
            format!(
                "the body is not well-formed JSON: {} (line {}, column {})",
                message_of(&error),
                error.line(),
                error.column()
            ),
        )
    })
}

/// What a value that is well-formed JSON and not of the type an endpoint
/// takes is answered with: `validation_failed`, the member's path as
/// `field` (none for the document as a whole).
pub(crate) fn type_refused(error: &serde_path_to_error::Error<serde_json::Error>) -> ApiError {
    let path = error.path().to_string();
    let inner = error.inner();
    let what = match inner.classify() {
        Category::Data => message_of(inner),
        // The document is well formed, so what the parser calls a syntax
        // error is a value of the wrong kind: a null or a number where a
        // name of an enumeration belongs.
        Category::Syntax | Category::Eof | Category::Io => {
            "not a value this member takes".to_owned()
        }
    };
    if path == "." {
        ApiError::new(ErrorCode::ValidationFailed, what)
    } else {
        ApiError::new(ErrorCode::ValidationFailed, format!("{path}: {what}")).with("field", path)
    }
}

/// Reads a value of type `T` from a document that passed
/// [`check_document`], naming the member a refusal is about.
///
/// # Errors
///
/// `validation_failed` naming the member.
pub(crate) fn typed<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    serde_path_to_error::deserialize(&mut deserializer).map_err(|error| type_refused(&error))
}

/// Reads a value of type `T` from a JSON value, naming the member a refusal
/// is about (relative to the value).
///
/// # Errors
///
/// `validation_failed` naming the member.
pub(crate) fn typed_value<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_path_to_error::deserialize(value).map_err(|error| type_refused(&error))
}

/// The body of a request, read as the module documentation says.
async fn read_json<S, T>(request: Request, state: &S) -> Result<T, ApiError>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    if !says_json(request.headers()) {
        return Err(unsupported());
    }
    let bytes = bytes_of(request, state).await?;
    check_document(&bytes)?;
    typed(&bytes)
}

/// A JSON body.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Body<T>(pub(crate) T);

impl<S, T> FromRequest<S> for Body<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        read_json(request, state).await.map(Self)
    }
}

/// A JSON body that may be left out: no body at all, or one of nothing but
/// white space, is `None` whatever its media type says. A body that is
/// there is read as [`Body`] reads one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionalBody<T>(pub(crate) Option<T>);

impl<S, T> FromRequest<S> for OptionalBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let json = says_json(request.headers());
        let bytes = bytes_of(request, state).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(None));
        }
        if !json {
            return Err(unsupported());
        }
        check_document(&bytes)?;
        typed(&bytes).map(|value| Self(Some(value)))
    }
}

/// The parameters of a query string.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Params<T>(pub(crate) T);

impl<S, T> FromRequestParts<S> for Params<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(query_refused(&rejection)),
        }
    }
}

fn query_refused(rejection: &QueryRejection) -> ApiError {
    ApiError::new(ErrorCode::ValidationFailed, rejection.body_text()).with("in", "query")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body as HttpBody;
    use axum::extract::{FromRequest, FromRequestParts};
    use axum::http::Request;
    use axum::http::header::CONTENT_TYPE;
    use serde::Deserialize;
    use serde_json::json;

    use super::{
        Body, BodyDeadline, OptionalBody, Params, check_document, path_of, typed, typed_value,
    };
    use crate::error::{ApiError, ErrorCode};

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Wanted {
        url: String,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        mode: Mode,
        #[serde(default)]
        inner: Option<Inner>,
    }

    #[derive(Debug, Default, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        #[default]
        Fast,
        Slow,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct Inner {
        list: Vec<Mode>,
    }

    fn wanted(url: &str) -> Wanted {
        Wanted {
            url: url.to_owned(),
            limit: None,
            mode: Mode::Fast,
            inner: None,
        }
    }

    async fn read(content_type: Option<&str>, text: &str) -> Result<Wanted, ApiError> {
        let mut request = Request::post("/");
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        let request = request.body(HttpBody::from(text.to_owned())).unwrap();
        Body::<Wanted>::from_request(request, &())
            .await
            .map(|Body(wanted)| wanted)
    }

    async fn body(content_type: Option<&str>, text: &str) -> Result<Wanted, ErrorCode> {
        read(content_type, text).await.map_err(|error| error.code())
    }

    fn field(error: &ApiError) -> Option<String> {
        error
            .problem()
            .extensions
            .get("field")
            .and_then(|field| field.as_str().map(str::to_owned))
    }

    #[tokio::test]
    async fn a_body_is_read_or_refused_with_a_code() {
        let json = Some("application/json");

        assert_eq!(
            body(json, r#"{"url":"https://example.org/"}"#).await,
            Ok(wanted("https://example.org/"))
        );
        assert_eq!(
            body(Some("application/json; charset=utf-8"), r#"{"url":"x"}"#).await,
            Ok(wanted("x"))
        );
        // Not JSON at all.
        assert_eq!(body(json, "{").await, Err(ErrorCode::InvalidRequest));
        assert_eq!(
            body(json, r#"{"url":"x"} {}"#).await,
            Err(ErrorCode::InvalidRequest)
        );
        // JSON, and not what the endpoint takes.
        assert_eq!(
            body(json, r#"{"url":"x","locale":"de-DE"}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
        assert_eq!(
            body(json, r#"{"url":7}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
        // No type, another type, and a `+json` type are not JSON (F134).
        for other in [
            None,
            Some("text/plain"),
            Some("application/problem+json"),
            Some("application/vnd.x+json"),
            Some("application/x-www-form-urlencoded"),
        ] {
            assert_eq!(
                body(other, r#"{"url":"x"}"#).await,
                Err(ErrorCode::UnsupportedMediaType),
                "{other:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_of_the_type_names_the_member_a_null_and_a_number_for_a_name_included() {
        // F115: a null or a wrong type for an enumerated member is the
        // member's, not a body that does not parse.
        for (text, member) in [
            (r#"{"url":"x","mode":null}"#, "mode"),
            (r#"{"url":"x","mode":5}"#, "mode"),
            (r#"{"url":"x","mode":"zzq"}"#, "mode"),
            (r#"{"url":"x","limit":"many"}"#, "limit"),
            (r#"{"url":"x","inner":{"list":[5]}}"#, "inner.list[0]"),
            (r#"{"url":"x","inner":{"list":["fast"],"x":1}}"#, "inner.x"),
            (r#"{"url":"x","locale":"de-DE"}"#, "locale"),
            (r#"{"url":null}"#, "url"),
        ] {
            let error = read(Some("application/json"), text).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::ValidationFailed, "{text}");
            assert_eq!(field(&error).as_deref(), Some(member), "{text}");
            assert!(
                error.detail().starts_with(&format!("{member}: ")),
                "{text}: {}",
                error.detail()
            );
            assert!(!error.detail().contains("line 1"), "{}", error.detail());
        }
        // A member that is missing is the document's.
        let missing = read(Some("application/json"), "{}").await.unwrap_err();
        assert_eq!(missing.code(), ErrorCode::ValidationFailed);
        assert!(missing.detail().contains("url"), "{}", missing.detail());
    }

    #[tokio::test]
    async fn a_nul_and_a_member_given_twice_are_refused_naming_the_member() {
        // F112: a NUL can be neither kept nor sent; F134: one member, once.
        for (text, member, nul) in [
            (r#"{"url":"a\u0000b"}"#, "url", true),
            (
                r#"{"url":"x","inner":{"list":["fast","sl\u0000ow"]}}"#,
                "inner.list[1]",
                true,
            ),
            (r#"{"url":"x","h":{"X-A\u0000":"1"}}"#, "h.X-A\u{0}", true),
            (r#"{"url":"x","url":"y"}"#, "url", false),
            (r#"{"url":"x","h":{"X-A":"1","X-A":"2"}}"#, "h.X-A", false),
            (r#"[{"a":1},{"a":1,"a":2}]"#, "[1].a", false),
        ] {
            let error = check_document(text.as_bytes()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::ValidationFailed, "{text}");
            assert_eq!(field(&error).as_deref(), Some(member), "{text}");
            let said = if nul { "NUL" } else { "twice" };
            assert!(error.detail().contains(said), "{}", error.detail());
        }
        assert_eq!(
            check_document(b"\"a\\u0000\"").unwrap_err().code(),
            ErrorCode::ValidationFailed
        );
        // Equal members of two objects, and equal values, are no repeat.
        check_document(br#"{"a":{"x":1},"b":{"x":1},"c":["y","y"],"d":null,"e":-1.5,"f":true}"#)
            .unwrap();
        assert_eq!(
            check_document(b"{\"a\":").unwrap_err().code(),
            ErrorCode::InvalidRequest
        );
        // Through the extractor: nothing of it is read as the type.
        assert_eq!(
            body(Some("application/json"), r#"{"url":"x","url":"y"}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
    }

    #[tokio::test]
    async fn a_body_that_is_too_large_or_too_slow_is_a_problem() {
        let request = Request::post("/")
            .header(CONTENT_TYPE, "application/json")
            .body(HttpBody::from(format!(
                "{{\"url\":\"{}\"}}",
                "x".repeat(100)
            )))
            .unwrap();
        let (mut parts, body) = request.into_parts();
        parts
            .extensions
            .insert(BodyDeadline(Duration::from_secs(5)));
        // What the router's `DefaultBodyLimit` makes of a body.
        let request =
            Request::from_parts(parts, HttpBody::new(http_body_util::Limited::new(body, 10)));
        let error = Body::<Wanted>::from_request(request, &())
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::PayloadTooLarge);

        // A body that never ends is cut at the deadline.
        let (sender, stream) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
        let mut request = Request::post("/")
            .header(CONTENT_TYPE, "application/json")
            .body(HttpBody::from_stream(tokio_stream_of(stream)))
            .unwrap();
        request
            .extensions_mut()
            .insert(BodyDeadline(Duration::from_millis(50)));
        let error = Body::<Wanted>::from_request(request, &())
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::RequestTimeout);
        drop(sender);
    }

    fn tokio_stream_of(
        mut receiver: tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, std::io::Error>>,
    ) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> {
        futures_util::stream::poll_fn(move |context| receiver.poll_recv(context))
    }

    #[tokio::test]
    async fn an_optional_body_may_be_left_out_and_is_read_when_it_is_there() {
        let optional = async |content_type: Option<&str>, text: &str| {
            let mut request = Request::post("/");
            if let Some(content_type) = content_type {
                request = request.header(CONTENT_TYPE, content_type);
            }
            let request = request.body(HttpBody::from(text.to_owned())).unwrap();
            OptionalBody::<Wanted>::from_request(request, &())
                .await
                .map(|OptionalBody(wanted)| wanted)
                .map_err(|error| error.code())
        };
        assert_eq!(optional(None, "").await, Ok(None));
        assert_eq!(optional(Some("text/plain"), " \n").await, Ok(None));
        assert_eq!(optional(Some("application/json"), "").await, Ok(None));
        assert_eq!(
            optional(Some("application/json"), r#"{"url":"x"}"#).await,
            Ok(Some(wanted("x")))
        );
        // F113: a body that is there is read by the rules of every other.
        assert_eq!(
            optional(Some("text/plain"), "{}").await,
            Err(ErrorCode::UnsupportedMediaType)
        );
        assert_eq!(
            optional(None, "{}").await,
            Err(ErrorCode::UnsupportedMediaType)
        );
        assert_eq!(
            optional(Some("application/json"), "{").await,
            Err(ErrorCode::InvalidRequest)
        );
        assert_eq!(
            optional(Some("application/json"), r#"{"url":5}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
    }

    #[test]
    fn a_value_is_read_naming_the_member() {
        let error = typed_value::<Wanted>(json!({ "url": "x", "limit": -1 })).unwrap_err();
        assert_eq!(field(&error).as_deref(), Some("limit"));
        assert_eq!(
            typed::<Wanted>(br#"{"url":"x","mode":"slow"}"#)
                .unwrap()
                .mode,
            Mode::Slow
        );
        assert_eq!(
            typed_value::<Wanted>(json!({ "url": "x", "mode": "fast" }))
                .unwrap()
                .mode,
            Mode::Fast
        );
        // A refusal of the whole has no field.
        let whole = typed_value::<Wanted>(json!("a text")).unwrap_err();
        assert!(field(&whole).is_none());
        assert_eq!(path_of(&[]), "");
    }

    #[tokio::test]
    async fn a_query_is_read_or_refused_with_a_code() {
        let params = async |uri: &str| {
            let (mut parts, ()) = Request::get(uri).body(()).unwrap().into_parts();
            Params::<Wanted>::from_request_parts(&mut parts, &())
                .await
                .map(|Params(wanted)| wanted)
        };

        assert_eq!(
            params("/?url=a&limit=5").await.unwrap(),
            Wanted {
                limit: Some(5),
                ..wanted("a")
            }
        );
        let refused = params("/?url=a&limit=many").await.unwrap_err();
        assert_eq!(refused.code(), ErrorCode::ValidationFailed);
        assert_eq!(refused.problem().extensions["in"], "query");
        assert_eq!(
            params("/?limit=1").await.unwrap_err().code(),
            ErrorCode::ValidationFailed
        );
    }
}
