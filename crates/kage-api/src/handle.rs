//! What a key that is not an admin's sees in place of a persona's id.
//!
//! A persona's id is a UUIDv7: it carries the millisecond the persona was
//! generated, which is when its address joined the pool or was replaced,
//! and it is the same for every consumer. A consumer's log of results
//! would be a map of the pool that another consumer's log could be joined
//! to. So a result names its persona by a handle:
//!
//! `p_` and 16 hexadecimal characters of HMAC-SHA-256 over the key's id and
//! the persona's id, under a key derived from the master key.
//!
//! - The same for one key and one persona, always: a consumer can still
//!   tell that two pages came from the same identity and that a retry used
//!   another.
//! - Different for every key, and not computable without the master key.
//! - Carries no time, and is not accepted anywhere: the admin endpoints
//!   take real ids only.
//!
//! What a handle does not hide: a consumer who fetches a page that prints
//! the visitor's address learns that persona's address. The handle keeps a
//! log from becoming a map by accident.

use std::fmt::Write as _;

use hmac::{Hmac, KeyInit, Mac};
use kage_core::ids::{ApiKeyId, PersonaId};
use sha2::Sha256;

/// What a handle starts with.
pub const HANDLE_PREFIX: &str = "p_";

/// Hexadecimal characters of a handle after the prefix: 64 bits.
const HANDLE_CHARS: usize = 16;

/// Makes the handles of personas.
#[derive(Clone)]
pub struct Handles {
    key: [u8; 32],
}

impl std::fmt::Debug for Handles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handles").finish_non_exhaustive()
    }
}

impl Handles {
    /// Handles under a key derived from the master key, so that the master
    /// key itself signs nothing.
    pub fn new(master_key: &[u8; 32]) -> Self {
        let mut key = [0_u8; 32];
        if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(master_key) {
            mac.update(b"kage-persona-handle-key-v1");
            key.copy_from_slice(&mac.finalize().into_bytes());
        }
        Self { key }
    }

    /// The handle of a persona as one key sees it.
    pub fn of(&self, key: ApiKeyId, persona: PersonaId) -> String {
        let mut handle = String::with_capacity(HANDLE_PREFIX.len() + HANDLE_CHARS);
        handle.push_str(HANDLE_PREFIX);
        if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&self.key) {
            mac.update(key.as_uuid().as_bytes());
            mac.update(persona.as_uuid().as_bytes());
            for byte in mac.finalize().into_bytes().iter().take(HANDLE_CHARS / 2) {
                // Writing to a String cannot fail.
                let _ = write!(handle, "{byte:02x}");
            }
        }
        handle
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kage_core::ids::{ApiKeyId, PersonaId};

    use super::{HANDLE_PREFIX, Handles};

    #[test]
    fn a_handle_is_stable_for_a_key_and_different_for_another() {
        let handles = Handles::new(&[7; 32]);
        let (key, other_key) = (ApiKeyId::new(), ApiKeyId::new());
        let persona = PersonaId::new();

        let handle = handles.of(key, persona);

        assert_eq!(handle, handles.of(key, persona));
        assert!(handle.starts_with(HANDLE_PREFIX));
        assert_eq!(handle.len(), 18);
        assert!(
            handle[2..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        // Two consumers' logs cannot be joined on it.
        assert_ne!(handle, handles.of(other_key, persona));
        // Another master key, other handles.
        assert_ne!(handle, Handles::new(&[8; 32]).of(key, persona));
        // The handle shows nothing of the id it stands for.
        let id = persona.as_uuid().simple().to_string();
        assert!(!id.contains(&handle[2..]) && !handle.contains(&id[..12]));
        assert_eq!(format!("{handles:?}"), "Handles { .. }");
    }

    #[test]
    fn twenty_personas_have_twenty_handles() {
        let handles = Handles::new(&[1; 32]);
        let key = ApiKeyId::new();

        let distinct: BTreeSet<String> =
            (0..20).map(|_| handles.of(key, PersonaId::new())).collect();

        assert_eq!(distinct.len(), 20);
    }
}
