package integration

// End-to-end tests of the AssettoServer features (extra_cfg.yml:
// EnableCustomUpdate, EnableWeatherFx, CSPServerScripts, EnableAi, user
// groups, the extra commands and kicks, plugins, live configuration). They
// run the server (serverBinary) in its own process:
//
//	cd test && go test . -run TestExt -v

import (
	"archive/zip"
	"bufio"
	"bytes"
	"compress/zlib"
	"encoding/base64"
	"encoding/binary"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
	"github.com/stretchr/testify/require"
)

// extFeatures is a CSP client that supports everything.
const extFeatures = "SPECTATING_AWARE,CUSTOM_UPDATE,WEATHERFX_V1,2144"

// luaApiKey is AS_ApiKey's LuaPacketType (docs/04 §4.4).
const luaApiKey = 0x27612FAB

// startExtServer starts the server under test (serverBinary) on a generated config. entries is the
// entry list body; extra the extra_cfg.yml body; content the --content root
// ("" for none).
func startExtServer(t *testing.T, entries, extra, content string, files map[string]string) extServer {
	t.Helper()
	return extServerOn(t, "ext_track", entries, extra, content, files)
}

// extServerOn is startExtServer on a named track.
func extServerOn(t *testing.T, track, entries, extra, content string, files map[string]string) extServer {
	t.Helper()
	bin := serverBinary(t)
	root := t.TempDir()
	cfgDir := filepath.Join(root, "cfg")
	require.NoError(t, os.MkdirAll(cfgDir, 0o755))
	p := freePorts(t)
	writeFile(t, filepath.Join(cfgDir, "server_cfg.ini"), fmt.Sprintf(`[SERVER]
NAME=rust extensions
CARS=ks_mazda_mx5_cup
TRACK=%s
CONFIG_TRACK=
SUN_ANGLE=16
ADMIN_PASSWORD=adm
UDP_PORT=%d
TCP_PORT=%d
HTTP_PORT=%d
MAX_CLIENTS=8
CLIENT_SEND_INTERVAL_HZ=18
SLEEP_TIME=1
REGISTER_TO_LOBBY=0
PICKUP_MODE_ENABLED=1
LOOP_MODE=1

[PRACTICE]
NAME=Practice
TIME=60
IS_OPEN=1

[WEATHER_0]
GRAPHICS=3_clear_type=17
BASE_TEMPERATURE_AMBIENT=18
BASE_TEMPERATURE_ROAD=6
`, track, p.game, p.game, p.http))
	writeFile(t, filepath.Join(cfgDir, "entry_list.ini"), entries)
	writeFile(t, filepath.Join(cfgDir, "extra_cfg.yml"), extra)
	for name, body := range files {
		writeFile(t, filepath.Join(cfgDir, name), body)
	}
	args := []string{"--cfg", cfgDir, "--content", content}
	proc := startPerfProcess(t, "ext", bin, args, root, p)
	t.Cleanup(func() { proc.stop(t) })
	return extServer{portSet: p, cfgDir: cfgDir}
}

// extServer is a running server under test, started in the parent of cfgDir.
type extServer struct {
	portSet
	cfgDir string
}

func (s extServer) file(t *testing.T, name string) string {
	return filepath.Join(s.cfgDir, name)
}

func carEntries(ai ...string) string {
	var b strings.Builder
	for i, mode := range ai {
		fmt.Fprintf(&b, "[CAR_%d]\nMODEL=ks_mazda_mx5_cup\nSKIN=00_official\nGUID=\nAI=%s\n\n", i, mode)
	}
	return b.String()
}

// extClient is a joined client whose TCP and UDP packets are collected.
type extClient struct {
	// lag is added to every pong's echoed time, faking that much ping.
	lag     atomic.Int32
	tcp     net.Conn
	udp     net.Conn
	id      uint8
	mu      sync.Mutex
	tcpPkts [][]byte
	udpPkts [][]byte
}

// joinExt joins with a features list and enters the world.
func joinExt(t *testing.T, p extServer, guid int, name, features string) (*extClient, []byte) {
	t.Helper()
	return joinExtOpts(t, p, guid, name, features, true)
}

// joinExtOpts is joinExt, optionally without answering the checksum.
func joinExtOpts(t *testing.T, p extServer, guid int, name, features string, checksum bool) (*extClient, []byte) {
	t.Helper()
	addr := fmt.Sprintf("127.0.0.1:%d", p.game)
	tcp, err := net.Dial("tcp", addr)
	require.NoError(t, err)
	t.Cleanup(func() { tcp.Close() })
	br := bufio.NewReader(tcp)
	hs := wire.NewWriterID(proto.RequestNewConnection)
	hs.UInt16(proto.ClientProtocolVersion)
	hs.String8(strconv.Itoa(76561198000000000 + guid))
	hs.WString8(name)
	hs.String8("")
	hs.String8("GBR")
	hs.String8("ks_mazda_mx5_cup")
	hs.String8("")
	hs.String16(features)
	_, err = tcp.Write(wire.FrameTCP(hs.Bytes()))
	require.NoError(t, err)
	tcp.SetReadDeadline(time.Now().Add(5 * time.Second))
	resp, err := wire.ReadTCPPacket(br)
	require.NoError(t, err)
	if len(resp) == 0 || resp[0] != proto.NewCarConnection {
		return nil, resp
	}
	tcp.SetReadDeadline(time.Time{})
	c := &extClient{tcp: tcp, id: sessionIDFromHandshake(resp)}

	udp, err := net.Dial("udp", addr)
	require.NoError(t, err)
	t.Cleanup(func() { udp.Close() })
	c.udp = udp
	assoc := wire.NewWriterID(proto.CarConnect)
	assoc.UInt8(c.id)
	udp.Write(assoc.Bytes())
	udp.SetReadDeadline(time.Now().Add(5 * time.Second))
	_, err = udp.Read(make([]byte, 64))
	require.NoError(t, err, "UDP association")
	udp.SetReadDeadline(time.Time{})
	// No checksum paths: just the final digest, unverifiable here.
	if checksum {
		cs := wire.NewWriterID(proto.ChecksumPacket)
		cs.Raw(make([]byte, 16))
		tcp.Write(wire.FrameTCP(cs.Bytes()))
	}

	go func() {
		for {
			pkt, err := wire.ReadTCPPacket(br)
			if err != nil {
				return
			}
			c.mu.Lock()
			c.tcpPkts = append(c.tcpPkts, pkt)
			c.mu.Unlock()
		}
	}()
	go func() {
		buf := make([]byte, 2048)
		for {
			n, err := udp.Read(buf)
			if err != nil {
				return
			}
			pkt := append([]byte(nil), buf[:n]...)
			// Answer pings as a real client does.
			if n >= 5 && pkt[0] == proto.PingUpdate {
				pong := wire.NewWriterID(proto.PingPong)
				pong.Int32(int32(binary.LittleEndian.Uint32(pkt[1:5])) - c.lag.Load())
				pong.Int32(0)
				udp.Write(pong.Bytes())
			}
			c.mu.Lock()
			c.udpPkts = append(c.udpPkts, pkt)
			c.mu.Unlock()
		}
	}()
	return c, resp
}

