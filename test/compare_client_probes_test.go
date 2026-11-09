package integration

// Probes from the game client's side of the protocol. Each one sends what
// acs.exe 1.16 sends, as rustyAC's port of the client has it
// (github.com/thesupersupersigma/rustyAC, crates/rustyac-net), where that
// differs from what the rest of this suite assumes, or reads a server packet
// the way that client reads it.

import (
	"encoding/binary"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/require"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
)

// ---------------------------------------------------------------------------
// The client's jump start (0x82 0x0C)
// ---------------------------------------------------------------------------

// jumpStartBare is a jump start as acs.exe writes it (rustyAC messages.rs
// ClientEvent::JumpStart): the event type alone, without the i16 count its
// collision report carries.
func jumpStartBare(p *player) []byte {
	return []byte{proto.ClientEvent, proto.EventJumpStart}
}

// probeJumpStartBare sends the bare jump start, then a chat line from the same
// player. The other player's view shows whether the jump start is relayed and
// whether the sender is still connected afterwards.
func probeJumpStartBare(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(jumpStartBare(b))
	time.Sleep(300 * time.Millisecond)
	w := wire.NewWriterID(proto.Chat)
	w.UInt8(b.sessionID)
	w.WString8("after the jump start")
	b.k.sendTCP(w.Bytes())

	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// ---------------------------------------------------------------------------
// Wide strings with characters outside the basic plane
// ---------------------------------------------------------------------------

// surrogateCells is "x😀" as acs.exe writes a wide string without CSP: one
// UTF-16 unit of its wchar_t per 32-bit cell, so the emoji travels as its two
// surrogates (rustyAC packet.rs Writer::wide).
var surrogateCells = []uint32{'x', 0xD83D, 0xDE00}

// writeCells8 writes a wstring8 from raw 32-bit cells.
func writeCells8(w *wire.Writer, cells []uint32) {
	w.UInt8(uint8(len(cells)))
	for _, c := range cells {
		w.UInt32(c)
	}
}

// readCells8 reads a wstring8 as its raw cells in hex, so a surrogate pair, a
// joined code point and a replacement character all stay distinct.
func readCells8(r *wire.Reader) string {
	n := int(r.UInt8())
	cells := make([]string, n)
	for i := range cells {
		cells[i] = fmt.Sprintf("%X", r.UInt32())
	}
	return strings.Join(cells, " ")
}

// decodeRawWide decodes the packets that carry a driver's wide strings with
// their cells kept raw; anything else as decodeTCP does.
func decodeRawWide(pkt []byte) packet {
	r := wire.NewReader(pkt[1:])
	switch pkt[0] {
	case proto.Chat:
		return packet{label: "TCP 0x47 Chat", fields: []field{
			{"sessionID", u8(r)}, {"cells", readCells8(r)}}}
	case proto.DriverInfoUpdate:
		n := int(r.UInt8())
		f := []field{{"count", fmt.Sprint(n)}}
		for i := 0; i < n; i++ {
			id := u8(r)
			f = append(f, field{"name[" + id + "]", readCells8(r)})
		}
		return packet{label: "TCP 0x5B DriverInfoUpdate", fields: f}
	case proto.CarConnected:
		id := u8(r)
		name := r.String8()
		return packet{label: "TCP 0x5A CarConnected", fields: []field{
			{"sessionID", id}, {"nameUTF8", fmt.Sprintf("%X", name)}, {"nation", r.String8()}}}
	}
	return decodeTCP(pkt)
}

// probeChatSurrogates relays a chat line holding a surrogate pair.
func probeChatSurrogates(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	w := wire.NewWriterID(proto.Chat)
	w.UInt8(b.sessionID)
	writeCells8(w, surrogateCells)
	b.k.sendTCP(w.Bytes())
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeRawWide(pkt))
	}
	return cap
}

// probeNameSurrogates joins a driver whose name holds a surrogate pair and
// captures how an in-world player is told of it: 0x5A as UTF-8, 0x5B as a
// wstring8.
func probeNameSurrogates(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Ascii"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	w := wire.NewWriterID(proto.RequestNewConnection)
	w.UInt16(proto.ClientProtocolVersion)
	w.String8(probeGUID2)
	writeCells8(w, surrogateCells)
	w.String8("TestTeam")
	w.String8("GBR")
	w.String8(probeCar)
	w.String8("")
	k := dial(t, tgt)
	defer k.close()
	k.sendTCP(w.Bytes())
	pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
	if !ok {
		cap.add(packet{label: "TCP <no 0x3E>"})
		return cap
	}
	b := &player{k: k, sessionID: sessionIDFromHandshake(pkt)}
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.CarConnected || pkt[0] == proto.DriverInfoUpdate {
			cap.add(decodeRawWide(pkt))
		}
	}
	return cap
}

