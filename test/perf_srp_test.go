package integration

// TestPerformanceSRP is a freeroam traffic server in the style of the Shutoko
// Revival Project: a long highway loop, a big grid of players driving it in
// convoys, and AI traffic around them. It runs our server (serverBinary),
// the original acServer (players only: it has no AI traffic) when a
// reference install is found and, with ASSETTOSERVER_DIR, AssettoServer on
// the same load, each as a separate process, and measures only the server.
// Opt-in:
//
//	cd test && RUN_PERF_SRP=1 ASSETTOSERVER_DIR=/path/to/assettoserver go test . \
//	  -run '^TestPerformanceSRP$' -v -count=1 -timeout=30m
//
// SRP_PLAYERS, SRP_AI_SLOTS, SRP_MAX_AI and SRP_DURATION change the load;
// PERF_TARGETS (vanilla, ours, assettoserver) picks the servers and
// PERF_LOG_DIR keeps their logs.

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math"
	"math/rand"
	"net"
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

const (
	// srpLoopRadius gives a 16 km loop, about the length of Shutoko's C1,
	// Wangan and Yokohane routes driven end to end.
	srpLoopRadius = 2550.0
	srpTrack      = "srp_sim"
	srpModel      = "ks_mazda_mx5_cup"
	// srpJoinWindow spreads the joins; the steady window starts after it
	// and srpSettle more, once the AI has spawned.
	srpJoinWindow = 30 * time.Second
	srpSettle     = 15 * time.Second
)

// srpMetrics is one target's result.
type srpMetrics struct {
	Target        string           `json:"target"`
	Players       int              `json:"players"`
	AISlots       int              `json:"ai_slots"`
	WallSeconds   float64          `json:"wall_seconds"`
	CPUSeconds    float64          `json:"cpu_seconds"`
	CPUPercent    float64          `json:"cpu_percent"`
	SteadySeconds float64          `json:"steady_seconds"`
	SteadyCPU     float64          `json:"steady_cpu_percent"`
	PeakRSSMB     float64          `json:"peak_rss_mb"`
	Joined        int              `json:"joined"`
	Dropped       int64            `json:"dropped"`
	PositionsSent int64            `json:"positions_sent"`
	UDPPackets    int64            `json:"udp_packets_seen"`
	UDPBytes      int64            `json:"udp_bytes_seen"`
	PlayerEntries int64            `json:"player_entries_seen"`
	AIEntries     int64            `json:"ai_entries_seen"`
	AISlotsSeen   int              `json:"ai_slots_seen"`
	SteadyUDP     int64            `json:"steady_udp_packets"`
	SteadyEntries int64            `json:"steady_entries"`
	TCPPackets    int64            `json:"tcp_packets_seen"`
	UDPByID       map[string]int64 `json:"udp_by_id"`
	Bandwidth
}

func srpEnvInt(name string, def int) int {
	if n, err := strconv.Atoi(os.Getenv(name)); err == nil && n > 0 {
		return n
	}
	return def
}

func TestPerformanceSRP(t *testing.T) {
	if os.Getenv("RUN_PERF_SRP") != "1" {
		t.Skip("set RUN_PERF_SRP=1 to run the freeroam traffic benchmark")
	}
	players := srpEnvInt("SRP_PLAYERS", 120)
	aiSlots := srpEnvInt("SRP_AI_SLOTS", 60)
	maxAI := srpEnvInt("SRP_MAX_AI", 300)
	require.LessOrEqual(t, players+aiSlots, 255, "session ids are one byte")
	duration := 4 * time.Minute
	if d, err := time.ParseDuration(os.Getenv("SRP_DURATION")); err == nil && d > srpJoinWindow+srpSettle {
		duration = d
	}
	wanted := func(label string) bool {
		list := os.Getenv("PERF_TARGETS")
		return list == "" || strings.Contains(","+list+",", ","+label+",")
	}

	// The same traffic settings for both, under AssettoServer's names.
	extra := fmt.Sprintf(`EnableAi: true
AiParams:
  MaxAiTargetCount: %d
  AiPerPlayerTargetCount: 10
  PlayerAfkTimeoutSeconds: 600
  MaxSpeedKph: 90
`, maxAI)

	type target struct {
		label   string
		bin     string
		install string
		args    func(root, cfgDir string) []string
		extra   string
	}
	var targets []target
	// vanilla has no AI traffic: it serves the players alone.
	if ref := referenceBinary(); ref != "" && wanted("vanilla") {
		targets = append(targets, target{label: "vanilla", bin: ref, install: referenceInstall(),
			args: func(string, string) []string { return referenceArgs() }})
	}
	if wanted("ours") {
		targets = append(targets, target{label: "ours", bin: serverBinary(t), extra: extra,
			args: func(root, cfgDir string) []string { return []string{"--cfg", cfgDir, "--content", root} }})
	}
	if dir := os.Getenv("ASSETTOSERVER_DIR"); dir != "" && wanted("assettoserver") {
		abs, err := filepath.Abs(dir)
		require.NoError(t, err)
		targets = append(targets, target{label: "assettoserver", bin: filepath.Join(abs, "AssettoServer"), install: abs,
			args: func(string, string) []string { return nil },
			extra: `IgnoreConfigurationErrors:
  MissingCarChecksums: true
  MissingTrackParams: true
` + extra})
	}
	if len(targets) == 0 {
		t.Skip("no target selected by PERF_TARGETS")
	}

	for _, tg := range targets {
		t.Run(tg.label, func(t *testing.T) {
			root := t.TempDir()
			p := freePorts(t)
			cfgDir := writeSRPConfig(t, root, p, players, aiSlots)
			writeFile(t, filepath.Join(cfgDir, "extra_cfg.yml"), tg.extra)
			circleSpline(t, filepath.Join(root, "content", "tracks", srpTrack, "ai"), srpLoopRadius)
			require.NoError(t, os.MkdirAll(filepath.Join(root, "content", "cars", srpModel), 0o755))
			cwd := root
			if tg.install != "" {
				linkInstall(t, tg.install, root)
			}
			proc := startPerfProcess(t, tg.label, tg.bin, tg.args(root, cfgDir), cwd, p)
			defer proc.stop(t)
			m := runSRP(t, tg.label, p, proc, players, aiSlots, duration)
			m.AISlots = aiSlots
			b, err := json.Marshal(m)
			require.NoError(t, err)
			t.Log(string(b))
		})
	}
}

