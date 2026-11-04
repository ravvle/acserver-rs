package integration

import (
	"fmt"
	"net"
	"sync"
	"testing"
	"time"

	"acserver-rs/test/internal/wire"
	"github.com/stretchr/testify/require"
)

// pluginListener records every datagram the server sends to the plugin port.
type pluginListener struct {
	sock      *net.UDPConn
	localPort int

	mu   sync.Mutex
	pkts [][]byte
}

func newPluginListener(t *testing.T, plugPort, localPort int) *pluginListener {
	sock, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: plugPort})
	require.NoError(t, err)
	t.Cleanup(func() { sock.Close() })
	l := &pluginListener{sock: sock, localPort: localPort}
	go func() {
		buf := make([]byte, 2048)
		for {
			n, _, err := sock.ReadFromUDP(buf)
			if err != nil {
				return
			}
			l.mu.Lock()
			l.pkts = append(l.pkts, append([]byte(nil), buf[:n]...))
			l.mu.Unlock()
		}
	}()
	return l
}

// take returns and clears everything received so far.
func (l *pluginListener) take() [][]byte {
	l.mu.Lock()
	defer l.mu.Unlock()
	out := l.pkts
	l.pkts = nil
	return out
}

func (l *pluginListener) send(b []byte) {
	l.sock.WriteToUDP(b, &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: l.localPort})
}

func pluginIDs(pkts [][]byte) (ids []byte, carUpdates int) {
	for _, p := range pkts {
		if p[0] == 0x35 {
			carUpdates++
			continue
		}
		ids = append(ids, p[0])
	}
	return ids, carUpdates
}

// TestPluginAPI pins the ACSP behaviour measured against acServer v1.15:
//   - startup sends 0x38 then a single 0x32, whose WeatherGraphics is a string8;
//   - no 0x35 CarUpdate before the plugin's 0xC8, and none at interval 0;
//   - 0x37 EndSession only when a results file was written (a lap was set),
//     carrying its path as a wstring8, just before the next 0x32.
func TestPluginAPI(t *testing.T) {
	root := t.TempDir()
	p := freePorts(t)
	plugPort, localPort := reserveProbePort(t), reserveProbePort(t)
	cfgDir := writeTemplatedCfg(t, root, p, func(t *testing.T, root, cfgDir string) {
		editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
			s = setSTR(s, "UDP_PLUGIN_ADDRESS", fmt.Sprintf("127.0.0.1:%d", plugPort))
			return setINT(s, "UDP_PLUGIN_LOCAL_PORT", localPort)
		})
	})
	l := newPluginListener(t, plugPort, localPort)
	tgt := startOurTarget(t, root, cfgDir, p)
	defer tgt.stop()

	pl := joinPlayer(t, tgt, joinAs("Plugin"))
	require.NotNil(t, pl)
	defer pl.k.close()
	_, ok := pl.enterWorld(t, tgt)
	require.True(t, ok)
	time.Sleep(2500 * time.Millisecond)

	startup := l.take()
	ids, updates := pluginIDs(startup)
	require.Equal(t, []byte{0x38, 0x32, 0x33, 0x3A}, ids, "startup, join and load")
	require.Zero(t, updates, "no 0x35 before 0xC8")
	r := wire.NewReader(startup[1][1:])
	r.Bytes(4) // version, session index, current index, count
	r.WString8()
	r.String8()
	r.String8()
	r.String8()
	r.Bytes(9) // type, time, laps, wait, ambient, road
	require.Equal(t, "3_clear", r.String8(), "WeatherGraphics is a string8")
	r.Int32()
	require.Zero(t, r.Remaining())
	require.False(t, r.Overflowed())

	w := wire.NewWriterID(0xC8)
	w.UInt16(200)
	l.send(w.Bytes())
	time.Sleep(time.Second)
	_, updates = pluginIDs(l.take())
	require.GreaterOrEqual(t, updates, 3, "0x35 at the requested interval")

	w = wire.NewWriterID(0xC8)
	w.UInt16(0)
	l.send(w.Bytes())
	time.Sleep(300 * time.Millisecond)
	l.take()
	time.Sleep(time.Second)
	_, updates = pluginIDs(l.take())
	require.Zero(t, updates, "interval 0 stops 0x35")

	// No lap, so no results file and no 0x37.
	l.send([]byte{0xCF})
	time.Sleep(time.Second)
	ids, _ = pluginIDs(l.take())
	require.Equal(t, []byte{0x32}, ids)

	pl.k.sendTCP(lapTimed(5000, 95000, 1))
	time.Sleep(500 * time.Millisecond)
	l.take()
	l.send([]byte{0xCF})
	time.Sleep(time.Second)
	pkts := l.take()
	ids, _ = pluginIDs(pkts)
	require.Equal(t, []byte{0x37, 0x32}, ids)
	path := wire.NewReader(pkts[0][1:]).WString8()
	require.FileExists(t, path)
}