// ---------------------------------------------------------------------------
// Narrow strings written in the client's code page
// ---------------------------------------------------------------------------

// latin1Team and latin1Nation are "Équipe" and "DÄN" as acs.exe writes a
// narrow string (rustyAC packet.rs Writer::ansi): the low byte of each UTF-16
// unit, so a non-ASCII character is one byte that is not valid UTF-8.
const (
	latin1Team   = "\xC9quipe"
	latin1Nation = "D\xC4N"
)

// probeLatin1Identity joins a driver whose team and nation hold Latin-1
// bytes and captures, in hex, what an in-world player is told of them: the
// nation in 0x5A and both in the 0x40 car list.
func probeLatin1Identity(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Ascii"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	w := wire.NewWriterID(proto.RequestNewConnection)
	w.UInt16(proto.ClientProtocolVersion)
	w.String8(probeGUID2)
	w.WString8("Latin")
	w.String8(latin1Team)
	w.String8(latin1Nation)
	w.String8(probeCar)
	w.String8("")
	k := dial(t, tgt)
	defer k.close()
	k.sendTCP(w.Bytes())
	pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
	if !ok {
		cap.add(packet{label: "TCP <no 0x3E>"})
		return cap
	}
	b := &player{k: k, sessionID: sessionIDFromHandshake(pkt)}
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.CarConnected {
			r := wire.NewReader(pkt[1:])
			cap.add(packet{label: "TCP 0x5A CarConnected", fields: []field{
				{"sessionID", u8(r)}, {"name", r.String8()},
				{"nationHex", fmt.Sprintf("%X", r.String8())}}})
		}
	}

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	a.k.sendTCP(req.Bytes())
	list, ok := a.k.awaitTCP(proto.CarList, 2*time.Second)
	if !ok {
		cap.add(packet{label: "TCP <no 0x40>"})
		return cap
	}
	r := wire.NewReader(list[1:])
	r.UInt8() // page index
	n := int(r.UInt8())
	for i := 0; i < n; i++ {
		id := u8(r)
		r.String8() // model
		r.String8() // skin
		name := r.String8()
		team := r.String8()
		nation := r.String8()
		r.Bool()
		r.Bytes(5 * 4)
		if id != fmt.Sprint(b.sessionID) {
			continue
		}
		cap.add(packet{label: "TCP 0x40 CarList row", fields: []field{
			{"sessionID", id}, {"name", name},
			{"teamHex", fmt.Sprintf("%X", team)}, {"nationHex", fmt.Sprintf("%X", nation)}}})
	}
	return cap
}

// ---------------------------------------------------------------------------
// A spectator slot and the client's row count
// ---------------------------------------------------------------------------

// cfgSpectatorSlot makes CAR_1 a spectator slot.
func cfgSpectatorSlot(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		re := regexp.MustCompile(`(?s)(\[CAR_1\].*?SPECTATOR_MODE=)0`)
		return re.ReplaceAllString(s, "${1}1")
	})
}

// carListRow is what the client keeps of one 0x40 entry.
type carListRow struct {
	sessionID uint8
	spectator bool
}

// readCarList asks for every page of the car list, as the client does
// (rustyAC client.rs Join: ten cars a page until a page is not full).
func readCarList(t *testing.T, k *kit) []carListRow {
	var rows []carListRow
	for page := 0; page < 256; page += 10 {
		req := wire.NewWriterID(proto.CarListRequest)
		req.UInt8(uint8(page))
		k.sendTCP(req.Bytes())
		pkt, ok := k.awaitTCP(proto.CarList, 2*time.Second)
		require.True(t, ok, "no 0x40 for page %d", page)
		r := wire.NewReader(pkt[1:])
		r.UInt8() // page index
		n := int(r.UInt8())
		for i := 0; i < n; i++ {
			row := carListRow{sessionID: r.UInt8()}
			for j := 0; j < 5; j++ { // model, skin, name, team, nation
				r.String8()
			}
			row.spectator = r.Bool()
			r.Bytes(5 * 4) // damage
			rows = append(rows, row)
		}
		if n != 10 {
			return rows
		}
	}
	return rows
}

// spectatorGrid is a 0x4A read twice: as the server wrote it, and as the
// client reads it.
type spectatorGrid struct {
	listed, spectators int
	gridOnWire         int   // grid bytes the server wrote
	clientGrid         int   // grid bytes the client reads
	startTime          int64 // the StartTime the server wrote
	clientStartTime    int64 // the StartTime the client reads
}

