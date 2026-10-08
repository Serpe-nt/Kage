//! Secret values that cannot leak through `Debug`, `Display`, logs or a
//! serialized configuration (spec §19).

use std::fmt;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// What a secret looks like wherever it is printed or serialized.
pub const REDACTED: &str = "[redacted]";

/// A secret string such as a database URL, token or API key.
///
/// `Debug`, `Display` and `Serialize` all produce [`REDACTED`]; the value is
/// only reachable through [`Secret::expose`], which makes every use easy to
/// find in review. The memory is zeroed on drop.
#[derive(Clone)]
pub struct Secret(SecretString);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(SecretString::from(value.into()))
    }

    /// The secret itself. Never pass the result to a log or an error message.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }

    /// Whether the secret is the empty string.
    pub fn is_empty(&self) -> bool {
        self.expose().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Ciphertext of a secret that is encrypted at rest, such as a proxy password
/// (spec §19). The bytes are opaque here: sealing and opening belong to the
/// crate that holds the master key.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedSecret(Vec<u8>);

impl SealedSecret {
    /// Wraps ciphertext.
    pub fn new(ciphertext: Vec<u8>) -> Self {
        Self(ciphertext)
    }

    /// The ciphertext bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the value and returns the ciphertext bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[sealed, {} bytes]", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{REDACTED, SealedSecret, Secret};

    #[test]
    fn secret_never_prints_its_value() {
        let secret = Secret::new("postgres://kage:hunter2@db/kage");

        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
        assert_eq!(
            serde_json::to_string(&secret).unwrap(),
            format!("\"{REDACTED}\"")
        );
        assert_eq!(secret.expose(), "postgres://kage:hunter2@db/kage");
    }

    #[test]
    fn secret_inside_a_struct_stays_redacted() {
        #[derive(Debug, serde::Serialize)]
        struct Holder {
            token: Secret,
        }
        let holder = Holder {
            token: Secret::new("s3cr3t-value"),
        };

        assert!(!format!("{holder:?}").contains("s3cr3t-value"));
        assert!(
            !serde_json::to_string(&holder)
                .unwrap()
                .contains("s3cr3t-value")
        );
    }

    #[test]
    fn secret_deserializes_from_a_string() {
        let secret: Secret = serde_json::from_str("\"abc\"").unwrap();

        assert_eq!(secret.expose(), "abc");
        assert!(!secret.is_empty());
        assert!(Secret::new("").is_empty());
        assert_eq!(secret.clone().expose(), "abc");
    }

    #[test]
    fn sealed_secret_shows_only_its_length() {
        let sealed = SealedSecret::new(vec![1, 2, 3, 4]);

        assert_eq!(format!("{sealed:?}"), "[sealed, 4 bytes]");
        assert_eq!(sealed.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(sealed.clone().into_bytes(), vec![1, 2, 3, 4]);
    }
}
