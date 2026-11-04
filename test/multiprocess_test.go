package integration

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

// TestMultiProcessGridRace runs the server and the clients in separate
// processes. In-process tests stall around 200 clients on the shared netpoller,
// not on the server. The default scale is small; to measure a ceiling:
//
//	cd test && LOADTEST_CLIENTS=255 LOADTEST_PROCS=5 go test -run TestMultiProcessGridRace . -v
//
// The binaries are built at run time, so use -count=1 after editing them.
// Above ~200 clients a simultaneous finish is erratic: an O(cars²) burst of
// 0x49 leaderboards, which no real race produces.
func TestMultiProcessGridRace(t *testing.T) {
	clients := envInt("LOADTEST_CLIENTS", 96)
	procs := envInt("LOADTEST_PROCS", 3)
	require.LessOrEqual(t, procs, clients, "more processes than clients")

	bin := t.TempDir()
	serverBin := serverBinary(t)
	loadBin := filepath.Join(bin, "loadtest")
	buildBinary(t, "./cmd/loadtest", loadBin)

	tcpPort := reserveProbePort(t)
	httpPort := reserveProbePort(t)
	cfgDir := writeGridConfig(t, clients, tcpPort, httpPort)

	// --- Start the server process. ---
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	// The default --content would resolve inside test/; run without it.
	srv := exec.CommandContext(ctx, serverBin, "--cfg", cfgDir, "--content", "")
	srv.Stdout, srv.Stderr = os.Stdout, os.Stderr
	require.NoError(t, srv.Start(), "start server")
	defer func() { cancel(); srv.Wait() }()
	waitForINFO(t, httpPort)

	// --- Spread the clients across loadtest processes. ---
	addr := fmt.Sprintf("127.0.0.1:%d", tcpPort)
	var wg sync.WaitGroup
	results := make([]procResult, procs)
	startedAll := time.Now()
	for j := 0; j < procs; j++ {
		n := clients / procs
		if j < clients%procs {
			n++
		}
		wg.Add(1)
		go func(j, n int) {
			defer wg.Done()
			// The context reaps the child when the test ends; -timeout is a
			// backstop.
			cmd := exec.CommandContext(ctx, loadBin,
				"-addr", addr,
				"-clients", strconv.Itoa(n),
				"-guidbase", strconv.FormatInt(76561190000000000+int64(j)*10_000_000, 10),
				"-name", fmt.Sprintf("P%d", j),
				"-race", "2s",
				"-finish",
				"-timeout", "40s",
			)
			out, err := cmd.Output()
			results[j] = parseProcResult(t, out, err)
		}(j, n)
	}
	wg.Wait()
	t.Logf("all %d clients across %d processes finished in %v", clients, procs, time.Since(startedAll))

	// --- Every process must have joined its whole share and seen RaceOver. ---
	totalJoined, totalSaw := 0, 0
	for j, r := range results {
		require.NoErrorf(t, r.err, "process %d failed: %s", j, r.stderr)
		require.Equalf(t, r.m.Clients, r.m.Joined, "process %d: only %d/%d joined", j, r.m.Joined, r.m.Clients)
		require.Truef(t, r.m.RaceOver, "process %d never saw RaceOver", j)
		totalJoined += r.m.Joined
		totalSaw += r.m.RaceOverSeen
		t.Logf("proc %d: joined %d in %.0fms, RaceOver in %.0fms, %d clients saw it",
			j, r.m.Joined, r.m.JoinMillis, r.m.RaceOverMs, r.m.RaceOverSeen)
	}
	require.Equal(t, clients, totalJoined, "not every client joined")
	t.Logf("TOTAL: %d clients joined, %d saw RaceOver", totalJoined, totalSaw)
}

type loadMetrics struct {
	Clients      int     `json:"clients"`
	Joined       int     `json:"joined"`
	JoinMillis   float64 `json:"join_ms"`
	RaceOver     bool    `json:"race_over"`
	RaceOverSeen int     `json:"race_over_seen"`
	RaceOverMs   float64 `json:"race_over_ms"`
	PositionsOut int64   `json:"positions_out"`
}

