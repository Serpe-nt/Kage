//! Shapes the OpenAPI document gives to JSON without fixed members.
//!
//! A bare `type: object` is "an object with no members" to the generators
//! that build typed clients from the document, and a client then cannot
//! read what the server sends. What is free-form says so.

use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{AdditionalProperties, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::{PartialSchema, ToSchema};

/// A JSON object with any members; what they are is said where it is used.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FreeForm;

impl PartialSchema for FreeForm {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::Object))
            .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(true)))
            .description(Some(
                "A JSON object with any members; what they are is said where it is used.",
            ))
            .into()
    }
}

impl ToSchema for FreeForm {}

/// Any JSON value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnyJson;

impl PartialSchema for AnyJson {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(SchemaType::AnyValue)
            .description(Some("Any JSON value."))
            .into()
    }
}

impl ToSchema for AnyJson {}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use utoipa::{PartialSchema, ToSchema};

    use super::{AnyJson, FreeForm};

    #[test]
    fn free_form_json_says_that_it_is() {
        let object = serde_json::to_value(FreeForm::schema()).unwrap();
        assert_eq!(object["type"], "object");
        assert_eq!(object["additionalProperties"], json!(true));
        assert_eq!(FreeForm::name(), "FreeForm");

        // Any value: no type at all.
        let any = serde_json::to_value(AnyJson::schema()).unwrap();
        assert!(any.get("type").is_none(), "{any}");
        assert_eq!(AnyJson::name(), "AnyJson");
    }
}
