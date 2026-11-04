use bytes::{Buf as _, TryGetError};

use super::Vec3;

/// Decodes a payload. A read past the end returns a zero value and sets
/// `overflowed` instead of panicking, so a handler can read every field and
/// check once at the end.
// COMPLEXITY: the reader borrows the packet (`Reader<'a>`) instead of owning a
// copy. Every inbound datagram (several thousand a second at a full grid) was
// copied just to be read, so borrowing removes an allocation per packet. The
// lifetime only ties a reader to the buffer it reads, which callers never
// keep; handlers write `&mut Reader` (an async one, `Reader<'_>`).
#[derive(Debug)]
pub struct Reader<'a> {
    /// The unread bytes; each read advances it (`bytes::Buf` for `&[u8]`).
    buf: &'a [u8],
    overflowed: bool,
}

impl<'a> Reader<'a> {
    /// Wraps a payload (already stripped of any TCP length prefix).
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader {
            buf,
            overflowed: false,
        }
    }

    /// The number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Whether any read ran past the end of the buffer.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Marks the overflow: nothing more is read and every value is zero.
    fn overflow(&mut self) {
        self.overflowed = true;
        self.buf = &[];
    }

    /// Returns the next `n` bytes, or `None` (and marks the overflow) when
    /// fewer remain.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.buf.len() {
            self.overflow();
            return None;
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Some(head)
    }

    /// The value of a `Buf::try_get_*` read, or zero (marking the overflow)
    /// when too few bytes remained.
    fn checked<T: Default>(&mut self, read: Result<T, TryGetError>) -> T {
        match read {
            Ok(value) => value,
            Err(_) => {
                self.overflow();
                T::default()
            }
        }
    }

    pub fn u8(&mut self) -> u8 {
        let read = self.buf.try_get_u8();
        self.checked(read)
    }

    pub fn bool(&mut self) -> bool {
        self.u8() != 0
    }

    pub fn i8(&mut self) -> i8 {
        let read = self.buf.try_get_i8();
        self.checked(read)
    }

    pub fn u16(&mut self) -> u16 {
        let read = self.buf.try_get_u16_le();
        self.checked(read)
    }

    pub fn i16(&mut self) -> i16 {
        let read = self.buf.try_get_i16_le();
        self.checked(read)
    }

    pub fn u32(&mut self) -> u32 {
        let read = self.buf.try_get_u32_le();
        self.checked(read)
    }

    pub fn i32(&mut self) -> i32 {
        let read = self.buf.try_get_i32_le();
        self.checked(read)
    }

    pub fn u64(&mut self) -> u64 {
        let read = self.buf.try_get_u64_le();
        self.checked(read)
    }

    pub fn i64(&mut self) -> i64 {
        let read = self.buf.try_get_i64_le();
        self.checked(read)
    }

    pub fn f32(&mut self) -> f32 {
        let read = self.buf.try_get_f32_le();
        self.checked(read)
    }

    /// Decodes an IEEE-754 binary16 value.
    pub fn f16(&mut self) -> f32 {
        half::f16::from_bits(self.u16()).to_f32()
    }

    /// Reads three consecutive `f32`s.
    pub fn vec3(&mut self) -> Vec3 {
        let x = self.f32();
        let y = self.f32();
        let z = self.f32();
        Vec3 { x, y, z }
    }

    /// Reads `n` raw bytes; empty on overflow.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        match self.take(n) {
            Some(bytes) => bytes.to_vec(),
            None => Vec::new(),
        }
    }

    /// Returns a copy of everything not yet consumed.
    pub fn remaining_bytes(&mut self) -> Vec<u8> {
        self.bytes(self.remaining())
    }

    /// Reads a u8 length prefix (in bytes), then that many UTF-8 bytes.
    pub fn string8(&mut self) -> String {
        let n = self.u8() as usize;
        self.utf8(n)
    }

    /// Reads a u16 byte-length prefix, then that many UTF-8 bytes.
    pub fn string16(&mut self) -> String {
        let n = self.u16() as usize;
        self.utf8(n)
    }

    /// Reads a u8 character-length prefix, then that many UTF-32LE characters.
    pub fn wstring8(&mut self) -> String {
        let chars = self.u8() as usize;
        self.utf32(chars)
    }

    /// Reads a u16 character-length prefix, then that many UTF-32LE characters.
    pub fn wstring16(&mut self) -> String {
        let chars = self.u16() as usize;
        self.utf32(chars)
    }

    /// Reads a fixed `n`-byte UTF-8 buffer, trimming trailing NULs.
    pub fn cstr(&mut self, n: usize) -> String {
        self.utf8(n).trim_end_matches('\0').to_string()
    }

    /// Reads `n` bytes as text. Go keeps invalid UTF-8 as raw bytes; a Rust
    /// `String` cannot, so invalid sequences become U+FFFD. Only a client
    /// sending malformed text can observe the difference.
    fn utf8(&mut self, n: usize) -> String {
        match self.take(n) {
            Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            None => String::new(),
        }
    }

    /// Reads `chars` UTF-32LE code points. An invalid code point (a surrogate
    /// or above U+10FFFF) becomes U+FFFD.
    fn utf32(&mut self, chars: usize) -> String {
        let Some(bytes) = self.take(chars * 4) else {
            return String::new();
        };
        let mut out = String::with_capacity(chars);
        let (units, _) = bytes.as_chunks::<4>();
        for unit in units {
            let code = u32::from_le_bytes(*unit);
            out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        out
    }
}
