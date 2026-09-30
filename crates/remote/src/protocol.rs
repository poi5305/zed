use anyhow::{Result, anyhow};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use rpc::proto::Envelope;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct MessageId(pub u32);

pub type MessageLen = u32;
pub const MESSAGE_LEN_SIZE: usize = size_of::<MessageLen>();

pub fn message_len_from_buffer(buffer: &[u8]) -> MessageLen {
    MessageLen::from_le_bytes(buffer.try_into().unwrap())
}

pub async fn read_message_with_len<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message_len: MessageLen,
) -> Result<Envelope> {
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;
    Ok(Envelope::decode_from_slice(buffer.as_slice())?)
}

pub async fn read_message<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<Envelope> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let len = message_len_from_buffer(buffer);

    read_message_with_len(stream, buffer, len).await
}

pub async fn write_message<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    message: Envelope,
) -> Result<()> {
    let message_len = message.encoded_size() as u32;
    stream
        .write_all(message_len.to_le_bytes().as_slice())
        .await?;
    buffer.clear();
    buffer.reserve(message_len as usize);
    message.encode_to_buffer(buffer)?;
    stream.write_all(buffer).await?;
    Ok(())
}

pub async fn write_size_prefixed_buffer<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let len = buffer.len() as u32;
    stream.write_all(len.to_le_bytes().as_slice()).await?;
    stream.write_all(buffer).await?;
    Ok(())
}

pub async fn read_message_raw<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    buffer.resize(MESSAGE_LEN_SIZE, 0);
    stream.read_exact(buffer).await?;

    let message_len = message_len_from_buffer(buffer);
    buffer.resize(message_len as usize, 0);
    stream.read_exact(buffer).await?;

    Ok(())
}

/// Sized for the largest envelope any legitimate peer is known to send with a wide margin,
/// while still refusing a corrupt length prefix before it makes the framer buffer gigabytes.
pub const DEFAULT_MAX_FRAME_LEN: usize = 1 << 30;

/// Appends one length-prefixed frame to `out`, leaving `out` untouched on error.
pub fn encode_frame(envelope: &Envelope, out: &mut Vec<u8>) -> Result<()> {
    let encoded_len = envelope.encoded_size();
    let frame_len = MessageLen::try_from(encoded_len)
        .map_err(|_| anyhow!("envelope of {encoded_len} bytes does not fit a u32 length prefix"))?;
    let original_len = out.len();
    out.reserve(MESSAGE_LEN_SIZE + encoded_len);
    out.extend_from_slice(&frame_len.to_le_bytes());
    if let Err(error) = envelope.encode_to_buffer(out) {
        out.truncate(original_len);
        return Err(error.into());
    }
    Ok(())
}

/// Incremental decoder for the same byte stream `read_message` reads, for transports that
/// deliver arbitrary chunks (a browser WebSocket) instead of an `AsyncRead`.
pub struct EnvelopeFramer {
    buffer: Vec<u8>,
    max_frame_len: usize,
    poisoned: bool,
}

