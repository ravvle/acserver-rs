package wire

import (
	"bufio"
	"bytes"
	"math"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestRoundTripPrimitives(t *testing.T) {
	w := NewWriter()
	w.Byte(0x42)
	w.Bool(true)
	w.Int8(-5)
	w.UInt16(0xBEEF)
	w.Int16(-1234)
	w.UInt32(0xDEADBEEF)
	w.Int32(-99999)
	w.UInt64(0x1122334455667788)
	w.Float32(3.14159)
	w.Vec3(Vec3{1.5, -2.5, 3.5})

	r := NewReader(w.Bytes())
	require.Equal(t, byte(0x42), r.Byte())
	require.True(t, r.Bool())
	require.Equal(t, int8(-5), r.Int8())
	require.Equal(t, uint16(0xBEEF), r.UInt16())
	require.Equal(t, int16(-1234), r.Int16())
	require.Equal(t, uint32(0xDEADBEEF), r.UInt32())
	require.Equal(t, int32(-99999), r.Int32())
	require.Equal(t, uint64(0x1122334455667788), r.UInt64())
	require.InDelta(t, 3.14159, r.Float32(), 1e-5)
	require.Equal(t, Vec3{1.5, -2.5, 3.5}, r.Vec3())
	require.False(t, r.Overflowed(), "unexpected overflow")
}

func TestWriterResetBufferUsesCallerStorage(t *testing.T) {
	w := NewWriter()
	first := append([]byte(nil), w.UInt32(1).Bytes()...)

	storage := make([]byte, 0, 8)
	got := w.ResetBuffer(storage).UInt32(2).Bytes()
	require.Equal(t, []byte{2, 0, 0, 0}, got)
	require.Equal(t, []byte{1, 0, 0, 0}, first)
	require.Equal(t, &storage[:cap(storage)][0], &got[:cap(got)][0])
}

func TestNewWriterCapPreallocatesRequestedCapacity(t *testing.T) {
	w := NewWriterCap(257)
	if got := cap(w.Bytes()); got < 257 {
		t.Fatalf("capacity = %d, want at least 257", got)
	}
}

func TestStringEncodings(t *testing.T) {
	w := NewWriter()
	w.String8("hello")
	w.String16("world!")
	w.WString8("Grüße") // multibyte
	w.WString16("日本語")  // CJK
	w.CStr("abc", 6)

	r := NewReader(w.Bytes())
	require.Equal(t, "hello", r.String8())
	require.Equal(t, "world!", r.String16())
	require.Equal(t, "Grüße", r.WString8())
	require.Equal(t, "日本語", r.WString16())
	require.Equal(t, "abc", r.CStr(6))
}

func TestUTF32Width(t *testing.T) {
	// wstring8 must count characters (each 4 bytes on the wire).
	w := NewWriter()
	w.WString8("日本") // 2 chars -> 1 length byte + 8 bytes
	require.Len(t, w.Bytes(), 1+2*4)
}

func TestReaderOverflow(t *testing.T) {
	r := NewReader([]byte{0x01})
	_ = r.UInt32() // reads past end
	require.True(t, r.Overflowed(), "expected overflow flag")
}

func TestTCPFraming(t *testing.T) {
	payload := []byte{0x3E, 0x01, 0x02}
	framed := FrameTCP(payload)
	require.Len(t, framed, len(payload)+2)
	require.Equal(t, []byte{0x03, 0x00}, framed[:2], "length prefix")
}

func TestTCPFramingSeparateClientAndServerLimits(t *testing.T) {
	payload := make([]byte, MaxTCPRecv+1)
	framed := FrameTCP(payload)

	got, err := ReadTCPPacket(bufio.NewReader(bytes.NewReader(framed)))
	require.NoError(t, err, "protocol clients must accept large server packets")
	require.Equal(t, payload, got)

	_, err = ReadTCPPacketLimit(
		bufio.NewReader(bytes.NewReader(framed)),
		MaxTCPRecv,
	)
	require.ErrorIs(t, err, ErrPacketTooLarge,
		"the server must retain its smaller untrusted inbound limit")
}

func TestHalfFloat(t *testing.T) {
	cases := []float32{0, 1, -1, 0.5, 3.5, 100, -273.15, 65504}
	for _, c := range cases {
		got := HalfToFloat(FloatToHalf(c))
		// half precision has limited mantissa; allow relative tolerance.
		tol := math.Max(math.Abs(float64(c))*0.01, 0.01)
		require.InDelta(t, c, got, tol, "half roundtrip %v", c)
	}
	// Special values.
	require.True(t, math.IsNaN(float64(HalfToFloat(FloatToHalf(float32(math.NaN()))))),
		"NaN not preserved")
}