// sendPosition sends a 0x46 at pos moving with vel.
func (c *extClient) sendPosition(seq uint8, ts uint32, pos, vel wire.Vec3, gas uint8) {
	w := wire.NewWriterID(proto.PositionUpdate)
	w.UInt8(seq)
	w.UInt32(ts)
	w.Vec3(pos)
	w.Vec3(wire.Vec3{X: float32(math.Atan2(float64(vel.Z), float64(vel.X))) - math.Pi/2})
	w.Vec3(vel)
	w.Raw([]byte{100, 100, 100, 100})
	w.UInt8(127)
	w.UInt8(127)
	w.UInt16(3000)
	w.UInt8(3)
	w.UInt32(0)
	w.Int16(0)
	w.UInt8(gas)
	w.Float32(0)
	c.udp.Write(w.Bytes())
}

// waitFor polls the collected packets until match finds one.
func (c *extClient) waitFor(t *testing.T, udp bool, within time.Duration, what string, match func([]byte) bool) []byte {
	t.Helper()
	deadline := time.Now().Add(within)
	for time.Now().Before(deadline) {
		c.mu.Lock()
		pkts := c.tcpPkts
		if udp {
			pkts = c.udpPkts
		}
		for _, p := range pkts {
			if match(p) {
				c.mu.Unlock()
				return p
			}
		}
		c.mu.Unlock()
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("never received %s", what)
	return nil
}

func (c *extClient) count(udp bool, match func([]byte) bool) int {
	c.mu.Lock()
	defer c.mu.Unlock()
	pkts := c.tcpPkts
	if udp {
		pkts = c.udpPkts
	}
	n := 0
	for _, p := range pkts {
		if match(p) {
			n++
		}
	}
	return n
}

// extraOptions decodes the INI hidden in a 0x51 welcome text.
func extraOptions(t *testing.T, welcome []byte) string {
	t.Helper()
	r := wire.NewReader(welcome[1:])
	r.UInt8()
	text := r.WString16()
	i := strings.Index(text, "$CSP0:")
	require.GreaterOrEqual(t, i, 0, "welcome carries extra options: %q", text)
	raw, err := base64.RawStdEncoding.DecodeString(text[i+6:])
	require.NoError(t, err)
	zr, err := zlib.NewReader(bytes.NewReader(raw))
	require.NoError(t, err)
	ini, err := io.ReadAll(zr)
	require.NoError(t, err)
	return string(ini)
}

func TestExtCustomUpdateWeatherFxAndScripts(t *testing.T) {
	extra := `EnableCustomUpdate: true
EnableWeatherFx: true
TrackTimezone: Asia/Tokyo
EnableServerDetails: true
CSPServerScripts:
  - hello.lua
`
	p := startExtServer(t, carEntries("none", "none", "none"), extra, "",
		map[string]string{"hello.lua": "ac.log('hello')\n"})

	// WeatherFX is mandatory.
	_, resp := joinExt(t, p, 1, "NoFx", "SPECTATING_AWARE,2144")
	require.Equal(t, byte(proto.AuthFailed), resp[0], "a client without WEATHERFX_V1 is refused")

	a, _ := joinExt(t, p, 2, "Alice", extFeatures)
	require.NotNil(t, a)
	b, _ := joinExt(t, p, 3, "Bob", extFeatures)
	require.NotNil(t, b)
	for i := 0; i < 40; i++ {
		ts := uint32(100 + i*56)
		a.sendPosition(uint8(i), ts, wire.Vec3{X: 1}, wire.Vec3{X: 1}, 0)
		b.sendPosition(uint8(i), ts, wire.Vec3{X: float32(i)}, wire.Vec3{X: 5}, 0)
		time.Sleep(20 * time.Millisecond)
	}

	// Bob's positions reach Alice as 0xAB 0x03 batches, not 0x46.
	batch := a.waitFor(t, true, 3*time.Second, "0xAB 0x03", func(p []byte) bool {
		return len(p) > 3 && p[0] == proto.Extended && p[1] == 0x03 && p[3] == b.id
	})
	require.Equal(t, byte(1), batch[2], "one car in the batch")
	require.Len(t, batch, 3+60, "60 bytes per car")
	require.Zero(t, a.count(true, func(p []byte) bool { return len(p) > 1 && p[0] == proto.PositionUpdate }),
		"no 0x46 to a custom-update client")

	// WeatherFX every second, with the type from GRAPHICS.
	wx := a.waitFor(t, true, 3*time.Second, "0xAB 0x01", func(p []byte) bool {
		return len(p) >= 12 && p[0] == proto.Extended && p[1] == 0x01
	})
	require.Equal(t, byte(17), wx[10], "ScatteredClouds")
	require.Equal(t, byte(17), wx[11])
	unix := binary.LittleEndian.Uint64(wx[2:10])
	require.InDelta(t, float64(time.Now().Unix()), float64(unix), 2*86400, "today's date")
	require.Zero(t, a.count(false, func(p []byte) bool { return len(p) > 0 && p[0] == proto.WeatherUpdate }),
		"no 0x78 with WeatherFX")

	// The welcome's extra options load the script and the update format.
	welcome := a.waitFor(t, false, 3*time.Second, "0x51", func(p []byte) bool { return p[0] == proto.WelcomeMessage })
	ini := extraOptions(t, welcome)
	require.Contains(t, ini, "[SCRIPT_10-hello.lua]")
	require.Contains(t, ini, "/api/scripts/0")
	require.Contains(t, ini, "CUSTOM_UPDATE_FORMAT")

	// The script is served; the features are advertised.
	res, err := http.Get(fmt.Sprintf("http://127.0.0.1:%d/api/scripts/0", p.http))
	require.NoError(t, err)
	body, _ := io.ReadAll(res.Body)
	res.Body.Close()
	require.Equal(t, 200, res.StatusCode)
	require.Equal(t, "ac.log('hello')\n", string(body))
	res, err = http.Get(fmt.Sprintf("http://127.0.0.1:%d/api/scripts/1", p.http))
	require.NoError(t, err)
	res.Body.Close()
	require.Equal(t, 404, res.StatusCode)
	res, err = http.Get(fmt.Sprintf("http://127.0.0.1:%d/api/details", p.http))
	require.NoError(t, err)
	body, _ = io.ReadAll(res.Body)
	res.Body.Close()
	require.Contains(t, string(body), `"CUSTOM_UPDATE"`)
	require.Contains(t, string(body), `"WEATHERFX_V1"`)

	// AS_ApiKey: a script asks, the server answers with this client's key.
	ask := wire.NewWriterID(proto.Extended)
	ask.UInt8(proto.ExtTCPClientMessage)
	ask.UInt16(60000)
	ask.UInt32(luaApiKey)
	ask.Raw(make([]byte, 32))
	a.tcp.Write(wire.FrameTCP(ask.Bytes()))
	reply := a.waitFor(t, false, 3*time.Second, "AS_ApiKey reply", func(p []byte) bool {
		return len(p) >= 9 && p[0] == proto.Extended && p[1] == proto.ExtTCPClientMessage &&
			binary.LittleEndian.Uint16(p[3:5]) == 60000 && binary.LittleEndian.Uint32(p[5:9]) == luaApiKey
	})
	require.Equal(t, byte(255), reply[2], "from the server")
	require.Len(t, reply, 9+32)
	_, err = strconv.ParseUint(string(reply[9:25]), 16, 64)
	require.NoError(t, err, "hex key %q", reply[9:])
	require.Zero(t, b.count(false, func(p []byte) bool {
		return len(p) >= 9 && p[0] == proto.Extended && binary.LittleEndian.Uint32(p[5:9]) == luaApiKey
	}), "the key goes to the asker only")
}

// circleSpline writes a closed one-lane AI spline: a circle of `radius`
// around the origin, counter-clockwise seen from above, one point per 2 m.
func circleSpline(t *testing.T, dir string, radius float64) {
	t.Helper()
	require.NoError(t, os.MkdirAll(dir, 0o755))
	n := int(2 * math.Pi * radius / 2)
	w := wire.NewWriter()
	w.Int32(-1)
	w.Int32(int32(n))
	for i := 0; i < n; i++ {
		a := 2 * math.Pi * float64(i) / float64(n)
		w.Vec3(wire.Vec3{X: float32(radius * math.Cos(a)), Z: float32(radius * math.Sin(a))})
		w.Float32(float32(radius))
		w.Float32(0)
	}
	require.NoError(t, os.WriteFile(filepath.Join(dir, "fast_lane.ai"), w.Bytes(), 0o644))
}

func TestExtAiTraffic(t *testing.T) {
	content := t.TempDir()
	const radius = 150.0
	circleSpline(t, filepath.Join(content, "content", "tracks", "ext_track", "ai"), radius)
	require.NoError(t, os.MkdirAll(filepath.Join(content, "content", "cars", "ks_mazda_mx5_cup"), 0o755))
	extra := `EnableAi: true
AiParams:
  MinSpawnDistancePoints: 30
  MaxSpawnDistancePoints: 50
  SpawnSafetyDistanceToPlayerMeters: 20
  MinAiSafetyDistanceMeters: 10
  MaxAiSafetyDistanceMeters: 15
  AiPerPlayerTargetCount: 3
  AiBehaviorUpdateIntervalHz: 5
  PlayerAfkTimeoutSeconds: 60
`
	p := startExtServer(t, carEntries("none", "auto", "auto", "auto"), extra, content, nil)

	alice, _ := joinExt(t, p, 1, "Alice", "")
	require.NotNil(t, alice)
	require.Equal(t, uint8(0), alice.id, "the player slot")

	// Drive along the circle at 10 m/s.
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		start := time.Now()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(55 * time.Millisecond):
			}
			s := time.Since(start).Seconds() * 10 / radius
			pos := wire.Vec3{X: float32(radius * math.Cos(s)), Z: float32(radius * math.Sin(s))}
			vel := wire.Vec3{X: float32(-10 * math.Sin(s)), Z: float32(10 * math.Cos(s))}
			alice.sendPosition(uint8(i), uint32(100+i*55), pos, vel, uint8(i%2))
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	// Traffic is announced and driven on the circle.
	alice.waitFor(t, false, 8*time.Second, "0x5A for an AI car", func(p []byte) bool {
		return len(p) > 2 && p[0] == proto.CarConnected && p[1] >= 1 && p[1] <= 3 &&
			strings.HasPrefix(string(p[3:3+int(p[2])]), "Traffic")
	})
	// AI cars come batched in 0x48s (docs/03), never as single 0x46s.
	mega := alice.waitFor(t, true, 5*time.Second, "0x48 for the AI cars", func(p []byte) bool {
		return len(p) > 0 && p[0] == proto.MegaPacket
	})
	count := int(mega[7])
	require.True(t, count >= 1 && count <= 3, "one car per AI slot, got %d", count)
	require.Len(t, mega, 8+57*count, "header, then 57 bytes a car")
	require.NotZero(t, binary.LittleEndian.Uint32(mega[1:5]), "the receiver's clock")
	first := megaEntries(mega)[0]
	require.True(t, first[1] >= 1 && first[1] <= 3, "an AI slot")
	ai := first[1]
	onCircle := func(p []byte) (float64, wire.Vec3) {
		r := wire.NewReader(p[1:])
		r.UInt8()
		r.UInt8()
		r.UInt32()
		r.UInt16()
		pos := r.Vec3()
		return math.Hypot(float64(pos.X), float64(pos.Z)), pos
	}
	dist, p0 := onCircle(first)
	require.InDelta(t, radius, dist, 1.0, "on the spline")
	time.Sleep(time.Second)
	var last []byte
	alice.mu.Lock()
	for _, mp := range alice.udpPkts {
		for _, p := range megaEntries(mp) {
			if p[1] == ai {
				last = p
			}
		}
	}
	alice.mu.Unlock()
	require.Zero(t, alice.count(true, func(p []byte) bool {
		return len(p) > 1 && p[0] == proto.PositionUpdate && p[1] >= 1 && p[1] <= 3
	}), "no 0x46 for an AI car")
	dist, p1 := onCircle(last)
	require.InDelta(t, radius, dist, 1.0, "still on the spline")
	moved := math.Hypot(float64(p1.X-p0.X), float64(p1.Z-p0.Z))
	require.Greater(t, moved, 5.0, "the AI car drives")
	flags := binary.LittleEndian.Uint32(last[54:58])
	require.NotZero(t, flags&0x20, "lights on")

	// A second player takes an AI=auto slot, which stops being traffic.
	bob, _ := joinExt(t, p, 2, "Bob", "")
	require.NotNil(t, bob)
	require.GreaterOrEqual(t, bob.id, uint8(1), "an AI=auto slot is open to players")
	alice.waitFor(t, false, 3*time.Second, "0x5A for Bob", func(p []byte) bool {
		return len(p) > 1 && p[0] == proto.CarConnected && p[1] == bob.id &&
			string(p[3:3+int(p[2])]) == "Bob"
	})
}

