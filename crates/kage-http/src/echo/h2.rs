//! Just enough HTTP/2 to see what a client sends before and with its first
//! request, and to answer it.
//!
//! The "Akamai" HTTP/2 fingerprint is four things a client reveals without
//! being asked, joined by `|`:
//!
//! 1. its `SETTINGS`, as `id:value` pairs in the order sent;
//! 2. the increment of its first connection-level `WINDOW_UPDATE`, or `00`;
//! 3. its `PRIORITY` frames, as `stream:exclusive:dependency:weight`, or
//!    `0` when it sends none;
//! 4. the order of the pseudo-headers of its request: `m,a,s,p` for Chrome,
//!    `m,p,a,s` for Firefox.
//!
//! An HTTP/2 library keeps all of this to itself, so the echo reads the
//! frames from the socket.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

use super::hpack::{Decoder, HpackError, MAX_BLOCK_BYTES};

/// What every HTTP/2 client sends first.
pub(super) const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(super) const DATA: u8 = 0x0;
pub(super) const HEADERS: u8 = 0x1;
pub(super) const PRIORITY: u8 = 0x2;
pub(super) const SETTINGS: u8 = 0x4;
pub(super) const PING: u8 = 0x6;
pub(super) const GOAWAY: u8 = 0x7;
pub(super) const WINDOW_UPDATE: u8 = 0x8;
pub(super) const CONTINUATION: u8 = 0x9;

pub(super) const FLAG_END_STREAM: u8 = 0x1;
pub(super) const FLAG_ACK: u8 = 0x1;
pub(super) const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;

/// The largest frame payload a peer may send without asking.
pub(super) const MAX_FRAME_BYTES: usize = 16_384;

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Frame {
    pub kind: u8,
    pub flags: u8,
    pub stream: u32,
    pub payload: Vec<u8>,
}

/// Reads one frame; `None` when the peer closed the connection between
/// frames.
pub(super) async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut header = [0_u8; 9];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a frame is larger than the default maximum",
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(Frame {
        kind: header[3],
        flags: header[4],
        stream: u32::from_be_bytes([header[5] & 0x7f, header[6], header[7], header[8]]),
        payload,
    }))
}

/// The bytes of one frame.
pub(super) fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap_or(0).to_be_bytes();
    let mut bytes = Vec::with_capacity(9 + payload.len());
    bytes.extend_from_slice(&length[1..]);
    bytes.push(kind);
    bytes.push(flags);
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

/// A stream's place in the priority tree, from a `PRIORITY` frame or from
/// the priority fields of a `HEADERS` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPriority {
    /// The stream the priority is for.
    pub stream: u32,
    /// Whether the stream is its parent's only child.
    pub exclusive: bool,
    /// The parent stream.
    pub depends_on: u32,
    /// The weight, 1 to 256.
    pub weight: u16,
}

impl StreamPriority {
    fn parse(stream: u32, bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; 5] = bytes.get(..5)?.try_into().ok()?;
        Some(Self {
            stream,
            exclusive: bytes[0] & 0x80 != 0,
            depends_on: u32::from_be_bytes([bytes[0] & 0x7f, bytes[1], bytes[2], bytes[3]]),
            weight: u16::from(bytes[4]) + 1,
        })
    }

    /// `stream:exclusive:dependency:weight`, as the fingerprint writes it.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.stream,
            u8::from(self.exclusive),
            self.depends_on,
            self.weight
        )
    }
}

/// A request whose header block is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Request {
    pub stream: u32,
    /// Every field in the order sent, the pseudo-headers included.
    pub fields: Vec<(String, String)>,
    /// The priority fields of the `HEADERS` frame, when it carried any.
    pub priority: Option<StreamPriority>,
}

/// Why a connection has to be closed.
#[derive(Debug, thiserror::Error)]
pub(super) enum H2Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Hpack(#[from] HpackError),
    #[error("the client broke the protocol: {0}")]
    Protocol(&'static str),
}

/// What the echo must do after a frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Nothing.
    Continue,
    /// Write these bytes.
    Write(Vec<u8>),
    /// Answer this request.
    Respond(Request),
    /// The client is going away.
    Close,
}

/// What one HTTP/2 connection has revealed so far.
#[derive(Debug, Default)]
pub(super) struct Connection {
    decoder: Decoder,
    /// The first `SETTINGS` frame, in the order sent.
    pub settings: Option<Vec<(u16, u32)>>,
    /// The first connection-level `WINDOW_UPDATE`.
    pub window_update: Option<u32>,
    /// Every `PRIORITY` frame.
    pub priorities: Vec<StreamPriority>,
    /// A header block still waiting for its `CONTINUATION` frames.
    pending: Option<(u32, Vec<u8>, Option<StreamPriority>)>,
}

