//! Reading a TLS `ClientHello` as it came off the wire, and its JA4
//! fingerprint.
//!
//! JA4 (by `FoxIO`) has three parts, `a_b_c`:
//!
//! - `a`: `t` for TLS over TCP, the highest TLS version offered, `d` when
//!   the hello names a server (SNI) or `i` when it does not, the number of
//!   cipher suites and of extensions as two digits each, and the first and
//!   last character of the first ALPN protocol;
//! - `b`: the first 12 hex digits of the SHA-256 of the cipher suites,
//!   sorted;
//! - `c`: the first 12 hex digits of the SHA-256 of the extensions, sorted,
//!   without SNI and ALPN, followed by the signature algorithms in the order
//!   sent.
//!
//! GREASE values are left out everywhere. Because the lists are sorted, a
//! browser that shuffles its extensions, as Chrome does, keeps one JA4.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

const EXT_SERVER_NAME: u16 = 0x0000;
const EXT_SUPPORTED_GROUPS: u16 = 0x000a;
const EXT_SIGNATURE_ALGORITHMS: u16 = 0x000d;
const EXT_ALPN: u16 = 0x0010;
const EXT_SUPPORTED_VERSIONS: u16 = 0x002b;

/// Why bytes are not a `ClientHello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    /// The bytes end before the structure does.
    #[error("the ClientHello is cut short")]
    Truncated,
    /// The handshake message is not a `ClientHello`.
    #[error("the first handshake message is not a ClientHello")]
    NotClientHello,
}

/// A `ClientHello`, with every list in the order it was sent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientHello {
    /// The version field of the hello itself; TLS 1.3 writes 1.2 here.
    pub legacy_version: u16,
    /// The cipher suites offered.
    pub ciphers: Vec<u16>,
    /// The extension types.
    pub extensions: Vec<u16>,
    /// The server name, when the hello carries one.
    pub sni: Option<String>,
    /// The ALPN protocols offered.
    pub alpn: Vec<String>,
    /// The versions in `supported_versions`.
    pub supported_versions: Vec<u16>,
    /// The key exchange groups in `supported_groups`.
    pub groups: Vec<u16>,
    /// The signature algorithms.
    pub signature_algorithms: Vec<u16>,
}

/// Whether a value is one of the GREASE placeholders (RFC 8701): `0x0a0a`,
/// `0x1a1a`, up to `0xfafa`.
pub const fn is_grease(value: u16) -> bool {
    (value & 0x0f0f) == 0x0a0a && (value >> 8) == (value & 0xff)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], HelloError> {
        if self.bytes.len() < count {
            return Err(HelloError::Truncated);
        }
        let (head, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, HelloError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, HelloError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self) -> Result<usize, HelloError> {
        let bytes = self.take(3)?;
        Ok((usize::from(bytes[0]) << 16) | (usize::from(bytes[1]) << 8) | usize::from(bytes[2]))
    }

    /// A vector with a one-byte length.
    fn vec8(&mut self) -> Result<Reader<'a>, HelloError> {
        let length = usize::from(self.u8()?);
        Ok(Reader::new(self.take(length)?))
    }

    /// A vector with a two-byte length.
    fn vec16(&mut self) -> Result<Reader<'a>, HelloError> {
        let length = usize::from(self.u16()?);
        Ok(Reader::new(self.take(length)?))
    }

    const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn u16_list(mut self) -> Vec<u16> {
        let mut values = Vec::with_capacity(self.bytes.len() / 2);
        while let Ok(value) = self.u16() {
            values.push(value);
        }
        values
    }
}

impl ClientHello {
    /// Reads a handshake message: one byte of type, three of length, then
    /// the `ClientHello` body.
    ///
    /// # Errors
    ///
    /// Fails when the message is another one or ends early.
    pub fn parse(handshake: &[u8]) -> Result<Self, HelloError> {
        let mut message = Reader::new(handshake);
        if message.u8()? != 1 {
            return Err(HelloError::NotClientHello);
        }
        let length = message.u24()?;
        let mut body = Reader::new(message.take(length)?);

        let legacy_version = body.u16()?;
        body.take(32)?; // random
        body.vec8()?; // session id
        let ciphers = body.vec16()?.u16_list();
        body.vec8()?; // compression methods

        let mut hello = Self {
            legacy_version,
            ciphers,
            ..Self::default()
        };
        if body.is_empty() {
            // A hello without extensions, as before TLS 1.2.
            return Ok(hello);
        }
        let mut extensions = body.vec16()?;
        while !extensions.is_empty() {
            let kind = extensions.u16()?;
            let mut data = extensions.vec16()?;
            hello.extensions.push(kind);
            // A malformed extension still counts as present; its content
            // is then left empty.
            match kind {
                EXT_SERVER_NAME => {
                    if let Ok(mut names) = data.vec16() {
                        while let (Ok(name_type), Ok(name)) = (names.u8(), names.vec16()) {
                            if name_type == 0 {
                                hello.sni = Some(String::from_utf8_lossy(name.bytes).into_owned());
                                break;
                            }
                        }
                    }
                }
                EXT_ALPN => {
                    if let Ok(mut protocols) = data.vec16() {
                        while let Ok(protocol) = protocols.vec8() {
                            hello
                                .alpn
                                .push(String::from_utf8_lossy(protocol.bytes).into_owned());
                        }
                    }
                }
                EXT_SUPPORTED_VERSIONS => {
                    if let Ok(versions) = data.vec8() {
                        hello.supported_versions = versions.u16_list();
                    }
                }
                EXT_SUPPORTED_GROUPS => {
                    if let Ok(groups) = data.vec16() {
                        hello.groups = groups.u16_list();
                    }
                }
                EXT_SIGNATURE_ALGORITHMS => {
                    if let Ok(algorithms) = data.vec16() {
                        hello.signature_algorithms = algorithms.u16_list();
                    }
                }
                _ => {}
            }
        }
        Ok(hello)
    }

