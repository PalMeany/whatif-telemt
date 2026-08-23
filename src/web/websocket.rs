//! Minimal RFC 6455 server framing for the WebSocket carriers.
//!
//! Only what the carrier contract needs is implemented: binary messages with
//! fragmentation, protocol ping/pong for liveness, and close. Text messages,
//! oversized messages, and unmasked client frames are rejected, matching the
//! reference relay.

use std::io;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::crypto::hash::sha1;

/// RFC 6455 handshake GUID.
const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// RFC 6455 close codes this carrier emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum CloseCode {
    /// The carrier finished normally.
    Normal = 1000,
    /// The relay is shutting down.
    GoingAway = 1001,
    /// The peer broke the carrier grammar: a text message, a cross-lane frame,
    /// a malformed batch, or a first message that was not `OPEN`.
    Protocol = 1002,
    /// A message exceeded the configured carrier body limit.
    TooLarge = 1009,
}

/// Largest control-frame payload allowed by the protocol.
const MAX_CONTROL_PAYLOAD: usize = 125;

/// Computes the `Sec-WebSocket-Accept` value for a client key.
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut input = String::with_capacity(client_key.len() + ACCEPT_GUID.len());
    input.push_str(client_key);
    input.push_str(ACCEPT_GUID);
    STANDARD.encode(sha1(input.as_bytes()))
}

/// One decoded client message.
pub(crate) enum WsMessage {
    /// A complete binary message, possibly reassembled from fragments.
    Binary(Vec<u8>),
    /// A ping that must be answered with the same payload.
    Ping(Vec<u8>),
    /// A pong, used only to refresh the liveness deadline.
    Pong,
    /// The peer closed the connection.
    Close,
}

/// Reads client frames from the upgraded connection.
///
/// Fragment assembly lives in the reader rather than in `read_message`, because
/// RFC 6455 §5.4 lets a control frame be injected between the fragments of a
/// message. Returning that control frame has to leave the partial message
/// intact, otherwise the relay answers its own liveness ping and then rejects
/// the client's continuation as unexpected.
pub(crate) struct WsReader<R> {
    inner: R,
    limit: usize,
    /// Fragments of the message currently being reassembled.
    assembled: Vec<u8>,
    /// Set between a non-final data frame and its final continuation.
    assembling: bool,
}

impl<R: AsyncRead + Unpin> WsReader<R> {
    pub(crate) fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            assembled: Vec::new(),
            assembling: false,
        }
    }

    /// Reads one complete message, transparently joining fragments.
    pub(crate) async fn read_message(&mut self) -> io::Result<WsMessage> {
        loop {
            let frame = self.read_frame().await?;
            if frame.opcode >= 0x8 {
                if !frame.fin || frame.payload.len() > MAX_CONTROL_PAYLOAD {
                    return Err(protocol_error("invalid control frame"));
                }
                match frame.opcode {
                    OPCODE_CLOSE => return Ok(WsMessage::Close),
                    OPCODE_PING => return Ok(WsMessage::Ping(frame.payload)),
                    OPCODE_PONG => return Ok(WsMessage::Pong),
                    _ => return Err(protocol_error("unknown control opcode")),
                }
            }
            match frame.opcode {
                OPCODE_TEXT => return Err(protocol_error("text messages are rejected")),
                OPCODE_BINARY => {
                    if self.assembling {
                        return Err(protocol_error("interleaved data frame"));
                    }
                    if frame.fin {
                        return Ok(WsMessage::Binary(frame.payload));
                    }
                    self.assembling = true;
                    self.assembled = frame.payload;
                }
                OPCODE_CONTINUATION => {
                    if !self.assembling {
                        return Err(protocol_error("unexpected continuation frame"));
                    }
                    if self.assembled.len() + frame.payload.len() > self.limit {
                        return Err(protocol_error("message exceeds read limit"));
                    }
                    self.assembled.extend_from_slice(&frame.payload);
                    if frame.fin {
                        self.assembling = false;
                        return Ok(WsMessage::Binary(std::mem::take(&mut self.assembled)));
                    }
                }
                _ => return Err(protocol_error("unknown data opcode")),
            }
        }
    }

    async fn read_frame(&mut self) -> io::Result<RawFrame> {
        let mut header = [0u8; 2];
        self.inner.read_exact(&mut header).await?;
        let fin = header[0] & 0x80 != 0;
        if header[0] & 0x70 != 0 {
            return Err(protocol_error("reserved bits set"));
        }
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        if !masked {
            return Err(protocol_error("client frame must be masked"));
        }
        let length = match header[1] & 0x7F {
            126 => {
                let mut extended = [0u8; 2];
                self.inner.read_exact(&mut extended).await?;
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                self.inner.read_exact(&mut extended).await?;
                let value = u64::from_be_bytes(extended);
                if value > self.limit as u64 {
                    return Err(protocol_error("frame exceeds read limit"));
                }
                value as usize
            }
            small => small as usize,
        };
        if length > self.limit {
            return Err(protocol_error("frame exceeds read limit"));
        }
        let mut mask = [0u8; 4];
        self.inner.read_exact(&mut mask).await?;
        let mut payload = vec![0u8; length];
        self.inner.read_exact(&mut payload).await?;
        unmask(&mut payload, &mask);
        Ok(RawFrame {
            fin,
            opcode,
            payload,
        })
    }
}

