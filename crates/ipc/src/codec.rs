//! Tokio codec wrapping [`crate::framing`].
//!
//! Use with [`tokio_util::codec::Framed`] to turn an `AsyncRead + AsyncWrite`
//! into a `Stream<Item = Vec<u8>>` + `Sink<&[u8]>`.

use tokio_util::bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::framing::{FrameError, MAX_FRAME_LEN};

const LEN_PREFIX: usize = 4;

#[derive(Debug, Default, Clone)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Vec<u8>;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < LEN_PREFIX {
            return Ok(None);
        }
        let len_bytes: [u8; 4] = src[..LEN_PREFIX].try_into().expect("slice is 4 bytes");
        let len = u32::from_be_bytes(len_bytes) as usize;
        if len > MAX_FRAME_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FrameError::Oversized { got: len }.to_string(),
            ));
        }
        if src.len() < LEN_PREFIX + len {
            src.reserve(LEN_PREFIX + len - src.len());
            return Ok(None);
        }
        src.advance(LEN_PREFIX);
        let payload = src.split_to(len).to_vec();
        Ok(Some(payload))
    }
}

impl Encoder<&[u8]> for FrameCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: &[u8], dst: &mut BytesMut) -> Result<(), Self::Error> {
        if item.len() > MAX_FRAME_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FrameError::Oversized { got: item.len() }.to_string(),
            ));
        }
        dst.reserve(LEN_PREFIX + item.len());
        dst.put_u32(item.len() as u32);
        dst.extend_from_slice(item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use tokio_stream::StreamExt;
    use tokio_util::codec::Framed;

    #[tokio::test]
    async fn round_trip_one_frame_through_duplex_pipe() {
        let (a, b) = tokio::io::duplex(1024);
        let mut writer = Framed::new(a, FrameCodec);
        let mut reader = Framed::new(b, FrameCodec);

        writer.send(b"hello".as_slice()).await.unwrap();
        let got = reader.next().await.unwrap().unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn multiple_frames_round_trip_in_order() {
        let (a, b) = tokio::io::duplex(1024);
        let mut writer = Framed::new(a, FrameCodec);
        let mut reader = Framed::new(b, FrameCodec);

        writer.send(b"one".as_slice()).await.unwrap();
        writer.send(b"two".as_slice()).await.unwrap();
        writer.send(b"three".as_slice()).await.unwrap();

        assert_eq!(reader.next().await.unwrap().unwrap(), b"one");
        assert_eq!(reader.next().await.unwrap().unwrap(), b"two");
        assert_eq!(reader.next().await.unwrap().unwrap(), b"three");
    }

    #[tokio::test]
    async fn frame_split_across_writes_is_assembled() {
        use tokio::io::AsyncWriteExt;

        let (mut a, b) = tokio::io::duplex(1024);
        let mut reader = Framed::new(b, FrameCodec);

        // Write a frame in two halves — the codec must wait for the rest.
        let framed = crate::framing::encode_frame(b"split-payload").unwrap();
        let (h1, h2) = framed.split_at(framed.len() / 2);
        a.write_all(h1).await.unwrap();
        a.write_all(h2).await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        let got = reader.next().await.unwrap().unwrap();
        assert_eq!(got, b"split-payload");
    }
}
