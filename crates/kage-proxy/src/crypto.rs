//! Proxy credentials encrypted at rest (spec §19).
//!
//! The user name and the password of every proxy are stored as ciphertext
//! under the master key (`KAGE__SECRETS__MASTER_KEY`, 32 random bytes written
//! as 64 hexadecimal characters). The cipher is XChaCha20-Poly1305 with a
//! fresh random 24-byte nonce per value; at that nonce size random nonces do
//! not collide in practice.
//!
//! A sealed value is laid out as
//!
//! ```text
//! version (1 byte, currently 1) | nonce (24 bytes) | ciphertext | tag (16 bytes)
//! ```
//!
//! The associated data binds every value to the proxy it belongs to and to
//! the field it holds, so a ciphertext copied into another row, or from the
//! password column into the user name column, does not open.
//!
//! Losing the master key makes the stored credentials unreadable. The pool
//! recovers by itself: the next sync with the provider finds that the stored
//! values no longer open and seals the provider's credentials again under
//! the new key.

use std::fmt;

use chacha20poly1305::aead::{Aead, Generate, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use kage_core::config::{MASTER_KEY_BYTES, SecretsConfig};
use kage_core::ids::ProxyId;
use kage_core::secret::{SealedSecret, Secret};
use zeroize::Zeroizing;

/// Version byte of the current layout.
const VERSION: u8 = 1;

/// Length of the nonce.
const NONCE_BYTES: usize = 24;

/// Length of the Poly1305 tag.
const TAG_BYTES: usize = 16;

/// Prefix of the associated data; changing it invalidates every stored value.
const AAD_PREFIX: &[u8] = b"kage:proxy-credential:v1:";

/// Which credential a sealed value holds. Part of the associated data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialField {
    /// The proxy user name.
    Username,
    /// The proxy password.
    Password,
}

impl CredentialField {
    const fn label(self) -> &'static [u8] {
        match self {
            Self::Username => b"username",
            Self::Password => b"password",
        }
    }
}

/// Why a credential could not be sealed or opened. No variant carries key
/// material or plaintext.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// The master key is not 32 bytes written as 64 hexadecimal characters.
    #[error("the master key must be 32 random bytes written as 64 hexadecimal characters")]
    InvalidKey,
    /// The operating system did not supply random bytes for a nonce.
    #[error("the operating system random source failed")]
    Random,
    /// The stored value is too short to be a sealed credential.
    #[error("the stored credential is malformed")]
    Malformed,
    /// The stored value was written in a layout this version does not know.
    #[error("the stored credential has unknown layout version {0}")]
    UnknownVersion(u8),
    /// Authentication failed: the master key is not the one the value was
    /// sealed with, the value was altered, or it belongs to another proxy
    /// or field.
    #[error(
        "the stored credential does not open: wrong master key, altered value, or a value that belongs to another proxy or field"
    )]
    Rejected,
    /// The opened value is not text.
    #[error("the stored credential is not valid UTF-8")]
    NotText,
}

/// Seals and opens proxy credentials under the master key.
#[derive(Clone)]
pub struct CredentialCipher {
    cipher: XChaCha20Poly1305,
}

impl fmt::Debug for CredentialCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialCipher(..)")
    }
}

