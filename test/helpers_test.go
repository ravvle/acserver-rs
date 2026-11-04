package integration

import (
	"encoding/json"
	"fmt"
	"math/rand"
	"net"
	"net/http"
	"testing"
	"time"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
	"github.com/stretchr/testify/require"
)

// sendChat sends a 0x47 chat line.
func sendChat(t *testing.T, conn net.Conn, msg string) {
	t.Helper()
	w := wire.NewWriterID(proto.Chat)
	w.UInt8(0)
	w.WString8(msg)
	_, err := conn.Write(wire.FrameTCP(w.Bytes()))
	require.NoError(t, err)
}

// sendSectorSplit sends a 0x58 sector split.
func sendSectorSplit(conn net.Conn, index uint8, splitTime uint32, cuts uint8) {
	w := wire.NewWriterID(proto.SectorSplit)
	w.UInt8(index)
	w.UInt32(splitTime)
	w.UInt8(cuts)
	conn.Write(wire.FrameTCP(w.Bytes()))
}

// randDur returns a uniformly random duration in [lo, hi].
func randDur(rng *rand.Rand, lo, hi time.Duration) time.Duration {
	if hi <= lo {
		return lo
	}
	return lo + time.Duration(rng.Int63n(int64(hi-lo)))
}

// sendRaceLap sends a 0x49 lap-completed with an explicit, strictly-increasing
// timestamp (the per-car anti-replay gate drops a lap whose timestamp does not
// advance, so multi-lap drivers must bump it each lap).
func sendRaceLap(conn net.Conn, numLap uint8, ts uint32) {
	lap := wire.NewWriterID(proto.LapCompleted)
	lap.UInt32(ts)    // timestamp
	lap.UInt32(95000) // lap time
	lap.UInt8(2)      // sector count
	lap.UInt32(31_000)
	lap.UInt32(63_000)
	lap.UInt8(0) // cuts
	lap.UInt8(numLap)
	conn.Write(wire.FrameTCP(lap.Bytes()))
}

func sendCarCollision(conn net.Conn, target uint8, speed float32) {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1)
	w.UInt8(proto.EventCollisionWithCar)
	w.UInt8(target)
	w.Float32(speed)
	w.Vec3(wire.Vec3{X: 10, Y: 0.5, Z: 20})
	w.Vec3(wire.Vec3{X: 0.8, Y: 0.2, Z: -1.1})
	conn.Write(wire.FrameTCP(w.Bytes()))
}

func sendEnvironmentCollision(conn net.Conn, speed float32) {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1)
	w.UInt8(proto.EventCollisionWithEnv)
	w.Float32(speed)
	w.Vec3(wire.Vec3{X: 100, Y: 1, Z: 250})
	w.Vec3(wire.Vec3{X: -0.9, Y: 0.1, Z: 0.4})
	conn.Write(wire.FrameTCP(w.Bytes()))
}

func sendDamage(conn net.Conn, base float32) {
	w := wire.NewWriterID(proto.DamageUpdate)
	for i := range 5 {
		w.Float32(base + float32(i)/2)
	}
	conn.Write(wire.FrameTCP(w.Bytes()))
}

func sendJumpStart(conn net.Conn) {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1)
	w.UInt8(proto.EventJumpStart)
	conn.Write(wire.FrameTCP(w.Bytes()))
}

func sendTyreCompound(conn net.Conn, compound string) {
	w := wire.NewWriterID(proto.TyreCompoundChange)
	w.String8(compound)
	conn.Write(wire.FrameTCP(w.Bytes()))
}

func sendMandatoryPit(conn net.Conn) {
	w := wire.NewWriterID(proto.MandatoryPitUpdate)
	w.Bool(true)
	conn.Write(wire.FrameTCP(w.Bytes()))
}

// sampleClients does a best-effort /INFO read of the connected count from a
// goroutine (no t methods, so it never trips FailNow off the test goroutine).
// Returns -1 on any error.
func sampleClients(httpPort int) int64 {
	resp, err := http.Get(fmt.Sprintf("http://127.0.0.1:%d/INFO", httpPort))
	if err != nil {
		return -1
	}
	defer resp.Body.Close()
	var info map[string]any
	if json.NewDecoder(resp.Body).Decode(&info) != nil {
		return -1
	}
	n, _ := info["clients"].(float64)
	return int64(n)
}