impl EnvelopeFramer {
    pub fn new(max_frame_len: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_len,
            poisoned: false,
        }
    }

    /// Appends bytes, returns every complete envelope in order. A frame whose declared length
    /// exceeds max_frame_len, or whose payload does not decode, is an error, and the framer is
    /// unusable afterwards (poisoned): the stream position can no longer be trusted, and the
    /// envelopes completed earlier in the same call are discarded with the error.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Envelope>> {
        if self.poisoned {
            return Err(anyhow!("envelope framer is poisoned by an earlier error"));
        }
        self.buffer.extend_from_slice(bytes);

        let mut envelopes = Vec::new();
        let mut consumed = 0;
        let outcome = loop {
            let Some(remaining) = self.buffer.get(consumed..) else {
                break Ok(());
            };
            let Some(prefix) = remaining.first_chunk::<MESSAGE_LEN_SIZE>() else {
                break Ok(());
            };
            let declared_len = MessageLen::from_le_bytes(*prefix) as usize;
            if declared_len > self.max_frame_len {
                break Err(anyhow!(
                    "frame of {declared_len} bytes exceeds the {} byte limit",
                    self.max_frame_len
                ));
            }
            let Some(frame_end) = declared_len.checked_add(MESSAGE_LEN_SIZE) else {
                break Err(anyhow!("frame length {declared_len} overflows usize"));
            };
            let Some(payload) = remaining.get(MESSAGE_LEN_SIZE..frame_end) else {
                break Ok(());
            };
            match Envelope::decode_from_slice(payload) {
                Ok(envelope) => envelopes.push(envelope),
                Err(error) => break Err(anyhow!("failed to decode envelope: {error}")),
            }
            // frame_end >= MESSAGE_LEN_SIZE, so every iteration makes progress.
            consumed += frame_end;
        };

        match outcome {
            Ok(()) => {
                self.buffer.drain(..consumed);
                Ok(envelopes)
            }
            Err(error) => {
                self.poisoned = true;
                self.buffer = Vec::new();
                Err(error)
            }
        }
    }

    pub fn pending_len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::proto;

    fn test_envelope(id: u32, message: &str) -> Envelope {
        Envelope {
            id,
            payload: Some(proto::envelope::Payload::Error(proto::Error {
                message: message.to_string(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn framed(envelopes: &[Envelope]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for envelope in envelopes {
            encode_frame(envelope, &mut bytes).expect("encode");
        }
        bytes
    }

    #[test]
    fn encode_frame_matches_write_message() {
        let envelope = test_envelope(7, "hello");
        let mut expected = Vec::new();
        smol::block_on(write_message(
            &mut futures::io::Cursor::new(&mut expected),
            &mut Vec::new(),
            envelope.clone(),
        ))
        .expect("write_message");
        assert_eq!(framed(&[envelope]), expected);
    }

    #[test]
    fn encode_frame_appends_to_existing_bytes() {
        let mut out = vec![9, 9];
        encode_frame(&test_envelope(1, "a"), &mut out).expect("encode");
        assert_eq!(&out[..2], &[9, 9]);
        assert_eq!(out.len(), 2 + framed(&[test_envelope(1, "a")]).len());
    }

    #[test]
    fn single_frame_in_one_push() {
        let envelope = test_envelope(1, "one frame");
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let decoded = framer.push(&framed(&[envelope.clone()])).expect("push");
        assert_eq!(decoded, vec![envelope]);
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn single_frame_one_byte_at_a_time() {
        let envelope = test_envelope(2, "byte by byte");
        let bytes = framed(&[envelope.clone()]);
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let mut decoded = Vec::new();
        for (index, byte) in bytes.iter().enumerate() {
            let produced = framer.push(std::slice::from_ref(byte)).expect("push");
            if index + 1 < bytes.len() {
                assert!(produced.is_empty(), "frame completed early at byte {index}");
            }
            decoded.extend(produced);
        }
        assert_eq!(decoded, vec![envelope]);
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn three_frames_in_one_push_keep_order() {
        let envelopes = vec![
            test_envelope(1, "first"),
            test_envelope(2, "second"),
            test_envelope(3, "third"),
        ];
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert_eq!(framer.push(&framed(&envelopes)).expect("push"), envelopes);
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn length_prefix_split_across_pushes() {
        let envelopes = vec![test_envelope(1, "split prefix"), test_envelope(2, "next")];
        let bytes = framed(&envelopes);
        for split in 1..MESSAGE_LEN_SIZE {
            let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
            assert!(framer.push(&bytes[..split]).expect("first").is_empty());
            assert_eq!(framer.pending_len(), split);
            assert_eq!(framer.push(&bytes[split..]).expect("second"), envelopes);
            assert_eq!(framer.pending_len(), 0);
        }
    }

    #[test]
    fn frame_and_a_partial_next_frame_keeps_the_remainder() {
        let first = test_envelope(1, "complete");
        let second = test_envelope(2, "partial");
        let first_bytes = framed(&[first.clone()]);
        let second_bytes = framed(&[second.clone()]);
        let mut chunk = first_bytes.clone();
        chunk.extend_from_slice(&second_bytes[..6]);
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert_eq!(framer.push(&chunk).expect("push"), vec![first]);
        assert_eq!(framer.pending_len(), 6);
        assert_eq!(framer.push(&second_bytes[6..]).expect("push"), vec![second]);
    }

    #[test]
    fn zero_length_envelope_round_trips() {
        let bytes = framed(&[Envelope::default()]);
        assert_eq!(bytes, vec![0, 0, 0, 0]);
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert_eq!(
            framer.push(&bytes).expect("push"),
            vec![Envelope::default()]
        );
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn many_zero_length_envelopes_in_one_push_terminate() {
        let bytes = vec![0u8; MESSAGE_LEN_SIZE * 1000];
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert_eq!(framer.push(&bytes).expect("push").len(), 1000);
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn sixty_four_mebibyte_envelope_is_not_rejected() {
        let envelope = test_envelope(9, &"x".repeat(64 << 20));
        let bytes = framed(&[envelope.clone()]);
        assert!(bytes.len() > 64 << 20);

        let mut whole = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let decoded = whole.push(&bytes).expect("64 MiB envelope in one push");
        assert!(decoded == vec![envelope.clone()]);
        assert_eq!(whole.pending_len(), 0);

        let mut chunked = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        let mut decoded = Vec::new();
        for chunk in bytes.chunks(1 << 20) {
            decoded.extend(
                chunked
                    .push(chunk)
                    .expect("64 MiB envelope in 1 MiB chunks"),
            );
        }
        assert!(decoded == vec![envelope]);
        assert_eq!(chunked.pending_len(), 0);
    }

    #[test]
    fn frame_exactly_at_the_limit_is_accepted() {
        let envelope = test_envelope(4, "boundary");
        let payload_len = envelope.encoded_size();
        let mut framer = EnvelopeFramer::new(payload_len);
        assert_eq!(
            framer.push(&framed(&[envelope.clone()])).expect("push"),
            vec![envelope]
        );
    }

    #[test]
    fn oversized_frame_is_an_error_and_poisons_the_framer() {
        let mut framer = EnvelopeFramer::new(16);
        let oversized_prefix = 17u32.to_le_bytes();
        assert!(framer.push(&oversized_prefix).is_err());

        let valid = framed(&[test_envelope(1, "ok")]);
        assert!(framer.push(&valid).is_err());
        assert!(framer.push(&[]).is_err());
        assert_eq!(framer.pending_len(), 0);
    }

    #[test]
    fn oversized_frame_after_valid_frames_in_the_same_push_is_an_error() {
        let mut bytes = framed(&[test_envelope(1, "ok")]);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert!(framer.push(&bytes).is_err());
        assert!(framer.push(&framed(&[test_envelope(2, "ok")])).is_err());
    }

    #[test]
    fn undecodable_payload_is_an_error_and_poisons_the_framer() {
        // Field 1 (id) with wire type 0 promises a varint that never terminates.
        let garbage = [0x08u8, 0xff, 0xff, 0xff];
        let mut bytes = (garbage.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&garbage);
        let mut framer = EnvelopeFramer::new(DEFAULT_MAX_FRAME_LEN);
        assert!(framer.push(&bytes).is_err());
        assert!(framer.push(&framed(&[test_envelope(1, "ok")])).is_err());
    }
}
