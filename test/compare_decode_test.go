package integration

// Packet decoding and capture comparison. Packets decode into named fields;
// values that vary between runs (ports, clocks, seeds) decode to volatile.

import (
	"encoding/hex"
	"fmt"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
)

// volatile is the placeholder substituted for run-dependent field values so
// they never trip the comparison.
const volatile = "«volatile»"

type field struct {
	name  string
	value string
}

// packet is one decoded server->client message.
type packet struct {
	label  string // e.g. "TCP 0x3E NewCarConnection"
	fields []field
}

// capture is the full ordered set of decoded packets a probe observed.
type capture struct {
	packets []packet
}

func (c *capture) add(p packet) { c.packets = append(c.packets, p) }

// ---------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------

var tcpNames = map[byte]string{
	proto.UnsupportedProtocol:  "UnsupportedProtocol",
	proto.NewCarConnection:     "NewCarConnection",
	proto.CarList:              "CarList",
	proto.AuthFailed:           "AuthFailed",
	proto.WrongPassword:        "WrongPassword",
	proto.SessionClosed:        "SessionClosed",
	proto.NoSlotsAvailable:     "NoSlotsAvailable",
	proto.Blacklisted:          "Blacklisted",
	proto.WelcomeMessage:       "WelcomeMessage",
	proto.WeatherUpdate:        "WeatherUpdate",
	proto.SunAngleUpdate:       "SunAngleUpdate",
	proto.DriverInfoUpdate:     "DriverInfoUpdate",
	proto.MandatoryPitUpdate:   "MandatoryPitUpdate",
	proto.P2PUpdate:            "P2PUpdate",
	proto.BoPUpdate:            "BoPUpdate",
	proto.CarSetup:             "CarSetup",
	proto.DrsZonesUpdate:       "DrsZonesUpdate",
	proto.LapCompleted:         "LapCompleted",
	proto.CarConnected:         "CarConnected",
	proto.CarDisconnected:      "CarDisconnected",
	proto.Chat:                 "Chat",
	proto.CurrentSessionUpdate: "CurrentSessionUpdate",
	proto.KickCar:              "KickCar",
	proto.RaceOver:             "RaceOver",
	proto.TyreCompoundChange:   "TyreCompoundChange",
	proto.DamageUpdate:         "DamageUpdate",
	proto.SectorSplit:          "SectorSplit",
	proto.ClientEvent:          "ClientEvent",
	proto.VoteNextSession:      "VoteNextSession",
	proto.VoteRestartSession:   "VoteRestartSession",
	proto.VoteKickUser:         "VoteKickUser",
	proto.VoteQuorumNotReached: "VoteQuorumNotReached",
}

