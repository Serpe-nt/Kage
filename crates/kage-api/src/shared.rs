//! The answers every operation of a kind shares, written into the OpenAPI
//! document once and named by each operation (F114).
//!
//! An operation documents its own answers; these are the ones the layers
//! around it give, which no handler writes:
//!
//! | Status | On | From |
//! |---|---|---|
//! | `401`, `403`, `429`, `500` | every operation that takes a key | the key, its scope, its limits; a store that does not answer |
//! | `400` | an operation with a body or a path parameter | a body that is not JSON, a path parameter that cannot be read |
//! | `408` | an operation with a body, or one with a deadline | `server.body_read_timeout`, `server.request_timeout` |
//! | `415`, `422` | an operation with a body | the media type, the members |
//! | `413`, `414`, `431` | every operation | the limits of spec §19 on a request's head and its stated length |
//!
//! An answer an operation already documents keeps its own description.

use utoipa::openapi::header::HeaderBuilder;
use utoipa::openapi::path::{Operation, ParameterIn};
use utoipa::openapi::response::{Response, ResponseBuilder};
use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat, SchemaType, Type};
use utoipa::openapi::{ContentBuilder, OpenApi, Ref, RefOr};

use crate::error::PROBLEM_JSON;

/// The operations that run without the deadline of `server.request_timeout`
/// and take no body: nothing of theirs is answered `408`.
const WITHOUT_DEADLINE: [&str; 1] = ["check_proxy"];

/// One shared answer: its name among the document's responses, its status
/// and what it says.
struct Shared {
    name: &'static str,
    status: &'static str,
    description: &'static str,
}

const UNAUTHORIZED: Shared = Shared {
    name: "Unauthorized",
    status: "401",
    description: "`missing_key`, `invalid_key` or `key_disabled`: the request carries no valid \
                  API key.",
};
const FORBIDDEN: Shared = Shared {
    name: "Forbidden",
    status: "403",
    description: "`scope_missing`: the key lacks a scope this operation needs; `scope` names it.",
};
const TOO_MANY: Shared = Shared {
    name: "TooManyRequests",
    status: "429",
    description: "`rate_limited`, `concurrency_limited` or `budget_exhausted`: a limit of the key \
                  or of the API; `Retry-After` and `retry_after_s` say how long to wait.",
};
const INTERNAL: Shared = Shared {
    name: "InternalError",
    status: "500",
    description: "`internal`: something failed inside Kage, the store most likely; the log has \
                  the cause under the request id.",
};
const BAD_REQUEST: Shared = Shared {
    name: "BadRequest",
    status: "400",
    description: "`invalid_request`: the body is not well-formed JSON, or a path parameter cannot \
                  be read (an escape that is no UTF-8).",
};
const TIMEOUT: Shared = Shared {
    name: "RequestTimeout",
    status: "408",
    description: "`request_timeout`: the body did not arrive within `server.body_read_timeout`, \
                  or the answer was not ready within `server.request_timeout`.",
};
const UNSUPPORTED: Shared = Shared {
    name: "UnsupportedMediaType",
    status: "415",
    description: "`unsupported_media_type`: a body is sent as `application/json`, and as no \
                  other type (a `+json` type included).",
};
const UNPROCESSABLE: Shared = Shared {
    name: "UnprocessableEntity",
    status: "422",
    description: "`validation_failed`, or another code of a request that cannot be made as \
                  asked: `field` names the member (a member given twice and a NUL character \
                  in a string included).",
};
const TOO_LARGE: Shared = Shared {
    name: "PayloadTooLarge",
    status: "413",
    description: "`payload_too_large`: the body is larger than `server.max_body_bytes`, or its \
                  `Content-Length` says so, on every operation.",
};
const URI_TOO_LONG: Shared = Shared {
    name: "UriTooLong",
    status: "414",
    description: "`uri_too_long`: the request target is longer than `server.max_uri_bytes`.",
};
const HEADERS_TOO_LARGE: Shared = Shared {
    name: "HeadersTooLarge",
    status: "431",
    description: "`headers_too_large`: more headers than `server.max_headers`, or longer than \
                  `server.max_header_bytes`. A head beyond twice those limits is cut off by \
                  the HTTP layer itself: an empty `431` without `X-Request-Id`.",
};

const ALL: [&Shared; 11] = [
    &UNAUTHORIZED,
    &FORBIDDEN,
    &TOO_MANY,
    &INTERNAL,
    &BAD_REQUEST,
    &TIMEOUT,
    &UNSUPPORTED,
    &UNPROCESSABLE,
    &TOO_LARGE,
    &URI_TOO_LONG,
    &HEADERS_TOO_LARGE,
];

/// A problem document's answer.
fn problem(shared: &Shared) -> Response {
    let mut response = ResponseBuilder::new()
        .description(shared.description)
        .content(
            PROBLEM_JSON,
            ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("Problem")))
                .build(),
        );
    if shared.status == "429" {
        response = response.header(
            "Retry-After",
            HeaderBuilder::new()
                .schema(Some(
                    ObjectBuilder::new()
                        .schema_type(SchemaType::Type(Type::Integer))
                        .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int64))),
                ))
                .description(Some("Seconds to wait before asking again."))
                .build(),
        );
    }
    if shared.status == "401" {
        response = response.header(
            "WWW-Authenticate",
            HeaderBuilder::new()
                .schema(Some(
                    ObjectBuilder::new().schema_type(SchemaType::Type(Type::String)),
                ))
                .description(Some("`Bearer realm=\"kage\"`."))
                .build(),
        );
    }
    response.build()
}

