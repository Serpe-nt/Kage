//! YAML for `kage.yaml`, the policy files and the static proxy list:
//! `serde-saphyr` behind a figment provider of Kage's own (F45), so that
//! the workspace has one YAML parser, written in Rust without `unsafe`,
//! and the `KAGE__SECTION__KEY` layering of figment stays as it is.
//!
//! How a document is read, beyond YAML 1.2's core schema:
//!
//! - only `true` and `false` are booleans (`NO` is Norway);
//! - a key said twice in one mapping is an error with its line and column;
//! - a merge key (`<<`) is an error: nothing of Kage's needs one;
//! - nesting, nodes and the bytes of scalars are bounded ([`budget`]), so a
//!   hostile or broken file is refused at once and in little memory;
//! - a message gives a line and a column but never quotes the document,
//!   which may hold credentials (the static proxy list);
//! - text that YAML reads as a number (`0123`, `1_000`, `12345`) is written
//!   in quotes where text is expected, and the message says so.

use figment::error::{Actual, Kind};
use figment::value::{Dict, Map};
use figment::{Figment, Metadata, Profile, Provider};
use serde::de::DeserializeOwned;
use serde_saphyr::options::{DuplicateKeyPolicy, MergeKeyPolicy};

/// The deepest a document nests, the top mapping counted.
pub(crate) const MAX_DEPTH: usize = 128;
/// The most nodes (mappings, sequences, scalars) of a document.
pub(crate) const MAX_NODES: usize = 1_000_000;
/// The most bytes of all scalars of a document, aliases replayed.
pub(crate) const MAX_SCALAR_BYTES: usize = 16 * 1024 * 1024;

/// A YAML document as a figment provider. Its top level is a mapping; an
/// empty document is an empty mapping.
#[derive(Debug, Clone)]
pub(crate) struct Yaml {
    text: String,
}

impl Yaml {
    /// A document held in memory.
    pub(crate) fn string(text: &str) -> Self {
        Self {
            text: text.to_owned(),
        }
    }
}

impl Provider for Yaml {
    fn metadata(&self) -> Metadata {
        Metadata::named("YAML source string")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let document: Option<Dict> = on_a_deep_stack(|| {
            serde_saphyr::from_str_with_options(&self.text, options())
                .map_err(|error| error.to_string())
        })
        .map_err(figment::Error::from)?;
        Ok(Profile::Default.collect(document.unwrap_or_default()))
    }
}

/// The stack a document is read on. Reading recurses once per level of
/// nesting, in the parser and in figment, and at [`MAX_DEPTH`] that is more
/// than the 2 MiB a Tokio worker or the policy watcher has (a debug build
/// overflowed it at 120 levels). Reserved, and used only as deep as a
/// document nests.
const STACK_BYTES: usize = 64 * 1024 * 1024;

/// Runs a reading on a thread of its own with [`STACK_BYTES`] of stack. A
/// reading that panicked, or one no thread could be started for, is an
/// error.
fn on_a_deep_stack<T: Send>(read: impl FnOnce() -> Result<T, String> + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        let Ok(reading) = std::thread::Builder::new()
            .name("kage-yaml".to_owned())
            .stack_size(STACK_BYTES)
            .spawn_scoped(scope, read)
        else {
            return Err("no thread could be started to read the YAML document".to_owned());
        };
        reading
            .join()
            .unwrap_or_else(|_| Err("the YAML document could not be read".to_owned()))
    })
}

/// The bounds of a document: generous for a policy file of tens of
/// thousands of domains, and far below what would take a second or a
/// gigabyte. Events are bounded above the nodes so that the nodes are what
/// a large file meets.
fn budget() -> Option<serde_saphyr::Budget> {
    serde_saphyr::budget! {
        max_depth: MAX_DEPTH,
        max_nodes: MAX_NODES,
        max_events: 3 * MAX_NODES,
        max_total_scalar_bytes: MAX_SCALAR_BYTES,
    }
}

/// How Kage reads YAML (see the module documentation).
fn options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: budget(),
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        strict_booleans: true,
        with_snippet: false,
    }
}

/// What a figment error says, and, where text was expected and YAML read a
/// number or a boolean, that such text is written in quotes.
pub(crate) fn explain(error: figment::Error) -> String {
    let text = error.to_string();
    let read_as_text = error.into_iter().any(|one| {
        matches!(
            &one.kind,
            Kind::InvalidType(
                Actual::Float(_) | Actual::Signed(_) | Actual::Unsigned(_) | Actual::Bool(_),
                expected,
            ) if expected.contains("string")
        )
    });
    if read_as_text {
        format!(
            "{text}; text that YAML reads as a number or a boolean is written in quotes, \
             as in \"0123\""
        )
    } else {
        text
    }
}