impl Connection {
    /// Takes one frame from the client.
    pub(super) fn on_frame(&mut self, frame: &Frame) -> Result<Step, H2Error> {
        if self.pending.is_some() && frame.kind != CONTINUATION {
            return Err(H2Error::Protocol(
                "a header block was interrupted by another frame",
            ));
        }
        match frame.kind {
            SETTINGS => {
                if frame.flags & FLAG_ACK != 0 {
                    return Ok(Step::Continue);
                }
                if self.settings.is_none() {
                    let (entries, _) = frame.payload.as_chunks::<6>();
                    self.settings = Some(
                        entries
                            .iter()
                            .map(|entry| {
                                (
                                    u16::from_be_bytes([entry[0], entry[1]]),
                                    u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]),
                                )
                            })
                            .collect(),
                    );
                }
                Ok(Step::Write(super::h2::frame(SETTINGS, FLAG_ACK, 0, &[])))
            }
            WINDOW_UPDATE => {
                if frame.stream == 0
                    && self.window_update.is_none()
                    && let Some(bytes) = frame.payload.get(..4)
                {
                    self.window_update = Some(u32::from_be_bytes([
                        bytes[0] & 0x7f,
                        bytes[1],
                        bytes[2],
                        bytes[3],
                    ]));
                }
                Ok(Step::Continue)
            }
            PRIORITY => {
                if let Some(priority) = StreamPriority::parse(frame.stream, &frame.payload) {
                    self.priorities.push(priority);
                }
                Ok(Step::Continue)
            }
            HEADERS => {
                let mut block = frame.payload.as_slice();
                let mut padding = 0;
                if frame.flags & FLAG_PADDED != 0 {
                    let (&length, rest) = block
                        .split_first()
                        .ok_or(H2Error::Protocol("a padded frame without a length"))?;
                    padding = usize::from(length);
                    block = rest;
                }
                let mut priority = None;
                if frame.flags & FLAG_PRIORITY != 0 {
                    priority = StreamPriority::parse(frame.stream, block);
                    block = block
                        .get(5..)
                        .ok_or(H2Error::Protocol("priority fields cut short"))?;
                }
                let block = block
                    .len()
                    .checked_sub(padding)
                    .map(|end| &block[..end])
                    .ok_or(H2Error::Protocol("more padding than payload"))?;
                self.pending = Some((frame.stream, block.to_vec(), priority));
                self.finish_block(frame.flags)
            }
            CONTINUATION => {
                let Some((stream, block, _)) = &mut self.pending else {
                    return Err(H2Error::Protocol("a continuation without a header block"));
                };
                if *stream != frame.stream {
                    return Err(H2Error::Protocol("a continuation on another stream"));
                }
                if block.len() + frame.payload.len() > MAX_BLOCK_BYTES {
                    return Err(H2Error::Protocol("a header block that is too large"));
                }
                block.extend_from_slice(&frame.payload);
                self.finish_block(frame.flags)
            }
            PING => {
                if frame.flags & FLAG_ACK != 0 {
                    Ok(Step::Continue)
                } else {
                    Ok(Step::Write(super::h2::frame(
                        PING,
                        FLAG_ACK,
                        0,
                        &frame.payload,
                    )))
                }
            }
            GOAWAY => Ok(Step::Close),
            // DATA of a request body, RST_STREAM and anything unknown: the
            // echo answers from the headers alone.
            _ => Ok(Step::Continue),
        }
    }

    fn finish_block(&mut self, flags: u8) -> Result<Step, H2Error> {
        if flags & FLAG_END_HEADERS == 0 {
            return Ok(Step::Continue);
        }
        let Some((stream, block, priority)) = self.pending.take() else {
            return Ok(Step::Continue);
        };
        let fields = self.decoder.decode(&block)?;
        Ok(Step::Respond(Request {
            stream,
            fields,
            priority,
        }))
    }

    /// The first three parts of the fingerprint; the fourth belongs to a
    /// request.
    fn fingerprint_prefix(&self) -> String {
        let settings = self
            .settings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|(id, value)| format!("{id}:{value}"))
            .collect::<Vec<_>>()
            .join(";");
        let window = self
            .window_update
            .map_or_else(|| "00".to_owned(), |increment| increment.to_string());
        let priorities = if self.priorities.is_empty() {
            "0".to_owned()
        } else {
            self.priorities
                .iter()
                .map(StreamPriority::fingerprint)
                .collect::<Vec<_>>()
                .join(",")
        };
        format!("{settings}|{window}|{priorities}")
    }

    /// The Akamai fingerprint of the connection with this request.
    pub(super) fn akamai(&self, request: &Request) -> String {
        format!(
            "{}|{}",
            self.fingerprint_prefix(),
            pseudo_order(&request.fields).join(",")
        )
    }
}