// decodeTCP turns a framed TCP payload into a labelled, decoded packet.
func decodeTCP(payload []byte) packet {
	if len(payload) == 0 {
		return packet{label: "TCP <empty>"}
	}
	id := payload[0]
	name := tcpNames[id]
	if name == "" {
		name = "?"
	}
	p := packet{label: fmt.Sprintf("TCP 0x%02X %s", id, name)}
	r := wire.NewReader(payload[1:])
	switch id {
	case proto.UnsupportedProtocol:
		p.fields = []field{{"requiredVersion", u16(r)}}
	case proto.NewCarConnection:
		p.fields = decodeHandshakeResp(r)
	case proto.CarList:
		p.fields = decodeCarList(r)
	case proto.AuthFailed:
		p.fields = []field{{"reason", r.WString8()}}

	// Rejections whose meaning is the packet id alone. Decoding them to an
	// explicit empty body (rather than falling through to rawHex) makes an
	// unexpected trailing byte on either server show up as a diff.
	case proto.WrongPassword, proto.SessionClosed, proto.NoSlotsAvailable, proto.Blacklisted:
		p.fields = []field{}

	// The first-update sequence.
	case proto.WelcomeMessage:
		p.fields = []field{{"unknown0", u8(r)}, {"message", r.WString16()}}
	case proto.WeatherUpdate:
		p.fields = []field{
			{"ambient", u8(r)},
			{"road", u8(r)},
			{"graphics", r.WString8()},
			{"windSpeed", i16(r)},
			{"windDirection", i16(r)},
		}
	case proto.SunAngleUpdate:
		p.fields = []field{{"sunAngle", sunAngle(r)}}
	case proto.DriverInfoUpdate:
		p.fields = decodeDriverInfo(r)
	case proto.MandatoryPitUpdate:
		p.fields = []field{{"sessionID", u8(r)}, {"mandatoryPitDone", b(r)}}
	case proto.P2PUpdate:
		p.fields = []field{{"sessionID", u8(r)}, {"count", i16(r)}, {"active", b(r)}}
	case proto.BoPUpdate:
		p.fields = []field{
			{"count", u8(r)},
			{"sessionID", u8(r)},
			{"ballast", f32(r)},
			{"restrictor", f32(r)},
		}
	case proto.CarSetup:
		p.fields = []field{{"fixed", b(r)}, {"count", u8(r)}}
	case proto.DrsZonesUpdate:
		p.fields = []field{{"count", u8(r)}}
	case proto.LapCompleted:
		p.fields = decodeLeaderboard(r)

	// Join / leave / chat / session.
	case proto.CarConnected:
		p.fields = []field{{"sessionID", u8(r)}, {"name", r.String8()}, {"nation", r.String8()}}
	case proto.CarDisconnected:
		p.fields = []field{{"sessionID", u8(r)}}
	case proto.Chat:
		p.fields = []field{{"sessionID", u8(r)}, {"message", r.WString8()}}
	case proto.CurrentSessionUpdate:
		p.fields = decodeCurrentSession(r)
	case proto.KickCar:
		p.fields = []field{{"sessionID", u8(r)}, {"reason", u8(r)}}

	// Relayed in-world telemetry.
	case proto.TyreCompoundChange:
		p.fields = []field{{"sessionID", u8(r)}, {"compound", r.String8()}}
	case proto.DamageUpdate:
		f := []field{{"sessionID", u8(r)}}
		for i := 0; i < 5; i++ {
			f = append(f, field{fmt.Sprintf("damage[%d]", i), f32(r)})
		}
		p.fields = f
	case proto.SectorSplit:
		p.fields = []field{
			{"sessionID", u8(r)},
			{"splitIndex", u8(r)},
			{"splitTime", u32(r)},
			{"cuts", u8(r)},
		}
	case proto.ClientEvent:
		p.fields = []field{{"eventType", u8(r)}, {"sessionID", u8(r)}}

	// Vote state. All three vote kinds share one body (docs/02 §0x64/0x65/0x66);
	// the remaining time counts down from VOTE_DURATION in real time, so only its
	// presence is comparable.
	case proto.VoteNextSession, proto.VoteRestartSession, proto.VoteKickUser:
		p.fields = []field{
			{"target", u8(r)},
			{"quorum", u8(r)},
			{"voteCount", u8(r)},
			{"remainingMs", markVol(u32(r))},
			{"lastVoter", u8(r)},
			{"lastVote", u8(r)},
		}
	case proto.VoteQuorumNotReached:
		// The payload is the vote's remaining time, which has just run past zero
		// and wrapped; how far past depends on when the expiry sweep noticed, so
		// only its presence is comparable.
		p.fields = []field{{"remainingMs", markVol(u32(r))}}

	default:
		p.fields = []field{{"rawHex", hex.EncodeToString(payload[1:])}}
	}
	if rem := r.Remaining(); rem != 0 && len(p.fields) > 0 {
		p.fields = append(p.fields, field{"trailingBytes",
			fmt.Sprintf("%d (%s)", rem, hex.EncodeToString(r.RemainingBytes()))})
	}
	return p
}

// decodePositionBroadcast decodes an outbound 0x46, masking the per-receiver
// timestamp and the ping.
func decodePositionBroadcast(r *wire.Reader) []field {
	f := []field{
		{"sessionID", u8(r)},
		{"sequenceID", u8(r)},
		{"timestamp", markVol(u32(r))}, // rebased by receiver clock offset
		{"ping", markVol(u16(r))},      // live measurement
	}
	for _, name := range []string{"position", "rotation", "velocity"} {
		v := r.Vec3()
		f = append(f, field{name, fmt.Sprintf("%.6g,%.6g,%.6g", v.X, v.Y, v.Z)})
	}
	for i := 0; i < 4; i++ {
		f = append(f, field{fmt.Sprintf("tyreAngularSpeed[%d]", i), u8(r)})
	}
	f = append(f,
		field{"steerAngle", u8(r)},
		field{"wheelAngle", u8(r)},
		field{"engineRpm", u16(r)},
		field{"gear", u8(r)},
		field{"statusFlags", u32(r)},
		field{"performanceDelta", i16(r)},
		field{"gas", u8(r)},
	)
	return f
}

// decodeDriverInfo decodes 0x5B: the full list of cars holding a driver.
func decodeDriverInfo(r *wire.Reader) []field {
	n := int(r.UInt8())
	f := []field{{"count", fmt.Sprint(n)}}
	for i := 0; i < n; i++ {
		f = append(f,
			field{fmt.Sprintf("driver[%d].sessionID", i), u8(r)},
			field{fmt.Sprintf("driver[%d].name", i), r.WString8()},
		)
	}
	return f
}

