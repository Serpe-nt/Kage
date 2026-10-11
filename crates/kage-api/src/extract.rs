//! Extractors whose refusals are problem documents (spec §16.1).
//!
//! axum's own `Json` and `Query` answer a body or a query they cannot read
//! with a line of plain text. These wrap them and answer with the stable
//! codes of [`crate::error`]. A body also has a deadline
//! (`server.body_read_timeout`): a client that trickles it is answered
//! `request_timeout` instead of holding a connection and a permit.

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::StatusCode;
use std::time::Duration;

use axum::Json;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::error::{ApiError, ErrorCode};

/// How long the body of a request may take to arrive. The outermost layer
/// puts it into the request's extensions; without it there is no deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BodyDeadline(pub(crate) Duration);

/// What a body that could not be read is answered with.
fn body_refused(rejection: &JsonRejection) -> ApiError {
    let detail = rejection.body_text();
    match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
            ErrorCode::PayloadTooLarge,
            "the body of the request is larger than server.max_body_bytes",
        ),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::new(
            ErrorCode::UnsupportedMediaType,
            "the body must be sent with Content-Type: application/json",
        ),
        // The JSON is well formed and is not what the endpoint takes: an
        // unknown field, a wrong type, a value outside an enumeration.
        StatusCode::UNPROCESSABLE_ENTITY => ApiError::new(ErrorCode::ValidationFailed, detail),
        _ => ApiError::new(ErrorCode::InvalidRequest, detail),
    }
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
        let deadline = request.extensions().get::<BodyDeadline>().copied();
        let reading = Json::<T>::from_request(request, state);
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
        match read {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(body_refused(&rejection)),
        }
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
    use axum::body::Body as HttpBody;
    use axum::extract::{FromRequest, FromRequestParts};
    use axum::http::Request;
    use axum::http::header::CONTENT_TYPE;
    use serde::Deserialize;

    use super::{Body, Params};
    use crate::error::ErrorCode;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Wanted {
        url: String,
        #[serde(default)]
        limit: Option<u32>,
    }

    async fn body(content_type: Option<&str>, text: &str) -> Result<Wanted, ErrorCode> {
        let mut request = Request::post("/");
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        let request = request.body(HttpBody::from(text.to_owned())).unwrap();
        Body::<Wanted>::from_request(request, &())
            .await
            .map(|Body(wanted)| wanted)
            .map_err(|error| error.code())
    }

    #[tokio::test]
    async fn a_body_is_read_or_refused_with_a_code() {
        let json = Some("application/json");

        assert_eq!(
            body(json, r#"{"url":"https://example.org/"}"#).await,
            Ok(Wanted {
                url: "https://example.org/".to_owned(),
                limit: None,
            })
        );
        // Not JSON at all.
        assert_eq!(body(json, "{").await, Err(ErrorCode::InvalidRequest));
        // JSON, and not what the endpoint takes.
        assert_eq!(
            body(json, r#"{"url":"x","locale":"de-DE"}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
        assert_eq!(
            body(json, r#"{"url":7}"#).await,
            Err(ErrorCode::ValidationFailed)
        );
        assert_eq!(
            body(None, r#"{"url":"x"}"#).await,
            Err(ErrorCode::UnsupportedMediaType)
        );
        assert_eq!(
            body(Some("text/plain"), r#"{"url":"x"}"#).await,
            Err(ErrorCode::UnsupportedMediaType)
        );
    }

    #[tokio::test]
    async fn an_unknown_field_is_named_in_the_refusal() {
        let request = Request::post("/")
            .header(CONTENT_TYPE, "application/json")
            .body(HttpBody::from(r#"{"url":"x","locale":"de-DE"}"#))
            .unwrap();

        let error = Body::<Wanted>::from_request(request, &())
            .await
            .unwrap_err();

        assert!(error.detail().contains("locale"), "{}", error.detail());
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
                url: "a".to_owned(),
                limit: Some(5),
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
