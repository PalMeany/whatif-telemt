//! Shared frame codec for the WEB proxy carrier protocol (v1).
//!
//! Wire layout is `type:u8 | stream_id:u24 | payload_length:u32 | payload`,
//! all integers unsigned big-endian. Parsing borrows from the carrier body so
//! an uplink batch is never copied before it reaches a stream queue.

/// Fixed frame header size in bytes.
pub(crate) const HEADER_SIZE: usize = 8;

/// Largest payload a single frame may carry.
pub(crate) const MAX_PAYLOAD: usize = 1024 * 1024;

/// Per-direction credit granted to a stream when it is opened.
pub(crate) const INITIAL_STREAM_WINDOW: u32 = 4 * 1024 * 1024;

/// Largest relay-produced DATA chunk.
pub(crate) const DATA_CHUNK: usize = 64 * 1024;

/// Highest representable stream id (24 bits).
pub(crate) const MAX_STREAM_ID: u32 = 0x00FF_FFFF;

/// Largest number of frames one carrier body may contain.
pub(crate) const MAX_BATCH_FRAMES: usize = 4096;

/// Frame type byte.
///
/// Unknown type bytes stay representable on purpose: `parse_all` must accept
/// any well-formed header and let shape validation reject the frame, exactly
/// like the reference relay does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameType(pub(crate) u8);

impl FrameType {
    pub(crate) const OPEN: FrameType = FrameType(0x01);
    pub(crate) const DATA: FrameType = FrameType(0x02);
    pub(crate) const CLOSE: FrameType = FrameType(0x03);
    pub(crate) const WINDOW: FrameType = FrameType(0x04);
    /// Reserved by the protocol; this implementation does not emit PING in v1.
    #[allow(dead_code)]
    pub(crate) const PING: FrameType = FrameType(0x05);
    pub(crate) const PONG: FrameType = FrameType(0x06);
    pub(crate) const HELLO: FrameType = FrameType(0x10);
    pub(crate) const WELCOME: FrameType = FrameType(0x11);
    /// Reserved by the protocol; this implementation does not emit BYE in v1.
    #[allow(dead_code)]
    pub(crate) const BYE: FrameType = FrameType(0x1F);
}

/// One parsed frame borrowing its payload from the carrier body.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Frame<'a> {
    pub(crate) kind: FrameType,
    pub(crate) stream_id: u32,
    pub(crate) payload: &'a [u8],
}

/// Frame decoding failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The batch ends inside a frame.
    Incomplete,
    /// A declared payload length exceeds the configured limit.
    Payload,
    /// The batch carries more frames than the protocol allows.
    TooManyFrames,
    /// The batch carries no frame at all.
    Empty,
    /// A frame is well-formed but not legal in this direction or state.
    Shape,
}