// decodeLeaderboard decodes 0x49, used both for a completed lap and (with
// sessionID 0xFF) for the first-update leaderboard sync.
func decodeLeaderboard(r *wire.Reader) []field {
	f := []field{
		{"sessionID", u8(r)},
		{"lapTime", u32(r)},
		{"cuts", u8(r)},
	}
	n := int(r.UInt8())
	f = append(f, field{"entryCount", fmt.Sprint(n)})
	for i := 0; i < n; i++ {
		f = append(f,
			field{fmt.Sprintf("entry[%d].sessionID", i), u8(r)},
			field{fmt.Sprintf("entry[%d].time", i), u32(r)},
			field{fmt.Sprintf("entry[%d].laps", i), u16(r)},
			field{fmt.Sprintf("entry[%d].completed", i), u8(r)},
		)
	}
	f = append(f, field{"trackGrip", f32(r)})
	return f
}

// decodeCurrentSession decodes 0x4A. The grid block is one byte per entry-list
// slot and is only delimited by the trailing int64 start time, so it is sized
// from what remains.
func decodeCurrentSession(r *wire.Reader) []field {
	f := []field{
		{"name", r.String8()},
		{"index", u8(r)},
		{"type", u8(r)},
		{"time", u16(r)},
		{"laps", u16(r)},
		{"trackGrip", f32(r)},
	}
	grid := r.Remaining() - 8 // everything before the int64 start time
	if grid < 0 {
		grid = 0
	}
	f = append(f, field{"gridSize", fmt.Sprint(grid)})
	for i := 0; i < grid; i++ {
		f = append(f, field{fmt.Sprintf("grid[%d]", i), u8(r)})
	}
	f = append(f, field{"startTimeMs", markVol(i64(r))}) // server clock
	return f
}

// decodeUDP turns a UDP datagram into a labelled, decoded packet.
func decodeUDP(payload []byte) packet {
	if len(payload) == 0 {
		return packet{label: "UDP <empty>"}
	}
	id := payload[0]
	switch id {
	case proto.LobbyCheck:
		return packet{
			label:  fmt.Sprintf("UDP 0x%02X LobbyCheck", id),
			fields: []field{{"httpPort", volatile}}, // ephemeral per run
		}
	case proto.CarConnect:
		return packet{
			label:  fmt.Sprintf("UDP 0x%02X CarConnect(assoc)", id),
			fields: []field{{"rawHex", hex.EncodeToString(payload[1:])}},
		}
	case proto.PositionUpdate:
		return packet{
			label:  fmt.Sprintf("UDP 0x%02X PositionUpdate", id),
			fields: decodePositionBroadcast(wire.NewReader(payload[1:])),
		}
	case proto.PingUpdate:
		r := wire.NewReader(payload[1:])
		return packet{
			label: fmt.Sprintf("UDP 0x%02X PingUpdate", id),
			fields: []field{
				{"serverTimeMs", markVol(u32(r))}, // server clock
				{"ping", markVol(u16(r))},         // measured round trip
			},
		}
	case proto.RaceStart:
		// Every field is clock- or link-derived, so only the packet's shape is
		// comparable; a missing or extra field still shows up as a length diff.
		r := wire.NewReader(payload[1:])
		return packet{
			label: fmt.Sprintf("UDP 0x%02X RaceStart", id),
			fields: []field{
				{"startTimeMs", markVol(i32(r))},
				{"timeOffset", markVol(u32(r))},
				{"ping", markVol(u16(r))},
				{"payloadLen", fmt.Sprint(len(payload) - 1)},
			},
		}
	default:
		return packet{
			label:  fmt.Sprintf("UDP 0x%02X ?", id),
			fields: []field{{"rawHex", hex.EncodeToString(payload[1:])}},
		}
	}
}

