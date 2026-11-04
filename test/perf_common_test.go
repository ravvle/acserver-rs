package integration

// Shared by the performance tests (PERFORMANCE.md): running a server as its
// own process, sampling its CPU and memory, and metering its clients' traffic.

import (
	"bufio"
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

type perfProcess struct {
	label  string
	cmd    *exec.Cmd
	cancel context.CancelFunc
	done   chan error
	peakKB atomic.Int64
}

type perfUsage struct {
	CPUSeconds float64
	PeakRSSMB  float64
}

// Bandwidth is a server's traffic as its clients see it, in Mbit/s on the
// wire: UDP payload plus 28 bytes of IPv4 and UDP header per datagram, and
// TCP payload with its 2-byte length prefix (TCP/IP headers left out; the
// TCP share is small). Out is the server's upload, In its download.
type Bandwidth struct {
	PeakOutMbps float64 `json:"peak_out_mbps"`
	AvgOutMbps  float64 `json:"avg_out_mbps"`
	PeakInMbps  float64 `json:"peak_in_mbps"`
	AvgInMbps   float64 `json:"avg_in_mbps"`
}

// udpHeaderBytes is an IPv4 header (20) plus a UDP header (8).
const udpHeaderBytes = 28

// bandwidthMeter counts the bytes a run's clients receive and send and
// keeps the busiest second of each direction.
type bandwidthMeter struct {
	out, in         atomic.Int64
	peakOut, peakIn int64
	start           time.Time
	stop, done      chan struct{}
}

func newBandwidthMeter() *bandwidthMeter {
	m := &bandwidthMeter{start: time.Now(), stop: make(chan struct{}), done: make(chan struct{})}
	go func() {
		defer close(m.done)
		tick := time.NewTicker(time.Second)
		defer tick.Stop()
		var lastOut, lastIn int64
		for {
			select {
			case <-m.stop:
				return
			case <-tick.C:
			}
			out, in := m.out.Load(), m.in.Load()
			m.peakOut = max(m.peakOut, out-lastOut)
			m.peakIn = max(m.peakIn, in-lastIn)
			lastOut, lastIn = out, in
		}
	}()
	return m
}

// recvUDP and sendUDP count one datagram of n payload bytes; recvTCP one
// framed TCP packet of n bytes.
func (m *bandwidthMeter) recvUDP(n int) { m.out.Add(int64(n + udpHeaderBytes)) }
func (m *bandwidthMeter) sendUDP(n int) { m.in.Add(int64(n + udpHeaderBytes)) }
func (m *bandwidthMeter) recvTCP(n int) { m.out.Add(int64(n + 2)) }

// result stops the sampler and returns the peaks and the averages.
func (m *bandwidthMeter) result() Bandwidth {
	close(m.stop)
	<-m.done
	mbps := func(bytes int64, secs float64) float64 { return float64(bytes) * 8 / 1e6 / secs }
	wall := time.Since(m.start).Seconds()
	return Bandwidth{
		PeakOutMbps: mbps(m.peakOut, 1), AvgOutMbps: mbps(m.out.Load(), wall),
		PeakInMbps: mbps(m.peakIn, 1), AvgInMbps: mbps(m.in.Load(), wall),
	}
}

func startPerfProcess(t *testing.T, label, bin string, args []string, cwd string, ports portSet) *perfProcess {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	cmd := exec.CommandContext(ctx, bin, args...)
	cmd.Dir = cwd
	logPath := filepath.Join(t.TempDir(), label+".log")
	logFile, err := os.Create(logPath)
	require.NoError(t, err)
	// A failed run shows the end of the server's log.
	t.Cleanup(func() {
		b, _ := os.ReadFile(logPath)
		if t.Failed() {
			t.Logf("%s log tail:\n%s", label, b[max(0, len(b)-8192):])
		}
		// PERF_LOG_DIR keeps every server's log.
		if dir := os.Getenv("PERF_LOG_DIR"); dir != "" {
			_ = os.WriteFile(filepath.Join(dir, label+".log"), b, 0o644)
		}
	})
	cmd.Stdout, cmd.Stderr = logFile, logFile
	require.NoError(t, cmd.Start(), "start %s", label)

	p := &perfProcess{label: label, cmd: cmd, cancel: cancel, done: make(chan error, 1)}
	go func() {
		p.done <- cmd.Wait()
		logFile.Close()
	}()
	go func() {
		tick := time.NewTicker(100 * time.Millisecond)
		defer tick.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-tick.C:
				u := readPerfUsage(cmd.Process.Pid, 0)
				kb := int64(u.PeakRSSMB * 1024)
				for old := p.peakKB.Load(); kb > old && !p.peakKB.CompareAndSwap(old, kb); old = p.peakKB.Load() {
				}
			}
		}
	}()
	if !waitTCP(ports.game, 15*time.Second) {
		cancel()
		<-p.done
		t.Fatalf("%s did not open game port %d", label, ports.game)
	}
	return p
}

func (p *perfProcess) stop(t *testing.T) {
	t.Helper()
	p.cancel()
	select {
	case <-p.done:
	case <-time.After(5 * time.Second):
		_ = p.cmd.Process.Kill()
		<-p.done
	}
}

// readPerfUsage returns cumulative CPU time and the largest RSS observed either
// by this read or the caller's sampler. Linux exposes both without perturbing
// the measured process.
func readPerfUsage(pid int, sampledPeakKB int64) perfUsage {
	stat, _ := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	var cpu float64
	if end := strings.LastIndexByte(string(stat), ')'); end >= 0 {
		fields := strings.Fields(string(stat)[end+1:])
		if len(fields) > 12 {
			user, _ := strconv.ParseFloat(fields[11], 64)
			system, _ := strconv.ParseFloat(fields[12], 64)
			cpu = (user + system) / 100 // Linux USER_HZ is 100 on supported runners.
		}
	}
	status, _ := os.ReadFile(fmt.Sprintf("/proc/%d/status", pid))
	peakKB := sampledPeakKB
	for _, line := range strings.Split(string(status), "\n") {
		if strings.HasPrefix(line, "VmRSS:") {
			f := strings.Fields(line)
			if len(f) >= 2 {
				if n, _ := strconv.ParseInt(f[1], 10, 64); n > peakKB {
					peakKB = n
				}
			}
		}
	}
	return perfUsage{CPUSeconds: cpu, PeakRSSMB: float64(peakKB) / 1024}
}

type perfClient struct {
	tcp       net.Conn
	br        *bufio.Reader
	udp       net.Conn
	sessionID uint8
}