/// Parses a YAML document into `T`. The error is a message that names the
/// offending key, or the line and column of what is not YAML.
pub(crate) fn from_str<T: DeserializeOwned + Send>(text: &str) -> Result<T, String> {
    on_a_deep_stack(|| Figment::from(Yaml::string(text)).extract().map_err(explain))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::from_str;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
        countries: Vec<String>,
        hosts: BTreeMap<String, u32>,
    }

    #[test]
    fn parses_maps_with_dotted_keys_and_norway() {
        let sample: Sample = from_str(
            "name: demo\ncountries: [NO, DE]\nhosts:\n  \"example.org\": 1\n  api.example.org: 2\n",
        )
        .unwrap();

        assert_eq!(sample.name, "demo");
        // YAML 1.1 would read NO as `false`.
        assert_eq!(sample.countries, ["NO", "DE"]);
        assert_eq!(sample.hosts["example.org"], 1);
        assert_eq!(sample.hosts["api.example.org"], 2);
    }

    #[test]
    fn errors_name_the_problem() {
        let unknown =
            from_str::<Sample>("name: x\ncountries: []\nhosts: {}\nextra: 1\n").unwrap_err();
        assert!(unknown.contains("extra"), "{unknown}");
        // Where it was read: the document, not a default.
        assert!(unknown.contains("YAML source string"), "{unknown}");

        let missing = from_str::<Sample>("name: x\n").unwrap_err();
        assert!(missing.contains("countries"), "{missing}");

        assert!(from_str::<Sample>("name: [unclosed").is_err());
        assert!(from_str::<Sample>("").is_err());
    }

    // The choices of F45, each pinned. Every one of them is a change from
    // the reading of `serde_yaml` before it.

    #[test]
    fn a_key_said_twice_is_an_error_that_says_where() {
        let said = from_str::<BTreeMap<String, u32>>("a: 1\nb: 2\na: 3\n").unwrap_err();
        assert!(said.contains("line 3"), "{said}");
        // Nested and in a flow mapping too.
        assert!(from_str::<BTreeMap<String, Value>>("x:\n  a: 1\n  a: 2\n").is_err());
        assert!(from_str::<BTreeMap<String, Value>>("x: {a: 1, a: 2}\n").is_err());
    }

    #[test]
    fn a_merge_key_is_refused() {
        let said =
            from_str::<BTreeMap<String, Value>>("base: &b {x: 1}\nother:\n  <<: *b\n").unwrap_err();
        assert!(said.contains("line 3"), "{said}");
        // An alias by itself is fine.
        let read: BTreeMap<String, Value> = from_str("base: &b {x: 1}\nother: *b\n").unwrap();
        assert_eq!(read["other"], json!({ "x": 1 }));
    }

    #[test]
    fn text_that_reads_as_a_number_is_written_in_quotes_and_the_message_says_so() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Login {
            #[allow(dead_code)]
            password: String,
        }
        // A number too large for any float is refused where it stands, and
        // never read as the text `.inf`.
        let said = from_str::<Login>("password: 1e400\n").unwrap_err();
        assert!(said.contains("line 1"), "{said}");
        assert!(from_str::<Login>("password: \"1e400\"\n").is_ok());
        for written in ["0123456", "007", "1_000", "12345"] {
            let said = from_str::<Login>(&format!("password: {written}\n")).unwrap_err();
            assert!(said.contains("password"), "{written}: {said}");
            assert!(said.contains("in quotes"), "{written}: {said}");
            assert!(from_str::<Login>(&format!("password: \"{written}\"\n")).is_ok());
        }
        // Booleans are `true` and `false` only: `yes` and `NO` are text.
        assert!(from_str::<Login>("password: yes\n").is_ok());
        assert!(from_str::<Login>("password: true\n").is_err());
    }

    #[test]
    fn nesting_and_size_are_bounded_and_answered_at_once() {
        let nested = |depth: usize| format!("x: {}1{}\n", "[".repeat(depth), "]".repeat(depth));
        assert!(from_str::<BTreeMap<String, Value>>(&nested(120)).is_ok());
        assert!(from_str::<BTreeMap<String, Value>>(&nested(130)).is_err());
        let started = Instant::now();
        assert!(from_str::<BTreeMap<String, Value>>(&nested(100_000)).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // 16 MiB of text, keys counted: one key of a byte and its value.
        let text = |bytes: usize| format!("x: \"{}\"\n", "x".repeat(bytes - 1));
        assert!(from_str::<BTreeMap<String, Value>>(&text(16 * 1024 * 1024)).is_ok());
        let said = from_str::<BTreeMap<String, Value>>(&text(16 * 1024 * 1024 + 1)).unwrap_err();
        assert!(said.contains("ScalarBytes"), "{said}");
        // One string of 100 KiB repeated by aliases to more than 16 MiB.
        let mut amplified = format!("big: &big \"{}\"\nmany: [", "x".repeat(100 * 1024));
        amplified.push_str(&vec!["*big"; 200].join(", "));
        amplified.push_str("]\n");
        let said = from_str::<BTreeMap<String, Value>>(&amplified).unwrap_err();
        assert!(said.contains("ScalarBytes"), "{said}");
        // Nine levels of ten aliases each, a billion strings: refused for the
        // events it would replay.
        let laughs: String = std::iter::once("a0: &a0 \"lol\"\n".to_owned())
            .chain((1..=9).map(|level| {
                let below = vec![format!("*a{}", level - 1); 10].join(", ");
                format!("a{level}: &a{level} [{below}]\n")
            }))
            .collect();
        let said = from_str::<BTreeMap<String, Value>>(&laughs).unwrap_err();
        assert!(said.contains("RecordedAnchorEvents"), "{said}");
    }

    #[test]
    fn a_message_never_quotes_the_document() {
        let said =
            from_str::<BTreeMap<String, Value>>("username: alice\npassword: hunter2\nbroken: [\n")
                .unwrap_err();
        assert!(
            !said.contains("hunter2") && !said.contains("alice"),
            "{said}"
        );
        assert!(said.contains("line"), "{said}");
    }

    #[test]
    fn a_byte_order_mark_and_windows_line_ends_are_read() {
        let read: BTreeMap<String, u32> = from_str("\u{feff}a: 1\r\nb: 2\r\n").unwrap();
        assert_eq!(read.len(), 2);
        // An empty document is an empty mapping.
        assert_eq!(
            from_str::<BTreeMap<String, u32>>("# nothing\n")
                .unwrap()
                .len(),
            0
        );
    }
}