    /// The highest TLS version offered, as JA4 writes it: `13`, `12`, ...
    pub fn version(&self) -> &'static str {
        let highest = self
            .supported_versions
            .iter()
            .copied()
            .filter(|version| !is_grease(*version))
            .max()
            .unwrap_or(self.legacy_version);
        match highest {
            0x0304 => "13",
            0x0303 => "12",
            0x0302 => "11",
            0x0301 => "10",
            0x0300 => "s3",
            _ => "00",
        }
    }

    fn sorted_ciphers(&self) -> Vec<u16> {
        let mut ciphers: Vec<u16> = self
            .ciphers
            .iter()
            .copied()
            .filter(|cipher| !is_grease(*cipher))
            .collect();
        ciphers.sort_unstable();
        ciphers
    }

    /// The extensions that enter the hash: sorted, without GREASE, SNI and
    /// ALPN.
    fn sorted_extensions(&self) -> Vec<u16> {
        let mut extensions: Vec<u16> = self
            .extensions
            .iter()
            .copied()
            .filter(|kind| !is_grease(*kind) && *kind != EXT_SERVER_NAME && *kind != EXT_ALPN)
            .collect();
        extensions.sort_unstable();
        extensions
    }

    fn part_a(&self) -> String {
        let count = |values: &[u16]| {
            values
                .iter()
                .filter(|value| !is_grease(**value))
                .count()
                .min(99)
        };
        let alpn = self.alpn.first().map_or_else(
            || "00".to_owned(),
            |protocol| {
                let bytes = protocol.as_bytes();
                match (bytes.first(), bytes.last()) {
                    (Some(first), Some(last))
                        if first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric() =>
                    {
                        format!("{}{}", char::from(*first), char::from(*last))
                    }
                    // Anything else is written as the outer hex digits.
                    (Some(first), Some(last)) => {
                        format!("{:x}{:x}", first >> 4, last & 0x0f)
                    }
                    _ => "00".to_owned(),
                }
            },
        );
        format!(
            "t{}{}{:02}{:02}{alpn}",
            self.version(),
            if self.sni.is_some() { 'd' } else { 'i' },
            count(&self.ciphers),
            count(&self.extensions),
        )
    }

    fn raw_lists(&self) -> (String, String) {
        let ciphers = hex_list(&self.sorted_ciphers());
        let mut extensions = hex_list(&self.sorted_extensions());
        if !self.signature_algorithms.is_empty() {
            extensions.push('_');
            extensions.push_str(&hex_list(&self.signature_algorithms));
        }
        (ciphers, extensions)
    }

    /// The JA4 fingerprint, for example
    /// `t13d1516h2_8daaf6152771_d8a2da3f94cd`.
    pub fn ja4(&self) -> String {
        let (ciphers, extensions) = self.raw_lists();
        format!(
            "{}_{}_{}",
            self.part_a(),
            truncated_hash(&ciphers),
            truncated_hash(&extensions)
        )
    }

    /// JA4 with the lists written out instead of hashed (`JA4_r`), which
    /// shows what a difference between two fingerprints is.
    pub fn ja4_raw(&self) -> String {
        let (ciphers, extensions) = self.raw_lists();
        format!("{}_{ciphers}_{extensions}", self.part_a())
    }
}

/// `002f,0035` for a list of code points.
pub fn hex_list(values: &[u16]) -> String {
    let mut text = String::with_capacity(values.len() * 5);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        let _ = write!(text, "{value:04x}");
    }
    text
}

