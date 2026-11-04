package integration

// TestPerformanceRaceSim is a realistic club race: 16, 32 and 64 players,
// one field size after another, start from a two-wide grid on a 3 km circuit and race three laps, each driving the
// track's speed profile at their own pace, so the field strings out from
// nose to tail (inside 100 m) to spread round the lap (100–500 m and beyond),
// with a few spins that leave cars far behind and get them lapped. It runs
// our server (serverBinary), the original acServer when a reference install
// is found and, with ASSETTOSERVER_DIR, AssettoServer on the same drive, each as a separate process, and measures the same as
// TestPerformanceSRP, plus the position rate each server delivered at each
// distance. Opt-in:
//
//	cd test && RUN_PERF_RACESIM=1 ASSETTOSERVER_DIR=/path/to/assettoserver go test . \
//	  -run '^TestPerformanceRaceSim$' -v -count=1 -timeout=60m
//
// RACESIM_PLAYERS runs one field size in place of the three; RACESIM_LAPS
// changes the race length. PERF_TARGETS (vanilla, ours, assettoserver)
// picks the servers and PERF_LOG_DIR keeps their logs.

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math"
	"math/rand"
	"os"
	"path/filepath"
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
	raceSimTrackName = "racesim_gp"
	// raceSimWait is the race session's WAIT_TIME: players join in the
	// first raceSimJoin, then sit on the grid until the green light.
	raceSimWait = 40 * time.Second
	raceSimJoin = 20 * time.Second
	// raceSimStep is the resolution of the precomputed drive.
	raceSimStep = 50 * time.Millisecond
	// Car performance: top speed, cornering, acceleration and braking.
	raceSimVMax   = 68.0 // m/s, about 245 km/h
	raceSimLatAcc = 13.0 // m/s²
	raceSimAccel  = 6.0  // m/s²
	raceSimBrake  = 13.0 // m/s²
)

// The distance bands the servers treat differently: Rust never thins a car
// within 100 m, ranks the rest in tiers, and both servers thin cars beyond
// their 500 m bubble.
var raceSimBands = []struct {
	name     string
	min, max float64
}{{"under_100m", 0, 100}, {"100_to_500m", 100, 500}, {"over_500m", 500, math.Inf(1)}}

// raceSimBand is one distance band's result: how long pairs of cars spent
// in it and how many of each other's positions they got there.
type raceSimBand struct {
	PairSeconds float64 `json:"pair_seconds"`
	Entries     int64   `json:"entries"`
	Hz          float64 `json:"hz"`
}

type raceSimMetrics struct {
	Target        string                 `json:"target"`
	Players       int                    `json:"players"`
	Laps          int                    `json:"laps"`
	TrackMeters   float64                `json:"track_meters"`
	WallSeconds   float64                `json:"wall_seconds"`
	CPUSeconds    float64                `json:"cpu_seconds"`
	CPUPercent    float64                `json:"cpu_percent"`
	RaceSeconds   float64                `json:"race_seconds"`
	RaceCPU       float64                `json:"race_cpu_percent"`
	PeakRSSMB     float64                `json:"peak_rss_mb"`
	Joined        int                    `json:"joined"`
	Dropped       int64                  `json:"dropped"`
	PositionsSent int64                  `json:"positions_sent"`
	UDPPackets    int64                  `json:"udp_packets_seen"`
	UDPBytes      int64                  `json:"udp_bytes_seen"`
	Entries       int64                  `json:"entries_seen"`
	RaceUDP       int64                  `json:"race_udp_packets"`
	TCPPackets    int64                  `json:"tcp_packets_seen"`
	LapsSeen      int64                  `json:"lap_broadcasts_seen"`
	RaceOverSeen  int64                  `json:"race_over_seen"`
	Bands         map[string]raceSimBand `json:"bands"`
	UDPByID       map[string]int64       `json:"udp_by_id"`
	TCPByID       map[string]int64       `json:"tcp_by_id"`
	Bandwidth
}