/// The pseudo-headers of a request by their initials, in the order sent:
/// `m` for `:method`, `a` for `:authority`, `s` for `:scheme`, `p` for
/// `:path`.
pub(super) fn pseudo_order(fields: &[(String, String)]) -> Vec<String> {
    fields
        .iter()
        .filter_map(|(name, _)| name.strip_prefix(':'))
        .map(|name| name.chars().take(1).collect())
        .collect()
}

/// The frames of a response: its header block and its body, the body cut
/// into frames of the default maximum size.
pub(super) fn response(stream: u32, fields: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let block = super::hpack::encode_plain(fields);
    let mut bytes = frame(HEADERS, FLAG_END_HEADERS, stream, &block);
    let mut chunks = body.chunks(MAX_FRAME_BYTES).peekable();
    if chunks.peek().is_none() {
        bytes.extend(frame(DATA, FLAG_END_STREAM, stream, &[]));
    }
    while let Some(chunk) = chunks.next() {
        let flags = if chunks.peek().is_none() {
            FLAG_END_STREAM
        } else {
            0
        };
        bytes.extend(frame(DATA, flags, stream, chunk));
    }
    bytes
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        CONTINUATION, Connection, DATA, FLAG_ACK, FLAG_END_HEADERS, FLAG_END_STREAM, Frame, GOAWAY,
        H2Error, HEADERS, PING, PRIORITY, Request, SETTINGS, Step, StreamPriority, WINDOW_UPDATE,
        frame, pseudo_order, read_frame, response,
    };
    use crate::echo::hpack::encode_plain;

    fn parsed(bytes: &[u8]) -> Vec<Frame> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut reader = bytes;
            let mut frames = Vec::new();
            while let Some(frame) = read_frame(&mut reader).await.unwrap() {
                frames.push(frame);
            }
            frames
        })
    }

    fn settings_payload(entries: &[(u16, u32)]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|(id, value)| {
                let mut bytes = id.to_be_bytes().to_vec();
                bytes.extend(value.to_be_bytes());
                bytes
            })
            .collect()
    }

    fn feed(connection: &mut Connection, kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Step {
        connection
            .on_frame(&Frame {
                kind,
                flags,
                stream,
                payload: payload.to_vec(),
            })
            .unwrap()
    }

    fn request_block(order: &[&str]) -> Vec<u8> {
        let fields: Vec<(&str, &str)> = order
            .iter()
            .map(|name| match *name {
                ":method" => (":method", "GET"),
                ":authority" => (":authority", "echo.kage.test"),
                ":scheme" => (":scheme", "https"),
                ":path" => (":path", "/"),
                other => (other, "v"),
            })
            .collect();
        encode_plain(&fields)
    }

    /// What Chrome sends when it opens a connection.
    fn chrome_opening() -> Connection {
        let mut connection = Connection::default();
        let step = feed(
            &mut connection,
            SETTINGS,
            0,
            0,
            &settings_payload(&[(1, 65_536), (2, 0), (4, 6_291_456), (6, 262_144)]),
        );
        assert_eq!(step, Step::Write(frame(SETTINGS, FLAG_ACK, 0, &[])));
        assert_eq!(
            feed(
                &mut connection,
                WINDOW_UPDATE,
                0,
                0,
                &15_663_105_u32.to_be_bytes()
            ),
            Step::Continue
        );
        connection
    }

    #[test]
    fn frames_are_read_and_written_in_the_wire_format() {
        let bytes = [
            frame(SETTINGS, 0, 0, &settings_payload(&[(4, 131_072)])),
            frame(HEADERS, FLAG_END_HEADERS | FLAG_END_STREAM, 1, &[0x82]),
            frame(DATA, 0, 0x8000_0003, b"hi"),
        ]
        .concat();

        let frames = parsed(&bytes);

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].kind, SETTINGS);
        assert_eq!(frames[0].payload.len(), 6);
        assert_eq!(
            (frames[1].kind, frames[1].flags, frames[1].stream),
            (HEADERS, 0x5, 1)
        );
        // The reserved bit of the stream id is ignored.
        assert_eq!(frames[2].stream, 3);
        assert_eq!(frames[2].payload, b"hi");
        assert_eq!(&bytes[..9], [0, 0, 6, SETTINGS, 0, 0, 0, 0, 0]);
        assert_eq!(parsed(&[]), []);
    }

    #[test]
    fn a_frame_that_is_cut_short_or_too_large_is_an_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let whole = frame(PING, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut cut = &whole[..12];
        assert!(runtime.block_on(read_frame(&mut cut)).is_err());

        let mut huge: &[u8] = &[0x01, 0x00, 0x00, DATA, 0, 0, 0, 0, 1];
        assert!(runtime.block_on(read_frame(&mut huge)).is_err());
    }

    #[test]
    fn chromes_opening_gives_chromes_fingerprint() {
        let mut connection = chrome_opening();

        let step = feed(
            &mut connection,
            HEADERS,
            FLAG_END_HEADERS | FLAG_END_STREAM | 0x20,
            1,
            &[
                // Exclusive, depends on stream 0, weight 256.
                &[0x80, 0, 0, 0, 255][..],
                &request_block(&[":method", ":authority", ":scheme", ":path", "user-agent"]),
            ]
            .concat(),
        );

        let Step::Respond(request) = step else {
            panic!("a complete header block is a request, got {step:?}");
        };
        assert_eq!(request.stream, 1);
        assert_eq!(request.fields.len(), 5);
        assert_eq!(request.fields[4], ("user-agent".to_owned(), "v".to_owned()));
        assert_eq!(
            request.priority,
            Some(StreamPriority {
                stream: 1,
                exclusive: true,
                depends_on: 0,
                weight: 256
            })
        );
        assert_eq!(
            connection.akamai(&request),
            "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p"
        );
        assert_eq!(pseudo_order(&request.fields), ["m", "a", "s", "p"]);
    }

    #[test]
    fn firefoxs_opening_gives_firefoxs_fingerprint() {
        let mut connection = Connection::default();
        feed(
            &mut connection,
            SETTINGS,
            0,
            0,
            &settings_payload(&[(1, 65_536), (2, 0), (4, 131_072), (5, 16_384)]),
        );
        feed(
            &mut connection,
            WINDOW_UPDATE,
            0,
            0,
            &12_517_377_u32.to_be_bytes(),
        );
        // A later settings frame and window update change nothing.
        feed(
            &mut connection,
            SETTINGS,
            0,
            0,
            &settings_payload(&[(3, 100)]),
        );
        feed(&mut connection, WINDOW_UPDATE, 0, 0, &1_u32.to_be_bytes());
        feed(&mut connection, WINDOW_UPDATE, 0, 3, &9_u32.to_be_bytes());

        let Step::Respond(request) = feed(
            &mut connection,
            HEADERS,
            FLAG_END_HEADERS,
            3,
            &request_block(&[":method", ":path", ":authority", ":scheme"]),
        ) else {
            panic!("expected a request");
        };

        assert_eq!(request.priority, None);
        assert_eq!(
            connection.akamai(&request),
            "1:65536;2:0;4:131072;5:16384|12517377|0|m,p,a,s"
        );
    }

    #[test]
    fn priority_frames_and_a_missing_window_update_show_in_the_fingerprint() {
        let mut connection = Connection::default();
        feed(
            &mut connection,
            SETTINGS,
            0,
            0,
            &settings_payload(&[(3, 100)]),
        );
        feed(&mut connection, PRIORITY, 0, 3, &[0, 0, 0, 0, 200]);
        feed(&mut connection, PRIORITY, 0, 5, &[0x80, 0, 0, 3, 0]);
        // A priority frame that is cut short is skipped.
        feed(&mut connection, PRIORITY, 0, 7, &[0, 0]);

        let Step::Respond(request) = feed(
            &mut connection,
            HEADERS,
            FLAG_END_HEADERS,
            9,
            &request_block(&[":method"]),
        ) else {
            panic!("expected a request");
        };

        assert_eq!(connection.akamai(&request), "3:100|00|3:0:0:201,5:1:3:1|m");
        // Nothing sent at all.
        let silent = Connection::default();
        assert_eq!(
            silent.akamai(&Request {
                stream: 1,
                fields: Vec::new(),
                priority: None
            }),
            "|00|0|"
        );
    }

    #[test]
    fn a_header_block_may_be_padded_and_split_over_continuations() {
        let block = request_block(&[":method", ":scheme", ":path", ":authority", "accept"]);
        let (first, rest) = block.split_at(7);
        let (second, third) = rest.split_at(5);
        let mut connection = Connection::default();

        let mut padded = vec![3];
        padded.extend_from_slice(first);
        padded.extend_from_slice(&[0, 0, 0]);
        assert_eq!(
            feed(&mut connection, HEADERS, 0x8, 1, &padded),
            Step::Continue
        );
        assert_eq!(
            feed(&mut connection, CONTINUATION, 0, 1, second),
            Step::Continue
        );
        let Step::Respond(request) =
            feed(&mut connection, CONTINUATION, FLAG_END_HEADERS, 1, third)
        else {
            panic!("expected a request");
        };

        assert_eq!(pseudo_order(&request.fields), ["m", "s", "p", "a"]);
        assert_eq!(request.fields[4].0, "accept");
    }

    #[test]
    fn pings_are_answered_and_goaway_closes() {
        let mut connection = Connection::default();

        assert_eq!(
            feed(&mut connection, PING, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8]),
            Step::Write(frame(PING, FLAG_ACK, 0, &[1, 2, 3, 4, 5, 6, 7, 8]))
        );
        assert_eq!(
            feed(&mut connection, PING, FLAG_ACK, 0, &[0; 8]),
            Step::Continue
        );
        assert_eq!(
            feed(&mut connection, SETTINGS, FLAG_ACK, 0, &[]),
            Step::Continue
        );
        assert_eq!(
            feed(&mut connection, DATA, FLAG_END_STREAM, 1, b"body"),
            Step::Continue
        );
        assert_eq!(
            feed(&mut connection, 0x3, 0, 1, &[0, 0, 0, 8]),
            Step::Continue
        );
        assert_eq!(feed(&mut connection, 0xfa, 0, 0, &[]), Step::Continue);
        assert_eq!(feed(&mut connection, GOAWAY, 0, 0, &[0; 8]), Step::Close);
    }

    #[test]
    fn protocol_violations_close_the_connection() {
        let error = |frames: &[(u8, u8, u32, &[u8])]| -> H2Error {
            let mut connection = Connection::default();
            let mut last = None;
            for (kind, flags, stream, payload) in frames {
                last = Some(connection.on_frame(&Frame {
                    kind: *kind,
                    flags: *flags,
                    stream: *stream,
                    payload: payload.to_vec(),
                }));
            }
            last.unwrap().unwrap_err()
        };

        assert!(matches!(
            error(&[(CONTINUATION, FLAG_END_HEADERS, 1, &[0x82])]),
            H2Error::Protocol(_)
        ));
        assert!(matches!(
            error(&[(HEADERS, 0, 1, &[0x82]), (PING, 0, 0, &[0; 8])]),
            H2Error::Protocol(_)
        ));
        assert!(matches!(
            error(&[(HEADERS, 0, 1, &[0x82]), (CONTINUATION, 0, 3, &[0x84])]),
            H2Error::Protocol(_)
        ));
        assert!(matches!(
            error(&[(HEADERS, 0x8, 1, &[])]),
            H2Error::Protocol(_)
        ));
        assert!(matches!(
            error(&[(HEADERS, 0x8, 1, &[9, 0x82])]),
            H2Error::Protocol(_)
        ));
        assert!(matches!(
            error(&[(HEADERS, 0x20, 1, &[0, 0])]),
            H2Error::Protocol(_)
        ));
        // A header block that does not decode.
        assert!(matches!(
            error(&[(HEADERS, FLAG_END_HEADERS, 1, &[0xff, 0x00])]),
            H2Error::Hpack(_)
        ));
    }

    #[test]
    fn a_response_is_a_header_block_and_data_frames() {
        let frames = parsed(&response(1, &[(":status", "200")], b"{}"));
        assert_eq!(frames.len(), 2);
        assert_eq!(
            (frames[0].kind, frames[0].flags),
            (HEADERS, FLAG_END_HEADERS)
        );
        assert_eq!((frames[1].kind, frames[1].flags), (DATA, FLAG_END_STREAM));
        assert_eq!(frames[1].payload, b"{}");

        // A long body is cut into frames; only the last one ends the stream.
        let body = vec![b'x'; 40_000];
        let frames = parsed(&response(5, &[(":status", "200")], &body));
        let data: Vec<&Frame> = frames.iter().filter(|frame| frame.kind == DATA).collect();
        assert_eq!(data.len(), 3);
        assert_eq!(
            data.iter().map(|frame| frame.payload.len()).sum::<usize>(),
            40_000
        );
        assert_eq!(
            data.iter().map(|frame| frame.flags).collect::<Vec<_>>(),
            [0, 0, FLAG_END_STREAM]
        );
        assert!(frames.iter().all(|frame| frame.stream == 5));

        let empty = parsed(&response(1, &[(":status", "204")], b""));
        assert_eq!(
            (empty[1].kind, empty[1].flags, empty[1].payload.len()),
            (DATA, FLAG_END_STREAM, 0)
        );
    }
}