// TestExtFarPositionBatch checks BatchFarPositions: cars outside a
// player's network bubble (500 m by default) reach them batched in 0x48s
// once per tick, while a car inside it keeps its single 0x46s.
func TestExtFarPositionBatch(t *testing.T) {
	for _, tc := range []struct {
		name  string
		extra string
		batch bool
	}{
		{"default", "", true},
		{"off", "BatchFarPositions: false\n", false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			p := startExtServer(t, carEntries("none", "none", "none", "none"), tc.extra, "", nil)
			// Alice and Bob 10 m apart; Carol and Erin 2 km away from both.
			alice, _ := joinExt(t, p, 1, "Alice", "")
			bob, _ := joinExt(t, p, 2, "Bob", "")
			carol, _ := joinExt(t, p, 3, "Carol", "")
			erin, _ := joinExt(t, p, 4, "Erin", "")
			stop := make(chan struct{})
			var wg sync.WaitGroup
			wg.Add(1)
			go func() {
				defer wg.Done()
				for i := 0; ; i++ {
					select {
					case <-stop:
						return
					case <-time.After(50 * time.Millisecond):
					}
					ts := uint32(100 + i*50)
					alice.sendPosition(uint8(i), ts, wire.Vec3{X: 0}, wire.Vec3{}, 0)
					bob.sendPosition(uint8(i), ts, wire.Vec3{X: 10}, wire.Vec3{}, 0)
					carol.sendPosition(uint8(i), ts, wire.Vec3{X: 2000}, wire.Vec3{}, 0)
					erin.sendPosition(uint8(i), ts, wire.Vec3{X: 2000, Z: 50}, wire.Vec3{}, 0)
				}
			}()
			defer func() { close(stop); wg.Wait() }()

			time.Sleep(2 * time.Second)
			// Over 2 s at 20/s: what Alice gets of each car, singly and batched.
			type tally struct{ single, batched, megas, both int }
			measure := func() tally {
				var n tally
				alice.mu.Lock()
				defer alice.mu.Unlock()
				for _, pkt := range alice.udpPkts {
					if len(pkt) >= 61 && pkt[0] == proto.PositionUpdate && (pkt[1] == carol.id || pkt[1] == erin.id) {
						n.single++
					}
					cars := megaEntries(pkt)
					if cars == nil {
						continue
					}
					n.megas++
					ids := map[byte]bool{}
					for _, car := range cars {
						ids[car[1]] = true
						r := wire.NewReader(car[1:])
						r.UInt8()
						r.UInt8()
						r.UInt32()
						r.UInt16()
						require.InDelta(t, 2000, r.Vec3().X, 1, "a far car's position")
					}
					require.False(t, ids[bob.id], "Bob is inside the bubble")
					n.batched += len(cars)
					if ids[carol.id] && ids[erin.id] {
						n.both++
					}
				}
				return n
			}
			bobs := func() int {
				return alice.count(true, func(p []byte) bool {
					return len(p) >= 61 && p[0] == proto.PositionUpdate && p[1] == bob.id
				})
			}
			n0, b0 := measure(), bobs()
			time.Sleep(2 * time.Second)
			n1, b1 := measure(), bobs()
			single, batched := n1.single-n0.single, n1.batched-n0.batched
			require.Greater(t, b1-b0, 25, "Bob inside the bubble: about 20/s as 0x46s")
			require.InDelta(t, 16, single+batched, 6, "Carol and Erin: about 4/s each")
			if tc.batch {
				require.GreaterOrEqual(t, n1.both-n0.both, 4, "Carol and Erin share 0x48s")
				require.Greater(t, batched, 3*single, "far cars come mostly in 0x48s")
			} else {
				require.Zero(t, n1.megas, "no 0x48 with BatchFarPositions off")
			}
		})
	}
}