// probeSpectatorGrid joins one player in a server with a spectator slot and
// reads a 0x4A the way acs.exe does (rustyAC messages.rs decode): one grid byte
// per car of the session, which is every car-list entry that is not a
// spectator's (netCars + 1).
func probeSpectatorGrid(t *testing.T, tgt *target) spectatorGrid {
	p := joinPlayer(t, tgt, joinAs("Grid"))
	require.NotNil(t, p)
	defer p.k.close()
	rows := readCarList(t, p.k)
	_, ok := p.enterWorld(t, tgt)
	require.True(t, ok)
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	w := wire.NewWriterID(proto.SessionRequest)
	w.UInt8(99) // not the running type: the server answers with a 0x4A
	p.k.sendUDP(w.Bytes())
	pkt, ok := p.k.awaitTCP(proto.CurrentSessionUpdate, 2*time.Second)
	require.True(t, ok, "no 0x4A")

	var g spectatorGrid
	g.listed = len(rows)
	for _, row := range rows {
		if row.spectator {
			g.spectators++
		}
	}
	g.clientGrid = g.listed - g.spectators

	r := wire.NewReader(pkt[1:])
	r.String8()    // name
	r.Bytes(1 + 1) // index, type
	r.Bytes(2 + 2) // time, laps
	r.Bytes(4)     // grip
	head := len(pkt) - 1 - r.Remaining()
	g.gridOnWire = r.Remaining() - 8
	body := pkt[1+head:]
	g.startTime = int64(binary.LittleEndian.Uint64(body[g.gridOnWire:]))
	// The client reads clientGrid bytes, then eight more, past the end when
	// it reads too far (rustyAC packet.rs Reader: zeros).
	tail := make([]byte, 8)
	if g.clientGrid < len(body) {
		copy(tail, body[g.clientGrid:])
	}
	g.clientStartTime = int64(binary.LittleEndian.Uint64(tail))
	return g
}

// refStarts runs the reference on root's cfg and reports whether it opens its
// game port, with its log.
func refStarts(t *testing.T, root string, p portSet) (bool, string) {
	if install := referenceInstall(); install != "" {
		linkInstall(t, install, root)
	}
	cmd := exec.Command(referenceBinary(), referenceArgs()...)
	cmd.Dir = root
	logPath := filepath.Join(root, "reference.log")
	logFile, err := os.Create(logPath)
	require.NoError(t, err)
	defer logFile.Close()
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	require.NoError(t, cmd.Start())
	exited := make(chan struct{})
	go func() { cmd.Wait(); close(exited) }()
	up := false
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) && !up {
		select {
		case <-exited:
			deadline = time.Now()
		default:
			up = waitTCP(p.game, 200*time.Millisecond)
		}
	}
	cmd.Process.Kill()
	<-exited
	out, _ := os.ReadFile(logPath)
	return up, string(out)
}

// TestSpectatorSlot pins what a spectator slot does on each server. The
// reference refuses to start with one. Ours starts, marks the slot in 0x40,
// and lists it in 0x4A's grid, so the client reads its StartTime one byte
// short per spectator slot.
func TestSpectatorSlot(t *testing.T) {
	t.Parallel()
	p := freePorts(t)
	root := t.TempDir()
	cfgDir := writeTemplatedCfg(t, root, p, cfgSpectatorSlot)

	var g spectatorGrid
	withTarget(t, startOurTarget(t, root, cfgDir, p), func(t *testing.T, tgt *target) capture {
		g = probeSpectatorGrid(t, tgt)
		return capture{}
	})
	t.Logf("ours: %d cars listed, %d spectators; 0x4A grid: %d bytes on the wire, the client reads %d; "+
		"StartTime written %d, read by the client as %d",
		g.listed, g.spectators, g.gridOnWire, g.clientGrid, g.startTime, g.clientStartTime)
	require.Equal(t, 1, g.spectators, "0x40 marks the spectator slot")
	require.Equal(t, g.listed, g.gridOnWire, "0x4A lists every slot")
	require.NotEqual(t, g.startTime, g.clientStartTime, "the client misreads StartTime")

	if referenceBinary() == "" {
		return
	}
	refRoot := t.TempDir()
	writeTemplatedCfg(t, refRoot, p, cfgSpectatorSlot)
	up, log := refStarts(t, refRoot, p)
	if os.Getenv("PROBE_DUMP") != "" {
		t.Logf("reference log:\n%s", log)
	}
	require.False(t, up, "the reference started with a spectator slot:\n%s", log)
}
