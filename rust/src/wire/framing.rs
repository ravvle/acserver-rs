//! TCP framing: every TCP packet is a u16 little-endian length, then the
//! payload (docs/01 §1.3). Reading is `LengthDelimitedCodec` in the server's
//! connection code; this module frames outgoing packets.

use bytes::{BufMut, Bytes, BytesMut};

/// The largest client packet accepted; a longer declared length closes the
/// connection (docs/01 §1.3).
pub const MAX_TCP_RECV: usize = 2046;

/// Prepends the u16 little-endian length prefix to a payload.
pub fn frame_tcp(payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(2 + payload.len());
    out.put_u16_le(payload.len() as u16);
    out.put_slice(payload);
    out.freeze()
}
