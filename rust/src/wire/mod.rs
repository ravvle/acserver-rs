//! The AC wire format: packed little-endian primitives and the string
//! encodings (docs/01 §1.2). Every packet is built and parsed through here,
//! so no other module does byte-order math or string encoding by hand.

mod framing;
mod narrow;
mod reader;
mod writer;

pub use framing::{MAX_TCP_RECV, frame_tcp};
pub use narrow::Narrow;
pub use reader::Reader;
pub use writer::Writer;

/// Three `f32` components, 12 bytes on the wire.
pub use glam::Vec3;

#[cfg(test)]
mod tests;