type procResult struct {
	m      loadMetrics
	err    error
	stderr string
}

func parseProcResult(t *testing.T, stdout []byte, err error) procResult {
	t.Helper()
	r := procResult{err: err}
	if ee, ok := err.(*exec.ExitError); ok {
		r.stderr = string(ee.Stderr)
	}
	// The last stdout line is the JSON metrics blob.
	for _, line := range strings.Split(strings.TrimSpace(string(stdout)), "\n") {
		if strings.HasPrefix(strings.TrimSpace(line), "{") {
			_ = json.Unmarshal([]byte(line), &r.m)
		}
	}
	return r
}

func buildBinary(t *testing.T, pkg, out string) {
	t.Helper()
	cmd := exec.Command("go", "build", "-o", out, pkg)
	cmd.Dir = "." // the module root, test/
	if b, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build %s: %v\n%s", pkg, err, b)
	}
}

// writeGridConfig writes a temp cfg dir with an n-car entry list and a single
// one-lap race, so a full field crossing the line ends the session.
func writeGridConfig(t *testing.T, n, tcpPort, httpPort int) string {
	t.Helper()
	dir := t.TempDir()
	serverCfg := fmt.Sprintf(`[SERVER]
NAME=loadtest
CARS=ks_mazda_mx5_cup
TRACK=ks_brands_hatch
CONFIG_TRACK=gp
SUN_ANGLE=16
PASSWORD=
UDP_PORT=%d
TCP_PORT=%d
HTTP_PORT=%d
MAX_CLIENTS=%d
CLIENT_SEND_INTERVAL_HZ=18
REGISTER_TO_LOBBY=0
PICKUP_MODE_ENABLED=1
LOOP_MODE=0
RACE_OVER_TIME=20
RESULT_SCREEN_TIME=1
ABS_ALLOWED=1
TC_ALLOWED=1
STABILITY_ALLOWED=0
AUTOCLUTCH_ALLOWED=1
ALLOWED_TYRES_OUT=2
MAX_CONTACTS_PER_KM=-1

[RACE]
NAME=Race
LAPS=1
IS_OPEN=1

[DYNAMIC_TRACK]
SESSION_START=96
RANDOMNESS=0
SESSION_TRANSFER=100
LAP_GAIN=30

[WEATHER_0]
GRAPHICS=3_clear
BASE_TEMPERATURE_AMBIENT=18
BASE_TEMPERATURE_ROAD=8
VARIATION_AMBIENT=0
VARIATION_ROAD=0
WIND_BASE_SPEED_MIN=0
WIND_BASE_SPEED_MAX=0
WIND_BASE_DIRECTION=0
WIND_VARIATION_DIRECTION=0
`, tcpPort, tcpPort, httpPort, n)
	require.NoError(t, os.WriteFile(filepath.Join(dir, "server_cfg.ini"), []byte(serverCfg), 0o644))

	var entry strings.Builder
	for i := 0; i < n; i++ {
		fmt.Fprintf(&entry, "[CAR_%d]\nMODEL=ks_mazda_mx5_cup\nSKIN=00_official\nSPECTATOR_MODE=0\nDRIVERNAME=\nTEAM=\nGUID=\nBALLAST=0\nRESTRICTOR=0\n\n", i)
	}
	require.NoError(t, os.WriteFile(filepath.Join(dir, "entry_list.ini"), []byte(entry.String()), 0o644))
	return dir
}

func waitForINFO(t *testing.T, httpPort int) {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	url := fmt.Sprintf("http://127.0.0.1:%d/INFO", httpPort)
	for time.Now().Before(deadline) {
		resp, err := http.Get(url)
		if err == nil {
			resp.Body.Close()
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatal("server did not start serving /INFO in time")
}

func envInt(key string, def int) int {
	if v := os.Getenv(key); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	return def
}