/// Names a shared answer on an operation that does not document the
/// status itself.
fn name(operation: &mut Operation, shared: &Shared) {
    operation
        .responses
        .responses
        .entry(shared.status.to_owned())
        .or_insert_with(|| RefOr::Ref(Ref::new(format!("#/components/responses/{}", shared.name))));
}

/// What the layers around one operation answer besides it.
fn complete_operation(operation: &mut Operation) {
    let keyed = operation
        .security
        .as_ref()
        .is_some_and(|requirements| !requirements.is_empty());
    let body = operation.request_body.is_some();
    let path_parameter = operation.parameters.iter().flatten().any(|parameter| {
        matches!(parameter, RefOr::T(parameter) if parameter.parameter_in == ParameterIn::Path)
    });
    let deadline =
        !WITHOUT_DEADLINE.contains(&operation.operation_id.as_deref().unwrap_or_default());
    if keyed {
        for shared in [&UNAUTHORIZED, &FORBIDDEN, &TOO_MANY, &INTERNAL] {
            name(operation, shared);
        }
        if body || deadline {
            name(operation, &TIMEOUT);
        }
    }
    if body || path_parameter {
        name(operation, &BAD_REQUEST);
    }
    if body {
        for shared in [&UNSUPPORTED, &UNPROCESSABLE] {
            name(operation, shared);
        }
    }
    for shared in [&TOO_LARGE, &URI_TOO_LONG, &HEADERS_TOO_LARGE] {
        name(operation, shared);
    }
}

/// Writes the shared answers into the document and names them on every
/// operation they apply to.
pub(crate) fn complete(openapi: &mut OpenApi) {
    let components = openapi.components.get_or_insert_with(Default::default);
    for shared in ALL {
        components
            .responses
            .insert(shared.name.to_owned(), RefOr::T(problem(shared)));
    }
    for item in openapi.paths.paths.values_mut() {
        for operation in [
            &mut item.get,
            &mut item.put,
            &mut item.post,
            &mut item.delete,
            &mut item.options,
            &mut item.head,
            &mut item.patch,
            &mut item.trace,
        ]
        .into_iter()
        .flatten()
        {
            complete_operation(operation);
        }
    }
}

/// The bytes of a file, in whatever media type it has: what the document
/// says of an artifact (`GET /v1/artifacts/{id}`), which is a document, an
/// image, a PDF or a body as the site sent it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binary;

impl utoipa::PartialSchema for Binary {
    fn schema() -> RefOr<utoipa::openapi::schema::Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::String))
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
            .description(Some(
                "The bytes as they were kept, in the media type `Content-Type` names.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for Binary {}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::openapi;

    fn statuses(document: &Value, path: &str, method: &str) -> Vec<String> {
        document["paths"][path][method]["responses"]
            .as_object()
            .unwrap_or_else(|| panic!("{method} {path}"))
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn every_operation_names_what_the_layers_around_it_answer() {
        let document = serde_json::to_value(openapi()).unwrap();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let has = statuses(&document, path, method);
                let keyed = operation["security"]
                    .as_array()
                    .is_some_and(|requirements| !requirements.is_empty());
                let body = operation.get("requestBody").is_some();
                let mut wanted = vec!["413", "414", "431"];
                if keyed {
                    wanted.extend(["401", "403", "429", "500"]);
                }
                if body {
                    wanted.extend(["400", "408", "415", "422"]);
                }
                for status in wanted {
                    assert!(
                        has.iter().any(|known| known == status),
                        "{method} {path} does not name {status}: {has:?}"
                    );
                }
                // Every error answer is a problem document.
                for (status, answer) in operation["responses"].as_object().unwrap() {
                    if !status.starts_with('4') && !status.starts_with('5') {
                        continue;
                    }
                    let answer = match answer["$ref"].as_str() {
                        Some(reference) => {
                            let name = reference.rsplit('/').next().unwrap();
                            &document["components"]["responses"][name]
                        }
                        None => answer,
                    };
                    if path == "/readyz" {
                        continue;
                    }
                    assert!(
                        answer["content"]["application/problem+json"].is_object(),
                        "{method} {path} {status}: {answer}"
                    );
                }
            }
        }
        // An operation's own description of a status stays its own.
        let fetch = &document["paths"]["/v1/fetch"]["post"]["responses"];
        assert!(fetch["403"].get("$ref").is_none(), "{}", fetch["403"]);
        assert_eq!(
            document["paths"]["/v1/results/{id}"]["get"]["responses"]["401"]["$ref"],
            "#/components/responses/Unauthorized"
        );
        // The probe of a proxy has no deadline and no body.
        let probe = statuses(&document, "/v1/admin/proxies/{id}/health", "post");
        assert!(!probe.iter().any(|status| status == "408"), "{probe:?}");
        // A path parameter that cannot be read.
        let job = statuses(&document, "/v1/jobs/{id}", "get");
        assert!(job.iter().any(|status| status == "400"), "{job:?}");
    }

    #[test]
    fn an_artifact_is_bytes_of_any_type() {
        let document = serde_json::to_value(openapi()).unwrap();
        let ok = &document["paths"]["/v1/artifacts/{id}"]["get"]["responses"]["200"]["content"];
        let schema = &ok["*/*"]["schema"];
        assert_eq!(schema["type"], "string", "{ok}");
        assert_eq!(schema["format"], "binary", "{ok}");
        assert!(ok.get("application/octet-stream").is_none(), "{ok}");
    }
}
