//go:build linux

package integration

// TestPerformanceLatency measures how long each server takes to relay a
// car's position: from one client writing its 0x46 to another client's
// socket receiving the relayed copy. It runs the race simulation's drive
// (TestPerformanceRaceSim) for fields of 2, 16, 32 and 64 players on our
// server, the original acServer when a reference install is found and, with
// ASSETTOSERVER_DIR, AssettoServer, each as a separate process. Opt-in:
//
//	cd test && RUN_PERF_LATENCY=1 ASSETTOSERVER_DIR=/path/to/assettoserver go test . \
//	  -run '^TestPerformanceLatency$' -v -count=1 -timeout=60m
//
// LATENCY_PLAYERS runs one field size in place of the four; LATENCY_DURATION
// (default 90s) is the measured window from the green light. PERF_TARGETS
// (vanilla, ours, assettoserver) picks the servers.
//
// Every client is in this process, so send and receive times share one
// clock. The send time is taken just before the write; the receive time is
// the kernel's (SO_TIMESTAMPNS), so the harness's own scheduling under load
// stays out of the figure. A relayed update is matched to its send by the
// sender's session id and PakSequenceId, which every server relays unchanged.

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math"
	"math/rand"
	"net"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
	"github.com/stretchr/testify/require"
)

// latencyMax is the histogram's range, at 1 µs resolution; a larger sample
// counts in the last bucket, and the exact maximum is kept apart.
const latencyMax = 200 * time.Millisecond

// latencyHist is a histogram of latencies in microseconds, shared by every
// receiver through atomic counters.
type latencyHist struct {
	buckets      []atomic.Int64
	count, sumNs atomic.Int64
	maxNs        atomic.Int64
}

func newLatencyHist() *latencyHist {
	return &latencyHist{buckets: make([]atomic.Int64, latencyMax/time.Microsecond+1)}
}

func (h *latencyHist) add(d time.Duration) {
	h.buckets[min(int(d/time.Microsecond), len(h.buckets)-1)].Add(1)
	h.count.Add(1)
	h.sumNs.Add(int64(d))
	for old := h.maxNs.Load(); int64(d) > old && !h.maxNs.CompareAndSwap(old, int64(d)); old = h.maxNs.Load() {
	}
}

// LatencyStats summarises a histogram, in microseconds.
type LatencyStats struct {
	Samples int64   `json:"samples"`
	MeanUs  float64 `json:"mean_us"`
	P50Us   int     `json:"p50_us"`
	P90Us   int     `json:"p90_us"`
	P99Us   int     `json:"p99_us"`
	P999Us  int     `json:"p999_us"`
	MaxUs   int     `json:"max_us"`
}

func (h *latencyHist) stats() LatencyStats {
	n := h.count.Load()
	if n == 0 {
		return LatencyStats{}
	}
	pct := func(p float64) int {
		rank := int64(math.Ceil(p * float64(n)))
		var seen int64
		for us := range h.buckets {
			if seen += h.buckets[us].Load(); seen >= rank {
				return us
			}
		}
		return len(h.buckets) - 1
	}
	return LatencyStats{
		Samples: n, MeanUs: float64(h.sumNs.Load()) / float64(n) / 1e3,
		P50Us: pct(0.5), P90Us: pct(0.9), P99Us: pct(0.99), P999Us: pct(0.999),
		MaxUs: int(h.maxNs.Load() / 1e3),
	}
}

// latencyBand is one distance band's result.
type latencyBand struct {
	LatencyStats
	// Delivered is the share of the band's updates that reached the
	// receiver (the rest were thinned or merged).
	Delivered float64 `json:"delivered"`
}