/// The first 12 hex digits of the SHA-256, or twelve zeros for nothing.
fn truncated_hash(text: &str) -> String {
    if text.is_empty() {
        return "000000000000".to_owned();
    }
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ClientHello, HelloError, hex_list, is_grease, truncated_hash};

    fn vec16(payload: &[u8]) -> Vec<u8> {
        let mut bytes = u16::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn vec8(payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![u8::try_from(payload.len()).unwrap()];
        bytes.extend_from_slice(payload);
        bytes
    }

    fn u16s(values: &[u16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn extension(kind: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = kind.to_be_bytes().to_vec();
        bytes.extend(vec16(data));
        bytes
    }

    /// Builds a handshake message holding a `ClientHello`.
    pub(crate) fn hello_bytes(ciphers: &[u16], extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![0x03, 0x03];
        body.extend([7_u8; 32]);
        body.extend(vec8(&[9; 32]));
        body.extend(vec16(&u16s(ciphers)));
        body.extend(vec8(&[0]));
        if !extensions.is_empty() {
            let all: Vec<u8> = extensions
                .iter()
                .flat_map(|(kind, data)| extension(*kind, data))
                .collect();
            body.extend(vec16(&all));
        }
        let mut message = vec![1];
        message.extend(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
        message.extend(body);
        message
    }

    pub(crate) fn sni(name: &str) -> (u16, Vec<u8>) {
        let mut entry = vec![0];
        entry.extend(vec16(name.as_bytes()));
        (0x0000, vec16(&entry))
    }

    pub(crate) fn alpn(protocols: &[&str]) -> (u16, Vec<u8>) {
        let list: Vec<u8> = protocols
            .iter()
            .flat_map(|protocol| vec8(protocol.as_bytes()))
            .collect();
        (0x0010, vec16(&list))
    }

    /// A hello shaped like a current Chrome's: GREASE in every list, the
    /// extensions in an arbitrary order.
    pub(crate) fn chrome_like(extension_order: &[usize]) -> Vec<u8> {
        let ciphers = [
            0x1a1a, 0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc013,
            0xc014, 0x009c, 0x009d, 0x002f, 0x0035,
        ];
        let all = [
            (0x2a2a, Vec::new()),
            sni("echo.kage.test"),
            (0x0017, Vec::new()),
            (0xff01, vec![0]),
            (
                0x000a,
                vec16(&u16s(&[0x3a3a, 0x11ec, 0x001d, 0x0017, 0x0018])),
            ),
            (0x000b, vec![1, 0]),
            (0x0023, Vec::new()),
            alpn(&["h2", "http/1.1"]),
            (0x0005, vec![1, 0, 0, 0, 0]),
            (
                0x000d,
                vec16(&u16s(&[
                    0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501, 0x0806, 0x0601,
                ])),
            ),
            (0x0012, Vec::new()),
            (0x0033, vec![0, 1, 0]),
            (0x002d, vec![1, 1]),
            (0x002b, vec8(&u16s(&[0x4a4a, 0x0304, 0x0303]))),
            (0x001b, vec![2, 0, 2]),
            (0x44cd, vec![0, 3, 2, b'h', b'2']),
            (0xfe0d, vec![0; 8]),
            (0x8a8a, vec![0]),
        ];
        let ordered: Vec<(u16, Vec<u8>)> = extension_order
            .iter()
            .map(|index| all[*index].clone())
            .collect();
        hello_bytes(&ciphers, &ordered)
    }

    #[test]
    fn grease_values_are_recognised() {
        for value in [0x0a0a, 0x1a1a, 0x2a2a, 0x7a7a, 0xfafa] {
            assert!(is_grease(value), "{value:04x}");
        }
        for value in [0x0000, 0x1301, 0x0a1a, 0x1a0a, 0x0a0b, 0xfe0d, 0x00ff] {
            assert!(!is_grease(value), "{value:04x}");
        }
    }

    #[test]
    fn a_chrome_like_hello_is_read_and_fingerprinted() {
        let order: Vec<usize> = (0..18).collect();
        let hello = ClientHello::parse(&chrome_like(&order)).unwrap();

        assert_eq!(hello.legacy_version, 0x0303);
        assert_eq!(hello.ciphers.len(), 16);
        assert_eq!(hello.extensions.len(), 18);
        assert_eq!(hello.sni.as_deref(), Some("echo.kage.test"));
        assert_eq!(hello.alpn, ["h2", "http/1.1"]);
        assert_eq!(hello.supported_versions, [0x4a4a, 0x0304, 0x0303]);
        assert_eq!(hello.groups, [0x3a3a, 0x11ec, 0x001d, 0x0017, 0x0018]);
        assert_eq!(hello.signature_algorithms.len(), 8);
        assert_eq!(hello.version(), "13");

        // 15 ciphers and 16 extensions without GREASE: Chrome's well-known
        // first part.
        assert_eq!(
            hello.ja4_raw(),
            "t13d1516h2_002f,0035,009c,009d,1301,1302,1303,c013,c014,c02b,c02c,c02f,c030,cca8,\
             cca9_0005,000a,000b,000d,0012,0017,001b,0023,002b,002d,0033,44cd,fe0d,ff01_0403,\
             0804,0401,0503,0805,0501,0806,0601"
        );
        // The value published for Chrome with this cipher list.
        assert_eq!(hello.ja4(), "t13d1516h2_8daaf6152771_d8a2da3f94cd");
    }

    #[test]
    fn shuffling_the_extensions_keeps_the_fingerprint() {
        let straight: Vec<usize> = (0..18).collect();
        let shuffled = [17, 3, 9, 1, 14, 0, 7, 12, 5, 16, 2, 11, 8, 13, 4, 15, 6, 10];

        let first = ClientHello::parse(&chrome_like(&straight)).unwrap();
        let second = ClientHello::parse(&chrome_like(&shuffled)).unwrap();

        assert_ne!(first.extensions, second.extensions);
        assert_eq!(first.ja4(), second.ja4());
        assert_eq!(first.ja4_raw(), second.ja4_raw());
    }

    #[test]
    fn the_first_part_reflects_sni_alpn_and_version() {
        let ciphers = [0x1301, 0x1302];
        let part = |extensions: &[(u16, Vec<u8>)]| {
            let hello = ClientHello::parse(&hello_bytes(&ciphers, extensions)).unwrap();
            hello.ja4().split('_').next().unwrap().to_owned()
        };
        let tls13 = (0x002b, vec8(&u16s(&[0x0304])));

        assert_eq!(
            part(&[sni("a.example"), alpn(&["h2"]), tls13.clone()]),
            "t13d0203h2"
        );
        // No server name: an address was dialled.
        assert_eq!(part(&[alpn(&["h2"]), tls13.clone()]), "t13i0202h2");
        assert_eq!(
            part(&[sni("a.example"), alpn(&["http/1.1"]), tls13.clone()]),
            "t13d0203h1"
        );
        assert_eq!(part(&[sni("a.example"), tls13]), "t13d020200");
        // Without supported_versions the hello's own version counts.
        assert_eq!(part(&[sni("a.example")]), "t12d020100");
        // An ALPN name that is not alphanumeric at its ends is written in hex.
        assert_eq!(part(&[alpn(&["\u{1}x\u{7f}"])]), "t12i02010f");
    }

    #[test]
    fn a_hello_without_extensions_or_signature_algorithms_still_has_a_fingerprint() {
        let bare = ClientHello::parse(&hello_bytes(&[0x002f, 0x0035], &[])).unwrap();
        assert_eq!(bare.extensions, Vec::<u16>::new());
        assert_eq!(
            bare.ja4(),
            format!("t12i020000_{}_000000000000", truncated_hash("002f,0035"))
        );
        assert_eq!(bare.ja4_raw(), "t12i020000_002f,0035_");

        // No signature algorithms: the last part ends without an underscore.
        let hello = ClientHello::parse(&hello_bytes(&[0x1301], &[(0x0017, Vec::new())])).unwrap();
        assert_eq!(hello.ja4_raw(), "t12i010100_1301_0017");

        let none = ClientHello::parse(&hello_bytes(&[0x0a0a], &[])).unwrap();
        assert_eq!(none.ja4(), "t12i000000_000000000000_000000000000");
    }

    #[test]
    fn broken_input_is_an_error_and_never_a_panic() {
        assert_eq!(ClientHello::parse(&[]), Err(HelloError::Truncated));
        assert_eq!(
            ClientHello::parse(&[2, 0, 0, 0]),
            Err(HelloError::NotClientHello)
        );
        assert_eq!(
            ClientHello::parse(&[1, 0, 0, 9, 3, 3]),
            Err(HelloError::Truncated)
        );

        let whole = chrome_like(&(0..18).collect::<Vec<_>>());
        for length in 0..whole.len() {
            // Cutting the message anywhere is detected through its length.
            assert_eq!(
                ClientHello::parse(&whole[..length]),
                Err(HelloError::Truncated),
                "cut at {length}"
            );
        }
        // A malformed extension body is kept as present and left empty.
        let odd = hello_bytes(
            &[0x1301],
            &[
                (0x0000, vec![0xff]),
                (0x0010, vec![0, 9, 1]),
                (0x002b, vec![9, 3]),
            ],
        );
        let hello = ClientHello::parse(&odd).unwrap();
        assert_eq!(hello.extensions, [0x0000, 0x0010, 0x002b]);
        assert_eq!(hello.sni, None);
        assert_eq!(hello.alpn, Vec::<String>::new());
        assert_eq!(hex_list(&[0x002f, 0xc02b]), "002f,c02b");
        assert_eq!(hex_list(&[]), "");
    }
}