func TestPerformanceRaceSim(t *testing.T) {
	if os.Getenv("RUN_PERF_RACESIM") != "1" {
		t.Skip("set RUN_PERF_RACESIM=1 to run the race simulation benchmark")
	}
	fields := []int{16, 32, 64}
	if os.Getenv("RACESIM_PLAYERS") != "" {
		fields = []int{srpEnvInt("RACESIM_PLAYERS", 32)}
	}
	laps := srpEnvInt("RACESIM_LAPS", 3)
	wanted := func(label string) bool {
		list := os.Getenv("PERF_TARGETS")
		return list == "" || strings.Contains(","+list+",", ","+label+",")
	}
	type target struct {
		label, bin, install, extra string
		args                       func(root, cfgDir string) []string
	}
	var targets []target
	if ref := referenceBinary(); ref != "" && wanted("vanilla") {
		targets = append(targets, target{label: "vanilla", bin: ref, install: referenceInstall(),
			args: func(string, string) []string { return referenceArgs() }})
	}
	if wanted("ours") {
		targets = append(targets, target{label: "ours", bin: serverBinary(t),
			args: func(root, cfgDir string) []string { return []string{"--cfg", cfgDir, "--content", ""} }})
	}
	if dir := os.Getenv("ASSETTOSERVER_DIR"); dir != "" && wanted("assettoserver") {
		abs, err := filepath.Abs(dir)
		require.NoError(t, err)
		targets = append(targets, target{label: "assettoserver", bin: filepath.Join(abs, "AssettoServer"), install: abs,
			args:  func(string, string) []string { return nil },
			extra: "IgnoreConfigurationErrors:\n  MissingCarChecksums: true\n  MissingTrackParams: true\n"})
	}
	if len(targets) == 0 {
		t.Skip("no target selected by PERF_TARGETS")
	}

	track := newRaceSimTrack()
	t.Logf("track: %.0f m, a clean lap %.1f s", track.length, track.lapTime())
	for _, players := range fields {
		for _, tg := range targets {
			t.Run(fmt.Sprintf("%d/%s", players, tg.label), func(t *testing.T) {
				root := t.TempDir()
				p := freePorts(t)
				cfgDir := writeRaceSimConfig(t, root, p, players, laps)
				writeFile(t, filepath.Join(cfgDir, "extra_cfg.yml"), tg.extra)
				if tg.install != "" {
					linkInstall(t, tg.install, root)
				}
				proc := startPerfProcess(t, tg.label, tg.bin, tg.args(root, cfgDir), root, p)
				defer proc.stop(t)
				m := runRaceSim(t, tg.label, p, proc, track, players, laps)
				b, err := json.Marshal(m)
				require.NoError(t, err)
				t.Log(string(b))
			})
		}
	}
}

func writeRaceSimConfig(t *testing.T, root string, p portSet, players, laps int) string {
	t.Helper()
	cfgDir := filepath.Join(root, "cfg")
	require.NoError(t, os.MkdirAll(cfgDir, 0o755))
	writeFile(t, filepath.Join(cfgDir, "server_cfg.ini"), fmt.Sprintf(`[SERVER]
NAME=race simulation
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
RACE_OVER_TIME=60
RESULT_SCREEN_TIME=30
MAX_CONTACTS_PER_KM=-1
ALLOWED_TYRES_OUT=-1

[RACE]
NAME=Race
LAPS=%d
WAIT_TIME=%d
IS_OPEN=1

[WEATHER_0]
GRAPHICS=3_clear
BASE_TEMPERATURE_AMBIENT=22
BASE_TEMPERATURE_ROAD=10
VARIATION_AMBIENT=0
VARIATION_ROAD=0
WIND_BASE_SPEED_MIN=0
WIND_BASE_SPEED_MAX=0
WIND_BASE_DIRECTION=0
WIND_VARIATION_DIRECTION=0
`, srpModel, raceSimTrackName, p.game, p.game, p.http, players, laps, int(raceSimWait/time.Second)))
	var entries strings.Builder
	for i := 0; i < players; i++ {
		fmt.Fprintf(&entries, "[CAR_%d]\nMODEL=%s\nSKIN=00_official\nSPECTATOR_MODE=0\nDRIVERNAME=\nTEAM=\nGUID=\nBALLAST=0\nRESTRICTOR=0\n\n", i, srpModel)
	}
	writeFile(t, filepath.Join(cfgDir, "entry_list.ini"), entries.String())
	return cfgDir
}

// raceSimTrack is a closed circuit sampled every metre, with the fastest
// speed a car can carry at each point.
type raceSimTrack struct {
	points []wire.Vec3
	speed  []float64
	length float64
}