// writeSRPConfig writes a freeroam config: one long practice session,
// players' slots first, then the AI=fixed traffic slots.
func writeSRPConfig(t *testing.T, root string, p portSet, players, aiSlots int) string {
	t.Helper()
	cfgDir := filepath.Join(root, "cfg")
	require.NoError(t, os.MkdirAll(cfgDir, 0o755))
	writeFile(t, filepath.Join(cfgDir, "server_cfg.ini"), fmt.Sprintf(`[SERVER]
NAME=srp simulation
CARS=%s
TRACK=%s
CONFIG_TRACK=
SUN_ANGLE=16
PASSWORD=
ADMIN_PASSWORD=perf-admin
UDP_PORT=%d
TCP_PORT=%d
HTTP_PORT=%d
MAX_CLIENTS=%d
CLIENT_SEND_INTERVAL_HZ=18
SLEEP_TIME=1
REGISTER_TO_LOBBY=0
PICKUP_MODE_ENABLED=1
LOOP_MODE=1
VOTING_QUORUM=75
VOTE_DURATION=20
BLACKLIST_MODE=1
MAX_CONTACTS_PER_KM=-1
ALLOWED_TYRES_OUT=-1

[PRACTICE]
NAME=Freeroam
TIME=600
IS_OPEN=1

[WEATHER_0]
GRAPHICS=3_clear
BASE_TEMPERATURE_AMBIENT=18
BASE_TEMPERATURE_ROAD=6
VARIATION_AMBIENT=0
VARIATION_ROAD=0
WIND_BASE_SPEED_MIN=0
WIND_BASE_SPEED_MAX=0
WIND_BASE_DIRECTION=0
WIND_VARIATION_DIRECTION=0
`, srpModel, srpTrack, p.game, p.game, p.http, players+aiSlots))
	var entries strings.Builder
	for i := 0; i < players+aiSlots; i++ {
		ai := "none"
		if i >= players {
			ai = "fixed"
		}
		fmt.Fprintf(&entries, "[CAR_%d]\nMODEL=%s\nSKIN=00_official\nSPECTATOR_MODE=0\nDRIVERNAME=\nTEAM=\nGUID=\nBALLAST=0\nRESTRICTOR=0\nAI=%s\n\n", i, srpModel, ai)
	}
	writeFile(t, filepath.Join(cfgDir, "entry_list.ini"), entries.String())
	return cfgDir
}

// srpDriver is where one player drives: arc length along the loop at the
// start, and speed in m/s.
type srpDriver struct {
	start, speed float64
	joinAt       time.Duration
}

// srpDrivers puts the players in convoys of 1 to 6, a few car lengths apart,
// with the convoys spread evenly round the loop and each at its own speed
// (80 to 120 km/h). Seeded, so both targets get the same drive.
func srpDrivers(players int) []srpDriver {
	rng := rand.New(rand.NewSource(7))
	var sizes []int
	for left := players; left > 0; {
		n := min(left, 1+rng.Intn(6))
		sizes = append(sizes, n)
		left -= n
	}
	loop := 2 * math.Pi * srpLoopRadius
	var drivers []srpDriver
	for g, n := range sizes {
		lead := loop * float64(g) / float64(len(sizes))
		speed := (80 + 40*rng.Float64()) / 3.6
		for k := 0; k < n; k++ {
			drivers = append(drivers, srpDriver{
				start:  lead - float64(k)*(15+15*rng.Float64()),
				speed:  speed,
				joinAt: randDur(rng, 0, srpJoinWindow),
			})
		}
	}
	return drivers
}