type latencyMetrics struct {
	Target        string                 `json:"target"`
	Players       int                    `json:"players"`
	WindowSeconds float64                `json:"window_seconds"`
	CPUPercent    float64                `json:"window_cpu_percent"`
	PeakRSSMB     float64                `json:"peak_rss_mb"`
	Joined        int                    `json:"joined"`
	Dropped       int64                  `json:"dropped"`
	Sent          int64                  `json:"positions_sent"`
	Repeats       int64                  `json:"repeats"`
	Unmatched     int64                  `json:"unmatched"`
	All           latencyBand            `json:"all"`
	Bands         map[string]latencyBand `json:"bands"`
}

func TestPerformanceLatency(t *testing.T) {
	if os.Getenv("RUN_PERF_LATENCY") != "1" {
		t.Skip("set RUN_PERF_LATENCY=1 to run the relay latency benchmark")
	}
	fields := []int{2, 16, 32, 64}
	if os.Getenv("LATENCY_PLAYERS") != "" {
		fields = []int{srpEnvInt("LATENCY_PLAYERS", 16)}
	}
	window := 90 * time.Second
	if d, err := time.ParseDuration(os.Getenv("LATENCY_DURATION")); err == nil && d > 0 {
		window = d
	}

	t.Run("floor", func(t *testing.T) {
		b, err := json.Marshal(latencyFloor(t, 30*time.Second))
		require.NoError(t, err)
		t.Log(string(b))
	})

	targets := perfTargets(t, "", false)
	track := newRaceSimTrack()
	for _, players := range fields {
		for _, tg := range targets {
			t.Run(fmt.Sprintf("%d/%s", players, tg.label), func(t *testing.T) {
				root := t.TempDir()
				p := freePorts(t)
				// Enough laps that nobody finishes inside the window.
				cfgDir := writeRaceSimConfig(t, root, p, players, 10)
				writeFile(t, filepath.Join(cfgDir, "extra_cfg.yml"), tg.extra)
				if tg.install != "" {
					linkInstall(t, tg.install, root)
				}
				proc := startPerfProcess(t, tg.label, tg.bin, tg.args(root, cfgDir), root, p)
				defer proc.stop(t)
				m := runLatency(t, tg.label, p, proc, track, players, window)
				b, err := json.Marshal(m)
				require.NoError(t, err)
				t.Log(string(b))
			})
		}
	}
}

