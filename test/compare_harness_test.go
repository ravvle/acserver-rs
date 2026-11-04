package integration

// Binary-comparison harness: replays a probe against this repository's server
// (the Rust build in rust/) and the original acServer so their replies can be
// diffed (COMPARE.md). Without a reference install, only the self-consistency
// test runs.

import (
	"bufio"
	"context"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"acserver-rs/test/internal/wire"
	"github.com/stretchr/testify/require"
)

// portSet is one server's listening ports. Assetto Corsa shares a single UDP
// and TCP "game" port; the HTTP API is a separate port.
type portSet struct {
	game int
	http int
}

// freePorts reserves two free localhost ports for a target.
//
// It walks a private range instead of asking the kernel, which can hand one
// port to two concurrent probes. Each port is checked on TCP and UDP.
func freePorts(t *testing.T) portSet {
	t.Helper()
	return portSet{game: reserveProbePort(t), http: reserveProbePort(t)}
}

// The range ends where the kernel's ephemeral range begins, so no outgoing
// socket (the load tests open thousands) can take a port between its check and
// the server's bind. The cursor starts at an offset derived from the pid so two
// `go test` runs on one machine do not walk the same numbers in lockstep.
const probePortBase = 10000

var (
	portMu     sync.Mutex
	portCursor int
	portEnd    int
)

// ephemeralPortStart is the first port the kernel gives outgoing sockets
// (Linux ip_local_port_range), else 32768, the Linux default.
func ephemeralPortStart() int {
	if b, err := os.ReadFile("/proc/sys/net/ipv4/ip_local_port_range"); err == nil {
		if f := strings.Fields(string(b)); len(f) > 0 {
			if n, err := strconv.Atoi(f[0]); err == nil && n > probePortBase+4000 {
				return n
			}
		}
	}
	return 32768
}

func reserveProbePort(t *testing.T) int {
	t.Helper()
	portMu.Lock()
	defer portMu.Unlock()
	if portCursor == 0 {
		portEnd = ephemeralPortStart()
		// Stay in the first half, so every run has half the range ahead.
		portCursor = probePortBase + (os.Getpid()%20)*((portEnd-probePortBase)/40)
	}
	for portCursor < portEnd {
		p := portCursor
		portCursor++
		if probePortUsable(p) {
			return p
		}
	}
	t.Fatalf("probe ports exhausted above %d", probePortBase)
	return 0
}

// probePortUsable reports whether a port is free for both protocols a target
// needs. Both listeners are closed again immediately; the cursor, not this
// check, is what keeps two probes apart.
func probePortUsable(port int) bool {
	addr := fmt.Sprintf("127.0.0.1:%d", port)
	l, err := net.Listen("tcp", addr)
	if err != nil {
		return false
	}
	l.Close()
	c, err := net.ListenPacket("udp", addr)
	if err != nil {
		return false
	}
	c.Close()
	return true
}

// target is a running server (Go or reference) addressable over loopback.
type target struct {
	label string
	ports portSet
	stop  func()
	// resultsDir is where the server writes results/*.json; cfgDir holds the
	// server's server_cfg.ini and entry_list.ini.
	resultsDir string
	cfgDir     string
}

// localInstall is where a reference install is found by default (gitignored).
const localInstall = "kunosserverfiles"

// detectInstall returns the absolute path of the in-repo reference install, or
// "" when there is none. Tests run with the package directory as cwd, so the
// repository root is one level up.
func detectInstall() string {
	root, err := filepath.Abs(filepath.Join("..", localInstall))
	if err != nil {
		return ""
	}
	if _, err := os.Stat(filepath.Join(root, refBinaryName)); err != nil {
		return ""
	}
	return root
}

// refBinaryName is the reference executable inside an install directory.
const refBinaryName = "acServer"

// referenceBinary returns the path to the original acServer binary, or "" when
// cross-binary comparison is disabled.
func referenceBinary() string {
	if bin := os.Getenv("AC_REFERENCE_SERVER"); bin != "" {
		return bin
	}
	if root := detectInstall(); root != "" {
		return filepath.Join(root, refBinaryName)
	}
	return ""
}

// referenceInstall returns the server install whose data files (content/,
// system/, …) the reference binary should resolve, or "" for none.
func referenceInstall() string {
	if dir := os.Getenv("AC_REFERENCE_DIR"); dir != "" {
		return dir
	}
	return detectInstall()
}

