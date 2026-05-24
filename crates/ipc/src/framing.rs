//! Sync byte-level framing: encode and decode length-prefixed payloads.

use thiserror::Error;

/// Cap on a single frame's payload size. 16 MiB — bigger than any realistic
/// IPC message; oversize frames are treated as a protocol error.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

const LEN_PREFIX: usize = 4;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame length {got} exceeds MAX_FRAME_LEN ({max})", max = MAX_FRAME_LEN)]
    Oversized { got: usize },
}

/// Prepend a 4-byte big-endian length to `payload`. Returns the framed bytes.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::Oversized { got: payload.len() });
    }
    let mut out = Vec::with_capacity(LEN_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Try to consume one frame from the front of `buf`. On success removes the
/// length prefix + payload from `buf` and returns the payload bytes. On
/// `None`, the buffer doesn't yet hold a complete frame.
pub fn decode_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, FrameError> {
    if buf.len() < LEN_PREFIX {
        return Ok(None);
    }
    let len_bytes: [u8; 4] = buf[..LEN_PREFIX].try_into().expect("slice is 4 bytes");
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::Oversized { got: len });
    }
    if buf.len() < LEN_PREFIX + len {
        return Ok(None);
    }
    let payload = buf[LEN_PREFIX..LEN_PREFIX + len].to_vec();
    buf.drain(..LEN_PREFIX + len);
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_frame_prefixes_length() {
        let framed = encode_frame(b"hello").unwrap();
        assert_eq!(framed[..4], [0, 0, 0, 5]);
        assert_eq!(&framed[4..], b"hello");
    }

    #[test]
    fn decode_frame_extracts_one_payload() {
        let mut buf = encode_frame(b"hello").unwrap();
        let payload = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(payload, b"hello");
        assert!(buf.is_empty(), "buf should be drained: {buf:?}");
    }

    #[test]
    fn decode_returns_none_when_buffer_lacks_length_prefix() {
        let mut buf = vec![0, 0];
        assert_eq!(decode_frame(&mut buf).unwrap(), None);
        // Buffer untouched so subsequent reads can complete the frame.
        assert_eq!(buf, vec![0, 0]);
    }

    #[test]
    fn decode_returns_none_when_buffer_lacks_full_payload() {
        let mut buf = vec![0, 0, 0, 5, b'h', b'i'];
        assert_eq!(decode_frame(&mut buf).unwrap(), None);
        assert_eq!(buf, vec![0, 0, 0, 5, b'h', b'i']);
    }

    #[test]
    fn decode_consumes_only_first_frame_leaves_remainder() {
        let mut buf = encode_frame(b"first").unwrap();
        buf.extend(encode_frame(b"second").unwrap());
        let p1 = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(p1, b"first");
        let p2 = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(p2, b"second");
        assert!(buf.is_empty());
    }

    #[test]
    fn oversized_length_prefix_errors() {
        // Length prefix advertises 17 MiB — over our 16 MiB cap.
        let mut buf = (17 * 1024 * 1024_u32).to_be_bytes().to_vec();
        let err = decode_frame(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Oversized { .. }));
    }
}