// newRaceSimTrack builds a 3 km club circuit: a long start straight, a
// hairpin, fast sweepers and a tight infield, through a closed Catmull-Rom
// curve, then the speed profile from cornering, braking and acceleration.
func newRaceSimTrack() *raceSimTrack {
	ctrl := [][2]float64{
		{0, 0}, {300, 0}, {600, 0}, {760, 40}, {800, 160}, {700, 230}, {560, 220},
		{470, 290}, {520, 420}, {450, 520}, {300, 500}, {220, 400}, {120, 430},
		{20, 520}, {-90, 470}, {-60, 330}, {-160, 230}, {-150, 90}, {-90, 15},
	}
	n := len(ctrl)
	var dense []wire.Vec3
	for i := 0; i < n; i++ {
		p0, p1, p2, p3 := ctrl[(i-1+n)%n], ctrl[i], ctrl[(i+1)%n], ctrl[(i+2)%n]
		for k := 0; k < 200; k++ {
			u := float64(k) / 200
			cr := func(a, b, c, d float64) float64 {
				return 0.5 * (2*b + (-a+c)*u + (2*a-5*b+4*c-d)*u*u + (-a+3*b-3*c+d)*u*u*u)
			}
			dense = append(dense, wire.Vec3{X: float32(cr(p0[0], p1[0], p2[0], p3[0])), Z: float32(cr(p0[1], p1[1], p2[1], p3[1]))})
		}
	}
	// Resample every metre.
	tr := &raceSimTrack{}
	carry := 0.0
	for i := range dense {
		a, b := dense[i], dense[(i+1)%len(dense)]
		seg := math.Hypot(float64(b.X-a.X), float64(b.Z-a.Z))
		for carry <= seg {
			f := float32(carry / seg)
			tr.points = append(tr.points, wire.Vec3{X: a.X + (b.X-a.X)*f, Z: a.Z + (b.Z-a.Z)*f})
			carry++
		}
		carry -= seg
	}
	m := len(tr.points)
	tr.length = float64(m)
	// Cornering limit from the radius through points 10 m either side.
	tr.speed = make([]float64, m)
	for i := range tr.points {
		a, b, c := tr.points[(i-10+m)%m], tr.points[i], tr.points[(i+10)%m]
		r := circumradius(a, b, c)
		tr.speed[i] = math.Min(raceSimVMax, math.Sqrt(raceSimLatAcc*r))
	}
	// Braking (backwards) and acceleration (forwards), twice round so the
	// limits carry across the line.
	for pass := 0; pass < 2; pass++ {
		for i := 2*m - 1; i >= 0; i-- {
			j, next := i%m, (i+1)%m
			tr.speed[j] = math.Min(tr.speed[j], math.Sqrt(tr.speed[next]*tr.speed[next]+2*raceSimBrake))
		}
		for i := 0; i < 2*m; i++ {
			j, prev := i%m, (i-1+m)%m
			tr.speed[j] = math.Min(tr.speed[j], math.Sqrt(tr.speed[prev]*tr.speed[prev]+2*raceSimAccel))
		}
	}
	return tr
}

func circumradius(a, b, c wire.Vec3) float64 {
	ab := math.Hypot(float64(b.X-a.X), float64(b.Z-a.Z))
	bc := math.Hypot(float64(c.X-b.X), float64(c.Z-b.Z))
	ca := math.Hypot(float64(a.X-c.X), float64(a.Z-c.Z))
	cross := math.Abs(float64((b.X-a.X)*(c.Z-a.Z) - (b.Z-a.Z)*(c.X-a.X)))
	if cross < 1e-6 {
		return 1e9
	}
	return ab * bc * ca / (2 * cross)
}

func (tr *raceSimTrack) lapTime() float64 {
	t := 0.0
	for _, v := range tr.speed {
		t += 1 / v
	}
	return t
}

