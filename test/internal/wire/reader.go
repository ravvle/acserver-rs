// Package wire implements the AC wire format: packed little-endian primitives
// and the string encodings (docs/01 §1.2).
package wire

import (
	"encoding/binary"
	"math"
)

// Reader decodes a payload. A read past the end returns a zero value and sets
// Overflowed instead of panicking.
type Reader struct {
	buf []byte
	pos int
	of  bool // overflow occurred
}

// NewReader wraps a payload (already stripped of any TCP length prefix).
func NewReader(buf []byte) *Reader { return &Reader{buf: buf} }

// Remaining returns the number of unread bytes.
func (r *Reader) Remaining() int { return len(r.buf) - r.pos }

// Overflowed reports whether any read ran past the end of the buffer.
func (r *Reader) Overflowed() bool { return r.of }

func (r *Reader) take(n int) []byte {
	if r.pos+n > len(r.buf) {
		r.of = true
		r.pos = len(r.buf)
		return nil
	}
	b := r.buf[r.pos : r.pos+n]
	r.pos += n
	return b
}

func (r *Reader) Byte() byte {
	b := r.take(1)
	if b == nil {
		return 0
	}
	return b[0]
}

func (r *Reader) Bool() bool   { return r.Byte() != 0 }
func (r *Reader) Int8() int8   { return int8(r.Byte()) }
func (r *Reader) UInt8() uint8 { return r.Byte() }

func (r *Reader) UInt16() uint16 {
	b := r.take(2)
	if b == nil {
		return 0
	}
	return binary.LittleEndian.Uint16(b)
}

func (r *Reader) Int16() int16 { return int16(r.UInt16()) }

func (r *Reader) UInt32() uint32 {
	b := r.take(4)
	if b == nil {
		return 0
	}
	return binary.LittleEndian.Uint32(b)
}

func (r *Reader) Int32() int32 { return int32(r.UInt32()) }

func (r *Reader) UInt64() uint64 {
	b := r.take(8)
	if b == nil {
		return 0
	}
	return binary.LittleEndian.Uint64(b)
}

func (r *Reader) Int64() int64 { return int64(r.UInt64()) }

func (r *Reader) Float32() float32 { return math.Float32frombits(r.UInt32()) }

// Float16 decodes an IEEE-754 binary16 value into a float32.
func (r *Reader) Float16() float32 { return HalfToFloat(r.UInt16()) }

// Vec3 reads three consecutive float32s.
func (r *Reader) Vec3() Vec3 {
	return Vec3{X: r.Float32(), Y: r.Float32(), Z: r.Float32()}
}

// Bytes reads n raw bytes.
func (r *Reader) Bytes(n int) []byte {
	b := r.take(n)
	if b == nil {
		return nil
	}
	out := make([]byte, n)
	copy(out, b)
	return out
}

// RemainingBytes returns a copy of everything not yet consumed.
func (r *Reader) RemainingBytes() []byte {
	return r.Bytes(r.Remaining())
}

// String8 reads a u8 length prefix (in bytes) then that many UTF-8 bytes.
func (r *Reader) String8() string {
	n := int(r.Byte())
	return string(r.take(n))
}

// String16 reads a u16 byte-length prefix then that many UTF-8 bytes.
func (r *Reader) String16() string {
	n := int(r.UInt16())
	return string(r.take(n))
}

// WString8 reads a u8 char-length prefix then length*4 bytes of UTF-32LE.
func (r *Reader) WString8() string {
	n := int(r.Byte())
	return r.readUTF32(n)
}

// WString16 reads a u16 char-length prefix then length*4 bytes of UTF-32LE.
func (r *Reader) WString16() string {
	n := int(r.UInt16())
	return r.readUTF32(n)
}

func (r *Reader) readUTF32(chars int) string {
	b := r.take(chars * 4)
	if b == nil {
		return ""
	}
	runes := make([]rune, chars)
	for i := 0; i < chars; i++ {
		runes[i] = rune(binary.LittleEndian.Uint32(b[i*4:]))
	}
	return string(runes)
}

// CStr reads a fixed n-byte UTF-8 buffer, trimming trailing NULs.
func (r *Reader) CStr(n int) string {
	b := r.take(n)
	if b == nil {
		return ""
	}
	end := len(b)
	for end > 0 && b[end-1] == 0 {
		end--
	}
	return string(b[:end])
}