// decodeHandshakeResp decodes the 0x3E NewCarConnection payload in wire order
// (see internal/server/handshake.go / docs/02-tcp-protocol.md). Fields tied to
// the process instance are marked volatile.
func decodeHandshakeResp(r *wire.Reader) []field {
	f := []field{
		{"serverName", r.WString8()},
		{"udpPort", markVol(u16(r))}, // ephemeral game port
		{"refreshRateHz", u8(r)},
		{"track", r.String8()},
		{"trackConfig", r.String8()},
		{"carModel", r.String8()},
		{"carSkin", r.String8()},
		{"sunAngle", sunAngle(r)},
		{"allowedTyresOut", i16(r)},
		{"tyreBlankets", b(r)},
		{"tractionControl", u8(r)},
		{"absAllowed", u8(r)},
		{"stabilityAllowed", b(r)},
		{"autoclutch", b(r)},
		{"jumpStartPenalty", u8(r)},
		{"mechDamage", f32(r)},
		{"fuelRate", f32(r)},
		{"tyreWearRate", f32(r)},
		{"forceVirtualMirror", b(r)},
		{"maxContactsPerKm", u8(r)},
		{"raceOverTimeMs", i32(r)},
		{"resultScreenTimeMs", i32(r)},
		{"raceExtraLap", b(r)},
		{"gasPenaltyDisabled", b(r)},
		{"pitWindowStart", i16(r)},
		{"pitWindowEnd", i16(r)},
		{"reversedGridPos", i16(r)},
		{"sessionID", u8(r)},
	}
	n := int(r.UInt8())
	f = append(f, field{"sessionCount", fmt.Sprint(n)})
	for i := 0; i < n; i++ {
		f = append(f,
			field{fmt.Sprintf("session[%d].type", i), u8(r)},
			field{fmt.Sprintf("session[%d].laps", i), u16(r)},
			field{fmt.Sprintf("session[%d].time", i), u16(r)},
		)
	}
	f = append(f,
		field{"cur.name", r.String8()},
		field{"cur.index", u8(r)},
		field{"cur.type", u8(r)},
		field{"cur.time", u16(r)},
		field{"cur.laps", u16(r)},
		field{"trackGrip", f32(r)},
		field{"spawnPosition", u8(r)},
		field{"sessionElapsedMs", markVol(i64(r))}, // server clock
	)
	np := int(r.UInt8())
	f = append(f, field{"checksumPathCount", fmt.Sprint(np)})
	for i := 0; i < np; i++ {
		f = append(f, field{fmt.Sprintf("checksumPath[%d]", i), r.String8()})
	}
	f = append(f,
		field{"legalTyres", r.String8()},
		// Both vary run to run; HandshakeSeedAndClock pins their meaning.
		field{"randomSeed", markVol(i32(r))},
		field{"currentTime", markVol(i32(r))}, // server clock
	)
	if rem := r.Remaining(); rem != 0 {
		f = append(f, field{"trailingBytes", fmt.Sprintf("%d (%s)", rem, hex.EncodeToString(r.RemainingBytes()))})
	}
	return f
}

// decodeCarList decodes the 0x40 CarList payload (docs/02-tcp-protocol.md).
func decodeCarList(r *wire.Reader) []field {
	page := int(r.UInt8())
	count := int(r.UInt8())
	f := []field{{"pageIndex", fmt.Sprint(page)}, {"count", fmt.Sprint(count)}}
	for i := 0; i < count; i++ {
		f = append(f,
			field{fmt.Sprintf("car[%d].sessionID", i), u8(r)},
			field{fmt.Sprintf("car[%d].model", i), r.String8()},
			field{fmt.Sprintf("car[%d].skin", i), r.String8()},
			field{fmt.Sprintf("car[%d].driver", i), r.String8()},
			field{fmt.Sprintf("car[%d].team", i), r.String8()},
			field{fmt.Sprintf("car[%d].nation", i), r.String8()},
			field{fmt.Sprintf("car[%d].spectator", i), b(r)},
		)
		for z := 0; z < 5; z++ {
			f = append(f, field{fmt.Sprintf("car[%d].damage[%d]", i, z), f32(r)})
		}
	}
	if rem := r.Remaining(); rem != 0 {
		f = append(f, field{"trailingBytes", fmt.Sprintf("%d (%s)", rem, hex.EncodeToString(r.RemainingBytes()))})
	}
	return f
}

// ---------------------------------------------------------------------------
// Small reader-to-string helpers (keep decoders reading like doc tables)
// ---------------------------------------------------------------------------

func u8(r *wire.Reader) string  { return fmt.Sprint(r.UInt8()) }
func u16(r *wire.Reader) string { return fmt.Sprint(r.UInt16()) }
func u32(r *wire.Reader) string { return fmt.Sprint(r.UInt32()) }
func i16(r *wire.Reader) string { return fmt.Sprint(r.Int16()) }
func i32(r *wire.Reader) string { return fmt.Sprint(r.Int32()) }
func i64(r *wire.Reader) string { return fmt.Sprint(r.Int64()) }
func b(r *wire.Reader) string   { return fmt.Sprint(r.Bool()) }

// f32 formats a float with enough precision to catch real differences while
// tolerating none of the noise a raw %v would add.
func f32(r *wire.Reader) string { return fmt.Sprintf("%.6g", r.Float32()) }

// sunAngle decodes to a tenth of a degree, hiding the reference's slow drift
// with wall-clock time.
func sunAngle(r *wire.Reader) string { return fmt.Sprintf("%.1f", r.Float32()) }

// markVol replaces a just-decoded value with the volatile sentinel. The value
// is still consumed from the reader (the caller passes the decode result), only
// its recorded form is masked.
func markVol(string) string { return volatile }
