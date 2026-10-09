//! An HPACK decoder (RFC 7541), so that the echo can report the header
//! fields of an HTTP/2 request in the order they were sent.
//!
//! An HTTP/2 library hands an application a header map, which has lost the
//! order of the fields and of the pseudo-headers. Both are part of a
//! browser's fingerprint, so the echo decodes the header block itself.

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;

use super::huffman::CODES;

/// The dynamic table size a connection starts with.
const DEFAULT_TABLE_SIZE: usize = 4096;

/// Longest header block the echo decodes.
pub(super) const MAX_BLOCK_BYTES: usize = 256 * 1024;

/// RFC 7541, Appendix A.
const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// Why a header block could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HpackError {
    /// The block ends inside a field.
    #[error("the header block is cut short")]
    Truncated,
    /// An index points outside both tables.
    #[error("the header block refers to a table entry that does not exist")]
    BadIndex,
    /// An integer does not fit.
    #[error("the header block holds a number that is too large")]
    Overflow,
    /// A Huffman-coded string is not valid.
    #[error("the header block holds an invalid Huffman code")]
    Huffman,
    /// A table size update asks for more than the connection allows.
    #[error("the header block resizes the table beyond its limit")]
    TableSize,
}

fn huffman_symbols() -> &'static HashMap<(u8, u32), u16> {
    static SYMBOLS: OnceLock<HashMap<(u8, u32), u16>> = OnceLock::new();
    SYMBOLS.get_or_init(|| {
        CODES
            .iter()
            .zip(0_u16..)
            .map(|(&(code, bits), symbol)| ((bits, code), symbol))
            .collect()
    })
}

/// Decodes a Huffman-coded string.
fn huffman_decode(input: &[u8]) -> Result<Vec<u8>, HpackError> {
    let symbols = huffman_symbols();
    let mut output = Vec::with_capacity(input.len() * 2);
    let (mut code, mut bits) = (0_u32, 0_u8);
    for byte in input {
        for shift in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> shift) & 1);
            bits += 1;
            if let Some(&symbol) = symbols.get(&(bits, code)) {
                // The end-of-string symbol must not appear in a string.
                output.push(u8::try_from(symbol).map_err(|_| HpackError::Huffman)?);
                (code, bits) = (0, 0);
            } else if bits >= 30 {
                return Err(HpackError::Huffman);
            }
        }
    }
    // What is left must be padding: fewer than eight bits, all ones.
    if bits > 7 || code != (1_u32 << bits) - 1 {
        return Err(HpackError::Huffman);
    }
    Ok(output)
}

struct Input<'a> {
    bytes: &'a [u8],
}

impl Input<'_> {
    fn byte(&mut self) -> Result<u8, HpackError> {
        let (&first, rest) = self.bytes.split_first().ok_or(HpackError::Truncated)?;
        self.bytes = rest;
        Ok(first)
    }

    /// An integer whose first byte, already read, holds `prefix_bits` of it.
    fn integer(&mut self, first: u8, prefix_bits: u8) -> Result<usize, HpackError> {
        let mask = (1_u16 << prefix_bits) - 1;
        let mut value = usize::from(u16::from(first) & mask);
        if value < usize::from(mask) {
            return Ok(value);
        }
        let mut shift = 0_u32;
        loop {
            let byte = self.byte()?;
            if shift > 28 {
                return Err(HpackError::Overflow);
            }
            value = value
                .checked_add(usize::from(byte & 0x7f) << shift)
                .ok_or(HpackError::Overflow)?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn string(&mut self) -> Result<String, HpackError> {
        let first = self.byte()?;
        let length = self.integer(first, 7)?;
        if self.bytes.len() < length {
            return Err(HpackError::Truncated);
        }
        let (raw, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        let decoded = if first & 0x80 == 0 {
            raw.to_vec()
        } else {
            huffman_decode(raw)?
        };
        Ok(String::from_utf8_lossy(&decoded).into_owned())
    }
}

/// The decoding side of one connection's header compression.
#[derive(Debug)]
pub struct Decoder {
    /// Newest entry first.
    dynamic: VecDeque<(String, String)>,
    size: usize,
    max_size: usize,
    limit: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            dynamic: VecDeque::new(),
            size: 0,
            max_size: DEFAULT_TABLE_SIZE,
            limit: DEFAULT_TABLE_SIZE,
        }
    }
}

