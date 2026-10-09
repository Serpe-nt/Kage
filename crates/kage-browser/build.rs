//! Generates the Rust types of the sidecar contract from the committed
//! OpenAPI document (spec §10.2).
//!
//! `sidecar/packages/contract/openapi.json` is emitted from the zod schemas
//! of the sidecar and committed. This script turns its component schemas
//! into Rust types at build time, so the two sides cannot drift: a contract
//! change that is not reflected here does not compile, and a schema this
//! script does not understand stops the build with its name.
//!
//! The document stays inside a small dialect, which the sidecar's own tests
//! enforce (`findDialectViolations` in the contract package):
//!
//! | In the document | In Rust |
//! |---|---|
//! | a string `enum` | a fieldless enum with `as_str` |
//! | a strict object | a struct; a property that is not required is an `Option` left out when `None` |
//! | `oneOf` of objects sharing a constant property | an enum tagged with that property |
//! | `anyOf` of a schema and `null`, or a type list with `"null"` | `Option` |
//! | `string`, with `format: uuid` or `date-time` | `String`, `uuid::Uuid`, `chrono::DateTime<Utc>` |
//! | `integer`, with a minimum of zero or more | `i64`, `u64` |
//! | `number`, `boolean`, `array` | `f64`, `bool`, `Vec` |
//! | an object with `additionalProperties` | `serde_json::Map` |
//! | `{}` | `serde_json::Value` |
//!
//! Every description becomes a doc comment.

// A build script talks to cargo through stdout and stops on what it cannot
// read: there is nobody to return an error to.
#![allow(clippy::print_stdout, clippy::panic, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

use serde_json::{Map, Value};

const SCHEMA_PREFIX: &str = "#/components/schemas/";

const KEYWORDS: [&str; 20] = [
    "type", "ref", "self", "match", "move", "loop", "use", "mod", "fn", "struct", "enum", "impl",
    "trait", "where", "as", "in", "for", "if", "else", "box",
];

struct Generator<'a> {
    schemas: &'a Map<String, Value>,
    /// Schemas that only exist as a variant of a tagged union.
    variants: BTreeSet<String>,
    out: String,
}

fn object<'a>(value: &'a Value, what: &str) -> &'a Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{what} is not an object"))
}

fn docs(out: &mut String, indent: &str, text: &str) {
    for line in text.lines() {
        if line.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

fn pascal(value: &str) -> String {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + characters.as_str()
            })
        })
        .collect()
}

fn field_name(name: &str) -> String {
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

fn reference(schema: &Map<String, Value>) -> Option<&str> {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|target| target.strip_prefix(SCHEMA_PREFIX))
}

