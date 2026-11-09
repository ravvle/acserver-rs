use super::{Narrow, Vec3};

/// Builds a payload, framed with `frame_tcp` for TCP or sent as-is for UDP.
///
/// Packet builders call one method per field, in wire order, so a builder
/// reads line by line against the doc table.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Writer {
        Writer::with_capacity(64)
    }

    /// An empty writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Writer {
        Writer {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// A writer whose payload starts with the packet id byte.
    pub fn with_id(id: u8) -> Writer {
        let mut w = Writer::new();
        w.u8(id);
        w
    }

    /// The payload so far (not framed).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the writer, returning its payload.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// The current payload length.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// A temperature in °C as one byte: truncated to whole degrees, with
    /// negatives in two's complement (-5.0 is 251), as the reference sends them.
    pub fn temperature(&mut self, celsius: f32) {
        self.u8(celsius as i32 as u8);
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub fn i8(&mut self, v: i8) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    /// Writes an `f32` as IEEE-754 binary16, rounded to nearest as
    /// AssettoServer's `(Half)` cast does.
    pub fn f16(&mut self, v: f32) {
        self.u16(half::f16::from_f32(v).to_bits());
    }

    pub fn vec3(&mut self, v: Vec3) {
        self.f32(v.x);
        self.f32(v.y);
        self.f32(v.z);
    }

    /// Appends bytes verbatim.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a u8 byte-length prefix, then the UTF-8 bytes, cut to 255 bytes.
    /// The cut can split a character.
    pub fn string8(&mut self, s: &str) {
        let bytes = &s.as_bytes()[..s.len().min(255)];
        self.u8(bytes.len() as u8);
        self.raw(bytes);
    }

    /// Writes a u8 byte-length prefix, then the bytes verbatim, cut to 255.
    pub fn narrow8(&mut self, s: &Narrow) {
        let bytes = &s.as_bytes()[..s.as_bytes().len().min(255)];
        self.u8(bytes.len() as u8);
        self.raw(bytes);
    }

    /// Writes a u16 byte-length prefix, then the UTF-8 bytes.
    pub fn string16(&mut self, s: &str) {
        let bytes = &s.as_bytes()[..s.len().min(0xFFFF)];
        self.u16(bytes.len() as u16);
        self.raw(bytes);
    }

    /// Writes a u8 character-length prefix, then UTF-32LE, cut to 255 characters.
    pub fn wstring8(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().take(255).collect();
        self.u8(chars.len() as u8);
        self.utf32(&chars);
    }

    /// Writes a u16 character-length prefix, then UTF-32LE.
    pub fn wstring16(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().take(0xFFFF).collect();
        self.u16(chars.len() as u16);
        self.utf32(&chars);
    }

    fn utf32(&mut self, chars: &[char]) {
        for &c in chars {
            self.u32(u32::from(c));
        }
    }

    /// Writes a fixed `n`-byte buffer: the UTF-8 bytes, cut or zero-padded to `n`.
    pub fn cstr(&mut self, s: &str, n: usize) {
        let bytes = s.as_bytes();
        let used = bytes.len().min(n);
        self.raw(&bytes[..used]);
        self.buf.resize(self.buf.len() + (n - used), 0);
    }
}