// TestExtFarPlayersShareAiMegaPacket checks that with AI traffic a
// player's far cars (BatchFarPositions) ride in the same 0x48s as its AI
// cars rather than in 0x48s of their own.
func TestExtFarPlayersShareAiMegaPacket(t *testing.T) {
	content := t.TempDir()
	const radius = 150.0
	circleSpline(t, filepath.Join(content, "content", "tracks", "ext_track", "ai"), radius)
	require.NoError(t, os.MkdirAll(filepath.Join(content, "content", "cars", "ks_mazda_mx5_cup"), 0o755))
	extra := `EnableAi: true
AiParams:
  MinSpawnDistancePoints: 30
  MaxSpawnDistancePoints: 50
  SpawnSafetyDistanceToPlayerMeters: 20
  MinAiSafetyDistanceMeters: 10
  MaxAiSafetyDistanceMeters: 15
  AiPerPlayerTargetCount: 1
  AiBehaviorUpdateIntervalHz: 5
  PlayerAfkTimeoutSeconds: 60
`
	p := startExtServer(t, carEntries("none", "none", "auto"), extra, content, nil)
	alice, _ := joinExt(t, p, 1, "Alice", "")
	carol, _ := joinExt(t, p, 2, "Carol", "")
	require.Equal(t, uint8(0), alice.id)
	require.Equal(t, uint8(1), carol.id)

	// Alice drives the circle; Carol sits 2 km away, outside the bubble.
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		start := time.Now()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(55 * time.Millisecond):
			}
			s := time.Since(start).Seconds() * 10 / radius
			pos := wire.Vec3{X: float32(radius * math.Cos(s)), Z: float32(radius * math.Sin(s))}
			vel := wire.Vec3{X: float32(-10 * math.Sin(s)), Z: float32(10 * math.Cos(s))}
			ts := uint32(100 + i*55)
			alice.sendPosition(uint8(i), ts, pos, vel, 0)
			carol.sendPosition(uint8(i), ts, wire.Vec3{X: 2000}, wire.Vec3{}, 0)
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	alice.waitFor(t, true, 8*time.Second, "a 0x48 with the AI car and Carol", func(p []byte) bool {
		ids := map[byte]bool{}
		for _, car := range megaEntries(p) {
			ids[car[1]] = true
		}
		return ids[2] && ids[carol.id]
	})
	// From now on Carol only arrives inside the AI's 0x48s.
	carols := func() (single, batched int) {
		alice.mu.Lock()
		defer alice.mu.Unlock()
		for _, pkt := range alice.udpPkts {
			if len(pkt) >= 61 && pkt[0] == proto.PositionUpdate && pkt[1] == carol.id {
				single++
			}
			for _, car := range megaEntries(pkt) {
				if car[1] == carol.id {
					batched++
				}
			}
		}
		return single, batched
	}
	s0, b0 := carols()
	time.Sleep(2 * time.Second)
	s1, b1 := carols()
	require.Zero(t, s1-s0, "no single 0x46 for Carol")
	require.InDelta(t, 7, b1-b0, 3, "Carol every fifth update: about 3.6/s")
}

// megaEntries splits a 0x48 into its cars, each reshaped as the 0x46 it
// stands for: the id, the 57 bytes the two share, then PerformanceDelta and
// Gas as zero. Anything else, or a malformed 0x48, gives nil.
func megaEntries(p []byte) [][]byte {
	if len(p) < 8 || p[0] != proto.MegaPacket || len(p) != 8+57*int(p[7]) {
		return nil
	}
	var cars [][]byte
	for i := 0; i < int(p[7]); i++ {
		car := append([]byte{proto.PositionUpdate}, p[8+57*i:8+57*(i+1)]...)
		cars = append(cars, append(car, 0, 0, 0))
	}
	return cars
}

// chatFromServer matches a server chat line containing text.
func chatFromServer(text string) func([]byte) bool {
	return func(p []byte) bool {
		if len(p) < 3 || p[0] != proto.Chat || p[1] != 255 {
			return false
		}
		r := wire.NewReader(p[2:])
		return strings.Contains(r.WString8(), text)
	}
}

func steamID(n int) string { return strconv.Itoa(76561198000000000 + n) }