/// Writes server frames to the upgraded connection.
pub(crate) struct WsWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> WsWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Sends one complete binary message.
    pub(crate) async fn write_binary(&mut self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(OPCODE_BINARY, payload).await
    }

    /// Sends a ping used as the carrier liveness probe.
    pub(crate) async fn write_ping(&mut self) -> io::Result<()> {
        self.write_frame(OPCODE_PING, &[]).await
    }

    /// Answers a client ping with the same payload.
    pub(crate) async fn write_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.write_frame(OPCODE_PONG, payload).await
    }

    /// Sends a close frame carrying `code`.
    ///
    /// The code is not decoration: a client that respects it reads `1000` as
    /// "this ended normally, do not reconnect", so answering a protocol error
    /// or a size violation with `1000` tells the peer to give up on a carrier
    /// it could simply have retried.
    pub(crate) async fn write_close(&mut self, code: CloseCode) -> io::Result<()> {
        self.write_frame(OPCODE_CLOSE, &(code as u16).to_be_bytes())
            .await
    }

    async fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut header = Vec::with_capacity(10);
        header.push(0x80 | opcode);
        if payload.len() < 126 {
            header.push(payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            header.push(126);
            header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            header.push(127);
            header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        self.inner.write_all(&header).await?;
        if !payload.is_empty() {
            self.inner.write_all(payload).await?;
        }
        self.inner.flush().await
    }
}

struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn unmask(payload: &mut [u8], mask: &[u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index & 3];
    }
}

fn protocol_error(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    fn masked_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
        out.push(0x80 | payload.len() as u8);
        out.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            out.push(byte ^ mask[index & 3]);
        }
        out
    }

    #[tokio::test]
    async fn reads_binary_and_fragmented_messages() {
        let mut input = masked_frame(OPCODE_BINARY, true, b"hello");
        input.extend_from_slice(&masked_frame(OPCODE_BINARY, false, b"ab"));
        input.extend_from_slice(&masked_frame(OPCODE_CONTINUATION, true, b"cd"));
        let mut reader = WsReader::new(input.as_slice(), 1024);
        match reader.read_message().await.expect("first") {
            WsMessage::Binary(payload) => assert_eq!(payload, b"hello"),
            _ => panic!("expected binary"),
        }
        match reader.read_message().await.expect("second") {
            WsMessage::Binary(payload) => assert_eq!(payload, b"abcd"),
            _ => panic!("expected binary"),
        }
    }

    #[tokio::test]
    async fn a_control_frame_between_fragments_keeps_the_partial_message() {
        let mut input = masked_frame(OPCODE_BINARY, false, b"ab");
        input.extend_from_slice(&masked_frame(OPCODE_PING, true, b"probe"));
        input.extend_from_slice(&masked_frame(OPCODE_CONTINUATION, true, b"cd"));
        let mut reader = WsReader::new(input.as_slice(), 1024);
        match reader.read_message().await.expect("ping") {
            WsMessage::Ping(payload) => assert_eq!(payload, b"probe"),
            _ => panic!("expected the interleaved ping"),
        }
        match reader.read_message().await.expect("binary") {
            WsMessage::Binary(payload) => assert_eq!(payload, b"abcd"),
            _ => panic!("expected the reassembled message"),
        }
    }

    #[tokio::test]
    async fn rejects_text_and_unmasked_frames() {
        let text = masked_frame(OPCODE_TEXT, true, b"x");
        let mut reader = WsReader::new(text.as_slice(), 1024);
        assert!(reader.read_message().await.is_err());

        let unmasked = vec![0x82, 0x01, b'x'];
        let mut reader = WsReader::new(unmasked.as_slice(), 1024);
        assert!(reader.read_message().await.is_err());
    }

    #[tokio::test]
    async fn writes_frames_with_correct_length_encoding() {
        let mut output = Vec::new();
        let mut writer = WsWriter::new(&mut output);
        writer.write_binary(b"hi").await.expect("small");
        writer.write_binary(&vec![0u8; 300]).await.expect("medium");
        assert_eq!(output[0], 0x82);
        assert_eq!(output[1], 2);
        assert_eq!(output[4], 0x82);
        assert_eq!(output[5], 126);
        assert_eq!(u16::from_be_bytes([output[6], output[7]]), 300);
    }
}
