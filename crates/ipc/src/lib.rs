//! IPC framing for the devstack daemon protocol.
//!
//! Wire format: a 4-byte big-endian `u32` length followed by `length` bytes
//! of JSON payload. Pure-byte framing (no newlines, no escaping concerns)
//! plus a length cap to bound memory.
//!
//! Use [`encode_frame`] / [`decode_frame`] for sync framing. The async
//! [`FrameCodec`] adapts these to a Tokio [`tokio_util::codec`].

mod codec;
mod framing;

pub use codec::FrameCodec;
pub use framing::{FrameError, MAX_FRAME_LEN, decode_frame, encode_frame};
