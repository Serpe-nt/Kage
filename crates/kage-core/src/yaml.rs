//! YAML parsing for the policy files, through figment's YAML provider so the
//! workspace has exactly one YAML parser.

use figment::Figment;
use figment::providers::{Format, Yaml};
use serde::de::DeserializeOwned;

/// Parses a YAML document into `T`. The error is a message that names the
/// offending key.
pub(crate) fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    Figment::from(Yaml::string(text))
        .extract()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::Deserialize;

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

        let missing = from_str::<Sample>("name: x\n").unwrap_err();
        assert!(missing.contains("countries"), "{missing}");

        assert!(from_str::<Sample>("name: [unclosed").is_err());
        assert!(from_str::<Sample>("").is_err());
    }
}