// at returns the position and heading direction at distance s along the lap.
func (tr *raceSimTrack) at(s float64) (pos, dir wire.Vec3) {
	m := len(tr.points)
	s = math.Mod(s, tr.length)
	if s < 0 {
		s += tr.length
	}
	i := int(s) % m
	a, b := tr.points[i], tr.points[(i+1)%m]
	f := float32(s - math.Floor(s))
	pos = wire.Vec3{X: a.X + (b.X-a.X)*f, Z: a.Z + (b.Z-a.Z)*f}
	d := math.Hypot(float64(b.X-a.X), float64(b.Z-a.Z))
	dir = wire.Vec3{X: float32(float64(b.X-a.X) / d), Z: float32(float64(b.Z-a.Z) / d)}
	return pos, dir
}

// raceSimFrame is one car's state at one step of the drive.
type raceSimFrame struct {
	s, v float64 // distance driven since the start line, speed
	gas  uint8
}

// raceSimDrive precomputes every car's race, from the green light until it
// has finished and coasted for a while: grid slot, pace (up to 3.5% off the
// fastest), and for a few cars a spin that stops them for 8–20 s.
func raceSimDrive(tr *raceSimTrack, players, laps int) (drive [][]raceSimFrame, finish []int) {
	rng := rand.New(rand.NewSource(11))
	pace := make([]float64, players)
	for i := range pace {
		pace[i] = 1 - 0.035*rng.Float64()
	}
	spinAt := make([]float64, players)
	spinFor := make([]int, players)
	for _, i := range rng.Perm(players)[:max(1, players/10)] {
		spinAt[i] = tr.length * (0.3 + float64(laps-1)*rng.Float64())
		spinFor[i] = int((8 + 12*rng.Float64()) * float64(time.Second/raceSimStep))
	}
	dt := raceSimStep.Seconds()
	drive = make([][]raceSimFrame, players)
	finish = make([]int, players)
	total := float64(laps) * tr.length
	for i := range players {
		// Two-wide grid, 8 m between rows, behind the line.
		s := -8*float64(i/2) - 4 - 2*float64(i%2)
		v := 0.0
		spun := false
		finish[i] = -1
		for {
			if finish[i] >= 0 && len(drive[i]) > finish[i]+int(20*time.Second/raceSimStep) {
				break
			}
			gas := uint8(0)
			switch {
			case spinFor[i] > 0 && !spun && s >= spinAt[i]:
				// Spun off: stopped for a while, then rejoins from rest.
				for k := 0; k < spinFor[i]; k++ {
					drive[i] = append(drive[i], raceSimFrame{s: s})
				}
				spun, v = true, 0
				continue
			default:
				idx := int(math.Mod(math.Mod(s, tr.length)+tr.length, tr.length)) % len(tr.speed)
				target := tr.speed[idx] * pace[i]
				if finish[i] >= 0 {
					target = 25 // cool-down lap
				}
				if v < target {
					v = math.Min(target, v+raceSimAccel*dt)
					gas = 230
				} else {
					v = math.Max(target, v-raceSimBrake*dt)
				}
			}
			s += v * dt
			if finish[i] < 0 && s >= total {
				finish[i] = len(drive[i])
			}
			drive[i] = append(drive[i], raceSimFrame{s: s, v: v, gas: gas})
		}
	}
	return drive, finish
}

// raceSimPos is car i's position, heading and speed at t into the race
// (t < 0: on the grid). Past the end of its drive it sits still.
func raceSimPos(tr *raceSimTrack, drive [][]raceSimFrame, i int, t time.Duration) (pos, vel, dir wire.Vec3, gas uint8) {
	var f raceSimFrame
	if t < 0 {
		f = raceSimFrame{s: -8*float64(i/2) - 4 - 2*float64(i%2)}
	} else {
		k := min(int(t/raceSimStep), len(drive[i])-1)
		f = drive[i][k]
	}
	pos, dir = tr.at(f.s)
	// The two grid columns sit 5 m apart, and the field keeps that offset.
	side := float32(2.5)
	if i%2 == 1 {
		side = -2.5
	}
	pos.X += -dir.Z * side
	pos.Z += dir.X * side
	vel = wire.Vec3{X: dir.X * float32(f.v), Z: dir.Z * float32(f.v)}
	return pos, vel, dir, f.gas
}

func raceSimDist(a, b wire.Vec3) float64 {
	return math.Hypot(float64(a.X-b.X), float64(a.Z-b.Z))
}

