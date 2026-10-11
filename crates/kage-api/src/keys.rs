//! API keys (spec §16.2): what a bearer token is made of, how a new key is
//! minted, and how a presented one is checked.
//!
//! A token is `kage_<keyid>_<secret>`. The key id is the key's UUID without
//! hyphens; it finds the row. The secret is 32 bytes from the operating
//! system, written as base64url without padding; only its SHA-256 is
//! stored, and the token is shown once, when the key is made or rotated.
//!
//! A fast hash is the right one here: the secret is 256 random bits, so
//! there is nothing to guess and nothing a slow hash would protect. The
//! stored hash is compared in constant time.

use std::collections::BTreeSet;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use kage_core::domain::{ApiKey, KeyLimits, Scope};
use kage_core::ids::ApiKeyId;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// What every token starts with.
pub const TOKEN_PREFIX: &str = "kage_";

/// The length of a key's secret in bytes.
pub const SECRET_BYTES: usize = 32;

/// The longest name a key may have.
pub const MAX_NAME_CHARS: usize = 100;

/// Limits a key gets when none are named.
pub const DEFAULT_LIMITS: KeyLimits = KeyLimits {
    requests_per_minute: 60,
    max_concurrent: 4,
    daily_page_budget: None,
};

/// The most requests per minute and requests in flight a key can be given.
pub const MAX_KEY_RATE: u32 = 100_000;

/// A token as it is shown once. `Debug` and `Display` of the wrapper are
/// redacted; [`Token::reveal`] is the one way to the text.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The token itself, for the one time it is shown.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

/// Why a key cannot be made as asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The name is empty, too long or holds a control character.
    #[error("a key's name is 1 to {MAX_NAME_CHARS} characters without control characters")]
    Name,
    /// No scope was given.
    #[error("a key needs at least one scope")]
    NoScopes,
    /// A limit is zero or beyond what a key can be given.
    #[error("{0} must be between 1 and {MAX_KEY_RATE}")]
    Limit(&'static str),
    /// The operating system gave no random bytes.
    #[error("the operating system gave no random bytes for the secret")]
    Random,
}

/// The SHA-256 of a secret.
fn hash(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

/// A fresh secret and the token that carries it.
fn secret_for(id: ApiKeyId) -> Result<([u8; 32], Token), KeyError> {
    let mut secret = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut secret).map_err(|_| KeyError::Random)?;
    let token = format!(
        "{TOKEN_PREFIX}{}_{}",
        id.as_uuid().simple(),
        URL_SAFE_NO_PAD.encode(secret)
    );
    Ok((hash(&secret), Token(token)))
}

/// What a new key is to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewKey {
    /// A label for humans.
    pub name: String,
    /// What the key may do.
    pub scopes: BTreeSet<Scope>,
    /// Its limits.
    pub limits: KeyLimits,
}

impl NewKey {
    fn validate(&self) -> Result<(), KeyError> {
        let name = self.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_NAME_CHARS
            || name.chars().any(char::is_control)
        {
            return Err(KeyError::Name);
        }
        if self.scopes.is_empty() {
            return Err(KeyError::NoScopes);
        }
        for (field, value) in [
            ("requests_per_minute", self.limits.requests_per_minute),
            ("max_concurrent", self.limits.max_concurrent),
        ] {
            if value == 0 || value > MAX_KEY_RATE {
                return Err(KeyError::Limit(field));
            }
        }
        if self.limits.daily_page_budget == Some(0) {
            return Err(KeyError::Limit("daily_page_budget"));
        }
        Ok(())
    }
}

/// Mints a key: the row to store and the token to show once.
///
/// # Errors
///
/// Fails when the name, the scopes or a limit is not acceptable, or when
/// the operating system gives no random bytes.
pub fn mint(new: &NewKey, now: DateTime<Utc>) -> Result<(ApiKey, Token), KeyError> {
    new.validate()?;
    let id = ApiKeyId::new();
    let (secret_hash, token) = secret_for(id)?;
    let key = ApiKey {
        id,
        name: new.name.trim().to_owned(),
        secret_hash,
        scopes: new.scopes.clone(),
        limits: new.limits,
        disabled_at: None,
        created_at: now,
        last_used_at: None,
    };
    Ok((key, token))
}

/// A new secret for an existing key: the hash to store and the token to
/// show once. The old token stops working when the hash is stored.
///
/// # Errors
///
/// Fails when the operating system gives no random bytes.
pub fn rotate(id: ApiKeyId) -> Result<([u8; 32], Token), KeyError> {
    secret_for(id)
}

/// A token as a request presented it: whose it claims to be, and the hash
/// of the secret it carries.
#[derive(Clone, PartialEq, Eq)]
pub struct Presented {
    /// The key the token names.
    pub id: ApiKeyId,
    secret_hash: [u8; 32],
}