func TestExtUserGroupsAndCommands(t *testing.T) {
	rconPort := reserveProbePort(t)
	extra := fmt.Sprintf(`EnableAdminCommands: true
EnableRcon: true
RconPort: %d
UserGroups:
  admins: cfg/admins.txt
  whitelist: cfg/whitelist.txt
  banned: cfg/banned.txt
  mods: cfg/mods.txt
AdminUserGroup: admins
WhitelistUserGroup: whitelist
BlacklistUserGroup: banned
UserGroupCommandPermissions:
  - UserGroup: mods
    Commands: [whois]
IgnoreConfigurationErrors:
  UnsafeAdminWhitelist: true
`, rconPort)
	p := startExtServer(t, carEntries("none", "none", "none", "none"), extra, "", map[string]string{
		"admins.txt":    steamID(1) + "\n",
		"whitelist.txt": steamID(1) + "\n" + steamID(2) + "\n",
		"mods.txt":      steamID(2) + "\n",
	})

	// The whitelist keeps out anyone it does not list.
	_, resp := joinExt(t, p, 3, "Carol", "")
	require.Equal(t, byte(proto.AuthFailed), resp[0])
	require.Contains(t, wire.NewReader(resp[1:]).WString8(), "not whitelisted")

	alice, _ := joinExt(t, p, 1, "Alice", "")
	require.NotNil(t, alice)
	bob, _ := joinExt(t, p, 2, "Bob", "")
	require.NotNil(t, bob)
	for i := 0; i < 5; i++ {
		alice.sendPosition(uint8(i), uint32(100+i*56), wire.Vec3{X: 1}, wire.Vec3{}, 0)
		bob.sendPosition(uint8(i), uint32(100+i*56), wire.Vec3{X: 9}, wire.Vec3{}, 0)
		time.Sleep(30 * time.Millisecond)
	}

	// Alice is admin by the admin group, without /admin.
	sendChat(t, alice.tcp, "/whois Bob")
	alice.waitFor(t, false, 3*time.Second, "whois reply", chatFromServer("Profile: https://steamcommunity.com/profiles/"+steamID(2)))

	// Bob may run /whois through the mods group, but nothing else.
	sendChat(t, bob.tcp, "/whois Alice")
	bob.waitFor(t, false, 3*time.Second, "permitted whois", chatFromServer("Ping:"))
	sendChat(t, bob.tcp, "/pit Alice")
	bob.waitFor(t, false, 3*time.Second, "refusal", chatFromServer("You are not admin on this server"))
	sendChat(t, bob.tcp, "/ping")
	bob.waitFor(t, false, 3*time.Second, "pong", chatFromServer("Pong!"))

	// /ban with a reason: everyone is told, Bob is kicked and listed.
	sendChat(t, alice.tcp, "/ban Bob speeding")
	alice.waitFor(t, false, 3*time.Second, "ban broadcast", chatFromServer("Bob has been banned from the server for speeding."))
	bob.waitFor(t, false, 3*time.Second, "0x68", func(p []byte) bool {
		return len(p) == 3 && p[0] == proto.KickCar && p[1] == bob.id && p[2] == proto.KickVoteBlacklisted
	})
	banned, err := os.ReadFile(p.file(t, "banned.txt"))
	require.NoError(t, err)
	require.Contains(t, string(banned), steamID(2))
	_, resp = joinExt(t, p, 2, "Bob", "")
	require.Equal(t, byte(proto.Blacklisted), resp[0], "Bob stays banned")

	// /unban lifts it; a whitelist file edit is picked up within seconds.
	sendChat(t, alice.tcp, "/unban "+steamID(2))
	alice.waitFor(t, false, 3*time.Second, "unban", chatFromServer("was unbanned"))
	f, err := os.OpenFile(p.file(t, "whitelist.txt"), os.O_APPEND|os.O_WRONLY, 0o644)
	require.NoError(t, err)
	f.WriteString(steamID(3) + "\n")
	f.Close()
	deadline := time.Now().Add(12 * time.Second)
	for {
		c, resp := joinExt(t, p, 3, "Carol", "")
		if c != nil {
			break
		}
		require.True(t, time.Now().Before(deadline), "whitelist was never reloaded: % x", resp)
		time.Sleep(time.Second)
	}

	// /time needs no player, so RCON answers it as chat does.
	require.Regexp(t, `^It is currently \d+:\d\d\.$`, rconExec(t, rconPort, "adm", "time"))
}

// rconExec logs in to Source RCON and returns one command's reply.
func rconExec(t *testing.T, port int, password, cmd string) string {
	t.Helper()
	conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), 3*time.Second)
	require.NoError(t, err)
	defer conn.Close()
	require.NoError(t, conn.SetDeadline(time.Now().Add(5*time.Second)))
	send := func(id, kind int32, body string) {
		w := wire.NewWriter().Int32(int32(len(body) + 10)).Int32(id).Int32(kind)
		_, err := conn.Write(append(append(w.Bytes(), body...), 0, 0))
		require.NoError(t, err)
	}
	read := func() (id, kind int32, body string) {
		head := make([]byte, 4)
		_, err := io.ReadFull(conn, head)
		require.NoError(t, err)
		packet := make([]byte, wire.NewReader(head).Int32())
		_, err = io.ReadFull(conn, packet)
		require.NoError(t, err)
		r := wire.NewReader(packet)
		id, kind = r.Int32(), r.Int32()
		return id, kind, string(bytes.TrimRight(r.RemainingBytes(), "\x00"))
	}
	send(1, 3, password)
	id, kind, _ := read()
	if kind == 0 { // an empty response value may precede the auth reply
		id, _, _ = read()
	}
	require.Equal(t, int32(1), id, "rcon login")
	send(2, 2, cmd)
	_, _, reply := read()
	return reply
}

func isKick(id, code byte) func([]byte) bool {
	return func(p []byte) bool { return len(p) == 3 && p[0] == proto.KickCar && p[1] == id && p[2] == code }
}

func TestExtModeration(t *testing.T) {
	extra := `EnableAntiAfk: true
MaxAfkTimeMinutes: 1
MaxPing: 300
MaxPingSeconds: 2
PlayerChecksumTimeoutSeconds: 2
NetworkBubbleDistance: 100
`
	p := startExtServer(t, carEntries("none", "none", "none", "none"), extra, "", nil)

	// Near, far and mute: Alice and Bob 10 m apart, Carol 1 km away.
	alice, _ := joinExt(t, p, 1, "Alice", "")
	bob, _ := joinExt(t, p, 2, "Bob", "")
	carol, _ := joinExt(t, p, 3, "Carol", "")
	dave, _ := joinExtOpts(t, p, 4, "Dave", "", false)
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(50 * time.Millisecond):
			}
			ts := uint32(100 + i*50)
			alice.sendPosition(uint8(i), ts, wire.Vec3{X: 0}, wire.Vec3{}, 0)
			bob.sendPosition(uint8(i), ts, wire.Vec3{X: 10}, wire.Vec3{}, 0)
			carol.sendPosition(uint8(i), ts, wire.Vec3{X: 1000}, wire.Vec3{}, 0)
			dave.sendPosition(uint8(i), ts, wire.Vec3{X: 20}, wire.Vec3{}, 0)
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	// Dave never answers the checksum request.
	alice.waitFor(t, false, 5*time.Second, "checksum timeout kick", isKick(dave.id, proto.KickChecksumFailed))
	alice.waitFor(t, false, time.Second, "its broadcast", chatFromServer("Dave did not send the requested checksums."))

	// Nobody touches the controls: a minute's limit warns at once.
	alice.waitFor(t, false, 3*time.Second, "AFK warning", chatFromServer("You will be kicked in 1 minute for being AFK."))

	// The network bubble: Bob, 10 m away, reaches Alice at the full rate;
	// Carol, 1 km away, every fifth update (4/s at 20/s).
	time.Sleep(2 * time.Second)
	from := func(id byte) func([]byte) bool {
		return func(p []byte) bool { return len(p) >= 61 && p[0] == proto.PositionUpdate && p[1] == id }
	}
	b0, c0 := alice.count(true, from(bob.id)), alice.count(true, from(carol.id))
	time.Sleep(2 * time.Second)
	b, c := alice.count(true, from(bob.id))-b0, alice.count(true, from(carol.id))-c0
	require.Greater(t, b, 25, "Bob inside the bubble: about 20/s")
	require.InDelta(t, 8, c, 3, "Carol outside it: 4/s")

	// Bob's ping goes to 2 s; after more than two such pongs he is kicked.
	bob.lag.Store(2000)
	alice.waitFor(t, false, 6*time.Second, "high-ping kick", isKick(bob.id, proto.KickKicked))
	alice.waitFor(t, false, time.Second, "its broadcast", chatFromServer("Bob has been kicked from the server for high ping"))
}