/// Appends one encoded frame to `out`.
///
/// The caller guarantees `stream_id` fits in 24 bits and `payload` fits in the
/// protocol maximum; both hold for every relay-produced frame.
pub(crate) fn encode_into(out: &mut Vec<u8>, kind: FrameType, stream_id: u32, payload: &[u8]) {
    debug_assert!(stream_id <= MAX_STREAM_ID);
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    out.reserve(HEADER_SIZE + payload.len());
    out.push(kind.0);
    out.push((stream_id >> 16) as u8);
    out.push((stream_id >> 8) as u8);
    out.push(stream_id as u8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

/// Encodes one frame into a fresh buffer.
pub(crate) fn encode(kind: FrameType, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    encode_into(&mut out, kind, stream_id, payload);
    out
}

/// Rewrites the payload length of an already encoded frame in place.
///
/// Used when adjacent DATA frames for one stream are coalesced while queued.
pub(crate) fn patch_length(encoded: &mut [u8]) {
    debug_assert!(encoded.len() >= HEADER_SIZE);
    let length = (encoded.len() - HEADER_SIZE) as u32;
    encoded[4..8].copy_from_slice(&length.to_be_bytes());
}

/// Parses a complete carrier body into borrowed frames.
///
/// `max_payload` clamps the per-frame payload to the configured limit; values
/// outside `1..=MAX_PAYLOAD` fall back to the protocol maximum.
pub(crate) fn parse_all(input: &[u8], max_payload: usize) -> Result<Vec<Frame<'_>>, FrameError> {
    let max_payload = if max_payload == 0 || max_payload > MAX_PAYLOAD {
        MAX_PAYLOAD
    } else {
        max_payload
    };
    let mut frames = Vec::with_capacity(4);
    let mut rest = input;
    while !rest.is_empty() {
        if frames.len() == MAX_BATCH_FRAMES {
            return Err(FrameError::TooManyFrames);
        }
        if rest.len() < HEADER_SIZE {
            return Err(FrameError::Incomplete);
        }
        let length = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        if length > max_payload {
            return Err(FrameError::Payload);
        }
        let full = HEADER_SIZE + length;
        if full > rest.len() {
            return Err(FrameError::Incomplete);
        }
        let stream_id = (u32::from(rest[1]) << 16) | (u32::from(rest[2]) << 8) | u32::from(rest[3]);
        frames.push(Frame {
            kind: FrameType(rest[0]),
            stream_id,
            payload: &rest[HEADER_SIZE..full],
        });
        rest = &rest[full..];
    }
    if frames.is_empty() {
        return Err(FrameError::Empty);
    }
    Ok(frames)
}

/// Protocol version this relay implements.
pub(crate) const PROTOCOL_VERSION: u8 = 1;

/// Reads the protocol version out of a session-creation body.
///
/// The body must be exactly one HELLO on stream zero whose first payload byte
/// is the client's protocol version. Bytes past the version are accepted and
/// ignored, and so is a version this relay does not implement: the relay then
/// answers with its own v1 WELCOME, which is the only downgrade signal a later
/// client can get. Rejecting either would produce the same deliberately
/// indistinguishable 404 an unrelated host returns, leaving a future client no
/// way to tell "relay speaking an older version" from "not a relay".
pub(crate) fn parse_hello(input: &[u8]) -> Result<u8, FrameError> {
    let frames = parse_all(input, MAX_PAYLOAD)?;
    if frames.len() != 1 {
        return Err(FrameError::Shape);
    }
    let value = frames[0];
    if value.kind != FrameType::HELLO || value.stream_id != 0 || value.payload.is_empty() {
        return Err(FrameError::Shape);
    }
    let version = value.payload[0];
    if version == 0 {
        return Err(FrameError::Shape);
    }
    Ok(version)
}

/// Decodes a WINDOW payload into its nonzero credit delta.
pub(crate) fn window_amount(payload: &[u8]) -> Result<u32, FrameError> {
    if payload.len() != 4 {
        return Err(FrameError::Shape);
    }
    let value = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if value == 0 {
        return Err(FrameError::Shape);
    }
    Ok(value)
}

/// Encodes a WINDOW credit delta payload.
pub(crate) fn window_payload(amount: u32) -> [u8; 4] {
    amount.to_be_bytes()
}

/// Rejects frames that are structurally illegal from client to relay.
///
/// Stream zero carries only PONG with a bounded echo token; nonzero streams
/// carry OPEN/CLOSE with empty payloads, nonempty DATA, or a valid WINDOW.
pub(crate) fn validate_client_shape(value: &Frame<'_>) -> Result<(), FrameError> {
    if value.stream_id == 0 {
        if value.kind != FrameType::PONG || value.payload.len() > 64 {
            return Err(FrameError::Shape);
        }
        return Ok(());
    }
    match value.kind {
        FrameType::OPEN | FrameType::CLOSE => {
            if !value.payload.is_empty() {
                return Err(FrameError::Shape);
            }
        }
        FrameType::DATA => {
            if value.payload.is_empty() {
                return Err(FrameError::Shape);
            }
        }
        FrameType::WINDOW => {
            window_amount(value.payload)?;
        }
        _ => return Err(FrameError::Shape),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_round_trip() {
        let mut body = encode(FrameType::OPEN, 7, &[]);
        body.extend_from_slice(&encode(FrameType::DATA, 7, b"abc"));
        let frames = parse_all(&body, MAX_PAYLOAD).expect("parse");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].kind, FrameType::OPEN);
        assert_eq!(frames[0].stream_id, 7);
        assert!(frames[0].payload.is_empty());
        assert_eq!(frames[1].payload, b"abc");
    }

    #[test]
    fn rejects_incomplete_and_empty_batches() {
        assert_eq!(
            parse_all(&[0x01, 0x00], MAX_PAYLOAD).err(),
            Some(FrameError::Incomplete)
        );
        assert_eq!(parse_all(&[], MAX_PAYLOAD).err(), Some(FrameError::Empty));
    }

    #[test]
    fn rejects_oversized_payload_declaration() {
        let mut header = vec![0x02, 0, 0, 1];
        header.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        assert_eq!(
            parse_all(&header, MAX_PAYLOAD).err(),
            Some(FrameError::Payload)
        );
    }

    #[test]
    fn hello_reports_the_client_protocol_version() {
        assert_eq!(parse_hello(&encode(FrameType::HELLO, 0, &[1])), Ok(1));
        // A newer client is admitted and answered with the v1 WELCOME, which is
        // how it learns to downgrade instead of seeing an opaque 404.
        assert_eq!(parse_hello(&encode(FrameType::HELLO, 0, &[2])), Ok(2));
        assert_eq!(parse_hello(&encode(FrameType::HELLO, 0, &[2, 9])), Ok(2));
        assert!(parse_hello(&encode(FrameType::HELLO, 0, &[0])).is_err());
        assert!(parse_hello(&encode(FrameType::HELLO, 0, &[])).is_err());
        assert!(parse_hello(&encode(FrameType::HELLO, 1, &[1])).is_err());
    }

    #[test]
    fn client_shape_rules_match_protocol() {
        let open = Frame {
            kind: FrameType::OPEN,
            stream_id: 1,
            payload: &[],
        };
        assert!(validate_client_shape(&open).is_ok());
        let bad_open = Frame {
            kind: FrameType::OPEN,
            stream_id: 1,
            payload: b"x",
        };
        assert!(validate_client_shape(&bad_open).is_err());
        let empty_data = Frame {
            kind: FrameType::DATA,
            stream_id: 1,
            payload: &[],
        };
        assert!(validate_client_shape(&empty_data).is_err());
        let zero_window = Frame {
            kind: FrameType::WINDOW,
            stream_id: 1,
            payload: &[0, 0, 0, 0],
        };
        assert!(validate_client_shape(&zero_window).is_err());
        let pong = Frame {
            kind: FrameType::PONG,
            stream_id: 0,
            payload: b"token",
        };
        assert!(validate_client_shape(&pong).is_ok());
        let ping = Frame {
            kind: FrameType::PING,
            stream_id: 0,
            payload: b"token",
        };
        assert!(validate_client_shape(&ping).is_err());
    }

    #[test]
    fn patch_length_updates_header() {
        let mut encoded = encode(FrameType::DATA, 3, b"ab");
        encoded.extend_from_slice(b"cd");
        patch_length(&mut encoded);
        let frames = parse_all(&encoded, MAX_PAYLOAD).expect("parse");
        assert_eq!(frames[0].payload, b"abcd");
    }
}