// at returns the driver's position and velocity t into the run.
func (d srpDriver) at(t time.Duration) (pos, vel wire.Vec3) {
	a := (d.start + d.speed*t.Seconds()) / srpLoopRadius
	pos = wire.Vec3{X: float32(srpLoopRadius * math.Cos(a)), Z: float32(srpLoopRadius * math.Sin(a))}
	vel = wire.Vec3{X: float32(-d.speed * math.Sin(a)), Z: float32(d.speed * math.Cos(a))}
	return pos, vel
}

func runSRP(t *testing.T, label string, p portSet, proc *perfProcess, players, aiSlots int, duration time.Duration) srpMetrics {
	t.Helper()
	drivers := srpDrivers(players)
	var joined, dropped, positions, tcpPkts, udpPkts, udpBytes, playerEntries, aiEntries atomic.Int64
	var udpByID [256]atomic.Int64
	var aiSeen [256]atomic.Bool
	meter := newBandwidthMeter()
	start := time.Now()
	end := start.Add(duration)
	steadyFrom := srpJoinWindow + srpSettle
	stop := make(chan struct{})
	var wg sync.WaitGroup
	errs := make([]error, players)

	// countEntries tallies the cars in one position datagram by sender.
	countEntries := func(pkt []byte) int64 {
		var ids []byte
		switch {
		case len(pkt) >= 2 && pkt[0] == proto.PositionUpdate:
			ids = pkt[1:2]
		case len(pkt) >= 8 && pkt[0] == proto.MegaPacket:
			for i := 0; i < int(pkt[7]) && 8+57*(i+1) <= len(pkt); i++ {
				ids = append(ids, pkt[8+57*i])
			}
		}
		for _, id := range ids {
			if int(id) >= players {
				aiEntries.Add(1)
				aiSeen[id].Store(true)
			} else {
				playerEntries.Add(1)
			}
		}
		return int64(len(ids))
	}

	for i := range players {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			d := drivers[i]
			time.Sleep(time.Until(start.Add(d.joinAt)))
			c, err := joinSRPClient(t, p, i)
			if err != nil {
				errs[i] = err
				return
			}
			joined.Add(1)
			defer c.tcp.Close()
			defer c.udp.Close()
			go func() {
				for {
					pkt, err := wire.ReadTCPPacket(c.br)
					if err != nil {
						select {
						case <-stop:
						default:
							dropped.Add(1)
						}
						return
					}
					tcpPkts.Add(1)
					meter.recvTCP(len(pkt))
				}
			}()
			go func() {
				buf := make([]byte, 2048)
				for {
					n, err := c.udp.Read(buf)
					if err != nil {
						return
					}
					udpPkts.Add(1)
					udpBytes.Add(int64(n))
					meter.recvUDP(n)
					if n == 0 {
						continue
					}
					udpByID[buf[0]].Add(1)
					countEntries(buf[:n])
					if n >= 5 && buf[0] == proto.PingUpdate {
						pong := wire.NewWriterID(proto.PingPong)
						pong.Int32(int32(binary.LittleEndian.Uint32(buf[1:5])))
						pong.Int32(int32(time.Since(start).Milliseconds()))
						_, _ = c.udp.Write(pong.Bytes())
						meter.sendUDP(len(pong.Bytes()))
					}
				}
			}()
			tick := time.NewTicker(time.Second / 18)
			defer tick.Stop()
			seq := uint8(2)
			ts := uint32(56)
			for {
				select {
				case <-stop:
					return
				case <-tick.C:
				}
				pos, vel := d.at(time.Since(start))
				meter.sendUDP(writeSRPPosition(c.udp, seq, ts, pos, vel))
				positions.Add(1)
				seq++
				ts += 56
			}
		}(i)
	}

	// The steady window: CPU and traffic once everyone is in and the AI has
	// spawned.
	time.Sleep(time.Until(start.Add(steadyFrom)))
	u0 := readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	udp0 := udpPkts.Load()
	e0 := playerEntries.Load() + aiEntries.Load()
	steadyStart := time.Now()
	time.Sleep(time.Until(end))
	u1 := readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	steady := time.Since(steadyStart).Seconds()
	udp1 := udpPkts.Load()
	e1 := playerEntries.Load() + aiEntries.Load()
	wall := time.Since(start).Seconds()
	close(stop)
	wg.Wait()
	bandwidth := meter.result()

	for i, err := range errs {
		require.NoErrorf(t, err, "%s player %d", label, i)
	}
	ai := 0
	for id := range aiSeen {
		if aiSeen[id].Load() {
			ai++
		}
	}
	byID := map[string]int64{}
	for id := range udpByID {
		if n := udpByID[id].Load(); n > 0 {
			byID[fmt.Sprintf("%02x", id)] = n
		}
	}
	m := srpMetrics{
		Target: label, Players: players, WallSeconds: wall,
		CPUSeconds: u1.CPUSeconds, CPUPercent: 100 * u1.CPUSeconds / wall,
		SteadySeconds: steady, SteadyCPU: 100 * (u1.CPUSeconds - u0.CPUSeconds) / steady,
		PeakRSSMB: u1.PeakRSSMB, Joined: int(joined.Load()), Dropped: dropped.Load(),
		PositionsSent: positions.Load(), UDPPackets: udpPkts.Load(), UDPBytes: udpBytes.Load(),
		PlayerEntries: playerEntries.Load(), AIEntries: aiEntries.Load(), AISlotsSeen: ai,
		SteadyUDP: udp1 - udp0, SteadyEntries: e1 - e0, TCPPackets: tcpPkts.Load(), UDPByID: byID,
		Bandwidth: bandwidth,
	}
	require.EqualValues(t, players, m.Joined, "%s: every player joins", label)
	require.Zero(t, m.Dropped, "%s: nobody is dropped", label)
	if label != "vanilla" { // vanilla has no AI traffic
		require.Positive(t, m.AIEntries, "%s: AI traffic reaches the players", label)
	}
	return m
}