func TestExtAiColoursAndDebug(t *testing.T) {
	content := t.TempDir()
	circleSpline(t, filepath.Join(content, "content", "tracks", "ext_track", "ai"), 150)
	require.NoError(t, os.MkdirAll(filepath.Join(content, "content", "cars", "ks_mazda_mx5_cup"), 0o755))
	extra := `EnableAi: true
AiParams:
  Debug: true
  MinSpawnDistancePoints: 30
  MaxSpawnDistancePoints: 50
  SpawnSafetyDistanceToPlayerMeters: 20
  MinAiSafetyDistanceMeters: 10
  MaxAiSafetyDistanceMeters: 15
  AiPerPlayerTargetCount: 2
  AiBehaviorUpdateIntervalHz: 5
  PlayerAfkTimeoutSeconds: 60
`
	// AI slots whose SKIN allows CSP colour changes (flag 0x10).
	packed := base64.RawStdEncoding.EncodeToString([]byte{0, 0x10, 0x10 ^ 0x17})
	entries := carEntries("none") + fmt.Sprintf(
		"[CAR_1]\nMODEL=ks_mazda_mx5_cup\nSKIN=00_official/%s\nAI=fixed\n\n[CAR_2]\nMODEL=ks_mazda_mx5_cup\nSKIN=00_official/%s\nAI=fixed\n", packed, packed)
	p := startExtServer(t, entries, extra, content, nil)
	alice, _ := joinExt(t, p, 1, "Alice", extFeatures)
	require.NotNil(t, alice)

	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		start := time.Now()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(55 * time.Millisecond):
			}
			s := time.Since(start).Seconds() * 10 / 150
			pos := wire.Vec3{X: float32(150 * math.Cos(s)), Z: float32(150 * math.Sin(s))}
			vel := wire.Vec3{X: float32(-10 * math.Sin(s)), Z: float32(10 * math.Cos(s))}
			alice.sendPosition(uint8(i), uint32(100+i*55), pos, vel, uint8(i%2))
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	paint := alice.waitFor(t, false, 8*time.Second, "CarColorChange from an AI car", func(p []byte) bool {
		return len(p) == 9 && p[0] == proto.Extended && p[1] == proto.ExtTCPClientMessage &&
			(p[2] == 1 || p[2] == 2) && binary.LittleEndian.Uint16(p[3:5]) == 15000
	})
	require.Equal(t, byte(255), paint[8], "opaque")
	dbg := alice.waitFor(t, false, 3*time.Second, "ai_debug event", func(p []byte) bool {
		return len(p) >= 9 && p[0] == proto.Extended && binary.LittleEndian.Uint16(p[3:5]) == 60000 &&
			binary.LittleEndian.Uint32(p[5:9]) == 0xB8DC08B3
	})
	require.Len(t, dbg, 9+20*6)
	ids := dbg[9+40+40 : 9+40+60]
	require.Contains(t, []byte{1, 2}, ids[0], "the first row names an AI car")
}

// laneFromPackage reads the points of one lane (a version 7 or -1 .ai file) from a
// fast_lane.aip.
func laneFromPackage(t *testing.T, aip, name string) []wire.Vec3 {
	t.Helper()
	zr, err := zip.OpenReader(aip)
	require.NoError(t, err)
	defer zr.Close()
	for _, f := range zr.File {
		if filepath.Base(f.Name) != name {
			continue
		}
		rc, err := f.Open()
		require.NoError(t, err)
		data, err := io.ReadAll(rc)
		rc.Close()
		require.NoError(t, err)
		r := wire.NewReader(data)
		version := int32(r.UInt32())
		n := int(r.UInt32())
		if version == 7 {
			r.UInt32()
			r.UInt32()
		}
		require.Contains(t, []int32{7, -1}, version)
		points := make([]wire.Vec3, n)
		for i := range points {
			// Version 7: length and id follow; -1: radius and camber.
			points[i] = r.Vec3()
			r.UInt32()
			r.UInt32()
		}
		return points
	}
	t.Fatalf("%s has no %s", aip, name)
	return nil
}

// TestExtAiOnRealSpline runs traffic on a real AI package:
// AI_SPLINE_AIP=<fast_lane.aip> AI_SPLINE_TRACK=<track id>.
func TestExtAiOnRealSpline(t *testing.T) {
	aip, track := os.Getenv("AI_SPLINE_AIP"), os.Getenv("AI_SPLINE_TRACK")
	if aip == "" || track == "" {
		t.Skip("set AI_SPLINE_AIP and AI_SPLINE_TRACK")
	}
	content := t.TempDir()
	aiDir := filepath.Join(content, "content", "tracks", track, "ai")
	require.NoError(t, os.MkdirAll(aiDir, 0o755))
	body, err := os.ReadFile(aip)
	require.NoError(t, err)
	require.NoError(t, os.WriteFile(filepath.Join(aiDir, "fast_lane.aip"), body, 0o644))
	require.NoError(t, os.MkdirAll(filepath.Join(content, "content", "cars", "ks_mazda_mx5_cup"), 0o755))
	lane := laneFromPackage(t, aip, "fast_lane1.ai")

	extra := `EnableAi: true
AiParams:
  AiPerPlayerTargetCount: 8
  AiBehaviorUpdateIntervalHz: 5
  PlayerAfkTimeoutSeconds: 60
`
	modes := []string{"none"}
	for i := 0; i < 8; i++ {
		modes = append(modes, "fixed")
	}
	p := extServerOn(t, track, carEntries(modes...), extra, content, nil)
	alice, _ := joinExt(t, p, 1, "Alice", "")
	require.NotNil(t, alice)

	// Drive along the lane at about 20 m/s, a point at a time.
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		at, travelled := 1000, 0.0
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(50 * time.Millisecond):
			}
			travelled += 1.0 // 20 m/s at 20 Hz
			for travelled > 0 && at+1 < len(lane) {
				a, b := lane[at], lane[at+1]
				seg := math.Hypot(float64(b.X-a.X), float64(b.Z-a.Z))
				if travelled < seg {
					break
				}
				travelled -= seg
				at++
			}
			a, b := lane[at], lane[at+1]
			seg := math.Hypot(float64(b.X-a.X), float64(b.Z-a.Z))
			k := float32(20 / seg)
			alice.sendPosition(uint8(i), uint32(100+i*50), a, wire.Vec3{X: (b.X - a.X) * k, Y: (b.Y - a.Y) * k, Z: (b.Z - a.Z) * k}, uint8(i%2))
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	alice.waitFor(t, true, 15*time.Second, "an AI car", func(p []byte) bool {
		return len(megaEntries(p)) > 0
	})
	time.Sleep(10 * time.Second)

	// Every AI car seen is on (near) the road network and moving.
	positions := map[byte][]wire.Vec3{}
	stamps := map[byte][]uint32{}
	alice.mu.Lock()
	for _, mp := range alice.udpPkts {
		for _, p := range megaEntries(mp) {
			// After id, sender, sequence, timestamp and ping.
			r := wire.NewReader(p[9:])
			positions[p[1]] = append(positions[p[1]], r.Vec3())
			stamps[p[1]] = append(stamps[p[1]], binary.LittleEndian.Uint32(p[3:7]))
		}
	}
	alice.mu.Unlock()
	require.NotEmpty(t, positions)
	for id, ps := range positions {
		first, last := ps[0], ps[len(ps)-1]
		moved := math.Hypot(float64(last.X-first.X), float64(last.Z-first.Z))
		t.Logf("AI car %d: %d updates, moved %.0f m, now at (%.0f, %.0f, %.0f)", id, len(ps), moved, last.X, last.Y, last.Z)
	}
	moving := 0
	for id, ps := range positions {
		for i := 1; i < len(ps); i++ {
			step := math.Hypot(float64(ps[i].X-ps[i-1].X), float64(ps[i].Z-ps[i-1].Z))
			dt := float64(stamps[id][i]-stamps[id][i-1]) / 1000
			if step > 100 || dt <= 0 {
				continue // respawned elsewhere, or a repeat
			}
			if step/dt > 50 {
				t.Logf("AI car %d: step %.2f m in %.3f s at update %d", id, step, dt, i)
			}
		}
		first, last := ps[0], ps[len(ps)-1]
		if math.Hypot(float64(last.X-first.X), float64(last.Z-first.Z)) > 20 {
			moving++
		}
		// Within the spawner's reach of the player, on the same map.
		near := false
		for _, q := range lane {
			if math.Hypot(float64(q.X-last.X), float64(q.Z-last.Z)) < 600 {
				near = true
				break
			}
		}
		require.True(t, near, "AI car %d is near the lane the player drives", id)
	}
	require.Positive(t, moving, "some AI cars drive")
}