// contentDir is our server's --content: the install root, so both servers
// produce the same checksum paths.
func contentDir() string {
	if dir := os.Getenv("AC_CONTENT"); dir != "" {
		return dir
	}
	return detectInstall()
}

// ---------------------------------------------------------------------------
// Config templating
// ---------------------------------------------------------------------------

// tweak edits a probe's config tree (root: the working directory, holding
// cfg/) before both servers start.
type tweak func(t *testing.T, root, cfgDir string)

// writeTemplatedCfg writes root/cfg from ./cfg with the given ports, no lobby
// registration and no extra_cfg.yml, then applies tw.
func writeTemplatedCfg(t *testing.T, root string, p portSet, tw tweak) string {
	t.Helper()
	cfgDir := filepath.Join(root, "cfg")
	require.NoError(t, os.MkdirAll(cfgDir, 0o755), "mkdir cfg")

	serverCfg := readRepoCfg(t, "server_cfg.ini")
	serverCfg = setINT(serverCfg, "UDP_PORT", p.game)
	serverCfg = setINT(serverCfg, "TCP_PORT", p.game)
	serverCfg = setINT(serverCfg, "HTTP_PORT", p.http)
	serverCfg = setINT(serverCfg, "REGISTER_TO_LOBBY", 0)
	// The shipped cfg has no admin password, which disables admin commands.
	serverCfg = setSTR(serverCfg, "ADMIN_PASSWORD", adminPassword)
	// Both servers independently randomise temperature, wind and dynamic-track
	// grip at session start. Pin every random input so 0x3E/0x49/0x78 are exactly
	// comparable instead of masking meaningful fields as volatile.
	serverCfg = setINT(serverCfg, "VARIATION_AMBIENT", 0)
	serverCfg = setINT(serverCfg, "VARIATION_ROAD", 0)
	serverCfg = setINT(serverCfg, "WIND_BASE_SPEED_MIN", 0)
	serverCfg = setINT(serverCfg, "WIND_BASE_SPEED_MAX", 0)
	serverCfg = setINT(serverCfg, "WIND_BASE_DIRECTION", 0)
	serverCfg = setINT(serverCfg, "WIND_VARIATION_DIRECTION", 0)
	serverCfg = setINT(serverCfg, "RANDOMNESS", 0)

	writeFile(t, filepath.Join(cfgDir, "server_cfg.ini"), serverCfg)
	writeFile(t, filepath.Join(cfgDir, "entry_list.ini"), readRepoCfg(t, "entry_list.ini"))
	// Deliberately no extra_cfg.yml: acserver-rs then runs pure-vanilla
	// defaults, matching the reference acServer.
	if tw != nil {
		tw(t, root, cfgDir)
	}
	return cfgDir
}

// editCfg rewrites a file under cfg/ through a transform, for use inside a
// probe's tweak.
func editCfg(t *testing.T, cfgDir, name string, fn func(string) string) {
	t.Helper()
	path := filepath.Join(cfgDir, name)
	b, err := os.ReadFile(path)
	require.NoError(t, err, "read %s", name)
	writeFile(t, path, fn(string(b)))
}

// setSTR replaces (or appends, under [SERVER]) a KEY=string line in an ini body.
func setSTR(body, key, val string) string {
	re := regexp.MustCompile(`(?m)^` + regexp.QuoteMeta(key) + `=.*$`)
	repl := key + "=" + val
	if re.MatchString(body) {
		return re.ReplaceAllString(body, repl)
	}
	hdr := regexp.MustCompile(`(?m)^\[SERVER\]\s*$`)
	return hdr.ReplaceAllString(body, "[SERVER]\n"+repl)
}

func readRepoCfg(t *testing.T, name string) string {
	t.Helper()
	b, err := os.ReadFile(filepath.Join("..", "cfg", name))
	require.NoError(t, err, "read repo cfg %s", name)
	return string(b)
}

func writeFile(t *testing.T, path, body string) {
	t.Helper()
	require.NoError(t, os.WriteFile(path, []byte(body), 0o644), "write %s", path)
}

// setINT replaces (or appends, under [SERVER]) a KEY=int line in an ini body.
func setINT(body, key string, val int) string {
	re := regexp.MustCompile(`(?m)^` + regexp.QuoteMeta(key) + `=.*$`)
	repl := fmt.Sprintf("%s=%d", key, val)
	if re.MatchString(body) {
		return re.ReplaceAllString(body, repl)
	}
	// Not present: insert right after the [SERVER] header.
	hdr := regexp.MustCompile(`(?m)^\[SERVER\]\s*$`)
	return hdr.ReplaceAllString(body, "[SERVER]\n"+repl)
}