func runRaceSim(t *testing.T, label string, p portSet, proc *perfProcess, tr *raceSimTrack, players, laps int) raceSimMetrics {
	t.Helper()
	drive, finish := raceSimDrive(tr, players, laps)
	lastFinish := 0
	for _, f := range finish {
		lastFinish = max(lastFinish, f)
	}
	raceLen := time.Duration(lastFinish)*raceSimStep + 15*time.Second

	rng := rand.New(rand.NewSource(5))
	joinAt := make([]time.Duration, players)
	for i := range joinAt {
		joinAt[i] = randDur(rng, 0, raceSimJoin)
	}

	var joined, dropped, positions, tcpPkts, udpPkts, udpBytes, entries, lapsSeen, raceOver atomic.Int64
	var udpByID, tcpByID [256]atomic.Int64
	bandEntries := make([]atomic.Int64, len(raceSimBands))
	carOf := make([]atomic.Int32, 256) // session id → car index + 1
	meter := newBandwidthMeter()
	start := time.Now()
	green := start.Add(raceSimWait)
	end := green.Add(raceLen)
	stop := make(chan struct{})
	var wg sync.WaitGroup
	errs := make([]error, players)

	band := func(d float64) int {
		for b, r := range raceSimBands {
			if d >= r.min && d < r.max {
				return b
			}
		}
		return len(raceSimBands) - 1
	}
	// tally counts one received car position, banded by how far apart the
	// two cars are right now, during the race only.
	tally := func(me int, sender byte) {
		entries.Add(1)
		other := int(carOf[sender].Load()) - 1
		now := time.Since(green)
		if other < 0 || now < 0 || now > raceLen {
			return
		}
		a, _, _, _ := raceSimPos(tr, drive, me, now)
		b, _, _, _ := raceSimPos(tr, drive, other, now)
		bandEntries[band(raceSimDist(a, b))].Add(1)
	}

	for i := range players {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			time.Sleep(time.Until(start.Add(joinAt[i])))
			c, err := joinSRPClient(t, p, i)
			if err != nil {
				errs[i] = err
				return
			}
			carOf[c.sessionID].Store(int32(i + 1))
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
					if len(pkt) > 0 {
						tcpByID[pkt[0]].Add(1)
						switch pkt[0] {
						case proto.LapCompleted:
							lapsSeen.Add(1)
						case proto.RaceOver:
							raceOver.Add(1)
						}
					}
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
					switch {
					case buf[0] == proto.PositionUpdate && n >= 2:
						tally(i, buf[1])
					case buf[0] == proto.MegaPacket && n >= 8:
						for k := 0; k < int(buf[7]) && 8+57*(k+1) <= n; k++ {
							tally(i, buf[8+57*k])
						}
					case buf[0] == proto.PingUpdate && n >= 5:
						pong := wire.NewWriterID(proto.PingPong)
						pong.Int32(int32(binary.LittleEndian.Uint32(buf[1:5])))
						pong.Int32(int32(time.Since(start).Milliseconds()))
						_, _ = c.udp.Write(pong.Bytes())
						meter.sendUDP(len(pong.Bytes()))
					}
				}
			}()

			// Lap and sector timing from the precomputed drive.
			go func() {
				lastLap := time.Duration(0)
				for lap := 1; lap <= laps; lap++ {
					for sector := 1; sector <= 3; sector++ {
						target := (float64(lap-1) + float64(sector)/3) * tr.length
						k := 0
						for k < len(drive[i]) && drive[i][k].s < target {
							k++
						}
						at := time.Duration(k) * raceSimStep
						select {
						case <-stop:
							return
						case <-time.After(time.Until(green.Add(at))):
						}
						if sector < 3 {
							sendSectorSplit(c.tcp, uint8(sector-1), uint32((at - lastLap).Milliseconds()), 0)
							continue
						}
						lapTime := uint32((at - lastLap).Milliseconds())
						_, _ = c.tcp.Write(wire.FrameTCP(lapPacket(uint32(time.Since(start).Milliseconds()), lapTime, 0, uint8(lap))))
						lastLap = at
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
				pos, vel, dir, gas := raceSimPos(tr, drive, i, time.Since(green))
				meter.sendUDP(writeRaceSimPosition(c.udp, seq, ts, pos, vel, dir, gas))
				positions.Add(1)
				seq++
				ts += 56
			}
		}(i)
	}

	// Pair-time in each band through the race, sampled every 100 ms.
	bandSeconds := make([]float64, len(raceSimBands))
	var u0 perfUsage
	var udp0 int64
	time.Sleep(time.Until(green))
	u0 = readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	udp0 = udpPkts.Load()
	for now := time.Since(green); now < raceLen; now = time.Since(green) {
		for a := range players {
			pa, _, _, _ := raceSimPos(tr, drive, a, now)
			for b := range players {
				if a != b {
					pb, _, _, _ := raceSimPos(tr, drive, b, now)
					bandSeconds[band(raceSimDist(pa, pb))] += 0.1
				}
			}
		}
		time.Sleep(100*time.Millisecond - time.Since(green.Add(now)))
	}
	u1 := readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	raceSecs := time.Since(green).Seconds()
	udp1 := udpPkts.Load()
	time.Sleep(time.Until(end))
	wall := time.Since(start).Seconds()
	close(stop)
	wg.Wait()
	bandwidth := meter.result()

	for i, err := range errs {
		require.NoErrorf(t, err, "%s player %d", label, i)
	}
	byID := func(counts *[256]atomic.Int64) map[string]int64 {
		m := map[string]int64{}
		for id := range counts {
			if n := counts[id].Load(); n > 0 {
				m[fmt.Sprintf("%02x", id)] = n
			}
		}
		return m
	}
	bands := map[string]raceSimBand{}
	for b, r := range raceSimBands {
		n := bandEntries[b].Load()
		hz := 0.0
		if bandSeconds[b] > 0 {
			hz = float64(n) / bandSeconds[b]
		}
		bands[r.name] = raceSimBand{PairSeconds: bandSeconds[b], Entries: n, Hz: hz}
	}
	m := raceSimMetrics{
		Target: label, Players: players, Laps: laps, TrackMeters: tr.length,
		WallSeconds: wall, CPUSeconds: u1.CPUSeconds, CPUPercent: 100 * u1.CPUSeconds / wall,
		RaceSeconds: raceSecs, RaceCPU: 100 * (u1.CPUSeconds - u0.CPUSeconds) / raceSecs,
		PeakRSSMB: u1.PeakRSSMB, Joined: int(joined.Load()), Dropped: dropped.Load(),
		PositionsSent: positions.Load(), UDPPackets: udpPkts.Load(), UDPBytes: udpBytes.Load(),
		Entries: entries.Load(), RaceUDP: udp1 - udp0, TCPPackets: tcpPkts.Load(),
		LapsSeen: lapsSeen.Load(), RaceOverSeen: raceOver.Load(), Bands: bands,
		UDPByID: byID(&udpByID), TCPByID: byID(&tcpByID), Bandwidth: bandwidth,
	}
	require.EqualValues(t, players, m.Joined, "%s: every player joins", label)
	require.Zero(t, m.Dropped, "%s: nobody is dropped", label)
	for _, r := range raceSimBands {
		require.Positive(t, bands[r.name].PairSeconds, "%s: the race spends time %s", label, r.name)
	}
	return m
}

// writeRaceSimPosition sends one car's 0x46 and returns its size.
func writeRaceSimPosition(conn interface{ Write([]byte) (int, error) }, seq uint8, ts uint32, pos, vel, dir wire.Vec3, gas uint8) int {
	speed := math.Hypot(float64(vel.X), float64(vel.Z))
	gear := uint8(2 + min(5, int(speed/12)))
	rpm := uint16(3000 + math.Mod(speed*180, 4500))
	w := wire.NewWriterID(proto.PositionUpdate)
	w.UInt8(seq)
	w.UInt32(ts)
	w.Vec3(pos)
	w.Vec3(wire.Vec3{X: float32(math.Atan2(float64(dir.Z), float64(dir.X))) - math.Pi/2})
	w.Vec3(vel)
	wheel := uint8(100 + min(155, int(speed*2)))
	w.Raw([]byte{wheel, wheel, wheel, wheel})
	w.UInt8(127)
	w.UInt8(127)
	w.UInt16(rpm)
	w.UInt8(gear)
	w.UInt32(0)
	w.Int16(0)
	w.UInt8(gas)
	w.Float32(0)
	_, _ = conn.Write(w.Bytes())
	return len(w.Bytes())
}