impl CredentialCipher {
    /// A cipher under the given 32-byte key.
    pub fn new(key: &[u8; MASTER_KEY_BYTES]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(&(*key).into()),
        }
    }

    /// A cipher under the configured master key.
    ///
    /// # Errors
    ///
    /// [`CryptoError::InvalidKey`] when the configured key is malformed. A
    /// configuration that loaded has a well-formed key.
    pub fn from_config(secrets: &SecretsConfig) -> Result<Self, CryptoError> {
        let key = Zeroizing::new(secrets.master_key_bytes().ok_or(CryptoError::InvalidKey)?);
        Ok(Self::new(&key))
    }

    /// Encrypts one credential of one proxy. Sealing the same value twice
    /// gives two different ciphertexts.
    ///
    /// # Errors
    ///
    /// [`CryptoError::Random`] when no nonce could be drawn.
    pub fn seal(
        &self,
        proxy: ProxyId,
        field: CredentialField,
        plaintext: &str,
    ) -> Result<SealedSecret, CryptoError> {
        let nonce = XNonce::try_generate().map_err(|_| CryptoError::Random)?;
        let aad = associated_data(proxy, field);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            // Encryption fails only for inputs beyond the cipher's length
            // limit of 256 GiB.
            .map_err(|_| CryptoError::Malformed)?;

        let mut sealed = Vec::with_capacity(1 + NONCE_BYTES + ciphertext.len());
        sealed.push(VERSION);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(SealedSecret::new(sealed))
    }

    /// Decrypts one credential of one proxy.
    ///
    /// # Errors
    ///
    /// Fails when the value is malformed, was sealed under another key, was
    /// altered, or belongs to another proxy or field.
    pub fn open(
        &self,
        proxy: ProxyId,
        field: CredentialField,
        sealed: &SealedSecret,
    ) -> Result<Secret, CryptoError> {
        let bytes = sealed.as_bytes();
        let (&version, rest) = bytes.split_first().ok_or(CryptoError::Malformed)?;
        if version != VERSION {
            return Err(CryptoError::UnknownVersion(version));
        }
        if rest.len() < NONCE_BYTES + TAG_BYTES {
            return Err(CryptoError::Malformed);
        }
        let (nonce, ciphertext) = rest.split_at(NONCE_BYTES);
        let nonce = XNonce::try_from(nonce).map_err(|_| CryptoError::Malformed)?;
        let aad = associated_data(proxy, field);
        let plaintext = Zeroizing::new(
            self.cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| CryptoError::Rejected)?,
        );
        let text = std::str::from_utf8(&plaintext).map_err(|_| CryptoError::NotText)?;
        Ok(Secret::new(text))
    }
}

fn associated_data(proxy: ProxyId, field: CredentialField) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + 16 + 1 + 8);
    aad.extend_from_slice(AAD_PREFIX);
    aad.extend_from_slice(proxy.as_uuid().as_bytes());
    aad.push(b':');
    aad.extend_from_slice(field.label());
    aad
}

#[cfg(test)]
mod tests {
    use kage_core::ids::ProxyId;
    use kage_core::secret::{SealedSecret, Secret};

    use super::{CredentialCipher, CredentialField, CryptoError, NONCE_BYTES, TAG_BYTES, VERSION};

    const KEY: [u8; 32] = [7; 32];

    fn cipher() -> CredentialCipher {
        CredentialCipher::new(&KEY)
    }

    #[test]
    fn a_sealed_value_opens_to_what_was_sealed() {
        let cipher = cipher();
        let proxy = ProxyId::new();

        for plaintext in [
            "hunter2",
            "",
            "pässwört with spaces: and ✓",
            &"x".repeat(4096),
        ] {
            let sealed = cipher
                .seal(proxy, CredentialField::Password, plaintext)
                .unwrap();

            assert_eq!(
                sealed.as_bytes().len(),
                1 + NONCE_BYTES + plaintext.len() + TAG_BYTES
            );
            assert_eq!(sealed.as_bytes()[0], VERSION);
            assert_eq!(
                cipher
                    .open(proxy, CredentialField::Password, &sealed)
                    .unwrap()
                    .expose(),
                plaintext
            );
        }
    }

    #[test]
    fn the_plaintext_does_not_appear_in_the_sealed_value() {
        let sealed = cipher()
            .seal(ProxyId::new(), CredentialField::Password, "hunter2-hunter2")
            .unwrap();

        let bytes = sealed.as_bytes();
        assert!(
            !bytes
                .windows(b"hunter2".len())
                .any(|window| window == b"hunter2")
        );
    }

    #[test]
    fn sealing_twice_gives_different_ciphertexts() {
        let cipher = cipher();
        let proxy = ProxyId::new();

        let first = cipher
            .seal(proxy, CredentialField::Username, "user")
            .unwrap();
        let second = cipher
            .seal(proxy, CredentialField::Username, "user")
            .unwrap();

        assert_ne!(first, second);
        // The nonces differ, not only the tags.
        assert_ne!(
            first.as_bytes()[1..=NONCE_BYTES],
            second.as_bytes()[1..=NONCE_BYTES]
        );
    }

    #[test]
    fn a_value_opens_only_for_its_own_proxy_and_field() {
        let cipher = cipher();
        let proxy = ProxyId::new();
        let sealed = cipher
            .seal(proxy, CredentialField::Password, "hunter2")
            .unwrap();

        // Moved to another row.
        assert_eq!(
            cipher
                .open(ProxyId::new(), CredentialField::Password, &sealed)
                .unwrap_err(),
            CryptoError::Rejected
        );
        // Moved to the other column of the same row.
        assert_eq!(
            cipher
                .open(proxy, CredentialField::Username, &sealed)
                .unwrap_err(),
            CryptoError::Rejected
        );
    }