// ---------------------------------------------------------------------------
// Reference target (external Kunos acServer process)
// ---------------------------------------------------------------------------

// startRefTarget runs the reference binary with cwd=root, where it finds cfg/
// and the install's symlinked data dirs. Requires referenceBinary() != "".
func startRefTarget(t *testing.T, root string, p portSet) *target {
	t.Helper()
	bin := referenceBinary()
	if bin == "" {
		t.Skip("AC_REFERENCE_SERVER not set")
	}
	if install := referenceInstall(); install != "" {
		linkInstall(t, install, root)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cmd := exec.CommandContext(ctx, bin, referenceArgs()...)
	cmd.Dir = root
	logPath := filepath.Join(root, "reference.log")
	logFile, err := os.Create(logPath)
	if err != nil {
		cancel()
		t.Fatalf("reference: create log: %v", err)
	}
	cmd.Stdout = logFile
	cmd.Stderr = logFile

	if err := cmd.Start(); err != nil {
		cancel()
		logFile.Close()
		t.Fatalf("reference: start %s: %v", bin, err)
	}

	stop := func() {
		cancel()
		cmd.Wait()
		logFile.Close()
	}
	if !waitTCP(p.game, 15*time.Second) {
		out, _ := os.ReadFile(logPath)
		stop()
		t.Fatalf("reference server never opened game port %d.\n--- %s ---\n%s",
			p.game, logPath, out)
	}
	return &target{label: "reference", ports: p, stop: stop,
		resultsDir: filepath.Join(root, "results"), cfgDir: filepath.Join(root, "cfg")}
}

// ---------------------------------------------------------------------------
// Our target (the Rust server, in its own process)
// ---------------------------------------------------------------------------

var (
	serverBuild    sync.Once
	serverBuildErr error
)

// serverBinary is the server under test: ACSERVER_BIN when set, else the
// release build in ../rust, built once per test run.
func serverBinary(t testing.TB) string {
	t.Helper()
	if bin := os.Getenv("ACSERVER_BIN"); bin != "" {
		return bin
	}
	serverBuild.Do(func() {
		cmd := exec.Command("cargo", "build", "--release", "--bin", "acserver-rs")
		cmd.Dir = "../rust"
		if out, err := cmd.CombinedOutput(); err != nil {
			serverBuildErr = fmt.Errorf("%v\n%s", err, out)
		}
	})
	require.NoError(t, serverBuildErr, "build the Rust server")
	bin, err := filepath.Abs("../rust/target/release/acserver-rs")
	require.NoError(t, err)
	return bin
}

// startOurTarget runs serverBinary() on the templated cfg with cwd=root, so a
// /ban_id writes its blacklist.txt there.
func startOurTarget(t *testing.T, root, cfgDir string, p portSet) *target {
	t.Helper()
	bin := serverBinary(t)
	ctx, cancel := context.WithCancel(context.Background())
	cmd := exec.CommandContext(ctx, bin, "--cfg", cfgDir, "--content", contentDir())
	cmd.Dir = root
	logPath := filepath.Join(root, "server.log")
	logFile, err := os.Create(logPath)
	if err != nil {
		cancel()
		t.Fatalf("server: create log: %v", err)
	}
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	if err := cmd.Start(); err != nil {
		cancel()
		logFile.Close()
		t.Fatalf("server: start %s: %v", bin, err)
	}
	stop := func() {
		cancel()
		cmd.Wait()
		logFile.Close()
	}
	if !waitTCP(p.game, 15*time.Second) {
		out, _ := os.ReadFile(logPath)
		stop()
		t.Fatalf("server never opened game port %d.\n--- %s ---\n%s", p.game, logPath, out)
	}
	return &target{label: "ours", ports: p, stop: stop,
		resultsDir: filepath.Join(cfgDir, "results"), cfgDir: cfgDir}
}

// referenceArgs splits AC_REFERENCE_ARGS on whitespace for extra CLI flags.
func referenceArgs() []string {
	raw := strings.TrimSpace(os.Getenv("AC_REFERENCE_ARGS"))
	if raw == "" {
		return nil
	}
	return strings.Fields(raw)
}

// installWritable are the directories the reference binary writes into. They
// are recreated empty inside the temp root rather than symlinked, so a test run
// never deposits logs or results in the user's install.
var installWritable = []string{"logs/error", "logs/session", "results", "setups"}

// skipLink lists entries linkInstall must not symlink: writable dirs, and
// blacklist.txt, which /ban_id would append to inside the real install.
var skipLink = map[string]bool{
	"logs": true, "results": true, "setups": true, "blacklist.txt": true,
}

// linkInstall symlinks install's top-level entries into root, except skipLink.
func linkInstall(t *testing.T, install, root string) {
	t.Helper()
	entries, err := os.ReadDir(install)
	require.NoError(t, err, "reference: read install %s", install)
	for _, dir := range installWritable {
		require.NoError(t, os.MkdirAll(filepath.Join(root, filepath.FromSlash(dir)), 0o755),
			"reference: mkdir %s", dir)
	}
	for _, e := range entries {
		if e.Name() == "cfg" || skipLink[e.Name()] {
			continue
		}
		src, _ := filepath.Abs(filepath.Join(install, e.Name()))
		dst := filepath.Join(root, e.Name())
		if _, err := os.Lstat(dst); err == nil {
			continue
		}
		require.NoError(t, os.Symlink(src, dst), "reference: symlink %s", e.Name())
	}
}

// waitTCP polls a loopback TCP port until it accepts a connection or the
// timeout elapses.
func waitTCP(port int, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		c, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), 200*time.Millisecond)
		if err == nil {
			c.Close()
			return true
		}
		time.Sleep(50 * time.Millisecond)
	}
	return false
}