// joinSRPClient joins as a vanilla client and enters the world.
func joinSRPClient(t *testing.T, p portSet, i int) (*perfClient, error) {
	addr := fmt.Sprintf("127.0.0.1:%d", p.game)
	tcp, err := net.Dial("tcp", addr)
	if err != nil {
		return nil, err
	}
	br := bufio.NewReader(tcp)
	hs := wire.NewWriterID(proto.RequestNewConnection)
	hs.UInt16(proto.ClientProtocolVersion)
	hs.String8(strconv.FormatInt(76561198020000000+int64(i), 10))
	hs.WString8(fmt.Sprintf("Driver-%03d", i))
	hs.String8("")
	hs.String8("JPN")
	hs.String8(srpModel)
	hs.String8("")
	_, _ = tcp.Write(wire.FrameTCP(hs.Bytes()))
	_ = tcp.SetReadDeadline(time.Now().Add(10 * time.Second))
	resp, err := wire.ReadTCPPacket(br)
	if err != nil || len(resp) == 0 || resp[0] != proto.NewCarConnection {
		tcp.Close()
		return nil, fmt.Errorf("handshake: packet=%x err=%w", resp, err)
	}
	_ = tcp.SetReadDeadline(time.Time{})
	sessionID := sessionIDFromHandshake(resp)
	udp, err := net.Dial("udp", addr)
	if err != nil {
		tcp.Close()
		return nil, err
	}
	assoc := wire.NewWriterID(proto.CarConnect)
	assoc.UInt8(sessionID)
	_, _ = udp.Write(assoc.Bytes())
	_ = udp.SetReadDeadline(time.Now().Add(5 * time.Second))
	if _, err = udp.Read(make([]byte, 64)); err != nil {
		tcp.Close()
		udp.Close()
		return nil, fmt.Errorf("UDP association: %w", err)
	}
	_ = udp.SetReadDeadline(time.Time{})
	_, _ = tcp.Write(wire.FrameTCP(checksumPacket(t, resp, true)))
	return &perfClient{tcp: tcp, br: br, udp: udp, sessionID: sessionID}, nil
}

// writeSRPPosition sends a moving car's 0x46 (on the throttle in third gear,
// heading along its velocity, lights on) and returns its size.
func writeSRPPosition(conn net.Conn, seq uint8, ts uint32, pos, vel wire.Vec3) int {
	w := wire.NewWriterID(proto.PositionUpdate)
	w.UInt8(seq)
	w.UInt32(ts)
	w.Vec3(pos)
	w.Vec3(wire.Vec3{X: float32(math.Atan2(float64(vel.Z), float64(vel.X))) - math.Pi/2})
	w.Vec3(vel)
	w.Raw([]byte{180, 180, 180, 180})
	w.UInt8(127)
	w.UInt8(127)
	w.UInt16(5200)
	w.UInt8(4)
	w.UInt32(0x20)
	w.Int16(0)
	w.UInt8(200)
	w.Float32(0)
	_, _ = conn.Write(w.Bytes())
	return len(w.Bytes())
}