impl Decoder {
    fn entry(&self, index: usize) -> Result<(String, String), HpackError> {
        if index == 0 {
            return Err(HpackError::BadIndex);
        }
        if let Some((name, value)) = STATIC_TABLE.get(index - 1) {
            return Ok(((*name).to_owned(), (*value).to_owned()));
        }
        self.dynamic
            .get(index - 1 - STATIC_TABLE.len())
            .cloned()
            .ok_or(HpackError::BadIndex)
    }

    fn evict(&mut self) {
        while self.size > self.max_size {
            match self.dynamic.pop_back() {
                Some((name, value)) => self.size -= name.len() + value.len() + 32,
                None => break,
            }
        }
    }

    fn insert(&mut self, name: &str, value: &str) {
        self.size += name.len() + value.len() + 32;
        self.dynamic.push_front((name.to_owned(), value.to_owned()));
        self.evict();
    }

    /// Decodes one header block into its fields, in the order sent, the
    /// pseudo-headers included.
    ///
    /// # Errors
    ///
    /// Fails when the block is malformed. The connection's compression
    /// state is then out of step and the connection must be closed.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<(String, String)>, HpackError> {
        let mut input = Input { bytes: block };
        let mut fields = Vec::new();
        while !input.bytes.is_empty() {
            let first = input.byte()?;
            if first & 0x80 != 0 {
                // An indexed field.
                let index = input.integer(first, 7)?;
                fields.push(self.entry(index)?);
            } else if first & 0xe0 == 0x20 {
                // A change of the table size.
                let size = input.integer(first, 5)?;
                if size > self.limit {
                    return Err(HpackError::TableSize);
                }
                self.max_size = size;
                self.evict();
            } else {
                // A literal field: added to the table (01), or not (0000
                // and 0001).
                let indexed = first & 0xc0 == 0x40;
                let index = input.integer(first, if indexed { 6 } else { 4 })?;
                let name = if index == 0 {
                    input.string()?
                } else {
                    self.entry(index)?.0
                };
                let value = input.string()?;
                if indexed {
                    self.insert(&name, &value);
                }
                fields.push((name, value));
            }
        }
        Ok(fields)
    }
}