func TestExtPlugins(t *testing.T) {
	content := t.TempDir()
	circleSpline(t, filepath.Join(content, "content", "tracks", "ext_track", "ai"), 150)
	require.NoError(t, os.MkdirAll(filepath.Join(content, "content", "cars", "ks_mazda_mx5_cup"), 0o755))
	extra := `EnableWeatherFx: true
TrackTimezone: Asia/Tokyo
TrackLatitude: 35.6
TrackLongitude: 139.7
EnableAi: true
EnablePlugins:
  - WordFilterPlugin
  - VotingWeatherPlugin
  - TimeDilationPlugin
  - AutoModerationPlugin
  - RaceChallengePlugin
  - ReportPlugin
---
!WordFilterConfiguration
ProhibitedUsernamePatterns: ['badname']
ProhibitedChatPatterns: ['darn']
BannableChatPatterns: ['banme']
---
!VotingWeatherConfiguration
VotingIntervalMinutes: 1
VotingDurationSeconds: 3
NumChoices: 2
---
!TimeDilationConfiguration
LookupTable:
  - { SunAngle: -90, TimeMult: 60 }
  - { SunAngle: 90, TimeMult: 60 }
---
!AutoModerationConfiguration
WrongWayKick:
  Enabled: true
  DurationSeconds: 2
---
!ReportConfiguration
ClipDurationSeconds: 30
ServerAddress: 127.0.0.1
`
	p := startExtServer(t, carEntries("none", "none", "none", "none", "auto"), extra, content, nil)

	// WordFilterPlugin refuses a name.
	_, resp := joinExt(t, p, 9, "xBadNamex", extFeatures)
	require.Equal(t, byte(proto.AuthFailed), resp[0])
	require.Contains(t, wire.NewReader(resp[1:]).WString8(), "Prohibited username")

	alice, _ := joinExt(t, p, 1, "Alice", extFeatures)
	bob, _ := joinExt(t, p, 2, "Bob", extFeatures)
	carol, _ := joinExt(t, p, 3, "Carol", extFeatures)
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		start := time.Now()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(50 * time.Millisecond):
			}
			ts := uint32(100 + i*50)
			alice.sendPosition(uint8(i), ts, wire.Vec3{X: 5000}, wire.Vec3{}, uint8(i%2))
			bob.sendPosition(uint8(i), ts, wire.Vec3{X: 5010}, wire.Vec3{}, uint8(i%2))
			// Carol drives the circle clockwise: against its lane, at 15 m/s.
			s := -time.Since(start).Seconds() * 15 / 150
			pos := wire.Vec3{X: float32(150 * math.Cos(s)), Z: float32(150 * math.Sin(s))}
			vel := wire.Vec3{X: float32(15 * math.Sin(s)), Z: float32(-15 * math.Cos(s))}
			carol.sendPosition(uint8(i), ts, pos, vel, uint8(i%2))
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	// AutoModerationPlugin: warn, flag on screen, then kick.
	carol.waitFor(t, false, 5*time.Second, "wrong-way warning", chatFromServer("You are driving the wrong way!"))
	carol.waitFor(t, false, time.Second, "wrong-way flag", func(p []byte) bool {
		return len(p) == 10 && p[0] == proto.Extended && binary.LittleEndian.Uint32(p[5:9]) == 0x79096D99 && p[9]&4 != 0
	})
	alice.waitFor(t, false, 5*time.Second, "wrong-way kick", chatFromServer("Carol has been kicked from the server for driving the wrong way."))

	// WordFilterPlugin drops a line, and bans for another.
	sendChat(t, bob.tcp, "oh darn")
	sendChat(t, bob.tcp, "hello there")
	alice.waitFor(t, false, 3*time.Second, "an allowed line", func(p []byte) bool {
		return len(p) > 2 && p[0] == proto.Chat && p[1] == bob.id && strings.Contains(wire.NewReader(p[2:]).WString8(), "hello there")
	})
	require.Zero(t, alice.count(false, func(p []byte) bool {
		return len(p) > 2 && p[0] == proto.Chat && p[1] == bob.id && strings.Contains(wire.NewReader(p[2:]).WString8(), "darn")
	}), "the filtered line is dropped")

	// RaceChallengePlugin: /race and /accept.
	sendChat(t, alice.tcp, "/race Bob")
	alice.waitFor(t, false, 3*time.Second, "challenge sent", chatFromServer("You have challenged Bob to a race."))
	bob.waitFor(t, false, 3*time.Second, "challenge received", chatFromServer("Alice has challenged you to a race."))
	sendChat(t, bob.tcp, "/accept")
	bob.waitFor(t, false, 3*time.Second, "line-up", chatFromServer("You have 15 seconds to line up."))

	// VotingWeatherPlugin: the first vote opened at start, before anyone
	// joined; the next comes a minute later. Vote, and the winner fades in.
	vote := alice.waitFor(t, false, 70*time.Second, "vote options", chatFromServer(" /w 0 - "))
	choice := strings.TrimPrefix(wire.NewReader(vote[2:]).WString8(), " /w 0 - ")
	sendChat(t, alice.tcp, "/w 0")
	alice.waitFor(t, false, 3*time.Second, "vote counted", chatFromServer("Your vote for "+choice+" has been counted."))
	alice.waitFor(t, false, 5*time.Second, "vote result", chatFromServer("Weather vote ended. Next weather: "+choice))

	// TimeDilationPlugin: time runs at 60x.
	stamp := func() (uint64, byte) {
		var ts uint64
		var upcoming byte
		alice.mu.Lock()
		for _, p := range alice.udpPkts {
			if len(p) >= 12 && p[0] == proto.Extended && p[1] == 0x01 {
				ts, upcoming = binary.LittleEndian.Uint64(p[2:10]), p[11]
			}
		}
		alice.mu.Unlock()
		return ts, upcoming
	}
	t0, _ := stamp()
	time.Sleep(3 * time.Second)
	t1, upcoming := stamp()
	require.Greater(t, t1-t0, uint64(100), "about 60 game seconds a second")
	require.Equal(t, choice, weatherName(upcoming), "the voted weather is coming")

	// ReportPlugin: a clip upload, then /report.
	welcome := alice.waitFor(t, false, time.Second, "0x51", func(p []byte) bool { return p[0] == proto.WelcomeMessage })
	ini := extraOptions(t, welcome)
	i := strings.Index(ini, "/report?key=")
	require.GreaterOrEqual(t, i, 0, "REPLAY_CLIPS in the extra options: %s", ini)
	require.Contains(t, ini, fmt.Sprintf("UPLOAD_URL = 'http://127.0.0.1:%d/report?key=", p.http), "ServerAddress is the upload host")
	key := ini[i+len("/report?key=") : i+len("/report?key=")+32]
	req, err := http.NewRequest("POST", fmt.Sprintf("http://127.0.0.1:%d/report?key=%s", p.http, key), strings.NewReader("PK fake clip"))
	require.NoError(t, err)
	req.Header.Set("X-Car-Index", strconv.Itoa(int(alice.id)))
	res, err := http.DefaultClient.Do(req)
	require.NoError(t, err)
	res.Body.Close()
	require.Equal(t, 200, res.StatusCode)
	alice.waitFor(t, false, 3*time.Second, "clip received", chatFromServer("Replay received."))
	sendChat(t, alice.tcp, "/report Bob rammed me")
	alice.waitFor(t, false, 3*time.Second, "report", chatFromServer("Your report has been submitted."))
	reports, _ := filepath.Glob(filepath.Join(filepath.Dir(p.cfgDir), "reports", "*.report.txt"))
	require.Len(t, reports, 1)
	body, _ := os.ReadFile(reports[0])
	require.Contains(t, string(body), "Reason: Bob rammed me")

	// Bannable chat ends with a ban.
	sendChat(t, bob.tcp, "please banme")
	alice.waitFor(t, false, 3*time.Second, "the ban", chatFromServer("Bob has been banned from the server for prohibited language."))
}

