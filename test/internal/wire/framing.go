package wire

import (
	"bufio"
	"encoding/binary"
	"errors"
	"io"
)

// MaxTCPRecv is the largest client packet accepted; a longer declared length
// closes the connection (docs/01 §1.3).
const MaxTCPRecv = 2046

// MaxTCPSend is the largest payload the server will frame for a TCP send.
const MaxTCPSend = 65535

// ErrPacketTooLarge reports a declared length above the reader's limit.
var ErrPacketTooLarge = errors.New("wire: tcp packet exceeds receive buffer")

// FrameTCP prepends the u16 little-endian length prefix to a payload.
func FrameTCP(payload []byte) []byte {
	out := make([]byte, 2+len(payload))
	binary.LittleEndian.PutUint16(out, uint16(len(payload)))
	copy(out[2:], payload)
	return out
}

// ReadTCPPacket reads one packet of any length, for server-to-client traffic
// (a full-grid leaderboard can exceed MaxTCPRecv).
func ReadTCPPacket(r *bufio.Reader) ([]byte, error) {
	return ReadTCPPacketLimit(r, MaxTCPSend)
}

// ReadTCPPacketLimit reads one packet, rejecting a length above maxPayload.
func ReadTCPPacketLimit(r *bufio.Reader, maxPayload int) ([]byte, error) {
	var lenBuf [2]byte
	if _, err := io.ReadFull(r, lenBuf[:]); err != nil {
		return nil, err
	}
	n := int(binary.LittleEndian.Uint16(lenBuf[:]))
	if n == 0 {
		return nil, io.EOF
	}
	if n > maxPayload {
		return nil, ErrPacketTooLarge
	}
	payload := make([]byte, n)
	if _, err := io.ReadFull(r, payload); err != nil {
		return nil, err
	}
	return payload, nil
}