// ---------------------------------------------------------------------------
// Client connection kit shared by every probe
// ---------------------------------------------------------------------------

// kit is a TCP+UDP connection pair to a target, plus the framing helpers a
// probe uses to script an exchange.
type kit struct {
	t   *testing.T
	tcp net.Conn
	br  *bufio.Reader
	udp net.Conn
}

// dial opens fresh TCP and UDP connections to a target's game port.
func dial(t *testing.T, tgt *target) *kit {
	t.Helper()
	addr := fmt.Sprintf("127.0.0.1:%d", tgt.ports.game)
	tcp, err := net.Dial("tcp", addr)
	require.NoError(t, err, "dial tcp %s", tgt.label)
	udp, err := net.Dial("udp", addr)
	if err != nil {
		tcp.Close()
		t.Fatalf("dial udp %s: %v", tgt.label, err)
	}
	return &kit{t: t, tcp: tcp, br: bufio.NewReader(tcp), udp: udp}
}

func (k *kit) close() {
	if k.tcp != nil {
		k.tcp.Close()
	}
	if k.udp != nil {
		k.udp.Close()
	}
}

// sendTCP frames and writes a payload (id byte + body).
func (k *kit) sendTCP(payload []byte) {
	_, err := k.tcp.Write(wire.FrameTCP(payload))
	require.NoError(k.t, err, "write tcp")
}

// readTCP reads one framed TCP packet, returning (payload, true) or (nil,
// false) on timeout/close.
func (k *kit) readTCP(timeout time.Duration) ([]byte, bool) {
	k.tcp.SetReadDeadline(time.Now().Add(timeout))
	pkt, err := wire.ReadTCPPacket(k.br)
	if err != nil {
		return nil, false
	}
	return pkt, true
}

// awaitTCP reads framed TCP packets until one with the wanted id arrives or the
// timeout elapses.
func (k *kit) awaitTCP(id byte, timeout time.Duration) ([]byte, bool) {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		pkt, ok := k.readTCP(time.Until(deadline))
		if !ok {
			return nil, false
		}
		if len(pkt) > 0 && pkt[0] == id {
			return pkt, true
		}
	}
	return nil, false
}

// drainTCP reads packets until the connection is quiet for idle, or budget
// elapses, capturing a whole burst in order.
func (k *kit) drainTCP(idle, budget time.Duration) [][]byte {
	var out [][]byte
	deadline := time.Now().Add(budget)
	for {
		wait := idle
		if rem := time.Until(deadline); rem < wait {
			wait = rem
		}
		if wait <= 0 {
			return out
		}
		pkt, ok := k.readTCP(wait)
		if !ok {
			return out
		}
		out = append(out, pkt)
	}
}

func (k *kit) sendUDP(payload []byte) {
	_, err := k.udp.Write(payload)
	require.NoError(k.t, err, "write udp")
}

func (k *kit) readUDP(timeout time.Duration) ([]byte, bool) {
	k.udp.SetReadDeadline(time.Now().Add(timeout))
	buf := make([]byte, 2048)
	n, err := k.udp.Read(buf)
	if err != nil || n == 0 {
		return nil, false
	}
	return buf[:n], true
}