/// Encodes response fields the simplest valid way: every field a literal
/// that is not added to the table, nothing Huffman-coded.
pub(super) fn encode_plain(fields: &[(&str, &str)]) -> Vec<u8> {
    fn string(output: &mut Vec<u8>, text: &str) {
        let mut length = text.len();
        if length < 127 {
            output.push(u8::try_from(length).unwrap_or(0));
        } else {
            output.push(127);
            length -= 127;
            while length >= 128 {
                output.push(u8::try_from(length % 128).unwrap_or(0) | 0x80);
                length /= 128;
            }
            output.push(u8::try_from(length).unwrap_or(0));
        }
        output.extend_from_slice(text.as_bytes());
    }
    let mut output = Vec::new();
    for (name, value) in fields {
        output.push(0x00);
        string(&mut output, name);
        string(&mut output, value);
    }
    output
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Decoder, HpackError, encode_plain, huffman_decode};
    use crate::echo::huffman::CODES;

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(u8::is_ascii_hexdigit)
            .map(|digit| u8::try_from(char::from(digit).to_digit(16).unwrap()).unwrap())
            .collect();
        digits
            .chunks(2)
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect()
    }

    fn pairs(fields: &[(String, String)]) -> Vec<(&str, &str)> {
        fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    /// Huffman-codes a string, for tests that need input for the decoder.
    pub(crate) fn huffman_encode(text: &[u8]) -> Vec<u8> {
        let (mut bits, mut count, mut output) = (0_u64, 0_u32, Vec::new());
        for &byte in text {
            let (code, length) = CODES[usize::from(byte)];
            bits = (bits << length) | u64::from(code);
            count += u32::from(length);
            while count >= 8 {
                count -= 8;
                output.push(((bits >> count) & 0xff) as u8);
            }
        }
        if count > 0 {
            let padding = 8 - count;
            output.push((((bits << padding) | ((1 << padding) - 1)) & 0xff) as u8);
        }
        output
    }

    #[test]
    fn the_huffman_table_decodes_the_examples_of_the_rfc() {
        // RFC 7541, Appendix C.4 and C.6.
        for (encoded, text) in [
            ("f1e3 c2e5 f23a 6ba0 ab90 f4ff", "www.example.com"),
            ("a8eb 1064 9cbf", "no-cache"),
            ("25a8 49e9 5ba9 7d7f", "custom-key"),
            ("25a8 49e9 5bb8 e8b4 bf", "custom-value"),
            ("6402", "302"),
            ("aec3 771a 4b", "private"),
            (
                "d07a be94 1054 d444 a820 0595 040b 8166 e082 a62d 1bff",
                "Mon, 21 Oct 2013 20:13:21 GMT",
            ),
            (
                "9d29 ad17 1863 c78f 0b97 c8e9 ae82 ae43 d3",
                "https://www.example.com",
            ),
            ("640e ff", "307"),
            ("9bd9 ab", "gzip"),
            (
                "94e7 821d d7f2 e6c7 b335 dfdf cd5b 3960 d5af 2708 7f36 72c1 ab27 0fb5 291f \
                 9587 3160 65c0 03ed 4ee5 b106 3d50 07",
                "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
            ),
        ] {
            assert_eq!(
                String::from_utf8(huffman_decode(&hex(encoded)).unwrap()).unwrap(),
                text
            );
        }
        assert_eq!(huffman_decode(&[]).unwrap(), b"");
    }

    #[test]
    fn every_byte_value_survives_the_huffman_code() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(huffman_decode(&huffman_encode(&all)).unwrap(), all);
        // The same bytes in another order, so that every code is read at
        // several bit offsets.
        let mixed: Vec<u8> = (0..=255_u8).map(|byte| byte.wrapping_mul(167)).collect();
        assert_eq!(huffman_decode(&huffman_encode(&mixed)).unwrap(), mixed);
        // The code is a prefix code with 257 distinct entries.
        assert_eq!(super::huffman_symbols().len(), 257);
    }

    #[test]
    fn invalid_huffman_input_is_refused() {
        // Padding that is not all ones.
        assert_eq!(
            huffman_decode(&hex("a8eb 1064 9cbe")),
            Err(HpackError::Huffman)
        );
        // A whole byte of padding.
        assert_eq!(
            huffman_decode(&hex("a8eb 1064 9cbf ff")),
            Err(HpackError::Huffman)
        );
        // The end-of-string symbol inside the string.
        assert_eq!(huffman_decode(&hex("ffff ffff")), Err(HpackError::Huffman));
    }

    #[test]
    fn the_request_examples_of_the_rfc_decode_with_the_dynamic_table() {
        // RFC 7541, Appendix C.4: three requests on one connection, with
        // Huffman coding.
        let mut decoder = Decoder::default();

        let first = decoder
            .decode(&hex("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff"))
            .unwrap();
        assert_eq!(
            pairs(&first),
            [
                (":method", "GET"),
                (":scheme", "http"),
                (":path", "/"),
                (":authority", "www.example.com"),
            ]
        );

        let second = decoder
            .decode(&hex("8286 84be 5886 a8eb 1064 9cbf"))
            .unwrap();
        assert_eq!(
            pairs(&second),
            [
                (":method", "GET"),
                (":scheme", "http"),
                (":path", "/"),
                (":authority", "www.example.com"),
                ("cache-control", "no-cache"),
            ]
        );

        let third = decoder
            .decode(&hex(
                "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf",
            ))
            .unwrap();
        assert_eq!(
            pairs(&third),
            [
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/index.html"),
                (":authority", "www.example.com"),
                ("custom-key", "custom-value"),
            ]
        );
        assert_eq!(decoder.dynamic.len(), 3);
        assert_eq!(decoder.size, 164);
    }

    #[test]
    fn the_response_examples_of_the_rfc_evict_old_entries() {
        // RFC 7541, Appendix C.5: a table of 256 bytes, without Huffman.
        let mut decoder = Decoder {
            max_size: 256,
            limit: 256,
            ..Decoder::default()
        };

        let first = decoder
            .decode(&hex(
                "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 \
                 3133 2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77 7777 2e65 \
                 7861 6d70 6c65 2e63 6f6d",
            ))
            .unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(first[0], (":status".to_owned(), "302".to_owned()));
        assert_eq!(decoder.size, 222);

        let second = decoder.decode(&hex("4803 3330 37c1 c0bf")).unwrap();
        assert_eq!(
            pairs(&second),
            [
                (":status", "307"),
                ("cache-control", "private"),
                ("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                ("location", "https://www.example.com"),
            ]
        );
        // `:status: 302` was evicted to make room.
        assert_eq!(decoder.size, 222);
        assert_eq!(decoder.dynamic.len(), 4);
        assert_eq!(decoder.dynamic[0].1, "307");
    }

    #[test]
    fn literals_that_are_not_indexed_leave_the_table_alone() {
        let mut decoder = Decoder::default();

        // C.2.2 (without indexing) and C.2.3 (never indexed).
        let fields = decoder
            .decode(&hex(
                "040c 2f73 616d 706c 652f 7061 7468 1008 7061 7373 776f 7264 0673 6563 7265 74",
            ))
            .unwrap();
        assert_eq!(
            pairs(&fields),
            [(":path", "/sample/path"), ("password", "secret")]
        );
        assert!(decoder.dynamic.is_empty());

        // A table size update to zero empties the table.
        decoder
            .decode(&hex(
                "400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572",
            ))
            .unwrap();
        assert_eq!(decoder.dynamic.len(), 1);
        decoder.decode(&[0x20]).unwrap();
        assert!(decoder.dynamic.is_empty());
        assert_eq!(decoder.size, 0);
    }

    #[test]
    fn malformed_blocks_are_refused() {
        let mut decoder = Decoder::default();

        assert_eq!(decoder.decode(&[0x80]), Err(HpackError::BadIndex));
        assert_eq!(decoder.decode(&[0xff, 0x00]), Err(HpackError::BadIndex));
        assert_eq!(decoder.decode(&[0xbe]), Err(HpackError::BadIndex));
        assert_eq!(decoder.decode(&[0x40]), Err(HpackError::Truncated));
        assert_eq!(
            decoder.decode(&[0x00, 0x05, b'a']),
            Err(HpackError::Truncated)
        );
        assert_eq!(decoder.decode(&[0x82, 0xff]), Err(HpackError::Truncated));
        assert_eq!(
            decoder.decode(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]),
            Err(HpackError::Overflow)
        );
        // A table larger than the connection allows.
        assert_eq!(
            decoder.decode(&[0x3f, 0xe2, 0x1f]),
            Err(HpackError::TableSize)
        );
        assert_eq!(
            decoder.decode(&[0x00, 0x81, 0x00, 0x00]),
            Err(HpackError::Huffman)
        );
    }

    #[test]
    fn plain_encoding_is_read_back_by_the_decoder() {
        let long = "v".repeat(300);
        let fields = [
            (":status", "200"),
            ("content-type", "application/json"),
            ("x-long", long.as_str()),
            ("x-edge", &"e".repeat(127)),
            ("x-empty", ""),
        ];

        let decoded = Decoder::default().decode(&encode_plain(&fields)).unwrap();

        assert_eq!(pairs(&decoded), fields);
    }
}