impl Generator<'_> {
    /// The Rust type of a schema below a named one, and whether the schema
    /// allows `null`.
    fn value_type(&self, schema: &Value, at: &str) -> (String, bool) {
        let schema = object(schema, at);
        if let Some(name) = reference(schema) {
            assert!(
                self.schemas.contains_key(name),
                "{at} refers to the unknown schema {name}"
            );
            return (name.to_owned(), false);
        }
        if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
            let others: Vec<&Value> = options
                .iter()
                .filter(|option| option.get("type").and_then(Value::as_str) != Some("null"))
                .collect();
            assert!(
                options.len() == 2 && others.len() == 1,
                "{at}: anyOf is only supported as \"a schema or null\""
            );
            return (self.value_type(others[0], at).0, true);
        }
        let (kind, nullable) = match schema.get("type") {
            None => return ("serde_json::Value".to_owned(), false),
            Some(Value::String(kind)) => (kind.as_str(), false),
            Some(Value::Array(kinds)) => {
                let named: Vec<&str> = kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|kind| *kind != "null")
                    .collect();
                assert!(
                    kinds.len() == 2 && named.len() == 1,
                    "{at}: a type list is only supported as [type, \"null\"]"
                );
                (named[0], true)
            }
            Some(other) => panic!("{at}: unsupported type {other}"),
        };
        let rust = match kind {
            "string" => match schema.get("format").and_then(Value::as_str) {
                Some("uuid") => "uuid::Uuid".to_owned(),
                Some("date-time") => "chrono::DateTime<chrono::Utc>".to_owned(),
                _ => "String".to_owned(),
            },
            "integer" => {
                let minimum = schema
                    .get("minimum")
                    .or_else(|| schema.get("exclusiveMinimum"))
                    .and_then(Value::as_f64);
                if minimum.is_some_and(|minimum| minimum >= 0.0) {
                    "u64".to_owned()
                } else {
                    "i64".to_owned()
                }
            }
            "number" => "f64".to_owned(),
            "boolean" => "bool".to_owned(),
            "array" => {
                let items = schema
                    .get("items")
                    .unwrap_or_else(|| panic!("{at}: an array without items"));
                let (inner, inner_nullable) = self.value_type(items, at);
                if inner_nullable {
                    format!("Vec<Option<{inner}>>")
                } else {
                    format!("Vec<{inner}>")
                }
            }
            "object" => {
                assert!(
                    !schema.contains_key("properties"),
                    "{at}: an inline object; give it an id so that it becomes a named schema"
                );
                assert!(
                    schema.contains_key("additionalProperties"),
                    "{at}: an object without a shape"
                );
                "serde_json::Map<String, serde_json::Value>".to_owned()
            }
            other => panic!("{at}: unsupported type {other}"),
        };
        (rust, nullable)
    }

    /// Writes the fields of an object schema, leaving out `skip`.
    fn fields(
        &mut self,
        name: &str,
        schema: &Map<String, Value>,
        indent: &str,
        public: bool,
        skip: Option<&str>,
    ) {
        let required: BTreeSet<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let properties = schema.get("properties").map_or_else(
            || panic!("{name} has no properties"),
            |properties| object(properties, name),
        );
        for (property, value) in properties {
            if skip == Some(property.as_str()) {
                continue;
            }
            let at = format!("{name}.{property}");
            let description = value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{at} has no description"));
            let (rust, nullable) = self.value_type(value, &at);
            let optional = !required.contains(property.as_str());
            docs(&mut self.out, indent, description);
            if optional {
                let _ = writeln!(
                    self.out,
                    "{indent}#[serde(default, skip_serializing_if = \"Option::is_none\")]"
                );
            }
            let rust = if optional || nullable {
                format!("Option<{rust}>")
            } else {
                rust
            };
            let visibility = if public { "pub " } else { "" };
            let _ = writeln!(
                self.out,
                "{indent}{visibility}{}: {rust},",
                field_name(property)
            );
        }
    }

    fn string_enum(&mut self, name: &str, schema: &Map<String, Value>, description: &str) {
        let values: Vec<&str> = schema["enum"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        docs(&mut self.out, "", description);
        let _ = writeln!(
            self.out,
            "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]\n\
             pub enum {name} {{"
        );
        for value in &values {
            let _ = writeln!(
                self.out,
                "    /// `{value}`.\n    #[serde(rename = {value:?})]\n    {},",
                pascal(value)
            );
        }
        let _ = writeln!(self.out, "}}\n\nimpl {name} {{");
        let _ = writeln!(
            self.out,
            "    /// Every value, in the order of the contract.\n    pub const ALL: &'static [Self] = &[{}];\n",
            values
                .iter()
                .map(|value| format!("Self::{}", pascal(value)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(
            self.out,
            "    /// The value as the contract writes it.\n    pub const fn as_str(self) -> &'static str {{\n        match self {{"
        );
        for value in &values {
            let _ = writeln!(
                self.out,
                "            Self::{} => {value:?},",
                pascal(value)
            );
        }
        let _ = writeln!(
            self.out,
            "        }}\n    }}\n}}\n\nimpl std::fmt::Display for {name} {{\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{\n        f.write_str(self.as_str())\n    }}\n}}\n"
        );
    }

    fn structure(&mut self, name: &str, schema: &Map<String, Value>, description: &str) {
        docs(&mut self.out, "", description);
        let _ = writeln!(
            self.out,
            "#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]\npub struct {name} {{"
        );
        self.fields(name, schema, "    ", true, None);
        let _ = writeln!(self.out, "}}\n");
    }

    /// The property every option of a union gives a constant value.
    fn discriminator(&self, name: &str, options: &[&str]) -> String {
        let first = object(&self.schemas[options[0]], options[0]);
        let properties = object(&first["properties"], options[0]);
        properties
            .keys()
            .find(|property| {
                options.iter().all(|option| {
                    self.schemas[*option]["properties"][property.as_str()]["const"].is_string()
                })
            })
            .cloned()
            .unwrap_or_else(|| panic!("{name}: the oneOf options share no constant property"))
    }

    fn union(&mut self, name: &str, schema: &Map<String, Value>, description: &str) {
        let options: Vec<&str> = schema["oneOf"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .map(|option| {
                        reference(object(option, name))
                            .unwrap_or_else(|| panic!("{name}: a oneOf option is not a $ref"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tag = self.discriminator(name, &options);
        docs(&mut self.out, "", description);
        let _ = writeln!(
            self.out,
            "#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]\n\
             #[serde(tag = {tag:?})]\n\
             #[allow(clippy::large_enum_variant)]\n\
             pub enum {name} {{"
        );
        for option in options {
            let variant = object(&self.schemas[option], option).clone();
            let value = variant["properties"][tag.as_str()]["const"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let variant_description = variant
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{option} has no description"))
                .to_owned();
            docs(&mut self.out, "    ", &variant_description);
            let _ = writeln!(
                self.out,
                "    #[serde(rename = {value:?})]\n    {} {{",
                pascal(&value)
            );
            self.fields(option, &variant, "        ", false, Some(&tag));
            let _ = writeln!(self.out, "    }},");
        }
        let _ = writeln!(self.out, "}}\n");
    }

    fn schema(&mut self, name: &str) {
        let schema = object(&self.schemas[name], name).clone();
        let description = schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{name} has no description"))
            .to_owned();
        if schema.contains_key("oneOf") {
            self.union(name, &schema, &description);
        } else if schema.contains_key("enum") {
            self.string_enum(name, &schema, &description);
        } else if schema.get("type").and_then(Value::as_str) == Some("object") {
            self.structure(name, &schema, &description);
        } else {
            panic!("{name} is neither an object, a string enum nor a oneOf");
        }
    }
}

/// The operations of the document: id, method and path.
fn operations(document: &Value) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    for (path, item) in object(&document["paths"], "paths") {
        for (method, operation) in object(item, path) {
            let id = operation["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
            found.push((id.to_owned(), method.to_uppercase(), path.clone()));
        }
    }
    found
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let source = Path::new(&manifest).join("../../sidecar/packages/contract/openapi.json");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-changed=build.rs");

    let text = fs::read_to_string(&source)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source.display()));
    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", source.display()));
    let schemas = object(&document["components"]["schemas"], "components.schemas");

    // A schema that is only ever an option of a union becomes a variant of
    // that union's enum and no type of its own.
    let mut variants = BTreeSet::new();
    for schema in schemas.values() {
        if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
            for option in options {
                if let Some(name) = reference(object(option, "oneOf")) {
                    variants.insert(name.to_owned());
                }
            }
        }
    }

    let mut generator = Generator {
        schemas,
        variants,
        out: String::from(
            "// Generated by build.rs from sidecar/packages/contract/openapi.json. Do not edit.\n\n",
        ),
    };
    let names: Vec<String> = schemas
        .keys()
        .filter(|name| !generator.variants.contains(*name))
        .cloned()
        .collect();
    for name in &names {
        generator.schema(name);
    }
    let mut out = generator.out;

    let version = document["info"]["version"]
        .as_str()
        .expect("info.version is a string");
    let header = document["components"]["securitySchemes"]["sidecarToken"]["name"]
        .as_str()
        .expect("the sidecarToken security scheme names a header");
    let _ = writeln!(
        out,
        "/// Semantic version of the contract these types were generated from.\n\
         pub const CONTRACT_VERSION: &str = {version:?};\n\n\
         /// Header that carries the shared secret.\n\
         pub const TOKEN_HEADER: &str = {header:?};\n\n\
         /// Every type generated from a component schema.\n\
         pub const SCHEMA_NAMES: &[&str] = &{names:?};\n"
    );

    let operations = operations(&document);
    let _ = writeln!(
        out,
        "/// The operations of the contract: `(operation id, method, path)`. A path\n\
         /// holds `{{id}}` where the context id goes.\n\
         pub const OPERATIONS: &[(&str, &str, &str)] = &{operations:?};\n"
    );

    // The examples of the document, read into the generated types and
    // written back: what the contract test compares.
    let _ = writeln!(
        out,
        "/// Reads a value into the type generated for `schema` and writes it back.\n\
         /// `None` for a name that has no type. The contract test uses it to\n\
         /// prove that every example of the document survives the Rust types\n\
         /// unchanged.\n\
         #[doc(hidden)]\n\
         pub fn roundtrip(schema: &str, value: serde_json::Value) -> Option<Result<serde_json::Value, serde_json::Error>> {{\n    Some(match schema {{"
    );
    for name in &names {
        let _ = writeln!(
            out,
            "        {name:?} => serde_json::from_value::<{name}>(value).and_then(serde_json::to_value),"
        );
    }
    let _ = writeln!(out, "        _ => return None,\n    }})\n}}");

    let examples: BTreeMap<&String, &Value> = schemas
        .iter()
        .filter_map(|(name, schema)| schema.get("examples").map(|examples| (name, examples)))
        .collect();
    let _ = writeln!(
        out,
        "\n/// The examples of the document as JSON: schema name to a list of values.\n\
         #[doc(hidden)]\n\
         pub const EXAMPLES_JSON: &str = {:?};",
        serde_json::to_string(&examples).expect("examples serialize")
    );

    let target = Path::new(&env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("contract.rs");
    fs::write(&target, out)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", target.display()));
}