func runLatency(t *testing.T, label string, p portSet, proc *perfProcess, tr *raceSimTrack, players int, window time.Duration) latencyMetrics {
	t.Helper()
	drive, _ := raceSimDrive(tr, players, 10)
	rng := rand.New(rand.NewSource(5))
	joinAt := make([]time.Duration, players)
	phase := make([]time.Duration, players)
	for i := range joinAt {
		joinAt[i] = randDur(rng, 0, raceSimJoin)
		phase[i] = randDur(rng, 0, time.Second/18)
	}

	var joined, dropped, repeats, unmatched atomic.Int64
	sentIn := make([]atomic.Int64, players) // updates each car sent inside the window
	sentAt := make([][256]atomic.Int64, players)
	carOf := make([]atomic.Int32, 256) // session id → car index + 1
	all := newLatencyHist()
	hists := make([]*latencyHist, len(raceSimBands))
	for b := range hists {
		hists[b] = newLatencyHist()
	}
	// Updates sent per band: the receiver-sender pairs that were in the band
	// when each update was sent.
	bandSent := make([]atomic.Int64, len(raceSimBands))

	start := time.Now()
	green := start.Add(raceSimWait)
	end := green.Add(window)
	inWindow := func(at time.Time) bool { return !at.Before(green) && at.Before(end) }
	band := func(a, b int, at time.Time) int {
		now := at.Sub(green)
		pa, _, _, _ := raceSimPos(tr, drive, a, now)
		pb, _, _, _ := raceSimPos(tr, drive, b, now)
		d := raceSimDist(pa, pb)
		for k, r := range raceSimBands {
			if d >= r.min && d < r.max {
				return k
			}
		}
		return len(raceSimBands) - 1
	}

	stop := make(chan struct{})
	var wg sync.WaitGroup
	errs := make([]error, players)
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
			udp := c.udp.(*net.UDPConn)
			if err := enableRxTimestamps(udp); err != nil {
				errs[i] = err
				return
			}
			carOf[c.sessionID].Store(int32(i + 1))
			joined.Add(1)
			defer c.tcp.Close()
			defer udp.Close()
			go func() {
				for {
					if _, err := wire.ReadTCPPacket(c.br); err != nil {
						select {
						case <-stop:
						default:
							dropped.Add(1)
						}
						return
					}
				}
			}()
			go func() {
				buf := make([]byte, 2048)
				oob := make([]byte, 128)
				var last [256]int16 // last sequence seen from each session id
				for k := range last {
					last[k] = -1
				}
				record := func(at time.Time, sid, seq byte) {
					sender := int(carOf[sid].Load()) - 1
					if sender < 0 || !inWindow(at) {
						return
					}
					if last[sid] == int16(seq) {
						repeats.Add(1)
						return
					}
					last[sid] = int16(seq)
					d := at.Sub(time.Unix(0, sentAt[sender][seq].Load()))
					if d < 0 || d > time.Second {
						unmatched.Add(1)
						return
					}
					all.add(d)
					hists[band(i, sender, at)].add(d)
				}
				for {
					n, oobn, _, _, err := udp.ReadMsgUDP(buf, oob)
					if err != nil {
						return
					}
					at, ok := rxTimestamp(oob[:oobn])
					if !ok || n == 0 {
						continue
					}
					switch {
					case buf[0] == proto.PositionUpdate && n >= 3:
						record(at, buf[1], buf[2])
					case buf[0] == proto.MegaPacket && n >= 8:
						for k := 0; k < int(buf[7]) && 8+57*(k+1) <= n; k++ {
							record(at, buf[8+57*k], buf[9+57*k])
						}
					case buf[0] == proto.PingUpdate && n >= 5:
						pong := wire.NewWriterID(proto.PingPong)
						pong.Int32(int32(binary.LittleEndian.Uint32(buf[1:5])))
						pong.Int32(int32(time.Since(start).Milliseconds()))
						_, _ = udp.Write(pong.Bytes())
					}
				}
			}()

			time.Sleep(phase[i])
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
				now := time.Now()
				sentAt[i][seq].Store(now.UnixNano())
				writeRaceSimPosition(udp, seq, ts, pos, vel, dir, gas)
				if inWindow(now) {
					sentIn[i].Add(1)
					for other := range players {
						if other != i {
							bandSent[band(other, i, now)].Add(1)
						}
					}
				}
				seq++
				ts += 56
			}
		}(i)
	}

	time.Sleep(time.Until(green))
	u0 := readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	time.Sleep(time.Until(end))
	u1 := readPerfUsage(proc.cmd.Process.Pid, proc.peakKB.Load())
	// Let the last relays arrive before closing the sockets.
	time.Sleep(500 * time.Millisecond)
	close(stop)
	wg.Wait()
	for i, err := range errs {
		require.NoErrorf(t, err, "%s player %d", label, i)
	}

	var sent int64
	for i := range sentIn {
		sent += sentIn[i].Load()
	}
	pairs := sent * int64(players-1)
	share := func(got, of int64) float64 {
		if of == 0 {
			return 0
		}
		return float64(got) / float64(of)
	}
	bands := map[string]latencyBand{}
	for k, r := range raceSimBands {
		s := hists[k].stats()
		if s.Samples > 0 || bandSent[k].Load() > 0 {
			bands[r.name] = latencyBand{LatencyStats: s, Delivered: share(s.Samples, bandSent[k].Load())}
		}
	}
	allStats := all.stats()
	m := latencyMetrics{
		Target: label, Players: players, WindowSeconds: window.Seconds(),
		CPUPercent: 100 * (u1.CPUSeconds - u0.CPUSeconds) / window.Seconds(), PeakRSSMB: u1.PeakRSSMB,
		Joined: int(joined.Load()), Dropped: dropped.Load(), Sent: sent,
		Repeats: repeats.Load(), Unmatched: unmatched.Load(),
		All:   latencyBand{LatencyStats: allStats, Delivered: share(allStats.Samples, pairs)},
		Bands: bands,
	}
	require.EqualValues(t, players, m.Joined, "%s: every player joins", label)
	require.Zero(t, m.Dropped, "%s: nobody is dropped", label)
	require.Positive(t, m.All.Samples, "%s: positions are relayed", label)
	return m
}