// weatherName is a WeatherFX type id's name, as the server names them.
func weatherName(id byte) string {
	names := []string{"LightThunderstorm", "Thunderstorm", "HeavyThunderstorm", "LightDrizzle", "Drizzle",
		"HeavyDrizzle", "LightRain", "Rain", "HeavyRain", "LightSnow", "Snow", "HeavySnow", "LightSleet", "Sleet",
		"HeavySleet", "Clear", "FewClouds", "ScatteredClouds", "BrokenClouds", "OvercastClouds", "Fog", "Mist",
		"Smoke", "Haze", "Sand", "Dust", "Squalls", "Tornado", "Hurricane", "Cold", "Hot", "Windy", "Hail"}
	if int(id) < len(names) {
		return names[id]
	}
	return "?"
}

func TestExtLiveConfiguration(t *testing.T) {
	// Plain 0x46 relays, so the forced lights flag can be read off them.
	p := startExtServer(t, carEntries("none", "none", "none"), "EnableLiveConfiguration: true\nEnableCustomUpdate: false\n", "", nil)
	alice, _ := joinExt(t, p, 1, "Alice", extFeatures)
	bob, _ := joinExt(t, p, 2, "Bob", extFeatures)
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			case <-time.After(50 * time.Millisecond):
			}
			ts := uint32(100 + i*50)
			alice.sendPosition(uint8(i), ts, wire.Vec3{X: 1}, wire.Vec3{}, 0)
			bob.sendPosition(uint8(i), ts, wire.Vec3{X: 9}, wire.Vec3{}, 0)
		}
	}()
	defer func() { close(stop); wg.Wait() }()

	// The window script is in the extra options.
	welcome := alice.waitFor(t, false, 3*time.Second, "0x51", func(p []byte) bool { return p[0] == proto.WelcomeMessage })
	require.Contains(t, extraOptions(t, welcome), "acserver_admin.lua")

	// /set needs admin; then it changes a setting, which applies at once.
	sendChat(t, alice.tcp, "/set Extra.ForceLights true")
	alice.waitFor(t, false, 3*time.Second, "refusal", chatFromServer("You are not admin on this server"))
	sendChat(t, alice.tcp, "/admin adm")
	alice.waitFor(t, false, 3*time.Second, "admin", chatFromServer("You are now Admin"))
	sendChat(t, alice.tcp, "/set Extra.ForceLights true")
	alice.waitFor(t, false, 3*time.Second, "set", chatFromServer("Property Extra.ForceLights set to true"))
	alice.waitFor(t, true, 3*time.Second, "Bob's lights forced on", func(p []byte) bool {
		return len(p) >= 61 && p[0] == proto.PositionUpdate && p[1] == bob.id && binary.LittleEndian.Uint32(p[54:58])&0x20 != 0
	})
	sendChat(t, alice.tcp, "/set Extra.Nope 1")
	alice.waitFor(t, false, 3*time.Second, "unknown key", chatFromServer("Could not find property Nope"))
	sendChat(t, alice.tcp, "/set Extra.MaxPing lots")
	alice.waitFor(t, false, 3*time.Second, "bad value", chatFromServer("MaxPing is a whole number"))

	// The HTTP API: the key comes from AS_ApiKey.
	ask := wire.NewWriterID(proto.Extended)
	ask.UInt8(proto.ExtTCPClientMessage)
	ask.UInt16(60000)
	ask.UInt32(luaApiKey)
	ask.Raw(make([]byte, 32))
	alice.tcp.Write(wire.FrameTCP(ask.Bytes()))
	reply := alice.waitFor(t, false, 3*time.Second, "AS_ApiKey reply", func(p []byte) bool {
		return len(p) == 9+32 && p[0] == proto.Extended && binary.LittleEndian.Uint32(p[5:9]) == luaApiKey
	})
	key := string(reply[9:])
	call := func(method, query, apiKey string) (int, string) {
		req, err := http.NewRequest(method, fmt.Sprintf("http://127.0.0.1:%d/api/configuration%s", p.http, query), nil)
		require.NoError(t, err)
		req.Header.Set("X-Car-Id", strconv.Itoa(int(alice.id)))
		req.Header.Set("X-Api-Key", apiKey)
		res, err := http.DefaultClient.Do(req)
		require.NoError(t, err)
		defer res.Body.Close()
		body, _ := io.ReadAll(res.Body)
		return res.StatusCode, string(body)
	}
	status, _ := call("GET", "", "wrong")
	require.Equal(t, 401, status)
	status, tree := call("GET", "", key)
	require.Equal(t, 200, status)
	require.Contains(t, tree, `Name="ForceLights"`)
	require.Contains(t, tree, `Name="TimeOfDayMultiplier"`)
	require.Contains(t, tree, `ReadOnly=true`, "Name and Track are shown read-only")
	status, body := call("POST", "?key=Root.Extra.MaxPing&value=250", key)
	require.Equal(t, 200, status)
	require.Equal(t, `{Status="OK"}`, body)
	_, tree = call("GET", "", key)
	require.Contains(t, tree, `Name="MaxPing",ReadOnly=false,Type="System.Int32",Value=250`)
	_, body = call("POST", "?key=Root.Server.Name&value=x", key)
	require.Contains(t, body, `Status="Error"`)
}