    #[test]
    fn another_key_does_not_open_it() {
        let proxy = ProxyId::new();
        let sealed = cipher()
            .seal(proxy, CredentialField::Password, "hunter2")
            .unwrap();
        let mut other_key = KEY;
        other_key[31] ^= 1;

        assert_eq!(
            CredentialCipher::new(&other_key)
                .open(proxy, CredentialField::Password, &sealed)
                .unwrap_err(),
            CryptoError::Rejected
        );
    }

    #[test]
    fn every_altered_byte_is_detected() {
        let cipher = cipher();
        let proxy = ProxyId::new();
        let sealed = cipher
            .seal(proxy, CredentialField::Password, "hunter2")
            .unwrap();

        for index in 0..sealed.as_bytes().len() {
            let mut altered = sealed.as_bytes().to_vec();
            altered[index] ^= 0x01;

            let error = cipher
                .open(
                    proxy,
                    CredentialField::Password,
                    &SealedSecret::new(altered),
                )
                .unwrap_err();

            if index == 0 {
                assert_eq!(error, CryptoError::UnknownVersion(VERSION ^ 0x01));
            } else {
                assert_eq!(error, CryptoError::Rejected, "byte {index}");
            }
        }
    }

    #[test]
    fn malformed_values_are_rejected_before_any_decryption() {
        let cipher = cipher();
        let proxy = ProxyId::new();
        let open = |bytes: Vec<u8>| {
            cipher
                .open(proxy, CredentialField::Password, &SealedSecret::new(bytes))
                .unwrap_err()
        };

        assert_eq!(open(Vec::new()), CryptoError::Malformed);
        assert_eq!(open(vec![VERSION]), CryptoError::Malformed);
        assert_eq!(
            open(vec![VERSION; NONCE_BYTES + TAG_BYTES]),
            CryptoError::Malformed
        );
        assert_eq!(open(vec![2; 64]), CryptoError::UnknownVersion(2));
        // Plain text left over from before encryption is not mistaken for a
        // sealed value.
        assert!(matches!(
            open(b"plain-user-name".to_vec()),
            CryptoError::UnknownVersion(_)
        ));
        // Long enough and the right version, but never sealed by anyone.
        assert_eq!(
            open(vec![VERSION; 1 + NONCE_BYTES + TAG_BYTES]),
            CryptoError::Rejected
        );
    }

    #[test]
    fn bytes_that_are_not_text_are_reported_as_such() {
        use chacha20poly1305::aead::{Aead, Payload};
        use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

        // Seal raw non-UTF-8 bytes with the same key, layout and binding.
        let proxy = ProxyId::new();
        let nonce = XNonce::from([9_u8; NONCE_BYTES]);
        let aad = super::associated_data(proxy, CredentialField::Password);
        let ciphertext = XChaCha20Poly1305::new(&KEY.into())
            .encrypt(
                &nonce,
                Payload {
                    msg: &[0xff, 0xfe, 0xfd],
                    aad: &aad,
                },
            )
            .unwrap();
        let mut sealed = vec![VERSION];
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);

        assert_eq!(
            cipher()
                .open(proxy, CredentialField::Password, &SealedSecret::new(sealed))
                .unwrap_err(),
            CryptoError::NotText
        );
    }

    #[test]
    fn the_cipher_is_built_from_the_configured_key() {
        let secrets = |key: &str| kage_core::config::SecretsConfig {
            master_key: Secret::new(key),
        };
        let proxy = ProxyId::new();

        let from_config = CredentialCipher::from_config(&secrets(&"07".repeat(32))).unwrap();
        let sealed = from_config
            .seal(proxy, CredentialField::Username, "user")
            .unwrap();
        // The same key as bytes opens it.
        assert_eq!(
            cipher()
                .open(proxy, CredentialField::Username, &sealed)
                .unwrap()
                .expose(),
            "user"
        );

        assert_eq!(
            CredentialCipher::from_config(&secrets("too-short")).unwrap_err(),
            CryptoError::InvalidKey
        );
    }

    #[test]
    fn nothing_prints_key_material() {
        assert_eq!(format!("{:?}", cipher()), "CredentialCipher(..)");
        let message = CryptoError::Rejected.to_string();
        assert!(message.contains("does not open"), "{message}");
        for error in [
            CryptoError::InvalidKey,
            CryptoError::Random,
            CryptoError::Malformed,
            CryptoError::UnknownVersion(9),
            CryptoError::NotText,
        ] {
            assert_ne!(error.to_string(), "");
        }
    }
}