// latencyFloor is the harness's own share: the same 62-byte datagrams at
// 18 Hz through a trivial relay in this process (one read, one write), timed
// the same way.
func latencyFloor(t *testing.T, d time.Duration) LatencyStats {
	t.Helper()
	listen := func() *net.UDPConn {
		c, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
		require.NoError(t, err)
		return c
	}
	relay, recv := listen(), listen()
	defer relay.Close()
	defer recv.Close()
	require.NoError(t, enableRxTimestamps(recv))
	send, err := net.DialUDP("udp", nil, relay.LocalAddr().(*net.UDPAddr))
	require.NoError(t, err)
	defer send.Close()
	go func() {
		buf := make([]byte, 2048)
		for {
			n, _, err := relay.ReadFromUDP(buf)
			if err != nil {
				return
			}
			_, _ = relay.WriteToUDP(buf[:n], recv.LocalAddr().(*net.UDPAddr))
		}
	}()

	var sentAt [256]atomic.Int64
	h := newLatencyHist()
	done := make(chan struct{})
	go func() {
		defer close(done)
		buf := make([]byte, 2048)
		oob := make([]byte, 128)
		for {
			n, oobn, _, _, err := recv.ReadMsgUDP(buf, oob)
			if err != nil {
				return
			}
			if at, ok := rxTimestamp(oob[:oobn]); ok && n >= 2 {
				h.add(at.Sub(time.Unix(0, sentAt[buf[1]].Load())))
			}
		}
	}()
	pkt := make([]byte, 62)
	pkt[0] = proto.PositionUpdate
	tick := time.NewTicker(time.Second / 18)
	for seq, end := uint8(0), time.Now().Add(d); time.Now().Before(end); seq++ {
		<-tick.C
		pkt[1] = seq
		sentAt[seq].Store(time.Now().UnixNano())
		_, _ = send.Write(pkt)
	}
	tick.Stop()
	time.Sleep(100 * time.Millisecond)
	recv.Close()
	<-done
	s := h.stats()
	require.Positive(t, s.Samples)
	return s
}

// enableRxTimestamps makes the kernel stamp each datagram it receives on
// conn with its arrival time (CLOCK_REALTIME).
func enableRxTimestamps(conn *net.UDPConn) error {
	raw, err := conn.SyscallConn()
	if err != nil {
		return err
	}
	var serr error
	if err := raw.Control(func(fd uintptr) {
		serr = syscall.SetsockoptInt(int(fd), syscall.SOL_SOCKET, syscall.SO_TIMESTAMPNS, 1)
	}); err != nil {
		return err
	}
	return serr
}

// rxTimestamp reads the kernel's receive time from a datagram's control
// messages.
func rxTimestamp(oob []byte) (time.Time, bool) {
	msgs, err := syscall.ParseSocketControlMessage(oob)
	if err != nil {
		return time.Time{}, false
	}
	i := slices.IndexFunc(msgs, func(m syscall.SocketControlMessage) bool {
		return m.Header.Level == syscall.SOL_SOCKET && m.Header.Type == syscall.SCM_TIMESTAMPNS && len(m.Data) >= 16
	})
	if i < 0 {
		return time.Time{}, false
	}
	d := msgs[i].Data
	return time.Unix(int64(binary.NativeEndian.Uint64(d[0:8])), int64(binary.NativeEndian.Uint64(d[8:16]))), true
}