impl fmt::Debug for Presented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presented")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Presented {
    /// Reads a token. `None` for anything that is not
    /// `kage_<keyid>_<secret>` with a UUID and 32 bytes of base64url.
    pub fn parse(token: &str) -> Option<Self> {
        let rest = token.strip_prefix(TOKEN_PREFIX)?;
        // The secret's alphabet has `_` in it; the key id's has not.
        let (id, secret) = rest.split_once('_')?;
        let id = Uuid::try_parse(id).ok()?;
        let secret = URL_SAFE_NO_PAD.decode(secret).ok()?;
        if secret.len() != SECRET_BYTES {
            return None;
        }
        Some(Self {
            id: ApiKeyId::from_uuid(id),
            secret_hash: hash(&secret),
        })
    }

    /// Reads the value of an `Authorization` header: `Bearer <token>`.
    pub fn from_authorization(value: &str) -> Option<Self> {
        let (scheme, token) = value.trim().split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        Self::parse(token.trim())
    }

    /// Whether the token's secret is the key's. Compared in constant time.
    pub fn opens(&self, key: &ApiKey) -> bool {
        self.id == key.id && bool::from(self.secret_hash.ct_eq(&key.secret_hash))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;
    use kage_core::domain::{KeyLimits, Scope};

    use super::{
        DEFAULT_LIMITS, KeyError, MAX_KEY_RATE, NewKey, Presented, TOKEN_PREFIX, mint, rotate,
    };

    fn new_key() -> NewKey {
        NewKey {
            name: " project-a ".to_owned(),
            scopes: BTreeSet::from([Scope::Fetch, Scope::Sessions]),
            limits: DEFAULT_LIMITS,
        }
    }

    #[test]
    fn a_minted_key_is_opened_by_its_token_and_by_no_other() {
        let (key, token) = mint(&new_key(), Utc::now()).unwrap();

        assert_eq!(key.name, "project-a");
        assert!(key.is_active());
        assert_eq!(key.limits, DEFAULT_LIMITS);
        let text = token.reveal();
        assert!(text.starts_with(TOKEN_PREFIX), "{text}");
        // kage_ + 32 hex + _ + 43 base64url characters.
        assert_eq!(text.len(), 5 + 32 + 1 + 43);
        // The token is not what is stored, and does not print by accident.
        assert_eq!(format!("{token:?}"), "Token([redacted])");

        let presented = Presented::parse(text).unwrap();
        assert_eq!(presented.id, key.id);
        assert!(presented.opens(&key));
        assert!(!format!("{presented:?}").contains(&text[38..]));

        // Another key's token does not open it, nor does its own id with
        // another secret.
        let (other, other_token) = mint(&new_key(), Utc::now()).unwrap();
        assert!(!Presented::parse(other_token.reveal()).unwrap().opens(&key));
        assert_ne!(other.secret_hash, key.secret_hash);
        let forged = format!(
            "{TOKEN_PREFIX}{}_{}",
            key.id.as_uuid().simple(),
            &other_token.reveal()[38..]
        );
        let forged = Presented::parse(&forged).unwrap();
        assert_eq!(forged.id, key.id);
        assert!(!forged.opens(&key));
    }

    #[test]
    fn a_rotated_secret_replaces_the_old_one() {
        let (mut key, old) = mint(&new_key(), Utc::now()).unwrap();

        let (hash, new) = rotate(key.id).unwrap();
        key.secret_hash = hash;

        assert_ne!(old.reveal(), new.reveal());
        assert!(Presented::parse(new.reveal()).unwrap().opens(&key));
        assert!(!Presented::parse(old.reveal()).unwrap().opens(&key));
    }

    #[test]
    fn only_a_well_formed_token_is_read() {
        let (key, token) = mint(&new_key(), Utc::now()).unwrap();
        let text = token.reveal();

        for header in [
            format!("Bearer {text}"),
            format!("bearer  {text} "),
            format!("BEARER {text}"),
        ] {
            let presented = Presented::from_authorization(&header).unwrap();
            assert!(presented.opens(&key), "{header:?}");
        }
        // The hyphenated form of the id names the same key.
        let hyphenated = format!("{TOKEN_PREFIX}{}_{}", key.id, &text[38..]);
        assert!(Presented::parse(&hyphenated).unwrap().opens(&key));

        let id = key.id.as_uuid().simple().to_string();
        for bad in [
            String::new(),
            "kage_".to_owned(),
            text[5..].to_owned(),
            format!("Basic {text}"),
            format!("kage_{id}"),
            format!("kage_{id}_"),
            format!("kage_{id}_c2hvcnQ"),
            format!("kage_nokey_{}", &text[38..]),
            format!("kage_{id}_{}=", &text[38..]),
            format!("sk_{id}_{}", &text[38..]),
        ] {
            assert!(Presented::parse(&bad).is_none(), "{bad:?}");
            assert!(Presented::from_authorization(&bad).is_none(), "{bad:?}");
        }
        assert!(Presented::from_authorization(text).is_none());
    }

    #[test]
    fn a_key_needs_a_name_a_scope_and_limits_that_can_be_kept() {
        let refused = |change: fn(&mut NewKey)| {
            let mut new = new_key();
            change(&mut new);
            mint(&new, Utc::now()).unwrap_err()
        };

        assert_eq!(refused(|new| new.name = "  ".to_owned()), KeyError::Name);
        assert_eq!(refused(|new| new.name = "x".repeat(101)), KeyError::Name);
        assert_eq!(refused(|new| new.name = "a\nb".to_owned()), KeyError::Name);
        assert_eq!(refused(|new| new.scopes.clear()), KeyError::NoScopes);
        assert_eq!(
            refused(|new| new.limits.requests_per_minute = 0),
            KeyError::Limit("requests_per_minute")
        );
        assert_eq!(
            refused(|new| new.limits.max_concurrent = MAX_KEY_RATE + 1),
            KeyError::Limit("max_concurrent")
        );
        assert_eq!(
            refused(|new| new.limits.daily_page_budget = Some(0)),
            KeyError::Limit("daily_page_budget")
        );
        let budgeted = NewKey {
            limits: KeyLimits {
                daily_page_budget: Some(500),
                ..DEFAULT_LIMITS
            },
            ..new_key()
        };
        assert_eq!(
            mint(&budgeted, Utc::now())
                .unwrap()
                .0
                .limits
                .daily_page_budget,
            Some(500)
        );
    }
}
