package wire

import (
	"encoding/binary"
	"math"
)

// Vec3 is three float32 components (12 bytes on the wire).
type Vec3 struct{ X, Y, Z float32 }

// Writer builds a payload, framed with FrameTCP for TCP or sent as-is for UDP.
type Writer struct {
	buf []byte
}

// NewWriter returns an empty writer.
func NewWriter() *Writer { return &Writer{buf: make([]byte, 0, 64)} }

// NewWriterCap returns an empty writer with room for capacity bytes.
func NewWriterCap(capacity int) *Writer {
	if capacity < 0 {
		capacity = 0
	}
	return &Writer{buf: make([]byte, 0, capacity)}
}

// NewWriterID starts a payload with the packet id byte already written.
func NewWriterID(id byte) *Writer {
	w := NewWriter()
	w.Byte(id)
	return w
}

// Bytes returns the accumulated payload (not framed).
func (w *Writer) Bytes() []byte { return w.buf }

// Reset empties the writer, keeping its buffer. Earlier Bytes() results must
// be consumed first.
func (w *Writer) Reset() *Writer {
	w.buf = w.buf[:0]
	return w
}

// ResetBuffer makes buf[:0] the writer's storage. Earlier Bytes() results stay
// valid, since their storage is no longer written.
func (w *Writer) ResetBuffer(buf []byte) *Writer {
	w.buf = buf[:0]
	return w
}

// Len returns the current payload length.
func (w *Writer) Len() int { return len(w.buf) }

func (w *Writer) Byte(b byte) *Writer { w.buf = append(w.buf, b); return w }

func (w *Writer) Bool(v bool) *Writer {
	if v {
		return w.Byte(1)
	}
	return w.Byte(0)
}

func (w *Writer) Int8(v int8) *Writer   { return w.Byte(byte(v)) }
func (w *Writer) UInt8(v uint8) *Writer { return w.Byte(v) }

func (w *Writer) UInt16(v uint16) *Writer {
	w.buf = binary.LittleEndian.AppendUint16(w.buf, v)
	return w
}
func (w *Writer) Int16(v int16) *Writer { return w.UInt16(uint16(v)) }

func (w *Writer) UInt32(v uint32) *Writer {
	w.buf = binary.LittleEndian.AppendUint32(w.buf, v)
	return w
}
func (w *Writer) Int32(v int32) *Writer { return w.UInt32(uint32(v)) }

func (w *Writer) UInt64(v uint64) *Writer {
	w.buf = binary.LittleEndian.AppendUint64(w.buf, v)
	return w
}
func (w *Writer) Int64(v int64) *Writer { return w.UInt64(uint64(v)) }

func (w *Writer) Float32(v float32) *Writer { return w.UInt32(math.Float32bits(v)) }

// Float16 writes a float32 as IEEE-754 binary16.
func (w *Writer) Float16(v float32) *Writer { return w.UInt16(FloatToHalf(v)) }

func (w *Writer) Vec3(v Vec3) *Writer {
	return w.Float32(v.X).Float32(v.Y).Float32(v.Z)
}

// Raw appends bytes verbatim.
func (w *Writer) Raw(b []byte) *Writer { w.buf = append(w.buf, b...); return w }

// String8 writes a u8 byte-length prefix then UTF-8 bytes (truncated to 255).
func (w *Writer) String8(s string) *Writer {
	b := []byte(s)
	if len(b) > 255 {
		b = b[:255]
	}
	w.Byte(byte(len(b)))
	return w.Raw(b)
}

// String16 writes a u16 byte-length prefix then UTF-8 bytes.
func (w *Writer) String16(s string) *Writer {
	b := []byte(s)
	if len(b) > 0xFFFF {
		b = b[:0xFFFF]
	}
	w.UInt16(uint16(len(b)))
	return w.Raw(b)
}

// WString8 writes a u8 char-length prefix then char*4 bytes of UTF-32LE.
func (w *Writer) WString8(s string) *Writer {
	runes := []rune(s)
	if len(runes) > 255 {
		runes = runes[:255]
	}
	w.Byte(byte(len(runes)))
	return w.writeUTF32(runes)
}

// WString16 writes a u16 char-length prefix then char*4 bytes of UTF-32LE.
func (w *Writer) WString16(s string) *Writer {
	runes := []rune(s)
	if len(runes) > 0xFFFF {
		runes = runes[:0xFFFF]
	}
	w.UInt16(uint16(len(runes)))
	return w.writeUTF32(runes)
}

func (w *Writer) writeUTF32(runes []rune) *Writer {
	for _, r := range runes {
		w.UInt32(uint32(r))
	}
	return w
}

// CStr writes a fixed n-byte zero-padded UTF-8 buffer.
func (w *Writer) CStr(s string, n int) *Writer {
	b := make([]byte, n)
	copy(b, []byte(s))
	return w.Raw(b)
}
