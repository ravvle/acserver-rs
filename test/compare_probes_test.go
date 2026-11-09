package integration

// Probe definitions, capture comparison, and the two entry points (see
// COMPARE.md): TestCompareSelfConsistent runs every probe against two of our
// servers; TestCompareWithReference against ours and the reference binary,
// skipping when there is none.

import (
	"crypto/md5"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"testing"
	"time"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
)

// probe is a named, scripted client interaction that returns the decoded
// server responses it observed.
type probe struct {
	name string
	run  func(t *testing.T, tgt *target) capture
	// cfg optionally mutates the templated config before either server starts,
	// for probes that need a non-default setup (a password, a full entry list,
	// a blacklist). Both targets get the identical tweaked tree.
	cfg tweak
	// dev, when set, marks a probe where this server deliberately differs from
	// vanilla. The captures are then asserted to differ in exactly the expected
	// way instead of being required to match.
	dev *deviation
}

// deviation is an intentional difference from vanilla. Both sides' answers are
// pinned, so the test fails if either server changes.
type deviation struct {
	why           string
	wantOurs      string // packet label this server must answer with
	wantReference string // packet label the original acServer must answer with
	// alsoDiffer marks a deviation where both servers answer with the *same*
	// packet but different contents. The captures are then required to differ,
	// so the deviation silently disappearing is caught too.
	alsoDiffer bool
}

// checkDeviation asserts that a deviating probe produced exactly the recorded
// difference.
func checkDeviation(t *testing.T, name string, d *deviation, got, want capture) {
	t.Helper()
	label := func(c capture) string {
		if len(c.packets) == 0 {
			return "<none>"
		}
		return c.packets[0].label
	}
	if g := label(got); g != d.wantOurs {
		t.Errorf("%s: deviation no longer holds: ours answered %q, expected %q (%s)",
			name, g, d.wantOurs, d.why)
	}
	if w := label(want); w != d.wantReference {
		t.Errorf("%s: deviation no longer holds: reference answered %q, expected %q (%s)",
			name, w, d.wantReference, d.why)
	}
	if d.alsoDiffer && capturesEqual(got, want) {
		t.Errorf("%s: recorded deviation has disappeared — the captures now match. "+
			"Re-check whether it still applies and drop it if not (%s)", name, d.why)
	}
}

// capturesEqual reports whether two captures are field-for-field identical.
func capturesEqual(a, b capture) bool {
	if len(a.packets) != len(b.packets) {
		return false
	}
	for i := range a.packets {
		if a.packets[i].label != b.packets[i].label {
			return false
		}
		af, bf := a.packets[i].fields, b.packets[i].fields
		if len(af) != len(bf) {
			return false
		}
		for j := range af {
			if af[j] != bf[j] {
				return false
			}
		}
	}
	return true
}

// Deviations this server keeps on purpose (COMPARE.md "Recorded deviations").
var (
	devNoGUID = &deviation{
		why:           "docs/01 §1.5 step 2 [AS]: vanilla admits a client with no Steam guid",
		wantOurs:      "TCP 0x6F AuthFailed",
		wantReference: "TCP 0x3E NewCarConnection",
	}
	devEmptyName = &deviation{
		why:           "docs/01 §1.5 step 6 [AS]: vanilla admits a client with a blank driver name",
		wantOurs:      "TCP 0x6F AuthFailed",
		wantReference: "TCP 0x3E NewCarConnection",
	}
	devTruncatedHandshake = &deviation{
		why:           "same blank-name check as RejectEmptyName; vanilla falls through to the empty car model",
		wantOurs:      "TCP 0x6F AuthFailed",
		wantReference: "TCP 0x45 NoSlotsAvailable",
	}
	devCarListPage = &deviation{
		why:           "vanilla over-reads its entry array for an out-of-range 0x3F page; this server clamps",
		wantOurs:      "TCP 0x40 CarList",
		wantReference: "TCP 0x40 CarList",
		alsoDiffer:    true,
	}
	devHTTPUnknownPath = &deviation{
		why:           "vanilla answers 200 + empty body for any unrouted path; this server 404s",
		wantOurs:      "HTTP unknown path",
		wantReference: "HTTP unknown path",
		alsoDiffer:    true,
	}
	devHTTPEntryEscaped = &deviation{
		why:           "vanilla writes driver names into /ENTRY's HTML verbatim; this server escapes them",
		wantOurs:      "HTTP /ENTRY",
		wantReference: "HTTP /ENTRY",
		alsoDiffer:    true,
	}
	devVoteNoTarget = &deviation{
		why:           "vanilla announces a kick vote against an empty slot or the voter itself; this server drops it",
		wantOurs:      "TCP <no vote broadcast>",
		wantReference: "TCP 0x66 VoteKickUser",
	}
	// The official server guide defines IS_OPEN for pickup sessions, including
	// the race-only value 2 cutoff. The reference binary ignores it while pickup
	// mode is enabled; this implementation follows the published rule.
	devSessionClosed = &deviation{
		why:           "official guide: IS_OPEN=0 closes the session to joins",
		wantOurs:      "TCP 0x6E SessionClosed",
		wantReference: "TCP 0x3E NewCarConnection",
	}
	devKickEmptySlot = &deviation{
		why:           "vanilla kicks an unoccupied slot and announces it with a blank driver name; this server refuses",
		wantOurs:      "TCP 0x47 Chat",
		wantReference: "TCP 0x47 Chat",
		alsoDiffer:    true,
	}
	devChatSpoof = &deviation{
		why:           "vanilla relays the claimed 0x47 sender id, letting a player impersonate another; this server substitutes the real one",
		wantOurs:      "TCP 0x47 Chat",
		wantReference: "TCP 0x47 Chat",
		alsoDiffer:    true,
	}
	devReconfig = &deviation{
		why:           "both discard /RECONFIG's settings; ours replies with serde's error text, and its HTTP library answers 400 to a raw '\"' in the query",
		wantOurs:      `HTTP /RECONFIG?json={"Name":"x","RaceLaps":3}`,
		wantReference: `HTTP /RECONFIG?json={"Name":"x","RaceLaps":3}`,
		alsoDiffer:    true,
	}
	devPositionNaN = &deviation{
		why:           "vanilla relays a NaN position to every other client; this server drops the update",
		wantOurs:      "verdict: NaN 0x46 relayed",
		wantReference: "verdict: NaN 0x46 relayed",
		alsoDiffer:    true,
	}
	devChatControl = &deviation{
		why:           "vanilla relays control characters in chat verbatim; this server strips them",
		wantOurs:      "TCP 0x47 Chat",
		wantReference: "TCP 0x47 Chat",
		alsoDiffer:    true,
	}
	devChatBlank = &deviation{
		why:           "vanilla relays a whitespace-only chat line; this server drops it",
		wantOurs:      "<none>",
		wantReference: "TCP 0x47 Chat",
	}
	devImplausibleLap = &deviation{
		why:           "vanilla accepts a sub-second lap time as a best lap; this server records the lap but not the best",
		wantOurs:      "TCP 0x49 LapCompleted",
		wantReference: "TCP 0x49 LapCompleted",
		alsoDiffer:    true,
	}
	devDuplicateGUID = &deviation{
		why:           "vanilla seats an already-connected guid a second time, leaving a ghost; this server drops the old connection",
		wantOurs:      "TCP 0x3E NewCarConnection",
		wantReference: "TCP 0x3E NewCarConnection",
		alsoDiffer:    true,
	}
	devHandshakeTwice = &deviation{
		why:           "vanilla re-runs the join for a repeated 0x3D and seats the connection in a second slot; this server ignores it",
		wantOurs:      "TCP <no answer>",
		wantReference: "TCP 0x3E NewCarConnection",
	}
	devClientList = &deviation{
		why:           "vanilla advertises /client_list in /help but never implemented it; this server does",
		wantOurs:      "TCP 0x47 Chat",
		wantReference: "TCP 0x47 Chat",
		alsoDiffer:    true,
	}
)

// probes is the full comparison matrix. Add a probe here to compare a new
// exchange against the reference binary.
func probes() []probe {
	return []probe{
		{name: "UnsupportedProtocol", run: probeUnsupported},
		{name: "Handshake", run: probeHandshake},
		{name: "CarList", run: probeCarList},
		{name: "LobbyCheck", run: probeLobbyCheck},
		{name: "HTTPInfo", run: probeHTTPInfo},
		{name: "HTTPEntry", run: entryProbe("Alpha")},
		{name: "HTTPEntryEscaped", run: entryProbe("<b>A&B</b>"), dev: devHTTPEntryEscaped},
		{name: "ManagerEndpoints", run: probeManagerEndpoints},
		{name: "ManagerReconfig", run: probeManagerReconfig, dev: devReconfig},
		{name: "BookingFlow", run: probeBookingFlow, cfg: cfgBooking},
		{name: "EntryListReload", run: probeEntryListReload, cfg: cfgEntryListReload},
		{name: "HTTPInfoPracticeOnly", run: probeHTTPInfo, cfg: cfgPracticeOnly},
		{name: "HTTPInfoTimedRace", run: probeHTTPInfo, cfg: cfgTimedRacePitStart},

		// Handshake rejection paths.
		{name: "RejectNoGUID", run: probeRejectNoGUID, dev: devNoGUID},
		{name: "RejectEmptyName", run: probeRejectEmptyName, dev: devEmptyName},
		{name: "RejectUnknownCar", run: probeRejectUnknownCar},
		{name: "RejectNoSlots", run: probeRejectNoSlots},
		{name: "RejectWrongPassword", run: probeWrongPassword, cfg: cfgPassword},
		{name: "AcceptPassword", run: probeRightPassword, cfg: cfgPassword},
		{name: "AcceptAdminPassword", run: probeAdminPassword, cfg: cfgPassword},
		{name: "RejectBlacklisted", run: probeBlacklisted, cfg: cfgBlacklist},

		// In-world behaviour.
		{name: "UDPAssociation", run: probeUDPAssociation},
		{name: "UDPReassociation", run: probeUDPReassociation},
		{name: "PingsAfterLoad", run: probePingsAfterLoad},
		{name: "PingAverageNoRamp", run: probePingAverageNoRamp},
		{name: "HandshakeSeedAndClock", run: probeHandshakeSeedAndClock},
		{name: "FirstUpdate", run: probeFirstUpdate},
		{name: "SecondClientJoin", run: probeSecondClientJoin},
		{name: "ChatBroadcast", run: probeChatBroadcast},
		{name: "ChatBurst", run: probeChatBurst},
		{name: "CarDisconnect", run: probeCarDisconnect},
		{name: "SessionRequestResync", run: probeSessionResync},
		{name: "SessionRequestMatching", run: probeSessionRequestMatching},
		{name: "CarListOccupied", run: probeCarListOccupied},

		// Config-sensitive surface.
		{name: "WelcomeMessage", run: probeFirstUpdate, cfg: cfgWelcome},
		{name: "FirstUpdateExtras", run: probeFirstUpdate, cfg: cfgFirstUpdateExtras},
		{name: "HandshakeFixedSetup", run: probeHandshake, cfg: cfgMultiCar},
		{name: "CarListMultiModel", run: probeCarList, cfg: cfgMultiCar},

		// HTTP surface beyond /INFO.
		{name: "HTTPJson", run: probeHTTPJSON},
		{name: "HTTPJsonOccupied", run: probeHTTPJSONOccupied},
		{name: "HTTPInfoOccupied", run: probeHTTPInfoOccupied},
		{name: "INISlashComments", run: probeHTTPInfo, cfg: cfgSlashComments},
		{name: "HTTPUnknownPath", run: probeHTTPUnknownPath, dev: devHTTPUnknownPath},

		// Checksums.
		{name: "ChecksumValid", run: probeChecksumValid},
		{name: "ChecksumInvalid", run: probeChecksumInvalid},

		// In-world telemetry relays, seen by the other player.
		{name: "RelayLapCompleted", run: relayProbe(lapCompleted)},
		{name: "RelayLapWithCut", run: relayProbe(lapCompletedWithCut)},
		{name: "RelaySectorSplit", run: relayProbe(sectorSplit)},
		{name: "RelayTyreCompound", run: relayProbe(tyreCompound)},
		{name: "RelayDamage", run: relayProbe(damageUpdate)},
		{name: "RelayP2PActivate", run: relayProbe(p2pActivate)},
		{name: "RelayMandatoryPit", run: relayProbe(mandatoryPit)},
		{name: "RelayJumpStart", run: relayProbe(jumpStartEvent)},
		{name: "RelayCollisionEnv", run: relayProbe(collisionWithEnv)},
		{name: "RelayCollisionCar", run: relayProbe(collisionWithCar)},
		{name: "LeaderboardOrder", run: probeLeaderboardOrder},
		{name: "BestLapProgression", run: probeBestLapProgression},
		{name: "LeaderboardSyncAfterLap", run: probeLeaderboardSyncAfterLap},
		{name: "LeaderboardAfterDisconnect", run: probeLeaderboardAfterDisconnect},
		{name: "SlotReuseResult", run: probeSlotReuseResult},
		{name: "RepeatedLap", run: probeRepeatedLap},
		{name: "FirstUpdateRaceState", run: probeFirstUpdateRaceState},
		{name: "RelayIllegalTyre", run: relayProbe(illegalTyreCompound)},
		{name: "RelaySectorSplitOOB", run: relayProbe(sectorSplitOOB)},
		{name: "RelayLapNoSplits", run: relayProbe(lapNoSplits)},
		{name: "NextSessionWhileLoading", run: probeNextSessionWhileLoading},
		{name: "LoadingGetsNoBroadcast", run: probeLoadingGetsNoBroadcast},
		{name: "ClientTimeRequest", run: probeClientTimeRequest},
		{name: "ClientSentBlacklisted", run: probeClientSentBlacklisted},

		// Balance of performance.
		{name: "FirstUpdateEntryBoP", run: probeFirstUpdate, cfg: cfgEntryBoP},
		{name: "BoPBeforeJoin", run: probeBoPBeforeJoin},

		// Voting.
		{name: "VoteNextSessionSolo", run: soloVoteProbe(proto.VoteNextSession)},
		{name: "VoteRestartSessionSolo", run: soloVoteProbe(proto.VoteRestartSession)},
		{name: "VoteNoQuorum", run: probeVoteNoQuorum},
		{name: "VoteKickNoQuorum", run: probeVoteKickNoQuorum},
		{name: "VoteKickPasses", run: probeVoteKickPasses, cfg: cfgFastKickVote},
		{name: "VoteKickSelf", run: probeVoteKickSelf, dev: devVoteNoTarget},
		{name: "VoteKickEmptySlot", run: probeVoteKickEmptySlot, dev: devVoteNoTarget},
		{name: "VoteExpiry", run: probeVoteExpiry, cfg: cfgShortVote},
		{name: "VotePassesAtExpiry", run: probeVotePassesAtExpiry, cfg: cfgShortVote},
		{name: "VoteQuorumAfterLeave", run: probeVoteQuorumAfterLeave, cfg: cfgShortVote},
		{name: "VoteAgainst", run: probeVoteAgainst},
		{name: "VoteAgainstAfterYes", run: probeVoteAgainstAfterYes},
		{name: "VoteChangedMind", run: probeVoteChangedMind},
		{name: "VoteTallyTwoYes", run: probeVoteTallyTwoYes},
		{name: "VoteQuorumFourDrivers", run: probeVoteQuorumFourDrivers},
		{name: "VoteKickQuorumThree", run: probeVoteKickQuorumThree},
		{name: "VoteDuration", run: probeVoteDuration},
		{name: "VoteCompetingKind", run: probeVoteCompetingKind},
		{name: "VoteKickRetarget", run: probeVoteKickRetarget},
		{name: "VoteKickThenRejoin", run: probeVoteKickThenRejoin, cfg: cfgFastKickVote},
		{name: "VoteAcrossSessionChange", run: probeVoteAcrossSessionChange, cfg: cfgShortVote},

		// Non-ASCII identities and chat.
		{name: "UnicodeName", run: probeUnicodeName},
		{name: "UnicodeJoinBroadcast", run: probeUnicodeJoinBroadcast},
		{name: "UnicodeChat", run: probeUnicodeChat},
		{name: "ChatMaxLength", run: probeChatMaxLength},
		{name: "ChatSurrogatePair", run: probeChatSurrogates},
		{name: "NameSurrogatePair", run: probeNameSurrogates},
		{name: "Latin1Identity", run: probeLatin1Identity},

		// The client's bare jump start (compare_client_probes_test.go).
		{name: "RelayJumpStartBare", run: probeJumpStartBare},
		{name: "EchoJumpStartBare", run: selfRelayProbe(jumpStartBare)},

		// Admission and per-car state.
		{name: "HandshakeAltRules", run: probeHandshake, cfg: cfgAltRules},
		{name: "PickupModeOff", run: probePickupOff, cfg: cfgPickupOff},
		{name: "MaxClientsCap", run: probeMaxClientsCap, cfg: cfgOneClient},
		{name: "MaxClientsGrid", run: probeMaxClientsGrid, cfg: cfgThreeOfTwelve},
		{name: "KickThenRejoin", run: rejoinProbe("/kick_id 1")},
		{name: "BanThenRejoin", run: rejoinProbe("/ban_id 1")},
		{name: "CarListWindow", run: probeCarListWindow, cfg: cfgTwelveCars},
		{name: "CarListAfterDamage", run: probeCarListAfterDamage},
		{name: "ChatWhileLoading", run: probeChatWhileLoading},
		{name: "ChatSpoofedSender", run: probeChatSpoofedSender, dev: devChatSpoof},
		{name: "SessionChangeResetsCarState", run: probeSessionChangeResetsCarState},

		// Session-scoped results.
		{name: "NextSessionClearsLeaderboard", run: sessionChangeProbe("/next_session")},
		{name: "RestartClearsLeaderboard", run: sessionChangeProbe("/restart_session")},
		{name: "QualifyToRaceGrid", run: probeQualifyToRaceGrid, cfg: cfgQualifyRace},

		// The same relays as the sender sees them (is the sender echoed?).
		{name: "EchoDamage", run: selfRelayProbe(damageUpdate)},
		{name: "EchoP2PQuery", run: selfRelayProbe(p2pQuery)},
		{name: "EchoTyreCompound", run: selfRelayProbe(tyreCompound)},
		{name: "EchoLap", run: selfRelayProbe(lapCompleted)},
		{name: "EchoLapWithCut", run: selfRelayProbe(lapCompletedWithCut)},
		{name: "EchoSectorSplit", run: selfRelayProbe(sectorSplit)},
		{name: "EchoSectorSplitOOB", run: selfRelayProbe(sectorSplitOOB)},
		{name: "EchoP2PActivate", run: selfRelayProbe(p2pActivate)},
		{name: "EchoMandatoryPit", run: selfRelayProbe(mandatoryPit)},
		{name: "EchoJumpStart", run: selfRelayProbe(jumpStartEvent)},
		{name: "EchoCollisionEnv", run: selfRelayProbe(collisionWithEnv)},
		{name: "EchoCollisionCar", run: selfRelayProbe(collisionWithCar)},
		{name: "EchoIllegalTyre", run: selfRelayProbe(illegalTyreCompound)},

		// UDP steady state.
		{name: "PositionBroadcast", run: probePositionBroadcast},
		{name: "PositionStaleTimestamp", run: probeStaleTimestamp},
		{name: "PositionTimestampRebase", run: probeTimestampRebase},
		{name: "PositionOffsetSlew", run: probeOffsetSlew},
		{name: "PositionRelayEachUpdate", run: probeRelayEachUpdate},
		{name: "PositionBurstMerges", run: probeRelayBurstMerges},
		{name: "PositionImplausible", run: probeImplausiblePosition, dev: devPositionNaN},
		{name: "PingUpdate", run: probePingUpdate},

		// Entry-list shapes.
		{name: "CarListPageOutOfRange", run: probeCarListPageOutOfRange, dev: devCarListPage},
		{name: "ReservedSlotOwner", run: probeReservedSlotOwner, cfg: cfgReservedSlot},
		{name: "ReservedSlotOther", run: probeReservedSlotOther, cfg: cfgReservedSlot},
		{name: "ReservedSlotJson", run: probeHTTPJSON, cfg: cfgReservedSlot},
		{name: "SessionClosed", run: probeSessionClosed, cfg: cfgClosedSessions, dev: devSessionClosed},

		// Admin commands: reply wording, packet order, side-effect broadcasts.
		{name: "CmdHelp", run: commandProbe("/help")},
		{name: "CmdAdminOK", run: nonAdminCommandProbe("/admin " + adminPassword)},
		{name: "CmdAdminWrong", run: nonAdminCommandProbe("/admin wrongpassword")},
		{name: "CmdUnknown", run: commandProbe("/bogus_cmd")},
		{name: "CmdAdminOnlyAsPlayer", run: nonAdminCommandProbe("/next_session")},
		{name: "CmdBallast", run: commandProbe("/ballast 0 50")},
		{name: "CmdRestrictor", run: commandProbe("/restrictor 0 20")},
		{name: "CmdBallastBadCar", run: commandProbe("/ballast 9 50")},
		{name: "CmdBallastNoArgs", run: commandProbe("/ballast")},
		{name: "CmdRestrictorNoArgs", run: commandProbe("/restrictor")},
		{name: "CmdBallastOutOfRange", run: commandProbe("/ballast 0 9999")},
		{name: "CmdRestrictorNegative", run: commandProbe("/restrictor 0 -5")},
		{name: "CmdKickIdEmptySlot", run: commandProbe("/kick_id 2"), dev: devKickEmptySlot},
		{name: "CmdKickIdOutOfRange", run: commandProbe("/kick_id 99")},
		{name: "CmdUpperCase", run: commandProbe("/HELP")},
		{name: "CmdTrailingSpace", run: commandProbe("/help ")},
		{name: "CmdKickUnknownName", run: commandProbe("/kick NoSuchDriver")},
		{name: "CmdClientList", run: commandProbe("/client_list"), dev: devClientList},
		{name: "CmdClientListTwo", run: clientListWithTwoPlayers, dev: devClientList},
		{name: "CmdKickByName", run: kickProbe("/kick Bravo")},
		{name: "CmdKickByID", run: kickProbe("/kick_id 1")},
		{name: "CmdBanByID", run: kickProbe("/ban_id 1")},

		// Session transitions.
		{name: "CmdNextSession", run: commandProbe("/next_session")},
		{name: "CmdKsns", run: commandProbe("/ksns")},
		{name: "CmdRestartSession", run: commandProbe("/restart_session")},
		{name: "CmdKsrs", run: commandProbe("/ksrs")},
		{name: "RaceStart", run: probeRaceStart, cfg: cfgOneLapRace},
		{name: "RaceLapRules", run: probeRaceLapRules, cfg: cfgTwoLapRace},
		{name: "RaceOverCap", run: probeRaceOverCap, cfg: cfgTwoLapRace},
		{name: "TimedRaceFinish", run: timedRaceProbe, cfg: cfgTimedRace},
		{name: "TimedRaceExtraLap", run: timedRaceProbe, cfg: cfgTimedRaceExtraLap},
		{name: "SunInCountdown", run: probeSunInCountdown, cfg: cfgRaceLongWait},
		{name: "ReversedRaceRepeat", run: probeReversedRaceRepeat, cfg: cfgPracticeThenReversedRace},
		{name: "EmptyRaceSkipped", run: probeEmptyRaceSkipped, cfg: cfgPracticeThenClosedRace},
		{name: "NoLoopShutsDown", run: probeNoLoopShutsDown, cfg: cfgNoLoop},
		{name: "PracticeLapCount", run: probePracticeLapCount},
		{name: "ResultScreenFloor", run: probeResultScreenFloor, cfg: cfgOneMinutePracticeShortResult},
		{name: "RaceStartCountdown", run: probeRaceStartCountdown, cfg: cfgOneLapRace},
		{name: "PracticeIgnoresWaitTime", run: probeStartVersusNow, cfg: cfgPracticeWait},
		{name: "SessionClockOffset", run: probeSessionClockOffset},
		{name: "SunOnSessionSwitch", run: probeSunOnSessionSwitch},
		{name: "SlotHandover", run: probeSlotHandover},
		{name: "LeaderboardHoldsOrder", run: probeLeaderboardHoldsOrder},
		{name: "VoteDurationExact", run: probeVoteDurationExact},
		{name: "DepartedDriverListings", run: probeDepartedDriverListings},
		{name: "BoPResetOnJoin", run: probeBoPResetOnJoin},
		{name: "StepTimers", run: probeStepTimers},
		{name: "SessionExpiry", run: probeSessionExpiry, cfg: cfgShortPractice},

		// Exploratory: hardening this server applies that vanilla does not.
		{name: "ChatControlChars", run: probeChatControlChars, dev: devChatControl},
		{name: "ChatWhitespaceOnly", run: probeChatWhitespaceOnly, dev: devChatBlank},
		{name: "ImplausibleLap", run: probeImplausibleLap, dev: devImplausibleLap},
		{name: "DuplicateGUID", run: probeDuplicateGUID, dev: devDuplicateGUID},
		{name: "HandshakeTwice", run: probeHandshakeTwice, dev: devHandshakeTwice},
		{name: "CmdBallastNonNumericID", run: commandProbe("/ballast alpha 50")},
		{name: "CmdBallastNonNumericValue", run: commandProbe("/ballast 0 bravo")},
		{name: "CmdBallastExtraArg", run: commandProbe("/ballast 0 50 99")},
		{name: "CmdRestrictorNonNumeric", run: commandProbe("/restrictor alpha bravo")},
		{name: "CmdKickIdNegative", run: commandProbe("/kick_id -1")},
		{name: "CmdAdminNoArgs", run: nonAdminCommandProbe("/admin")},

		// Malformed input: neither server may crash, hang, or diverge.
		{name: "TruncatedHandshake", run: probeTruncatedHandshake, dev: devTruncatedHandshake},
		{name: "UnknownPacketID", run: probeUnknownPacketID},
		{name: "PreHandshakePacket", run: probePreHandshakePacket},
	}
}

// ---------------------------------------------------------------------------
// Config tweaks
// ---------------------------------------------------------------------------

const testPassword = "letmein"

// cfgPassword puts the server behind a join password.
func cfgPassword(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setSTR(s, "PASSWORD", testPassword)
	})
}

// cfgBlacklist drops a blacklist.txt naming the probe's guid next to the
// server, which is where vanilla acServer reads it from.
func cfgBlacklist(t *testing.T, root, cfgDir string) {
	writeFile(t, filepath.Join(root, "blacklist.txt"), probeGUID+"\n")
	writeFile(t, filepath.Join(cfgDir, "blacklist.txt"), probeGUID+"\n")
}

// cfgWelcome sets a WELCOME_MESSAGE file. The Go target runs in-process with
// the test package as its cwd, so the file also goes in the cfg dir.
// cfgSlashComments puts "//" comments in server_cfg.ini values. Vanilla's
// ks/ini cuts each line at "//", so both servers must report the name and
// MAX_CLIENTS without the comment.
func cfgSlashComments(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = strings.Replace(s, "NAME=MX-5 Cup - Brands Hatch", "NAME=MX-5 Cup//see http://example.com", 1)
		return strings.Replace(s, "MAX_CLIENTS=12", "MAX_CLIENTS=10// ten", 1)
	})
}

// cfgFirstUpdateExtras gives the first update every optional tail packet: a
// welcome message, a fixed setup for slot 0 and Imola's DRS zones.
func cfgFirstUpdateExtras(t *testing.T, root, cfgDir string) {
	cfgWelcome(t, root, cfgDir)
	// One section, with no VALUE: vanilla sends it as 0. (Two would come out
	// in vanilla's random map order.)
	const setup = "[CAR]\nMODEL=" + probeCar + "\n"
	for _, dir := range []string{root, cfgDir} {
		if err := os.MkdirAll(filepath.Join(dir, "setups"), 0o755); err != nil {
			t.Fatal(err)
		}
		writeFile(t, filepath.Join(dir, "setups", "probe.ini"), setup)
	}
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setSTR(s, "TRACK", "imola")
		return setSTR(s, "CONFIG_TRACK", "")
	})
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		return strings.Replace(s, "[CAR_0]\n", "[CAR_0]\nFIXED_SETUP=probe.ini\n", 1)
	})
}

func cfgWelcome(t *testing.T, root, cfgDir string) {
	const body = "Hello from the comparison suite\nSecond line\n"
	writeFile(t, filepath.Join(root, "welcome.txt"), body)
	writeFile(t, filepath.Join(cfgDir, "welcome.txt"), body)
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setSTR(s, "WELCOME_MESSAGE", "welcome.txt")
	})
}

// cfgReservedSlot reserves the first entry-list slot for probeGUID.
func cfgReservedSlot(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		return strings.Replace(s, "[CAR_0]\nMODEL=", "[CAR_0]\nGUID="+probeGUID+"\nMODEL=", 1)
	})
}

// sessionBlock matches the [PRACTICE]…[RACE] run of session sections plus the
// [DYNAMIC_TRACK] header that follows it. Go's RE2 has no lookahead, so the
// trailing header is captured and put back by the replacement.
var sessionBlock = regexp.MustCompile(`(?s)\[PRACTICE\].*\[DYNAMIC_TRACK\]`)

// setSessions replaces the whole session block with the given sections.
func setSessions(cfgDir string, t *testing.T, sections string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return sessionBlock.ReplaceAllString(s, sections+"[DYNAMIC_TRACK]")
	})
}

// cfgOneLapRace makes the server a single one-lap race, so the race can be run
// to its end inside a probe.
func cfgOneLapRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[RACE]\nNAME=Race\nLAPS=1\nWAIT_TIME=0\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "RACE_OVER_TIME", 5)
		return setINT(s, "RESULT_SCREEN_TIME", 5)
	})
}

// cfgShortPractice runs a one-minute practice followed by a qualify, with a
// short result screen, so a probe can watch a real session expire and advance.
// One minute is the floor: vanilla's TIME is in whole minutes.
func cfgShortPractice(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t,
		"[PRACTICE]\nNAME=Practice\nTIME=1\nIS_OPEN=1\n\n"+
			"[QUALIFY]\nNAME=Qualify\nTIME=10\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "RESULT_SCREEN_TIME", 10)
	})
}

// cfgClosedSessions closes every session, so no pickup join is allowed.
func cfgClosedSessions(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "IS_OPEN", 0)
	})
}

// cfgMultiCar widens the entry list to two car models so model-dependent
// surfaces (checksum path list, car list, slot allocation) are exercised with
// more than one model present.
func cfgMultiCar(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setSTR(s, "CARS", "ks_mazda_mx5_cup;ks_toyota_ae86")
	})
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		return s + "\n[CAR_3]\nMODEL=ks_toyota_ae86\nSKIN=00_official\nSPECTATOR_MODE=0\nBALLAST=0\nRESTRICTOR=0\n"
	})
}

// cfgLowKickQuorum sets KICK_QUORUM to the floor, so one yes carries a kick on
// a two-car grid.
func cfgLowKickQuorum(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "KICK_QUORUM", 1)
	})
}

// cfgFastKickVote combines the two: a kick vote that both carries on a single
// yes and settles within a probe's patience. A vote is only decided when its
// clock runs out, so KICK_QUORUM alone is not enough to watch a kick land.
func cfgFastKickVote(t *testing.T, root, cfgDir string) {
	cfgLowKickQuorum(t, root, cfgDir)
	cfgShortVote(t, root, cfgDir)
}

// cfgShortVote shortens VOTE_DURATION so a probe can wait out a vote that never
// reaches quorum in a few seconds instead of the configured twenty.
func cfgShortVote(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "VOTE_DURATION", 3)
	})
}

// cfgEntryBoP puts ballast and a restrictor on the *first* entry-list slot only
// (the first BALLAST=0/RESTRICTOR=0 pair in the file belongs to CAR_0), so a
// probe can check per-slot balance-of-performance reaches its occupant.
func cfgEntryBoP(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		return strings.Replace(s, "BALLAST=0\nRESTRICTOR=0", "BALLAST=50\nRESTRICTOR=20", 1)
	})
}

// cfgQualifyRace runs a qualifying session followed by a short race, so a probe
// can set a qualifying order and then read the grid the race starts on.
func cfgQualifyRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t,
		"[QUALIFY]\nNAME=Qualify\nTIME=10\nIS_OPEN=1\n\n"+
			"[RACE]\nNAME=Race\nLAPS=3\nWAIT_TIME=0\nIS_OPEN=1\n\n")
}

// cfgAltRules sets every driving rule to a non-default value, catching a key
// read under the wrong name, scaled wrongly, or written in the wrong slot.
func cfgAltRules(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		for k, v := range map[string]int{
			"ABS_ALLOWED":                  2,
			"TC_ALLOWED":                   2,
			"STABILITY_ALLOWED":            1,
			"AUTOCLUTCH_ALLOWED":           0,
			"TYRE_BLANKETS_ALLOWED":        1,
			"FORCE_VIRTUAL_MIRROR":         0,
			"FUEL_RATE":                    150,
			"DAMAGE_MULTIPLIER":            50,
			"TYRE_WEAR_RATE":               200,
			"ALLOWED_TYRES_OUT":            4,
			"MAX_CONTACTS_PER_KM":          5,
			"RACE_OVER_TIME":               30,
			"RESULT_SCREEN_TIME":           20,
			"JUMP_START_PENALTY":           1,
			"RACE_EXTRA_LAP":               1,
			"RACE_GAS_PENALTY_DISABLED":    1,
			"RACE_PIT_WINDOW_START":        10,
			"RACE_PIT_WINDOW_END":          30,
			"REVERSED_GRID_RACE_POSITIONS": 2,
			"CLIENT_SEND_INTERVAL_HZ":      15,
			"SUN_ANGLE":                    -40,
		} {
			s = setINT(s, k, v)
		}
		return setSTR(s, "LEGAL_TYRES", "SM")
	})
}

// cfgPickupOff turns off pickup mode, which is what decides whether a driver may
// join a session already under way.
func cfgPickupOff(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "PICKUP_MODE_ENABLED", 0)
	})
}

// cfgOneClient caps the server at a single client while leaving three slots in
// the entry list, so the cap and the slot count disagree.
func cfgOneClient(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "MAX_CLIENTS", 1)
	})
}

// cfgTwelveCars widens the entry list past one car-list page (ten cars), so
// paging can be exercised at a non-zero offset.
func cfgTwelveCars(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		for i := 3; i < 12; i++ {
			s += fmt.Sprintf("\n[CAR_%d]\nMODEL=%s\nSKIN=00_official\nSPECTATOR_MODE=0\nBALLAST=0\nRESTRICTOR=0\n",
				i, probeCar)
		}
		return s
	})
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "MAX_CLIENTS", 12)
	})
}

// probeGUID is the Steam guid every probe joins with unless it needs a second
// distinct identity (see probeGUID2).
const (
	probeGUID  = "76561198000000001"
	probeGUID2 = "76561198000000002"
	probeGUID3 = "76561198000000003"
	probeGUID4 = "76561198000000004"
	probeCar   = "ks_mazda_mx5_cup"
)

// join describes a 0x3D RequestNewConnection. The zero value is not useful;
// build one with joinAs so the defaults stay in one place.
type join struct {
	guid     string
	name     string
	car      string
	password string
}

// joinAs returns a valid join for the given driver name, which probes then
// mutate to exercise a specific rejection path.
func joinAs(name string) join {
	return join{guid: probeGUID, name: name, car: probeCar}
}

// encode builds the 0x3D packet, matching a real vanilla client join.
func (j join) encode() []byte {
	w := wire.NewWriterID(proto.RequestNewConnection)
	w.UInt16(proto.ClientProtocolVersion)
	w.String8(j.guid)
	w.WString8(j.name)
	w.String8("TestTeam")
	w.String8("GBR")
	w.String8(j.car)
	w.String8(j.password)
	return w.Bytes()
}

// handshakeRequest builds a valid 0x3D for the given driver.
func handshakeRequest(name string) []byte { return joinAs(name).encode() }

// joinAndCapture joins and captures whatever answer comes back, so a wrong
// rejection shows as a diff rather than a timeout.
func joinAndCapture(t *testing.T, tgt *target, k *kit, j join) capture {
	k.sendTCP(j.encode())

	var cap capture
	pkt, ok := k.readTCP(3 * time.Second)
	if !ok {
		t.Errorf("%s: no handshake response", tgt.label)
		cap.add(packet{label: "TCP <none>"})
		return cap
	}
	cap.add(decodeTCP(pkt))
	return cap
}

// probeUnsupported sends a handshake with the wrong protocol version and
// captures the 0x42 rejection.
func probeUnsupported(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	w := wire.NewWriterID(proto.RequestNewConnection)
	w.UInt16(1) // wrong version
	w.String8("76561198000000001")
	w.WString8("Nope")
	w.String8("")
	w.String8("GBR")
	w.String8("ks_mazda_mx5_cup")
	w.String8("")
	k.sendTCP(w.Bytes())

	var cap capture
	if pkt, ok := k.awaitTCP(proto.UnsupportedProtocol, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		t.Errorf("%s: no 0x42 UnsupportedProtocol", tgt.label)
	}
	return cap
}

// probeHandshake performs a valid join and captures the 0x3E response.
func probeHandshake(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	k.sendTCP(handshakeRequest("CmpDriver"))

	var cap capture
	if pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		t.Errorf("%s: no 0x3E NewCarConnection", tgt.label)
	}
	return cap
}

// probeCarList joins, then requests the car list (0x3F) and captures 0x40.
func probeCarList(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	k.sendTCP(handshakeRequest("CmpDriver"))
	if _, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second); !ok {
		t.Errorf("%s: handshake failed before car list", tgt.label)
		return capture{}
	}

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0) // page index
	k.sendTCP(req.Bytes())

	var cap capture
	if pkt, ok := k.awaitTCP(proto.CarList, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		t.Errorf("%s: no 0x40 CarList", tgt.label)
	}
	return cap
}

// ---------------------------------------------------------------------------
// Handshake rejection paths
// ---------------------------------------------------------------------------

// probeRejectNoGUID joins without a Steam guid (what AC Pro would send).
func probeRejectNoGUID(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("NoGuid")
	j.guid = ""
	return joinAndCapture(t, tgt, k, j)
}

// probeRejectEmptyName joins with a blank driver name.
func probeRejectEmptyName(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("")
	return joinAndCapture(t, tgt, k, j)
}

// probeRejectUnknownCar requests a car model that is not in the entry list.
func probeRejectUnknownCar(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("WrongCar")
	j.car = "ks_ferrari_488_gt3"
	return joinAndCapture(t, tgt, k, j)
}

// probeRejectNoSlots fills every entry-list slot, then joins once more.
func probeRejectNoSlots(t *testing.T, tgt *target) capture {
	guids := []string{probeGUID, probeGUID2, probeGUID3}
	for i, g := range guids {
		k := dial(t, tgt)
		defer k.close()
		j := joinAs(fmt.Sprintf("Filler%d", i))
		j.guid = g
		k.sendTCP(j.encode())
		if _, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second); !ok {
			t.Errorf("%s: filler %d could not join", tgt.label, i)
			return capture{}
		}
	}

	k := dial(t, tgt)
	defer k.close()
	j := joinAs("TooLate")
	j.guid = "76561198000000009"
	return joinAndCapture(t, tgt, k, j)
}

// probeWrongPassword joins a password-protected server with the wrong password.
func probeWrongPassword(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("BadPass")
	j.password = "nope"
	return joinAndCapture(t, tgt, k, j)
}

// probeRightPassword joins a password-protected server correctly.
func probeRightPassword(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("GoodPass")
	j.password = testPassword
	return joinAndCapture(t, tgt, k, j)
}

// probeAdminPassword joins a password-protected server with the *admin*
// password, which vanilla accepts in place of the join password.
func probeAdminPassword(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	j := joinAs("AdminPass")
	j.password = "admin123" // ADMIN_PASSWORD from the repo cfg
	return joinAndCapture(t, tgt, k, j)
}

// probeBlacklisted joins with a guid listed in blacklist.txt.
func probeBlacklisted(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	return joinAndCapture(t, tgt, k, joinAs("Banned"))
}

// probeLobbyCheck sends the 0xC8 UDP reachability probe and captures the reply.
func probeLobbyCheck(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	k.sendUDP([]byte{proto.LobbyCheck})

	var cap capture
	if pkt, ok := k.readUDP(3 * time.Second); ok {
		cap.add(decodeUDP(pkt))
	} else {
		t.Errorf("%s: no 0xC8 LobbyCheck reply", tgt.label)
	}
	return cap
}

// ---------------------------------------------------------------------------
// In-world behaviour
// ---------------------------------------------------------------------------

// player is a joined client: its kit plus the session id the server assigned.
type player struct {
	k         *kit
	sessionID uint8
}

// joinPlayer performs a full handshake and returns the joined client. It fails
// the probe (and returns nil) if the handshake is refused.
func joinPlayer(t *testing.T, tgt *target, j join) *player {
	k := dial(t, tgt)
	k.sendTCP(j.encode())
	pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
	if !ok {
		t.Errorf("%s: %s: no 0x3E handshake response", tgt.label, j.name)
		k.close()
		return nil
	}
	return &player{k: k, sessionID: sessionIDFromHandshake(pkt)}
}

// sessionIDFromHandshake pulls the assigned session id out of a 0x3E payload by
// replaying the decode and reading the named field.
func sessionIDFromHandshake(pkt []byte) uint8 {
	for _, f := range decodeTCP(pkt).fields {
		if f.name == "sessionID" {
			var id uint8
			fmt.Sscan(f.value, &id)
			return id
		}
	}
	return 0
}

// enterWorld associates the player's UDP endpoint (0x4E) and sends the first
// position update, which is what triggers the server's first-update sequence.
// It returns the UDP association reply.
func (p *player) enterWorld(t *testing.T, tgt *target) ([]byte, bool) {
	w := wire.NewWriterID(proto.CarConnect)
	w.UInt8(p.sessionID)
	p.k.sendUDP(w.Bytes())

	reply, ok := p.k.readUDP(3 * time.Second)
	if !ok {
		t.Errorf("%s: no 0x4E UDP association reply", tgt.label)
		return nil, false
	}
	p.k.sendUDP(positionUpdate(0))
	return reply, true
}

// positionUpdate builds an inbound 0x46: a 57-byte core plus the trailing
// NormalizedPosition float (62 bytes including the packet ID).
func positionUpdate(seq uint8) []byte {
	w := wire.NewWriterID(proto.PositionUpdate)
	w.UInt8(seq)
	w.UInt32(1000) // Timestamp
	w.Vec3(wire.Vec3{X: 10, Y: 0, Z: 20})
	w.Vec3(wire.Vec3{X: 0, Y: 0, Z: 1})
	w.Vec3(wire.Vec3{X: 1, Y: 0, Z: 0})
	for i := 0; i < 4; i++ {
		w.UInt8(100) // TyreAngularSpeed
	}
	w.UInt8(128) // SteerAngle
	w.UInt8(128) // WheelAngle
	w.UInt16(3000)
	w.UInt8(2) // Gear
	w.UInt32(0)
	w.Int16(0)
	w.UInt8(50)     // Gas
	w.Float32(0.25) // NormalizedPosition
	return w.Bytes()
}

// firstUpdateIdle is how long a probe waits for the server to fall quiet before
// deciding a burst is over. The reference sends its sequence back to back, so a
// short gap is enough; the budget bounds a server that never stops.
const (
	firstUpdateIdle   = 700 * time.Millisecond
	firstUpdateBudget = 4 * time.Second
)

// probeUDPAssociation captures the reply to the 0x4E UDP association alone.
func probeUDPAssociation(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Assoc"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	w := wire.NewWriterID(proto.CarConnect)
	w.UInt8(p.sessionID)
	p.k.sendUDP(w.Bytes())

	if reply, ok := p.k.readUDP(3 * time.Second); ok {
		cap.add(decodeUDP(reply))
	} else {
		t.Errorf("%s: no 0x4E association reply", tgt.label)
	}
	return cap
}

// probeFirstUpdate joins, enters the world, and captures the entire burst the
// server sends in response — welcome, weather, sun angle, the per-car state
// snapshot, and the leaderboard sync — in order.
func probeFirstUpdate(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Loader"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeSecondClientJoin captures what an already-in-world client is told when a
// second player joins and loads (0x5A CarConnected, 0x5B DriverInfoUpdate).
func probeSecondClientJoin(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("First"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) // discard A's own sequence

	second := joinAs("Second")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}

	// Only A's view is captured: B's own first-update sequence is already
	// covered by probeFirstUpdate, and A's view is what the join broadcast is.
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeChatBroadcast has one in-world player send chat and captures what the
// other receives.
func probeChatBroadcast(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	w := wire.NewWriterID(proto.Chat)
	w.UInt8(b.sessionID)
	w.WString8("hello world")
	b.k.sendTCP(w.Bytes())

	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeChatBurst sends three messages back to back from one in-world player.
// Vanilla rate-limits nothing, so all three must reach the other player; the
// probe fails if this server ever starts silently dropping the tail of a burst.
func probeChatBurst(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	for _, msg := range []string{"one", "two", "three"} {
		w := wire.NewWriterID(proto.Chat)
		w.UInt8(b.sessionID)
		w.WString8(msg)
		b.k.sendTCP(w.Bytes())
	}

	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeCarDisconnect has a player leave with 0x43 and captures the other's 0x4D.
// An abrupt close is not compared: vanilla announces nothing for 40 s or more,
// while this server frees the slot at once.
func probeCarDisconnect(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()

	b.k.sendTCP([]byte{proto.CleanExitDrive})
	time.Sleep(200 * time.Millisecond)
	b.k.close()
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// twoPlayersInWorld joins two players, brings both into the world, and drains
// the resulting bursts so the caller starts from a quiet connection.
func twoPlayersInWorld(t *testing.T, tgt *target) (*player, *player) {
	a := joinPlayer(t, tgt, joinAs("Alpha"))
	if a == nil {
		return nil, nil
	}
	if _, ok := a.enterWorld(t, tgt); !ok {
		a.k.close()
		return nil, nil
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Bravo")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		a.k.close()
		return nil, nil
	}
	if _, ok := b.enterWorld(t, tgt); !ok {
		a.k.close()
		b.k.close()
		return nil, nil
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	return a, b
}

// ---------------------------------------------------------------------------
// In-world telemetry relays
//
// One player sends a packet; the capture is what the other player is told.
// ---------------------------------------------------------------------------

// relayProbe runs the shared shape: two players in world, one sends, the other
// is captured. send receives the sending player so it can use its session id.
func relayProbe(send func(p *player) []byte) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a, b := twoPlayersInWorld(t, tgt)
		if a == nil || b == nil {
			return cap
		}
		defer a.k.close()
		defer b.k.close()

		b.k.sendTCP(send(b))
		for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// selfRelayProbe is relayProbe's mirror: it captures what the *sender* gets
// back, which is how "is the sender echoed?" is settled for each relay.
func selfRelayProbe(send func(p *player) []byte) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a, b := twoPlayersInWorld(t, tgt)
		if a == nil || b == nil {
			return cap
		}
		defer a.k.close()
		defer b.k.close()

		b.k.sendTCP(send(b))
		for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// lapCompleted builds a 0x49 lap report with three sector splits.
func lapCompleted(p *player) []byte {
	w := wire.NewWriterID(proto.LapCompleted)
	w.UInt32(120000) // timestamp
	w.UInt32(95430)  // lap time
	w.UInt8(3)       // split count
	w.UInt32(31000)
	w.UInt32(32000)
	w.UInt32(32430)
	w.UInt8(0) // cuts
	w.UInt8(1) // lap number
	return w.Bytes()
}

// lapCompletedWithCut is the same lap with a cut, which must not become a
// personal best.
func lapCompletedWithCut(p *player) []byte {
	w := wire.NewWriterID(proto.LapCompleted)
	w.UInt32(120000)
	w.UInt32(90000) // faster than the clean lap...
	w.UInt8(3)
	w.UInt32(30000)
	w.UInt32(30000)
	w.UInt32(30000)
	w.UInt8(2) // ...but cut twice
	w.UInt8(1)
	return w.Bytes()
}

func sectorSplit(p *player) []byte {
	w := wire.NewWriterID(proto.SectorSplit)
	w.UInt8(1)      // split index
	w.UInt32(31500) // split time
	w.UInt8(0)      // cuts
	return w.Bytes()
}

func tyreCompound(p *player) []byte {
	w := wire.NewWriterID(proto.TyreCompoundChange)
	w.String8("SV")
	return w.Bytes()
}

// lapNoSplits reports a lap with no splits; ignoring the count would misread
// the fields after it.
func lapNoSplits(p *player) []byte {
	w := wire.NewWriterID(proto.LapCompleted)
	w.UInt32(120000)
	w.UInt32(94000)
	w.UInt8(0) // no splits follow
	w.UInt8(0) // cuts
	w.UInt8(1) // lap number
	return w.Bytes()
}

// illegalTyreCompound fits a compound not in LEGAL_TYRES (SM;SV).
func illegalTyreCompound(p *player) []byte {
	w := wire.NewWriterID(proto.TyreCompoundChange)
	w.String8("ZZ")
	return w.Bytes()
}

// sectorSplitOOB reports a split index past the track's sector count. A real
// client never does, so this pins what happens to the timing feed when one does.
func sectorSplitOOB(p *player) []byte {
	w := wire.NewWriterID(proto.SectorSplit)
	w.UInt8(9) // no track has ten sectors
	w.UInt32(31500)
	w.UInt8(0)
	return w.Bytes()
}

func damageUpdate(p *player) []byte {
	w := wire.NewWriterID(proto.DamageUpdate)
	for _, v := range []float32{1.5, 2.5, 3.5, 4.5, 5.5} {
		w.Float32(v)
	}
	return w.Bytes()
}

func p2pActivate(p *player) []byte {
	w := wire.NewWriterID(proto.P2PUpdate)
	w.Int16(1) // activation (not the -1 query)
	w.Bool(true)
	return w.Bytes()
}

func p2pQuery(p *player) []byte {
	w := wire.NewWriterID(proto.P2PUpdate)
	w.Int16(-1) // query current count
	w.Bool(false)
	return w.Bytes()
}

func mandatoryPit(p *player) []byte {
	w := wire.NewWriterID(proto.MandatoryPitUpdate)
	w.Bool(true)
	return w.Bytes()
}

func jumpStartEvent(p *player) []byte {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1) // one event in this batch
	w.UInt8(proto.EventJumpStart)
	return w.Bytes()
}

func collisionWithEnv(p *player) []byte {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1)
	w.UInt8(proto.EventCollisionWithEnv)
	w.Float32(42.5) // impact speed
	w.Vec3(wire.Vec3{X: 1, Y: 2, Z: 3})
	w.Vec3(wire.Vec3{X: 4, Y: 5, Z: 6})
	return w.Bytes()
}

// collisionWithCar builds a 0x82 car collision against session 0. Like the
// environment variant, it goes only to the plugin, never to other clients.
func collisionWithCar(p *player) []byte {
	w := wire.NewWriterID(proto.ClientEvent)
	w.Int16(1)
	w.UInt8(proto.EventCollisionWithCar)
	w.UInt8(0) // hit the car in session 0
	w.Float32(42.5)
	w.Vec3(wire.Vec3{X: 1, Y: 2, Z: 3})
	w.Vec3(wire.Vec3{X: 4, Y: 5, Z: 6})
	return w.Bytes()
}

// lapAt builds a clean, plausible 0x49 lap report of the given time, so a probe
// can hand two cars different lap times and watch the leaderboard order them.
func lapAt(ms uint32) []byte { return lapAtN(ms, 1) }

// lapAtN is lapAt with an explicit lap number, at the fixed Timestamp 120000.
// Probes that report a single lap per car use it; probeRepeatedLap relies on the
// fixed Timestamp to make its second report fail Gate 1.
func lapAtN(ms uint32, numLap uint8) []byte { return lapTimed(120000, ms, numLap) }

// lapTimed builds a clean lap with the given Timestamp. Successive laps from one
// car need rising Timestamps to pass acceptLapTimestamp.
func lapTimed(ts, ms uint32, numLap uint8) []byte {
	w := wire.NewWriterID(proto.LapCompleted)
	w.UInt32(ts) // timestamp
	w.UInt32(ms) // lap time
	w.UInt8(3)   // split count
	w.UInt32(ms / 3)
	w.UInt32(ms / 3)
	w.UInt32(ms - 2*(ms/3))
	w.UInt8(0)      // no cuts
	w.UInt8(numLap) // lap number
	return w.Bytes()
}

// probeLeaderboardOrder has both cars lap, the second faster, and captures the
// leaderboard: the faster car must rank first.
func probeLeaderboardOrder(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	// a (session 0) posts a slow best lap first.
	a.k.sendTCP(lapAt(95430))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	// b (session 1) then beats it; capture the leaderboard a receives, which
	// must now rank b ahead of a.
	b.k.sendTCP(lapAt(90000))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeRepeatedLap sends two laps with the same Timestamp (lapAtN reuses it):
// the second is dropped by the lap-timestamp gate, so the other car sees one
// 0x49.
func probeRepeatedLap(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	// First lap: accepted, leaderboard broadcast to the other car.
	b.k.sendTCP(lapAtN(90000, 1))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}

	// Second lap with a non-advancing timestamp: dropped, no broadcast. Anything
	// captured here would be added to the capture and fail the comparison.
	b.k.sendTCP(lapAtN(85000, 2))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeBestLapProgression runs three laps (set, slower, faster): Time must
// track the fastest lap and the counter every lap.
func probeBestLapProgression(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	laps := []struct {
		ts, ms uint32
		num    uint8
	}{
		{120000, 92000, 1}, // sets the best
		{240000, 95000, 2}, // slower: best retained, lap counter advances
		{360000, 90000, 3}, // faster: best improves
	}
	for _, lp := range laps {
		b.k.sendTCP(lapTimed(lp.ts, lp.ms, lp.num))
		for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			if pkt[0] == proto.LapCompleted {
				cap.add(decodeTCP(pkt))
			}
		}
	}
	return cap
}

// probeLeaderboardSyncAfterLap sets a lap, then joins a second player: its
// first-update sync must already show the lap.
func probeLeaderboardSyncAfterLap(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Leader"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(lapTimed(120000, 91000, 1))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Latecomer")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	// Only the leaderboard sync matters here; the rest of B's first-update burst
	// is already pinned by probeFirstUpdate.
	for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeLeaderboardAfterDisconnect: a car laps and leaves, the other laps. Does
// the departed slot keep its time in the next leaderboard?
func probeLeaderboardAfterDisconnect(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()

	// b (session 1) sets a lap, then leaves cleanly.
	b.k.sendTCP(lapTimed(120000, 91000, 1))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.sendTCP([]byte{proto.CleanExitDrive})
	time.Sleep(300 * time.Millisecond)
	b.k.close()
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) // discard the 0x4D

	// a (session 0) now laps, forcing a fresh leaderboard broadcast whose slot-1
	// entry reveals whether b's departed result survived.
	a.k.sendTCP(lapTimed(130000, 92000, 1))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeSlotReuseResult: a car laps and leaves, and a new driver joins. It is
// seated in a slot nobody has used, so its sync still shows the departed
// driver's lap (docs/01 §1.5 "Which slot").
func probeSlotReuseResult(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	b.k.sendTCP(lapTimed(120000, 91000, 1))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.sendTCP([]byte{proto.CleanExitDrive})
	time.Sleep(300 * time.Millisecond)
	b.k.close()
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	third := joinAs("Reuser")
	third.guid = probeGUID3
	c := joinPlayer(t, tgt, third)
	if c == nil {
		return cap
	}
	defer c.k.close()
	if _, ok := c.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range c.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeFirstUpdateRaceState: one car sets P2P and pit state, then a second
// joins; its snapshot must show that state, not defaults.
func probeFirstUpdateRaceState(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Setter"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(p2pActivate(a)) // count=1, active
	a.k.sendTCP(mandatoryPit(a))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Joiner")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	// Only the per-slot P2P and mandatory-pit lines of B's first update matter
	// here; the rest of the burst is already pinned by probeFirstUpdate.
	for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.P2PUpdate || pkt[0] == proto.MandatoryPitUpdate {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeBoPBeforeJoin sets BoP on an empty slot, then seats a driver there: its
// first update must carry that BoP.
func probeBoPBeforeJoin(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Steward")
	if a == nil {
		return cap
	}
	defer a.k.close()

	a.k.sendTCP(chatMessage("/ballast 1 50"))
	a.k.sendTCP(chatMessage("/restrictor 1 20"))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Recruit")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second) // takes slot 1, the one just adjusted
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.BoPUpdate {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// ---------------------------------------------------------------------------
// Non-ASCII identities and chat
//
// Names cross both string encodings (wstring in 0x3D and 0x5B, string8 in
// 0x40 and 0x5A), which ASCII cannot tell apart.
// ---------------------------------------------------------------------------

// unicodeName mixes Latin-1 accents, a CJK ideograph and a non-BMP emoji, so a
// server that counts UTF-16 code units, bytes, or runes gives three different
// answers for its length.
const unicodeName = "Ünïcødé 走 🏁"

// probeUnicodeName joins under that name and captures both places the server
// echoes it back: the 0x3E handshake (which does not carry it) and the 0x40 car
// list, where it appears as a UTF-8 string8.
func probeUnicodeName(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	cap := joinAndCapture(t, tgt, k, joinAs(unicodeName))
	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	k.sendTCP(req.Bytes())
	if pkt, ok := k.awaitTCP(proto.CarList, 2*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no car list>"})
	}
	return cap
}

// probeUnicodeJoinBroadcast captures what an in-world player is told when a
// driver with a non-ASCII name joins: 0x5A carries the name as a string8 and
// 0x5B as a wstring8, so a re-encoding bug shows up in exactly one of them.
func probeUnicodeJoinBroadcast(t *testing.T, tgt *target) capture {
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

	second := joinAs(unicodeName)
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.CarConnected || pkt[0] == proto.DriverInfoUpdate {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeUnicodeChat relays non-ASCII chat, including a non-BMP emoji.
func probeUnicodeChat(t *testing.T, tgt *target) capture {
	return chatRelay(t, tgt, "héllo 世界 🏁 —")
}

// probeChatMaxLength sends the longest message a wstring8 can carry: 255
// characters, the count prefix's ceiling. A server that truncates, drops or
// re-frames at the boundary shows up here.
func probeChatMaxLength(t *testing.T, tgt *target) capture {
	return chatRelay(t, tgt, strings.Repeat("abcdefghij", 25)+"abcde")
}

// chatRelay is the shared body of the chat-content probes: two players in world,
// one sends the message, the other's view is captured.
func chatRelay(t *testing.T, tgt *target, text string) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	w := wire.NewWriterID(proto.Chat)
	w.UInt8(b.sessionID)
	w.WString8(text)
	b.k.sendTCP(w.Bytes())

	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// ---------------------------------------------------------------------------
// Session-scoped results
// ---------------------------------------------------------------------------

// sessionChangeProbe sets a lap, changes session with cmd, then joins a new
// driver: its sync must not carry the old session's lap.
func sessionChangeProbe(cmd string) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a := joinAdmin(t, tgt, "Alpha")
		if a == nil {
			return cap
		}
		defer a.k.close()

		a.k.sendTCP(lapTimed(120000, 91000, 1))
		a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

		a.k.sendTCP(chatMessage(cmd))
		a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) // let the transition land

		second := joinAs("Fresh")
		second.guid = probeGUID2
		b := joinPlayer(t, tgt, second)
		if b == nil {
			return cap
		}
		defer b.k.close()
		if _, ok := b.enterWorld(t, tgt); !ok {
			return cap
		}
		for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			if pkt[0] == proto.LapCompleted {
				cap.add(decodeTCP(pkt))
			}
		}
		return cap
	}
}

// probeQualifyToRaceGrid: the second car wins qualifying, then the race's 0x4A
// must put it first on the grid.
func probeQualifyToRaceGrid(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha") // session 0
	if a == nil {
		return cap
	}
	defer a.k.close()

	second := joinAs("Bravo")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second) // session 1
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(lapTimed(120000, 95000, 1)) // slower qualifying lap
	b.k.sendTCP(lapTimed(130000, 90000, 1)) // pole
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(chatMessage("/next_session"))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.CurrentSessionUpdate || pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// ---------------------------------------------------------------------------
// Admission: pickup mode, client cap, kicks and bans
// ---------------------------------------------------------------------------

// probePickupOff joins with PICKUP_MODE_ENABLED=0.
func probePickupOff(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	k.sendTCP(joinAs("Latecomer").encode())
	var cap capture
	// Silence is a legitimate outcome here (it is vanilla's), so unlike
	// joinAndCapture this does not fail the probe when nothing comes back.
	if pkt, ok := k.readTCP(3 * time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no handshake response>"})
	}
	return cap
}

// probeMaxClientsCap: MAX_CLIENTS=1 with free slots left; the second join must
// be refused.
func probeMaxClientsCap(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("First"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	k := dial(t, tgt)
	defer k.close()
	second := joinAs("Second")
	second.guid = probeGUID2
	return joinAndCapture(t, tgt, k, second)
}

// rejoinProbe removes a driver with cmd and captures the answer to their
// rejoin: allowed after /kick_id, refused after /ban_id.
func rejoinProbe(cmd string) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a := joinAdmin(t, tgt, "Alpha")
		if a == nil {
			return cap
		}
		defer a.k.close()

		second := joinAs("Bravo")
		second.guid = probeGUID2
		b := joinPlayer(t, tgt, second)
		if b == nil {
			return cap
		}
		if _, ok := b.enterWorld(t, tgt); !ok {
			b.k.close()
			return cap
		}
		a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

		a.k.sendTCP(chatMessage(cmd))
		a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
		b.k.close()
		time.Sleep(300 * time.Millisecond)

		k := dial(t, tgt)
		defer k.close()
		k.sendTCP(second.encode())

		// Only the verdict: the slot differs (vanilla frees it later), and the
		// reference's sun angle has drifted by now.
		outcome := "TCP <no response>"
		if pkt, ok := k.readTCP(3 * time.Second); ok {
			outcome = decodeTCP(pkt).label
		}
		cap.add(packet{label: "verdict: rejoin outcome", fields: []field{{"answer", outcome}}})
		return cap
	}
}

// probeCarListWindow asks for a 0x3F start index past the first page of a
// twelve-slot list (it is an index, not a page number).
func probeCarListWindow(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Pager"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(5) // five slots in, with twelve in the list
	p.k.sendTCP(req.Bytes())
	if pkt, ok := p.k.awaitTCP(proto.CarList, 2*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no car list>"})
	}
	return cap
}

// probeCarListAfterDamage reports damage, then reads 0x40: is it stored?
func probeCarListAfterDamage(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(damageUpdate(b))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	a.k.sendTCP(req.Bytes())
	if pkt, ok := a.k.awaitTCP(proto.CarList, 2*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no car list>"})
	}
	return cap
}

// probeChatWhileLoading sends chat from a client that has not yet loaded.
func probeChatWhileLoading(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Alpha"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Loading")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second) // handshaked only: no UDP, no first update
	if b == nil {
		return cap
	}
	defer b.k.close()

	w := wire.NewWriterID(proto.Chat)
	w.UInt8(b.sessionID)
	w.WString8("am I here yet")
	b.k.sendTCP(w.Bytes())

	// Only the chat matters: the join announcement B produced is pinned by
	// probeSecondClientJoin, and its timing relative to this is not the question.
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.Chat {
			cap.add(decodeTCP(pkt))
		}
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <nothing relayed>"})
	}
	return cap
}

// probeChatSpoofedSender sends chat claiming another car's session id.
func probeChatSpoofedSender(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	w := wire.NewWriterID(proto.Chat)
	w.UInt8(a.sessionID) // b claims to be a
	w.WString8("I am definitely Alpha")
	b.k.sendTCP(w.Bytes())

	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <nothing relayed>"})
	}
	return cap
}

// probeNextSessionWhileLoading asks for /next_session while another client
// loads; both servers refuse.
func probeNextSessionWhileLoading(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha")
	if a == nil {
		return cap
	}
	defer a.k.close()

	second := joinAs("Loading")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second) // no enterWorld: still loading
	if b == nil {
		return cap
	}
	defer b.k.close()
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(chatMessage("/next_session"))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no response>"})
	}
	return cap
}

// probeSessionChangeResetsCarState sets P2P, pit and ballast, changes session,
// and reads them from a new joiner: race state resets, BoP stays.
func probeSessionChangeResetsCarState(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha")
	if a == nil {
		return cap
	}
	defer a.k.close()

	a.k.sendTCP(p2pActivate(a))
	a.k.sendTCP(mandatoryPit(a))
	a.k.sendTCP(chatMessage("/ballast 0 40"))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(chatMessage("/next_session"))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	second := joinAs("Fresh")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		switch pkt[0] {
		case proto.P2PUpdate, proto.MandatoryPitUpdate, proto.BoPUpdate:
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probePositionBroadcast captures the 0x46 one in-world car's movement produces
// on another car's UDP socket — the packet the whole game is built on.
func probePositionBroadcast(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	// Keep B moving so A's tick loop has something fresh to relay.
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		b.k.sendUDP(positionUpdate(7))
		pkt, ok := a.k.readUDP(500 * time.Millisecond)
		if !ok {
			continue
		}
		if pkt[0] == proto.PositionUpdate {
			cap.add(decodeUDP(pkt))
			return cap
		}
	}
	t.Errorf("%s: no 0x46 position broadcast", tgt.label)
	return cap
}

// stampedUpdate is positionUpdate with a caller-chosen Timestamp and a unique
// Position.X tag, so a probe can tell exactly which update got relayed.
func stampedUpdate(seq uint8, ts uint32, x float32) []byte {
	w := wire.NewWriterID(proto.PositionUpdate)
	w.UInt8(seq)
	w.UInt32(ts)
	w.Vec3(wire.Vec3{X: x, Y: 0, Z: 20})
	w.Vec3(wire.Vec3{X: 0, Y: 0, Z: 1})
	w.Vec3(wire.Vec3{X: 1, Y: 0, Z: 0})
	for i := 0; i < 4; i++ {
		w.UInt8(100) // TyreAngularSpeed
	}
	w.UInt8(128) // SteerAngle
	w.UInt8(128) // WheelAngle
	w.UInt16(3000)
	w.UInt8(2) // Gear
	w.UInt32(0)
	w.Int16(0)
	w.UInt8(50)     // Gas
	w.Float32(0.25) // NormalizedPosition
	return w.Bytes()
}

// relayedX reads Position.X out of an outbound 0x46 (docs/03 §0x46).
func relayedX(pkt []byte) (float32, bool) {
	if len(pkt) == 0 || pkt[0] != proto.PositionUpdate {
		return 0, false
	}
	r := wire.NewReader(pkt[1:])
	r.UInt8()  // SessionId
	r.UInt8()  // PakSequenceId
	r.UInt32() // Timestamp
	r.UInt16() // Ping
	pos := r.Vec3()
	if r.Overflowed() {
		return 0, false
	}
	return pos.X, true
}

// awaitRelayedX reports whether a car tagged with the given Position.X reaches
// the listener within the window.
func awaitRelayedX(p *player, x float32, timeout time.Duration) ([]byte, bool) {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		pkt, ok := p.k.readUDP(time.Until(deadline))
		if !ok {
			return nil, false
		}
		if got, ok := relayedX(pkt); ok && got == x {
			return pkt, true
		}
	}
	return nil, false
}

// probeImplausiblePosition sends NaN coordinates: are they relayed?
func probeImplausiblePosition(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	nan := float32(math.NaN())
	b.k.sendUDP(stampedUpdate(1, 6000, nan))

	relayed := false
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		pkt, ok := a.k.readUDP(time.Until(deadline))
		if !ok {
			break
		}
		if x, ok := relayedX(pkt); ok && math.IsNaN(float64(x)) {
			relayed = true
			break
		}
	}
	cap.add(packet{
		label:  "verdict: NaN 0x46 relayed",
		fields: []field{{"relayed", fmt.Sprint(relayed)}},
	})
	return cap
}

// probeStaleTimestamp: a 0x46 whose Timestamp goes backwards must be dropped.
// Only the verdict is compared, since broadcast cadence differs.
func probeStaleTimestamp(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	// enterWorld already banked ts=1000, so this advances and must be accepted.
	b.k.sendUDP(stampedUpdate(1, 5000, 111))
	pkt, ok := awaitRelayedX(a, 111, 2*time.Second)
	if !ok {
		t.Errorf("%s: fresh position update was never relayed", tgt.label)
		return cap
	}
	cap.add(decodeUDP(pkt))

	// Still ahead of the banked 1000, but behind the 5000 just accepted: a
	// reordered datagram. It must not reach the other car.
	b.k.sendUDP(stampedUpdate(2, 4900, 222))
	_, relayed := awaitRelayedX(a, 222, time.Second)
	cap.add(packet{
		label:  "verdict: stale 0x46 relayed",
		fields: []field{{"relayed", fmt.Sprint(relayed)}},
	})
	return cap
}

// probeTimestampRebase gives two players clocks 9.8 million ms apart, then
// checks that a relayed 0x46 timestamp lands in the receiver's clock with the
// sender's skew intact (docs/03 §0x46).
func probeTimestampRebase(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	start := time.Now()
	clock := func(base int32) int32 { return base + int32(time.Since(start).Milliseconds()) }
	const baseA, baseB = 10_000_000, 200_000

	// Answer pings so the server learns both offsets.
	for time.Now().Before(start.Add(4 * time.Second)) {
		for _, p := range []struct {
			pl   *player
			base int32
		}{{a, baseA}, {b, baseB}} {
			pkt, ok := p.pl.k.readUDP(20 * time.Millisecond)
			if !ok || pkt[0] != proto.PingUpdate {
				continue
			}
			w := wire.NewWriterID(proto.PingPong)
			w.Int32(int32(binary.LittleEndian.Uint32(pkt[1:5])))
			w.Int32(clock(p.base))
			p.pl.k.sendUDP(w.Bytes())
		}
	}

	// A car's offset is sampled when its 0x46 is accepted, so both send one
	// now. The reference also drops an update sent right after a relay tick,
	// so every send waits a quarter second.
	settle := 250 * time.Millisecond
	a.k.sendUDP(stampedUpdate(10, uint32(clock(baseA)), 400))
	b.k.sendUDP(stampedUpdate(10, uint32(clock(baseB)), 401))
	time.Sleep(settle)

	for i, c := range []struct {
		from, to         *player
		fromBase, toBase int32
		skew             int32
	}{{b, a, baseB, baseA, 0}, {a, b, baseA, baseB, 0}, {b, a, baseB, baseA, 3000}} {
		time.Sleep(settle)
		x := float32(500 + i)
		c.from.k.sendUDP(stampedUpdate(uint8(11+i), uint32(clock(c.fromBase)+c.skew), x))
		pkt, ok := awaitRelayedX(c.to, x, 2*time.Second)
		if !ok {
			t.Errorf("%s: position update %d was never relayed", tgt.label, i)
			return cap
		}
		// Loopback ping is ~0, so only scheduling jitter separates the two.
		drift := int32(binary.LittleEndian.Uint32(pkt[3:7])) - (clock(c.toBase) + c.skew)
		cap.add(packet{
			label:  fmt.Sprintf("verdict: relay %d in receiver clock (skew %d)", i, c.skew),
			fields: []field{{"withinOneSecond", fmt.Sprint(drift > -1000 && drift < 1000)}},
		})
	}
	return cap
}

// probeOffsetSlew syncs a sender's clock, then answers one ping with its clock
// 100 ms ahead. Vanilla moves the sender's offset 3 ms per 0x46 toward the new
// estimate rather than at once (docs/03 0xF8), which shows in the relayed
// timestamps: relayed − sent is exactly the offset difference.
func probeOffsetSlew(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	start := time.Now()
	const base = 1_000_000
	clock := func() int32 { return base + int32(time.Since(start).Milliseconds()) }
	pong := func(pkt []byte, skew int32) {
		w := wire.NewWriterID(proto.PingPong)
		w.Int32(int32(binary.LittleEndian.Uint32(pkt[1:5])))
		w.Int32(clock() + skew)
		a.k.sendUDP(w.Bytes())
	}
	for time.Since(start) < 2500*time.Millisecond {
		if pkt, ok := a.k.readUDP(20 * time.Millisecond); ok && pkt[0] == proto.PingUpdate {
			pong(pkt, 0)
		}
	}

	// relayedDelta sends one update and returns relayed − sent timestamp.
	seq := uint8(20)
	relayedDelta := func() (int32, bool) {
		time.Sleep(250 * time.Millisecond)
		seq++
		ts := uint32(clock())
		x := float32(300 + int(seq))
		a.k.sendUDP(stampedUpdate(seq, ts, x))
		pkt, ok := awaitRelayedX(b, x, 2*time.Second)
		if !ok {
			return 0, false
		}
		return int32(binary.LittleEndian.Uint32(pkt[3:7]) - ts), true
	}
	baseline, ok := relayedDelta() // the first update snaps to the synced offset
	if !ok {
		t.Errorf("%s: baseline update was never relayed", tgt.label)
		return cap
	}

	// One fresh ping, answered 100 ms ahead.
	for d := time.Now().Add(1500 * time.Millisecond); time.Now().Before(d); {
		if pkt, ok := a.k.readUDP(20 * time.Millisecond); ok && pkt[0] == proto.PingUpdate {
			pong(pkt, 100)
			break
		}
	}
	time.Sleep(100 * time.Millisecond)
	for i, want := range [][2]int32{{1, 6}, {4, 9}, {7, 12}, {10, 15}} {
		d, ok := relayedDelta()
		if !ok {
			t.Errorf("%s: update %d was never relayed", tgt.label, i)
			return cap
		}
		shift := max(d-baseline, baseline-d)
		cap.add(verdict(fmt.Sprintf("update %d after the pong shifted %d-%d ms", i+1, want[0], want[1]),
			shift >= want[0] && shift <= want[1]))
	}
	return cap
}

// joinInWorld joins under its own guid, enters the world and returns the
// player with the first-update packets it was sent.
func joinInWorld(t *testing.T, tgt *target, name, guid string) (*player, [][]byte) {
	j := joinAs(name)
	j.guid = guid
	p := joinPlayer(t, tgt, j)
	if p == nil {
		return nil, nil
	}
	if _, ok := p.enterWorld(t, tgt); !ok {
		p.k.close()
		return nil, nil
	}
	return p, p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
}

// leave exits cleanly and gives the server time to free the slot.
func (p *player) leave() {
	p.k.sendTCP([]byte{proto.CleanExitDrive})
	time.Sleep(250 * time.Millisecond)
	p.k.close()
	time.Sleep(750 * time.Millisecond)
}

// tyreCompoundNamed is an inbound 0x50 with the given compound.
func tyreCompoundNamed(name string) []byte {
	w := wire.NewWriterID(proto.TyreCompoundChange)
	w.String8(name)
	return w.Bytes()
}

// addSlotState records a player's slot and the parts of its snapshot that
// carry other slots' state: names, leaderboard and tyre compounds.
func addSlotState(cap *capture, who string, p *player, snapshot [][]byte) {
	cap.add(packet{label: who + " slot", fields: []field{{"sessionID", fmt.Sprint(p.sessionID)}}})
	for _, pkt := range snapshot {
		switch pkt[0] {
		case proto.DriverInfoUpdate, proto.LapCompleted, proto.TyreCompoundChange:
			cap.add(decodeTCP(pkt))
		}
	}
}

// probeSlotHandover fills all three slots, lets two drivers go, then rejoins
// one and seats a newcomer (docs/01 §1.5). A returning guid gets its old slot
// back with its laps; a newcomer with no unused slot left takes the first
// free one and its laps are cleared. Departed drivers keep their name in 0x5B
// and their tyre compound in the snapshot.
func probeSlotHandover(t *testing.T, tgt *target) capture {
	var cap capture
	a, _ := joinInWorld(t, tgt, "Alpha", probeGUID)
	if a == nil {
		return cap
	}
	a.k.sendTCP(tyreCompoundNamed("SM"))
	a.k.sendTCP(lapTimed(120000, 90000, 1))
	b, _ := joinInWorld(t, tgt, "Bravo", probeGUID2)
	if b == nil {
		a.k.close()
		return cap
	}
	b.k.sendTCP(tyreCompoundNamed("ST"))
	b.k.sendTCP(lapTimed(120000, 95000, 1))
	c, _ := joinInWorld(t, tgt, "Charlie", probeGUID3)
	if c == nil {
		a.k.close()
		b.k.close()
		return cap
	}
	defer c.k.close()
	time.Sleep(250 * time.Millisecond)
	a.leave()
	b.leave()

	b2, snap := joinInWorld(t, tgt, "Bravo", probeGUID2)
	if b2 == nil {
		return cap
	}
	defer b2.k.close()
	addSlotState(&cap, "returning Bravo", b2, snap)

	d, snap := joinInWorld(t, tgt, "Delta", probeGUID4)
	if d == nil {
		return cap
	}
	defer d.k.close()
	addSlotState(&cap, "newcomer Delta", d, snap)
	return cap
}

// probeDepartedDriverListings: Alpha laps and leaves, then an admin under
// another guid reads the car list (0x40) and /JSON and ends the session. Each
// listing, and the results file, shows how vanilla names a slot whose driver
// has left (docs/02 0x5B).
func probeDepartedDriverListings(t *testing.T, tgt *target) capture {
	var cap capture
	a, _ := joinInWorld(t, tgt, "Alpha", probeGUID)
	if a == nil {
		return cap
	}
	a.k.sendTCP(lapTimed(120000, 90000, 1))
	time.Sleep(250 * time.Millisecond)
	a.leave()

	j := joinAs("Admin")
	j.guid, j.password = probeGUID2, adminPassword
	b := joinPlayer(t, tgt, j)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	b.k.sendTCP([]byte{proto.CarListRequest, 0})
	if pkt, ok := b.k.awaitTCP(proto.CarList, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no 0x40>"})
	}
	cap.packets = append(cap.packets, fetchJSON(t, tgt, "/JSON|"+probeGUID2, "HTTP /JSON").packets...)

	// Vanilla takes the chat sender from the packet, so it must name b's slot.
	cmd := chatMessage("/next_session")
	cmd[1] = b.sessionID
	b.k.sendTCP(cmd)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	time.Sleep(time.Second)
	cap.packets = append(cap.packets, latestResults(t, tgt).packets...)
	return cap
}

// latestResults flattens the newest results file, as fetchJSON does /JSON.
func latestResults(t *testing.T, tgt *target) capture {
	var cap capture
	files, _ := filepath.Glob(filepath.Join(tgt.resultsDir, "*.json"))
	if len(files) == 0 {
		cap.add(packet{label: "results <no file>"})
		return cap
	}
	sort.Strings(files)
	b, err := os.ReadFile(files[len(files)-1])
	if err != nil {
		t.Errorf("%s: read results: %v", tgt.label, err)
		return cap
	}
	var body any
	if err := json.Unmarshal(b, &body); err != nil {
		t.Errorf("%s: decode results: %v", tgt.label, err)
		return cap
	}
	p := packet{label: "results file"}
	flattenJSON("", body, &p.fields)
	sort.Slice(p.fields, func(i, j int) bool { return p.fields[i].name < p.fields[j].name })
	cap.add(p)
	return cap
}

// probeBoPResetOnJoin: an admin ballasts Bravo, who leaves and rejoins the
// same slot. Vanilla restores the entry-list BoP for every connection, so the
// admin hears a 0x70 with the ballast gone (docs/02 0x70).
func probeBoPResetOnJoin(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	b, _ := joinInWorld(t, tgt, "Bravo", probeGUID2)
	if b == nil {
		return cap
	}
	a.k.sendTCP(chatMessage(fmt.Sprintf("/ballast %d 50", b.sessionID)))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.leave()
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	b2, snap := joinInWorld(t, tgt, "Bravo", probeGUID2)
	if b2 == nil {
		return cap
	}
	defer b2.k.close()
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		switch pkt[0] {
		case proto.BoPUpdate, proto.CarConnected:
			cap.add(decodeTCP(pkt))
		}
	}
	for _, pkt := range snap {
		if pkt[0] == proto.BoPUpdate {
			cap.add(decodeTCP(pkt))
		}
	}
	return cap
}

// probeLeaderboardHoldsOrder: the leaderboard order is re-sorted only on a
// counted lap (docs/02 0x49). After Alpha leaves, Bravo's cut lap leaves the
// order as it was; Bravo's clean lap then moves Bravo up.
func probeLeaderboardHoldsOrder(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer b.k.close()
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	a.leave()
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	for _, lap := range [][]byte{lapPacket(120000, 95000, 2, 1), lapPacket(240000, 94000, 0, 2)} {
		b.k.sendTCP(lap)
		if pkt, ok := b.k.awaitTCP(proto.LapCompleted, 3*time.Second); ok {
			cap.add(decodeTCP(pkt))
		} else {
			cap.add(packet{label: "TCP <no 0x49>"})
		}
	}
	return cap
}

// probeSunOnSessionSwitch waits until both servers are over a minute old, then
// restarts and skips the session. Each switch resets the sun to SUN_ANGLE and
// sends it in a 0x54 right after the 0x4A and 0x78 (docs/02 0x54). A younger
// server sends none, which is what the plain command probes see.
func probeSunOnSessionSwitch(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	// The harness starts the server before the probe; 62 s from here is past
	// a minute of uptime and the first periodic 0x54.
	for deadline := time.Now().Add(62 * time.Second); time.Now().Before(deadline); {
		a.k.drainTCP(time.Second, 2*time.Second)
	}
	for _, cmd := range []string{"/restart_session", "/next_session"} {
		a.k.sendTCP(chatMessage(cmd))
		for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			switch pkt[0] {
			case proto.CurrentSessionUpdate, proto.WeatherUpdate, proto.SunAngleUpdate:
				cap.add(decodeTCP(pkt))
			}
		}
	}
	return cap
}

// probeStepTimers: vanilla pings a car in the pass it becomes ready and sends
// a session switch's first 0x57 in the next pass, from timers of their own,
// not on a shared one-second tick (docs/03 0xF9, 0x57).
func probeStepTimers(t *testing.T, tgt *target) capture {
	var cap capture
	j := joinAs("Admin")
	j.password = adminPassword
	p := joinPlayer(t, tgt, j)
	if p == nil {
		return cap
	}
	defer p.k.close()
	// Past the first second of uptime, before which vanilla pings nobody.
	time.Sleep(1500 * time.Millisecond)
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	loaded := time.Now()
	pkt, ok := p.k.readUDP(3 * time.Second)
	for ok && pkt[0] != proto.PingUpdate {
		pkt, ok = p.k.readUDP(3 * time.Second)
	}
	cap.add(verdict("first 0xF9 within 200 ms of loading", ok && time.Since(loaded) < 200*time.Millisecond))
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	// Past the first session's 0x57 window.
	time.Sleep(5 * time.Second)
	p.k.sendTCP(chatMessage("/next_session"))
	if _, ok := p.k.awaitTCP(proto.CurrentSessionUpdate, 3*time.Second); !ok {
		cap.add(packet{label: "TCP <no 0x4A>"})
		return cap
	}
	switched := time.Now()
	pkt, ok = p.k.readUDP(3 * time.Second)
	for ok && pkt[0] != proto.RaceStart {
		pkt, ok = p.k.readUDP(3 * time.Second)
	}
	cap.add(verdict("first 0x57 within 200 ms of the 0x4A", ok && time.Since(switched) < 200*time.Millisecond))
	return cap
}

// probeVoteDurationExact: a new vote's 0x64 carries exactly VOTE_DURATION in
// milliseconds, as vanilla's millisecond clock gives (docs/02 0x64).
func probeVoteDurationExact(t *testing.T, tgt *target) capture {
	var cap capture
	p, _ := joinInWorld(t, tgt, "Voter", probeGUID)
	if p == nil {
		return cap
	}
	defer p.k.close()
	p.k.sendTCP([]byte{proto.VoteNextSession, 1})
	pkt, ok := p.k.awaitTCP(proto.VoteNextSession, 3*time.Second)
	if !ok {
		cap.add(packet{label: "TCP <no 0x64>"})
		return cap
	}
	cap.add(verdict("0x64 time is 20000", binary.LittleEndian.Uint32(pkt[4:8]) == 20000))
	return cap
}

// probeRelayEachUpdate streams 30 updates at ~60 Hz, faster than any tick,
// then goes silent: vanilla relays each one once, and nothing after
// (docs/03 0x46).
func probeRelayEachUpdate(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()
	// Let the join-time relays drain.
	for d := time.Now().Add(500 * time.Millisecond); time.Now().Before(d); {
		a.k.readUDP(50 * time.Millisecond)
	}

	const n = 30
	seen := map[float32]int{}
	after := 0
	start := time.Now()
	stop := start.Add(n * 16 * time.Millisecond)
	end := stop.Add(time.Second)
	next, sent := start, 0
	for time.Now().Before(end) {
		if sent < n && !time.Now().Before(next) {
			sent++
			b.k.sendUDP(stampedUpdate(uint8(sent), uint32(20000+sent*16), float32(100+sent)))
			next = next.Add(16 * time.Millisecond)
		}
		pkt, ok := a.k.readUDP(2 * time.Millisecond)
		if !ok || pkt[0] != proto.PositionUpdate || pkt[1] != b.sessionID {
			continue
		}
		if x, ok := relayedX(pkt); ok {
			seen[x]++
			// Allow 200 ms for the last relays to land.
			if time.Now().After(stop.Add(200 * time.Millisecond)) {
				after++
			}
		}
	}
	once := len(seen) == n
	for _, c := range seen {
		once = once && c == 1
	}
	cap.add(verdict("every update relayed exactly once", once))
	cap.add(verdict("relays after the sender went silent", after > 0))
	return cap
}

// probeRelayBurstMerges sends five updates back to back, well inside one of
// vanilla's main-loop passes. Vanilla relays a car's latest status once per
// pass, so the other player sees fewer than five, the newest among them.
func probeRelayBurstMerges(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()
	for d := time.Now().Add(500 * time.Millisecond); time.Now().Before(d); {
		a.k.readUDP(50 * time.Millisecond)
	}

	// Within the Rust server's 500 m network bubble, which would thin out
	// a car further away.
	const n = 5
	for i := 1; i <= n; i++ {
		b.k.sendUDP(stampedUpdate(uint8(i), uint32(30000+i*16), float32(200+i)))
	}
	seen := map[float32]bool{}
	for end := time.Now().Add(500 * time.Millisecond); time.Now().Before(end); {
		pkt, ok := a.k.readUDP(20 * time.Millisecond)
		if !ok || pkt[0] != proto.PositionUpdate || pkt[1] != b.sessionID {
			continue
		}
		if x, ok := relayedX(pkt); ok {
			seen[x] = true
		}
	}
	cap.add(verdict("burst merged into fewer relays", len(seen) < n))
	cap.add(verdict("newest update relayed", seen[float32(200+n)]))
	return cap
}

// verdict records a named yes/no observation.
func verdict(label string, v bool) packet {
	return packet{label: "verdict: " + label, fields: []field{{"value", fmt.Sprint(v)}}}
}

// probeUDPReassociation repeats 0x4E on the same port, then moves the player
// to a new port, as a NAT rebinding would (docs/03 0x4E).
func probeUDPReassociation(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()
	assoc := wire.NewWriterID(proto.CarConnect)
	assoc.UInt8(a.sessionID)
	awaitAssoc := func() bool {
		deadline := time.Now().Add(2 * time.Second)
		for time.Now().Before(deadline) {
			if pkt, ok := a.k.readUDP(time.Until(deadline)); ok && pkt[0] == proto.CarConnect {
				return true
			}
		}
		return false
	}

	a.k.sendUDP(assoc.Bytes())
	cap.add(verdict("same-port 0x4E answered", awaitAssoc()))

	old := a.k.udp
	moved, err := net.Dial("udp", fmt.Sprintf("127.0.0.1:%d", tgt.ports.game))
	if err != nil {
		t.Fatalf("dial udp: %v", err)
	}
	a.k.udp = moved
	defer old.Close()
	a.k.sendUDP(stampedUpdate(1, 5000, 111))
	_, relayed := awaitRelayedX(b, 111, time.Second)
	cap.add(verdict("0x46 from unassociated port relayed", relayed))

	a.k.sendUDP(assoc.Bytes())
	cap.add(verdict("new-port 0x4E answered", awaitAssoc()))
	a.k.sendUDP(stampedUpdate(2, 6000, 222))
	_, relayed = awaitRelayedX(b, 222, time.Second)
	cap.add(verdict("0x46 from new port relayed", relayed))

	a.k.udp = old
	a.k.sendUDP(stampedUpdate(3, 7000, 333))
	_, relayed = awaitRelayedX(b, 333, time.Second)
	a.k.udp = moved
	cap.add(verdict("0x46 from old port relayed", relayed))
	return cap
}

// probePingsAfterLoad associates UDP but holds back the first 0x46: no 0xF9
// may arrive until it is sent.
func probePingsAfterLoad(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Loader"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	w := wire.NewWriterID(proto.CarConnect)
	w.UInt8(p.sessionID)
	p.k.sendUDP(w.Bytes())

	pinged := false
	deadline := time.Now().Add(2500 * time.Millisecond)
	for time.Now().Before(deadline) {
		if pkt, ok := p.k.readUDP(time.Until(deadline)); ok && pkt[0] == proto.PingUpdate {
			pinged = true
		}
	}
	cap.add(verdict("0xF9 before the first 0x46", pinged))

	p.k.sendUDP(positionUpdate(0))
	pinged = false
	deadline = time.Now().Add(2500 * time.Millisecond)
	for time.Now().Before(deadline) && !pinged {
		if pkt, ok := p.k.readUDP(time.Until(deadline)); ok && pkt[0] == proto.PingUpdate {
			pinged = true
		}
	}
	cap.add(verdict("0xF9 after the first 0x46", pinged))
	return cap
}

// probePingAverageNoRamp answers every 0xF9 100 ms late: the reported ping
// is the mean of the samples so far, so it reads ~100 from the second ping on,
// with no ramp from zero (docs/03 0xF8).
func probePingAverageNoRamp(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Laggy"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	type pong struct {
		at  time.Time
		pkt []byte
	}
	var queue []pong
	var pings []uint16
	deadline := time.Now().Add(4500 * time.Millisecond)
	for time.Now().Before(deadline) {
		if pkt, ok := p.k.readUDP(3 * time.Millisecond); ok && pkt[0] == proto.PingUpdate {
			pings = append(pings, binary.LittleEndian.Uint16(pkt[5:7]))
			w := wire.NewWriterID(proto.PingPong)
			w.Int32(int32(binary.LittleEndian.Uint32(pkt[1:5])))
			w.Int32(0)
			queue = append(queue, pong{time.Now().Add(100 * time.Millisecond), w.Bytes()})
		}
		for len(queue) > 0 && time.Now().After(queue[0].at) {
			p.k.sendUDP(queue[0].pkt)
			queue = queue[1:]
		}
	}
	if len(pings) < 3 {
		t.Errorf("%s: only %d pings in 4.5 s", tgt.label, len(pings))
		return cap
	}
	last := pings[len(pings)-1]
	cap.add(verdict("ping reads 80-150 ms after 3 pongs", last >= 80 && last <= 150))
	return cap
}

// probeHandshakeSeedAndClock joins twice, 1.5 s apart: both 0x3E carry the
// same RandomSeed, and CurrentTime is the server clock (docs/02 0x3E).
func probeHandshakeSeedAndClock(t *testing.T, tgt *target) capture {
	var cap capture
	var seeds, clocks []int32
	for i, guid := range []string{probeGUID, probeGUID2} {
		if i > 0 {
			time.Sleep(1500 * time.Millisecond)
		}
		j := joinAs(fmt.Sprintf("Seed%d", i))
		j.guid = guid
		k := dial(t, tgt)
		defer k.close()
		k.sendTCP(j.encode())
		pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
		if !ok || len(pkt) < 9 {
			t.Errorf("%s: no 0x3E", tgt.label)
			return cap
		}
		tail := pkt[len(pkt)-8:]
		seeds = append(seeds, int32(binary.LittleEndian.Uint32(tail[0:4])))
		clocks = append(clocks, int32(binary.LittleEndian.Uint32(tail[4:8])))
	}
	cap.add(verdict("same seed for both clients", seeds[0] == seeds[1]))
	gap := clocks[1] - clocks[0]
	cap.add(verdict("CurrentTime advances with the clock", gap > 1000 && gap < 2500))
	return cap
}

// keepAlive streams positions and answers pings for p until stop closes, as
// a driving client would.
func keepAlive(p *player, stop chan struct{}) {
	start := time.Now()
	seq := uint8(0)
	for {
		select {
		case <-stop:
			return
		case <-time.After(50 * time.Millisecond):
		}
		seq++
		p.k.sendUDP(stampedUpdate(seq, uint32(5000+time.Since(start).Milliseconds()), 1))
		for {
			u, ok := p.k.readUDP(time.Millisecond)
			if !ok {
				break
			}
			if u[0] == proto.PingUpdate {
				w := wire.NewWriterID(proto.PingPong)
				w.Int32(int32(binary.LittleEndian.Uint32(u[1:5])))
				w.Int32(int32(time.Since(start).Milliseconds()))
				p.k.sendUDP(w.Bytes())
			}
		}
	}
}

// lapPacket is an inbound 0x49 with no splits and the client's lap number.
func lapPacket(ts, lapTime uint32, cuts, clientLap uint8) []byte {
	w := wire.NewWriterID(proto.LapCompleted)
	w.UInt32(ts)
	w.UInt32(lapTime)
	w.UInt8(0) // splits
	w.UInt8(cuts)
	w.UInt8(clientLap)
	return w.Bytes()
}

// boardEntry is one car's row in an outbound 0x49.
type boardEntry struct {
	laps     uint16
	time     uint32
	finished bool
}

// parseBoard reads an outbound 0x49 into who completed the lap and each row.
func parseBoard(pkt []byte) (uint8, map[uint8]boardEntry) {
	r := wire.NewReader(pkt[1:])
	who := r.UInt8()
	r.UInt32()
	r.UInt8()
	rows := map[uint8]boardEntry{}
	for n := int(r.UInt8()); n > 0; n-- {
		id := r.UInt8()
		rows[id] = boardEntry{time: r.UInt32(), laps: r.UInt16(), finished: r.UInt8() == 1}
	}
	return who, rows
}

// cfgTwoLapRace is a two-lap race with vanilla's minimum 20 s wait, a short
// RACE_OVER_TIME and room for three cars.
func cfgTwoLapRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[RACE]\nNAME=Race\nLAPS=2\nWAIT_TIME=0\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "RACE_OVER_TIME", 3)
		s = setINT(s, "RESULT_SCREEN_TIME", 3)
		return setINT(s, "MAX_CLIENTS", 3)
	})
}

// raceField joins the given players, keeps them driving, and returns them
// with a stop function.
func raceField(t *testing.T, tgt *target, guids ...string) ([]*player, func()) {
	var ps []*player
	stop := make(chan struct{})
	done := func() {
		close(stop)
		for _, p := range ps {
			p.k.close()
		}
	}
	for i, guid := range guids {
		j := joinAs(fmt.Sprintf("Racer%d", i))
		j.guid = guid
		p := joinPlayer(t, tgt, j)
		if p == nil {
			done()
			return nil, func() {}
		}
		ps = append(ps, p)
		if _, ok := p.enterWorld(t, tgt); !ok {
			done()
			return nil, func() {}
		}
		if !awaitReady(p) {
			t.Errorf("%s: %s never became ready", tgt.label, j.name)
			done()
			return nil, func() {}
		}
		go keepAlive(p, stop)
	}
	return ps, done
}

// awaitReady waits for the server's first 0xF9 ping, which both servers send
// in the pass that marks a client ready (its first 0x46 accepted), and
// answers it. Until then a lap's 0x49 skips the client: the 0x46 and a TCP
// packet sent right after it race on the server's separate sockets.
func awaitReady(p *player) bool {
	start := time.Now()
	for deadline := start.Add(3 * time.Second); time.Now().Before(deadline); {
		u, ok := p.k.readUDP(time.Until(deadline))
		if !ok {
			return false
		}
		if u[0] == proto.PingUpdate && len(u) >= 5 {
			w := wire.NewWriterID(proto.PingPong)
			w.Int32(int32(binary.LittleEndian.Uint32(u[1:5])))
			w.Int32(0)
			p.k.sendUDP(w.Bytes())
			return true
		}
	}
	return false
}

// probeRaceLapRules drives a two-lap race (docs/02 "End of a race"): a lap
// before the start is dropped, the server counts laps and times from the
// start, the leader finishes at LAPS and everyone else on their next lap, a
// finished car's laps are dropped, and 0x4B follows the last finisher.
func probeRaceLapRules(t *testing.T, tgt *target) capture {
	var cap capture
	ps, done := raceField(t, tgt, probeGUID, probeGUID2, probeGUID3)
	defer done()
	if ps == nil {
		return cap
	}
	a, b, c := ps[0], ps[1], ps[2]
	start := time.Now()
	clock := func() uint32 { return uint32(100000 + time.Since(start).Milliseconds()) }

	// await reads a's TCP until a 0x49 from who (or a 0x4B), up to 2 s.
	await := func(who uint8) (map[uint8]boardEntry, bool, bool) {
		deadline := time.Now().Add(2 * time.Second)
		for time.Now().Before(deadline) {
			pkt, ok := a.k.readTCP(time.Until(deadline))
			if !ok {
				break
			}
			if pkt[0] == proto.LapCompleted {
				if w, rows := parseBoard(pkt); w == who {
					return rows, true, false
				}
			}
			if pkt[0] == proto.RaceOver {
				return nil, false, true
			}
		}
		return nil, false, false
	}

	a.k.sendTCP(lapPacket(clock(), 60000, 0, 1))
	_, relayed, _ := await(a.sessionID)
	cap.add(verdict("lap before the start relayed", relayed))

	time.Sleep(time.Until(start.Add(22 * time.Second)))
	lap := func(p *player) (map[uint8]boardEntry, bool) {
		p.k.sendTCP(lapPacket(clock(), 60000, 0, 1)) // the client always says lap 1
		rows, ok, _ := await(p.sessionID)
		time.Sleep(300 * time.Millisecond)
		return rows, ok
	}
	rows, _ := lap(a)
	cap.add(verdict("race total is time since the start", rows[a.sessionID].time > 0 && rows[a.sessionID].time < 10_000))
	lap(b)
	rows, _ = lap(a)
	cap.add(verdict("leader finishes at LAPS", rows[a.sessionID].laps == 2 && rows[a.sessionID].finished && !rows[b.sessionID].finished))
	rows, _ = lap(b)
	cap.add(verdict("server counts laps, ignoring the client's", rows[b.sessionID].laps == 2))
	c.k.sendTCP(lapPacket(clock(), 60000, 0, 1))
	rows, _, _ = await(c.sessionID)
	cap.add(verdict("next lap after the leader finishes a car", rows[c.sessionID].laps == 1 && rows[c.sessionID].finished))
	_, _, over := await(255)
	cap.add(verdict("0x4B right after the last finisher", over))
	return cap
}

// cfgTimedRace is a one-minute race on TIME (LAPS=0); timedRaceExtraLap adds
// RACE_EXTRA_LAP.
func cfgTimedRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[RACE]\nNAME=Race\nLAPS=0\nTIME=1\nWAIT_TIME=0\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "RACE_OVER_TIME", 3)
		s = setINT(s, "RESULT_SCREEN_TIME", 3)
		return setINT(s, "MAX_CLIENTS", 3)
	})
}

func cfgTimedRaceExtraLap(t *testing.T, root, cfgDir string) {
	cfgTimedRace(t, root, cfgDir)
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "RACE_EXTRA_LAP", 1)
	})
}

// timedRaceProbe drives a one-minute timed race (docs/02 "End of a race"):
// before time-up the leader does two laps and the back-marker one; after it,
// the back-marker crosses first, then the leader, then each again. Vanilla
// finishes the leader on its first crossing after time-up (one lap later with
// RACE_EXTRA_LAP) and a back-marker only on a lap after the leader's finish.
func timedRaceProbe(t *testing.T, tgt *target) capture {
	var cap capture
	ps, done := raceField(t, tgt, probeGUID, probeGUID2)
	defer done()
	if ps == nil {
		return cap
	}
	a, b := ps[0], ps[1]
	start := time.Now()
	clock := func() uint32 { return uint32(100000 + time.Since(start).Milliseconds()) }
	lap := func(p *player) string {
		p.k.sendTCP(lapPacket(clock(), 60000, 0, 1))
		deadline := time.Now().Add(2 * time.Second)
		for time.Now().Before(deadline) {
			pkt, ok := a.k.readTCP(time.Until(deadline))
			if !ok {
				break
			}
			if pkt[0] == proto.LapCompleted {
				if who, rows := parseBoard(pkt); who == p.sessionID {
					time.Sleep(300 * time.Millisecond)
					return fmt.Sprintf("laps=%d finished=%v", rows[who].laps, rows[who].finished)
				}
			}
		}
		return "no 0x49"
	}
	// The race starts 20 s after the switch and runs a minute.
	time.Sleep(time.Until(start.Add(22 * time.Second)))
	lap(a)
	lap(a)
	lap(b)
	time.Sleep(time.Until(start.Add(83 * time.Second)))
	for i, p := range []*player{b, a, b, a, b} {
		who := "leader"
		if p == b {
			who = "back-marker"
		}
		cap.add(packet{label: fmt.Sprintf("crossing %d after time-up (%s)", i+1, who),
			fields: []field{{"result", lap(p)}}})
	}
	_, over := a.k.awaitTCP(proto.RaceOver, 5*time.Second)
	cap.add(verdict("0x4B once every car finished", over))
	return cap
}

// probeSunInCountdown joins during a race's 60 s countdown: vanilla's sun is
// computed from the start, so it sits below SUN_ANGLE until the green light
// (docs/02 0x54).
func probeSunInCountdown(t *testing.T, tgt *target) capture {
	var cap capture
	k := dial(t, tgt)
	defer k.close()
	k.sendTCP(joinAs("Early").encode())
	pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
	if !ok {
		cap.add(packet{label: "TCP <no 0x3E>"})
		return cap
	}
	// SUN_ANGLE is 16: about 15.7 a minute before the start, 16.0 if the sun
	// counted from the switch instead.
	for _, f := range decodeTCP(pkt).fields {
		if f.name == "sunAngle" {
			cap.add(packet{label: "0x3E sun in the countdown", fields: []field{f}})
		}
	}
	return cap
}

// cfgPracticeThenReversedRace is a practice and a two-lap race whose grid is
// reversed for a repeat (REVERSED_GRID_RACE_POSITIONS=-1).
func cfgPracticeThenReversedRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=30\nIS_OPEN=1\n\n"+
		"[RACE]\nNAME=Race\nLAPS=2\nWAIT_TIME=0\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "REVERSED_GRID_RACE_POSITIONS", -1)
		s = setINT(s, "RACE_OVER_TIME", 3)
		return setINT(s, "RESULT_SCREEN_TIME", 3)
	})
}

// probeReversedRaceRepeat: with REVERSED_GRID_RACE_POSITIONS set, vanilla
// runs a last race again on a reversed grid before moving on (docs/02 0x4A).
// An admin skips to the race, both cars finish it, and the next 0x4A is
// captured.
func probeReversedRaceRepeat(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	b, _ := joinInWorld(t, tgt, "Bravo", probeGUID2)
	if b == nil {
		return cap
	}
	defer b.k.close()
	stop := make(chan struct{})
	defer close(stop)
	go keepAlive(a, stop)
	go keepAlive(b, stop)

	a.k.sendTCP(chatMessage("/next_session"))
	if _, ok := a.k.awaitTCP(proto.CurrentSessionUpdate, 3*time.Second); !ok {
		cap.add(packet{label: "TCP <no 0x4A for the race>"})
		return cap
	}
	start := time.Now()
	time.Sleep(22 * time.Second)
	for _, p := range []*player{a, b, a, b} {
		p.k.sendTCP(lapPacket(uint32(100000+time.Since(start).Milliseconds()), 60000, 0, 1))
		time.Sleep(400 * time.Millisecond)
	}
	if _, ok := a.k.awaitTCP(proto.RaceOver, 5*time.Second); !ok {
		cap.add(packet{label: "TCP <no 0x4B>"})
		return cap
	}
	if pkt, ok := a.k.awaitTCP(proto.CurrentSessionUpdate, 15*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no 0x4A after the race>"})
	}
	return cap
}

// cfgPracticeThenClosedRace is a practice and a race closed to joins.
func cfgPracticeThenClosedRace(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=30\nIS_OPEN=1\n\n"+
		"[RACE]\nNAME=Race\nLAPS=5\nWAIT_TIME=60\nIS_OPEN=0\n\n")
}

// probeEmptyRaceSkipped: a race with IS_OPEN=0 and fewer than two loaded cars
// is over at once (vanilla Session.IsOver), with no 0x4B: the lone admin who
// skips into it is moved on to the practice.
func probeEmptyRaceSkipped(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	a.k.sendTCP(chatMessage("/next_session"))
	var types []string
	deadline := time.Now().Add(4 * time.Second)
	for time.Now().Before(deadline) {
		pkt, ok := a.k.readTCP(time.Until(deadline))
		if !ok {
			break
		}
		switch pkt[0] {
		case proto.CurrentSessionUpdate:
			r := wire.NewReader(pkt[1:])
			r.String8()
			r.UInt8()
			types = append(types, fmt.Sprintf("0x4A type %d", r.UInt8()))
		case proto.RaceOver:
			types = append(types, "0x4B")
		}
	}
	cap.add(packet{label: "after /next_session", fields: []field{{"sequence", strings.Join(types, ", ")}}})
	return cap
}

// cfgNoLoop is a single practice with LOOP_MODE=0.
func cfgNoLoop(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=30\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "LOOP_MODE", 0)
	})
}

// probeNoLoopShutsDown: without LOOP_MODE, vanilla exits after the last
// session, so skipping past it closes the connection.
func probeNoLoopShutsDown(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	a.k.sendTCP(chatMessage("/next_session"))
	closed := false
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); {
		if _, ok := a.k.readTCP(time.Until(deadline)); !ok {
			closed = time.Now().Before(deadline)
			break
		}
	}
	cap.add(verdict("connection closed after the last session", closed))
	return cap
}

// cfgRaceLongWait is a lap race with a 60 s countdown.
func cfgRaceLongWait(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[RACE]\nNAME=Race\nLAPS=5\nWAIT_TIME=60\nIS_OPEN=1\n\n")
}

// probeRaceOverCap: the leader finishes and nobody else does. Vanilla ends the
// race max(RACE_OVER_TIME, 30) s later; RACE_OVER_TIME is 3 here.
func probeRaceOverCap(t *testing.T, tgt *target) capture {
	var cap capture
	ps, done := raceField(t, tgt, probeGUID, probeGUID2)
	defer done()
	if ps == nil {
		return cap
	}
	a := ps[0]
	start := time.Now()
	time.Sleep(22 * time.Second)
	for i := 0; i < 2; i++ {
		a.k.sendTCP(lapPacket(uint32(100000+time.Since(start).Milliseconds()), 60000, 0, 1))
		time.Sleep(300 * time.Millisecond)
	}
	finishedAt := time.Now()
	pkt, ok := a.k.awaitTCP(proto.RaceOver, 40*time.Second)
	gap := time.Since(finishedAt)
	cap.add(verdict("0x4B 28-33 s after the leader finished", ok && pkt != nil && gap > 28*time.Second && gap < 33*time.Second))
	return cap
}

// probePracticeLapCount: in practice the server counts uncut laps itself;
// the client's lap numbers (5, 1, 1, 9) are ignored.
func probePracticeLapCount(t *testing.T, tgt *target) capture {
	var cap capture
	ps, done := raceField(t, tgt, probeGUID)
	defer done()
	if ps == nil {
		return cap
	}
	a := ps[0]
	var counts []uint16
	for i, n := range []uint8{5, 1, 1, 9} {
		a.k.sendTCP(lapPacket(uint32(100000+i*1000), uint32(60000+i*1000), uint8(i%2), n))
		deadline := time.Now().Add(2 * time.Second)
		for time.Now().Before(deadline) {
			pkt, ok := a.k.readTCP(time.Until(deadline))
			if !ok {
				break
			}
			if pkt[0] == proto.LapCompleted {
				if who, rows := parseBoard(pkt); who == a.sessionID {
					counts = append(counts, rows[a.sessionID].laps)
					break
				}
			}
		}
	}
	cap.add(packet{label: "lap counts after laps (cut every 2nd)", fields: []field{{"counts", fmt.Sprint(counts)}}})
	return cap
}

// cfgOneMinutePracticeShortResult is a one-minute practice with a 3 s result
// screen, below vanilla's 10 s floor.
func cfgOneMinutePracticeShortResult(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=P\nTIME=1\nIS_OPEN=1\n\n[QUALIFY]\nNAME=Q\nTIME=30\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "RESULT_SCREEN_TIME", 3)
	})
}

// probeResultScreenFloor waits out a one-minute practice. The minute brings
// the periodic 0x54 with the sun moved 0.264° (SUN_ANGLE is 16); then the next
// 0x4A comes max(RESULT_SCREEN_TIME, 10) s after 0x4B, followed by 0x78 and
// the reset 0x54 (docs/02 0x54, "End of a session").
func probeResultScreenFloor(t *testing.T, tgt *target) capture {
	var cap capture
	ps, done := raceField(t, tgt, probeGUID)
	defer done()
	if ps == nil {
		return cap
	}
	a := ps[0]
	var sun []float32
	gotOver := false
	for deadline := time.Now().Add(75 * time.Second); time.Now().Before(deadline) && !gotOver; {
		pkt, ok := a.k.readTCP(time.Until(deadline))
		if !ok {
			break
		}
		switch pkt[0] {
		case proto.SunAngleUpdate:
			sun = append(sun, wire.NewReader(pkt[1:]).Float32())
		case proto.RaceOver:
			gotOver = true
		}
	}
	cap.add(verdict("periodic 0x54 before 0x4B", len(sun) > 0))
	if len(sun) > 0 {
		cap.add(verdict("sun moved 0.2-0.35°", sun[0] > 16.2 && sun[0] < 16.35))
	}
	if !gotOver {
		t.Errorf("%s: no 0x4B", tgt.label)
		return cap
	}
	over := time.Now()
	_, ok := a.k.awaitTCP(proto.CurrentSessionUpdate, 15*time.Second)
	gap := time.Since(over)
	cap.add(verdict("next 0x4A 9-12 s after 0x4B", ok && gap > 9*time.Second && gap < 12*time.Second))
	var after []string
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] != proto.LapCompleted {
			after = append(after, fmt.Sprintf("0x%02X", pkt[0]))
		}
	}
	cap.add(packet{label: "after the 0x4A", fields: []field{{"ids", strings.Join(after, " ")}}})
	return cap
}

// cfgThreeOfTwelve sets MAX_CLIENTS=3 on the repo's 12-car entry list.
func cfgThreeOfTwelve(t *testing.T, root, cfgDir string) {
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string { return setINT(s, "MAX_CLIENTS", 3) })
}

// probeMaxClientsGrid: vanilla's grid is the first MAX_CLIENTS entries, so
// every slot list has 3 rows, not 12 (docs/08 §8.1).
func probeMaxClientsGrid(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Grid"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	counts := map[string]int{}
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		switch pkt[0] {
		case proto.DriverInfoUpdate, proto.BoPUpdate:
			counts[fmt.Sprintf("0x%02X rows", pkt[0])] = int(pkt[1])
		case proto.LapCompleted:
			counts["0x49 rows"] = int(pkt[7])
		case proto.P2PUpdate, proto.MandatoryPitUpdate:
			counts[fmt.Sprintf("0x%02X packets", pkt[0])]++
		}
	}
	w := wire.NewWriterID(proto.CarListRequest)
	w.UInt8(0)
	a.k.sendTCP(w.Bytes())
	if pkt, ok := a.k.awaitTCP(proto.CarList, 2*time.Second); ok {
		counts["0x40 page 0 rows"] = int(pkt[2])
	}
	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var fields []field
	for _, k := range keys {
		fields = append(fields, field{k, fmt.Sprint(counts[k])})
	}
	cap.add(packet{label: "slot lists", fields: fields})
	return cap
}

// probeRaceStartCountdown joins during a WAIT_TIME=0 race's countdown: 0x57
// repeats every second, starts 20 s after the switch, and carries the server
// time (docs/03 0x57).
func probeRaceStartCountdown(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Gridder")
	if a == nil {
		return cap
	}
	defer a.k.close()

	var starts, nows []int64
	var pingTime int64
	deadline := time.Now().Add(3500 * time.Millisecond)
	for time.Now().Before(deadline) {
		pkt, ok := a.k.readUDP(time.Until(deadline))
		if !ok {
			break
		}
		switch pkt[0] {
		case proto.RaceStart:
			r := wire.NewReader(pkt[1:])
			starts = append(starts, int64(r.Int32()))
			nows = append(nows, int64(r.UInt32()))
		case proto.PingUpdate:
			pingTime = int64(binary.LittleEndian.Uint32(pkt[1:5]))
		}
	}
	cap.add(verdict("several 0x57", len(starts) >= 3))
	if len(starts) == 0 {
		return cap
	}
	last := len(starts) - 1
	lead := starts[last] - nows[last]
	cap.add(verdict("start 15-20 s ahead", lead > 15_000 && lead <= 20_000))
	cap.add(verdict("start fixed", starts[0] == starts[last]))
	cap.add(verdict("second field is server time", pingTime != 0 && nows[last]-pingTime > -1500 && nows[last]-pingTime < 1500))
	return cap
}

// cfgPracticeOnly has a single practice and no race, so /INFO's "timed"
// (vanilla TimedRace) is false even though every session runs on time.
func cfgPracticeOnly(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=9999\nIS_OPEN=1\n\n")
}

// cfgTimedRacePitStart has a race on TIME (LAPS=0), which makes "timed" true,
// and a pit window with only a start, which vanilla's "pit" ignores.
func cfgTimedRacePitStart(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=10\nIS_OPEN=1\n\n"+
		"[RACE]\nNAME=Race\nLAPS=0\nTIME=15\nWAIT_TIME=60\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		return setINT(s, "RACE_PIT_WINDOW_START", 2)
	})
}

// cfgPracticeWait is a practice whose WAIT_TIME vanilla ignores.
func cfgPracticeWait(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=10\nWAIT_TIME=30\nIS_OPEN=1\n\n")
}

// probeStartVersusNow reads the 0x57 a fresh session sends a loaded client
// and reports whether the session has already started.
func probeStartVersusNow(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Early")
	if a == nil {
		return cap
	}
	defer a.k.close()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		pkt, ok := a.k.readUDP(time.Until(deadline))
		if !ok {
			break
		}
		if pkt[0] == proto.RaceStart {
			r := wire.NewReader(pkt[1:])
			start, now := int64(r.Int32()), int64(r.UInt32())
			cap.add(verdict("session started", start <= now))
			return cap
		}
	}
	cap.add(packet{label: "UDP <no 0x57 RaceStart>"})
	return cap
}

// probeSessionClockOffset gives an admin a clock 5 million ms away from the
// server's, then starts a new session. The 0x4A StartTime and both 0x57 times
// must land in the client's clock (docs/02 0x4A, docs/03 0x57): a real client
// read raw server time as a start hours away and kept the car in neutral.
func probeSessionClockOffset(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Skewed")
	if a == nil {
		return cap
	}
	defer a.k.close()

	start := time.Now()
	const base = 5_000_000
	clock := func() int64 { return base + time.Since(start).Milliseconds() }
	pong := func(pkt []byte) {
		w := wire.NewWriterID(proto.PingPong)
		w.Int32(int32(binary.LittleEndian.Uint32(pkt[1:5])))
		w.Int32(int32(clock()))
		a.k.sendUDP(w.Bytes())
	}
	// Past the first session's 0x57 window, so the next 0x57 is the new one's.
	for time.Since(start) < 6*time.Second {
		if pkt, ok := a.k.readUDP(50 * time.Millisecond); ok && pkt[0] == proto.PingUpdate {
			pong(pkt)
		}
	}
	// Vanilla adopts a pong's offset on the next accepted 0x46.
	a.k.sendUDP(stampedUpdate(1, uint32(clock()), 300))
	time.Sleep(250 * time.Millisecond)
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	near := func(v int64) bool { d := v - clock(); return d > -3000 && d < 3000 }
	a.k.sendTCP(chatMessage("/next_session"))
	var sawSession, sawStart bool
	deadline := time.Now().Add(4 * time.Second)
	for time.Now().Before(deadline) && !(sawSession && sawStart) {
		if pkt, ok := a.k.readTCP(20 * time.Millisecond); ok && pkt[0] == proto.CurrentSessionUpdate && !sawSession {
			sawSession = true
			r := wire.NewReader(pkt[len(pkt)-8:])
			cap.add(verdict("0x4A StartTime in client clock", near(r.Int64())))
		}
		pkt, ok := a.k.readUDP(20 * time.Millisecond)
		if !ok {
			continue
		}
		switch {
		case pkt[0] == proto.PingUpdate:
			pong(pkt)
		case pkt[0] == proto.RaceStart && sawSession && !sawStart:
			sawStart = true
			r := wire.NewReader(pkt[1:])
			st, now := int64(r.Int32()), int64(r.UInt32())
			cap.add(verdict("0x57 StartTime in client clock", near(st)))
			cap.add(verdict("0x57 ServerTime in client clock", near(now)))
		}
	}
	if !sawSession {
		cap.add(packet{label: "TCP <no 0x4A CurrentSessionUpdate>"})
	}
	if !sawStart {
		cap.add(packet{label: "UDP <no 0x57 RaceStart>"})
	}
	return cap
}

// probePingUpdate captures the server's periodic 0xF9 ping request.
func probePingUpdate(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Pinger"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}

	deadline := time.Now().Add(4 * time.Second)
	for time.Now().Before(deadline) {
		pkt, ok := p.k.readUDP(time.Until(deadline))
		if !ok {
			break
		}
		if pkt[0] == proto.PingUpdate {
			cap.add(decodeUDP(pkt))
			return cap
		}
	}
	t.Errorf("%s: no 0xF9 ping update within 4s", tgt.label)
	return cap
}

// probeSessionRequestMatching sends a correct 0x4F: no resync may follow.
func probeSessionRequestMatching(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Insync"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	w := wire.NewWriterID(proto.SessionRequest)
	w.UInt8(proto.SessionPractice) // the session the repo cfg starts in
	p.k.sendUDP(w.Bytes())

	for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no resync>"})
	}
	return cap
}

// probeSessionResync sends a UDP 0x4F claiming the wrong current session type
// and captures the resync the server sends back.
func probeSessionResync(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Resync"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	w := wire.NewWriterID(proto.SessionRequest)
	w.UInt8(99) // a session type the server is definitely not running
	p.k.sendUDP(w.Bytes())

	for _, pkt := range p.k.drainTCP(firstUpdateIdle, 2*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeCarListOccupied requests the car list with two players in world, so the
// entry rows carry real driver identities rather than empty slots.
func probeCarListOccupied(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	a.k.sendTCP(req.Bytes())

	if pkt, ok := a.k.awaitTCP(proto.CarList, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		t.Errorf("%s: no 0x40 CarList", tgt.label)
	}
	return cap
}

// ---------------------------------------------------------------------------
// Voting
//
// Vanilla always processes votes (docs/02 "Vote state"), so the whole
// subsystem is compared.
// ---------------------------------------------------------------------------

// voteSession builds a client 0x64/0x65 session vote.
func voteSession(id byte, yes bool) []byte {
	w := wire.NewWriterID(id)
	w.Bool(yes)
	return w.Bytes()
}

// voteKick builds a client 0x66 vote to kick a session id.
func voteKick(target uint8, yes bool) []byte {
	w := wire.NewWriterID(proto.VoteKickUser)
	w.UInt8(target)
	w.Bool(yes)
	return w.Bytes()
}

// soloVoteProbe has a lone driver vote (id 0x64 or 0x65) and captures the
// state broadcast. The vote is decided only at expiry, so nothing else follows
// yet.
func soloVoteProbe(id byte) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		p := joinPlayer(t, tgt, joinAs("Voter"))
		if p == nil {
			return cap
		}
		defer p.k.close()
		if _, ok := p.enterWorld(t, tgt); !ok {
			return cap
		}
		p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

		p.k.sendTCP(voteSession(id, true))
		for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// probeVoteNoQuorum: one of two players votes; captures what the other hears.
func probeVoteNoQuorum(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickNoQuorum has one player open a kick vote against the other and
// captures the *target's* view. At the repo cfg's KICK_QUORUM=85 one vote cannot
// carry it, so the target must see the vote against it and must not be kicked.
func probeVoteKickNoQuorum(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	a.k.sendTCP(voteKick(b.sessionID, true))
	for _, pkt := range b.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickPasses: with KICK_QUORUM at the floor, one yes kicks the target
// at expiry; captures the target's view.
func probeVoteKickPasses(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	a.k.sendTCP(voteKick(b.sessionID, true))
	for _, pkt := range b.k.drainTCP(6*time.Second, 15*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickSelf has a player vote to kick itself. A grid can be made to fan
// vote packets out by anyone who can send one, so what a server does with a
// nonsensical target is worth pinning: this server drops it before any broadcast.
func probeVoteKickSelf(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	a.k.sendTCP(voteKick(a.sessionID, true))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no vote broadcast>"})
	}
	return cap
}

// probeVoteKickEmptySlot votes to kick a slot nobody is in — the third entry-list
// car, which no probe player occupies. A vote against an empty slot can never do
// anything, so the question is only whether the server announces one anyway.
func probeVoteKickEmptySlot(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	a.k.sendTCP(voteKick(2, true)) // CAR_2, unoccupied
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no vote broadcast>"})
	}
	return cap
}

// probeVotePassesAtExpiry runs a lone vote (quorum 0) to expiry: the state
// broadcast, then the session change.
func probeVotePassesAtExpiry(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Voter"))
	if p == nil {
		return cap
	}
	defer p.k.close()
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	p.k.sendTCP(voteSession(proto.VoteNextSession, true))
	for _, pkt := range p.k.drainTCP(6*time.Second, 15*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteQuorumAfterLeave: a vote one short carries once the other driver
// leaves, because the quorum is recomputed live.
func probeVoteQuorumAfterLeave(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP([]byte{proto.CleanExitDrive})
	time.Sleep(300 * time.Millisecond)
	a.k.close()

	for _, pkt := range b.k.drainTCP(6*time.Second, 15*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteExpiry runs a failing vote to expiry (3 s, cfgShortVote) and
// captures the 0x67.
func probeVoteExpiry(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	// VOTE_DURATION=3 s, and the expiry is noticed by a one-second sweep on this
	// server; the idle gap has to outlast that quiet stretch before the 0x67.
	for _, pkt := range a.k.drainTCP(6*time.Second, 15*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// voteGrid brings n players into the world and drains them.
func voteGrid(t *testing.T, tgt *target, n int) []*player {
	names := []string{"Alpha", "Bravo", "Charlie", "Delta"}
	guids := []string{probeGUID, probeGUID2, probeGUID3, probeGUID4}
	if n > len(names) {
		t.Fatalf("voteGrid: %d players requested, %d identities available", n, len(names))
	}
	var players []*player
	closeAll := func() {
		for _, p := range players {
			p.k.close()
		}
	}
	for i := 0; i < n; i++ {
		j := joinAs(names[i])
		j.guid = guids[i]
		p := joinPlayer(t, tgt, j)
		if p == nil {
			closeAll()
			return nil
		}
		if _, ok := p.enterWorld(t, tgt); !ok {
			p.k.close()
			closeAll()
			return nil
		}
		players = append(players, p)
	}
	for _, p := range players {
		p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	}
	return players
}

// probeVoteAgainst has a driver vote no.
func probeVoteAgainst(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, false))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteAgainstAfterYes: one driver votes yes, another no. LastVoter must
// stay on the yes voter.
func probeVoteAgainstAfterYes(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 3)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteSession(proto.VoteNextSession, true))
	time.Sleep(300 * time.Millisecond)
	ps[2].k.sendTCP(voteSession(proto.VoteNextSession, false))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteChangedMind: one driver votes yes then no; the tally returns to 0.
func probeVoteChangedMind(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	time.Sleep(300 * time.Millisecond)
	b.k.sendTCP(voteSession(proto.VoteNextSession, false))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteTallyTwoYes: two of three drivers vote yes. Quorum is
// trunc(3 × 80%) = 2, so the tally reaches 2 without carrying.
func probeVoteTallyTwoYes(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 3)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteSession(proto.VoteNextSession, true))
	time.Sleep(300 * time.Millisecond)
	ps[2].k.sendTCP(voteSession(proto.VoteNextSession, true))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteQuorumFourDrivers: four at 80% gives trunc(3.2) = 3.
func probeVoteQuorumFourDrivers(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 4)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteSession(proto.VoteNextSession, true))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickQuorumThree: a kick excludes its target, so three at 85% gives
// trunc(2 × 85%) = 1, not 2.
func probeVoteKickQuorumThree(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 3)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteKick(ps[2].sessionID, true))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteDuration compares VOTE_DURATION, which the volatile mask otherwise
// hides, by rounding the first countdown to whole seconds.
func probeVoteDuration(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	pkt, ok := a.k.awaitTCP(proto.VoteNextSession, 3*time.Second)
	if !ok {
		t.Errorf("%s: no 0x64 vote broadcast", tgt.label)
		return cap
	}
	r := wire.NewReader(pkt[1:])
	r.UInt8() // target
	r.UInt8() // quorum
	r.UInt8() // voteCount
	secs := (r.UInt32() + 500) / 1000
	cap.add(packet{label: "verdict: vote countdown", fields: []field{
		{"roundedSeconds", fmt.Sprintf("%d", secs)},
	}})
	return cap
}

// probeVoteCompetingKind: a restart vote while a next-session vote runs. Two
// drivers send them, so the per-driver start cooldown is not what is tested.
func probeVoteCompetingKind(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 3)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteSession(proto.VoteNextSession, true))
	time.Sleep(300 * time.Millisecond)
	ps[2].k.sendTCP(voteSession(proto.VoteRestartSession, true))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickRetarget: a kick vote against a second driver while one runs.
func probeVoteKickRetarget(t *testing.T, tgt *target) capture {
	var cap capture
	ps := voteGrid(t, tgt, 3)
	if ps == nil {
		return cap
	}
	defer func() {
		for _, p := range ps {
			p.k.close()
		}
	}()

	ps[1].k.sendTCP(voteKick(ps[2].sessionID, true))
	time.Sleep(300 * time.Millisecond)
	ps[2].k.sendTCP(voteKick(ps[1].sessionID, true))
	for _, pkt := range ps[0].k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeVoteKickThenRejoin: after a vote kick, the driver rejoins with the same
// guid.
func probeVoteKickThenRejoin(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()

	a.k.sendTCP(voteKick(b.sessionID, true))
	b.k.drainTCP(6*time.Second, 15*time.Second) // wait out VOTE_DURATION and the kick
	b.k.close()
	time.Sleep(300 * time.Millisecond)

	second := joinAs("Bravo")
	second.guid = probeGUID2
	k := dial(t, tgt)
	defer k.close()
	k.sendTCP(second.encode())

	// As in rejoinProbe, only the verdict is comparable: the slot handed back and
	// the reference's drifted sun angle are not.
	outcome := "TCP <no response>"
	if pkt, ok := k.readTCP(3 * time.Second); ok {
		outcome = decodeTCP(pkt).label
	}
	cap.add(packet{label: "verdict: rejoin outcome", fields: []field{{"answer", outcome}}})
	return cap
}

// probeVoteAcrossSessionChange: an admin changes session during a vote. Is it
// dropped, or closed later with 0x67?
func probeVoteAcrossSessionChange(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha")
	if a == nil {
		return cap
	}
	defer a.k.close()

	second := joinAs("Bravo")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	b.k.sendTCP(voteSession(proto.VoteNextSession, true))
	time.Sleep(300 * time.Millisecond)
	a.k.sendTCP(chatMessage("/next_session"))
	// The admin's own stream first, so the command's outcome is part of the
	// comparison whether it switches the session or refuses to, and then the
	// voter's, which is where the poll's fate shows up.
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	for _, pkt := range b.k.drainTCP(6*time.Second, 15*time.Second) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// ---------------------------------------------------------------------------
// Admin commands and session transitions
//
// Session changes use admin commands, to keep these fast. SessionExpiry waits
// out a real clock.
// ---------------------------------------------------------------------------

// chatMessage builds a client 0x47. The claimed session id is ignored by this
// server, which substitutes the real sender; vanilla relays the claim (see
// probeChatSpoofedSender).
func chatMessage(text string) []byte {
	w := wire.NewWriterID(proto.Chat)
	w.UInt8(0)
	w.WString8(text)
	return w.Bytes()
}

// joinAdmin joins and enters the world as an administrator, using the admin
// password as the join password.
func joinAdmin(t *testing.T, tgt *target, name string) *player {
	j := joinAs(name)
	j.password = adminPassword
	p := joinPlayer(t, tgt, j)
	if p == nil {
		return nil
	}
	if _, ok := p.enterWorld(t, tgt); !ok {
		p.k.close()
		return nil
	}
	p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	return p
}

// adminPassword is ADMIN_PASSWORD from the repo cfg.
const adminPassword = "admin123"

// commandProbe sends one chat command as an admin and captures everything the
// server sends back, so reply wording, packet order and any side-effect
// broadcasts are all compared.
func commandProbe(cmd string) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a := joinAdmin(t, tgt, "Admin")
		if a == nil {
			return cap
		}
		defer a.k.close()

		a.k.sendTCP(chatMessage(cmd))
		for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// nonAdminCommandProbe sends a command as an ordinary player, which vanilla
// answers exactly as it answers an unknown command.
func nonAdminCommandProbe(cmd string) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		p := joinPlayer(t, tgt, joinAs("Peasant"))
		if p == nil {
			return cap
		}
		defer p.k.close()
		if _, ok := p.enterWorld(t, tgt); !ok {
			return cap
		}
		p.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

		p.k.sendTCP(chatMessage(cmd))
		for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// kickProbe has an admin remove a second player and captures the admin's view:
// the announcement, the 0x68, and the private confirmation.
func kickProbe(cmd string) func(*testing.T, *target) capture {
	return func(t *testing.T, tgt *target) capture {
		var cap capture
		a := joinAdmin(t, tgt, "Alpha")
		if a == nil {
			return cap
		}
		defer a.k.close()

		second := joinAs("Bravo")
		second.guid = probeGUID2
		b := joinPlayer(t, tgt, second)
		if b == nil {
			return cap
		}
		defer b.k.close()
		if _, ok := b.enterWorld(t, tgt); !ok {
			return cap
		}
		a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

		a.k.sendTCP(chatMessage(cmd))
		for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
			cap.add(decodeTCP(pkt))
		}
		return cap
	}
}

// clientListWithTwoPlayers runs /client_list with a second driver on the grid,
// so the multi-line output (one message per driver) is exercised rather than
// just the single-admin case.
func clientListWithTwoPlayers(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha")
	if a == nil {
		return cap
	}
	defer a.k.close()

	second := joinAs("Bravo")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	if _, ok := b.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	a.k.sendTCP(chatMessage("/client_list"))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// probeRaceStart captures the UDP 0x57 RaceStart a client receives when a race
// session begins. The race is (re)started under the client's feet with
// /restart_session so the probe does not have to be connected before boot.
func probeRaceStart(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Racer")
	if a == nil {
		return cap
	}
	defer a.k.close()

	a.k.sendTCP(chatMessage("/restart_session"))

	deadline := time.Now().Add(4 * time.Second)
	for time.Now().Before(deadline) {
		pkt, ok := a.k.readUDP(time.Until(deadline))
		if !ok {
			break
		}
		if pkt[0] == proto.RaceStart {
			cap.add(decodeUDP(pkt))
			return cap
		}
	}
	cap.add(packet{label: "UDP <no 0x57 RaceStart>"})
	return cap
}

// No "finish a race on laps" probe: vanilla ignores laps from a car that never
// properly took the start. probeSessionExpiry covers the session end.

// probeSessionExpiry waits out a one-minute practice session and captures the
// end-of-session sequence: 0x4B at the expiry, then the next session's 0x4A
// once the result screen has run. This is the slow probe.
func probeSessionExpiry(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Waiter")
	if a == nil {
		return cap
	}
	defer a.k.close()

	// TIME=1 plus RESULT_SCREEN_TIME=10, with headroom for a slow start.
	for _, pkt := range a.k.drainTCP(20*time.Second, 100*time.Second) {
		if pkt[0] == proto.RaceOver || pkt[0] == proto.CurrentSessionUpdate {
			cap.add(decodeTCP(pkt))
		}
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no session transition>"})
	}
	return cap
}

// ---------------------------------------------------------------------------
// Entry-list shapes and malformed input
// ---------------------------------------------------------------------------

// probeCarListPageOutOfRange asks for a page past the end of a 3-car entry list.
func probeCarListPageOutOfRange(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Pager"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(200) // far past the end
	p.k.sendTCP(req.Bytes())

	if pkt, ok := p.k.awaitTCP(proto.CarList, 2*time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no car list>"})
	}
	return cap
}

// probeReservedSlotOwner joins as the guid that CAR_0 is reserved for.
func probeReservedSlotOwner(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()
	return joinAndCapture(t, tgt, k, joinAs("Owner"))
}

// probeReservedSlotOther joins as a *different* guid, which must not be handed
// the reserved slot.
func probeReservedSlotOther(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()
	j := joinAs("Stranger")
	j.guid = probeGUID2
	return joinAndCapture(t, tgt, k, j)
}

// No spectator probe: vanilla refuses to start with SPECTATOR_MODE=1.

// probeSessionClosed joins a server whose sessions are all IS_OPEN=0.
func probeSessionClosed(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()
	return joinAndCapture(t, tgt, k, joinAs("Closed"))
}

// probeTruncatedHandshake sends a 0x3D that ends mid-field. Neither server may
// crash or hang: the connection is expected to be refused or dropped, and the
// probe records which.
func probeTruncatedHandshake(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	w := wire.NewWriterID(proto.RequestNewConnection)
	w.UInt16(proto.ClientProtocolVersion)
	w.String8(probeGUID)
	// Stops here: no name, team, nation, car or password follow.
	k.sendTCP(w.Bytes())

	var cap capture
	if pkt, ok := k.readTCP(2 * time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <connection dropped>"})
	}
	return cap
}

// probeUnknownPacketID sends an unallocated packet id on an established
// connection and records whether the server answers, ignores it, or hangs up.
func probeUnknownPacketID(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Garbage"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	p.k.sendTCP([]byte{0xEE, 0x01, 0x02, 0x03})
	if pkt, ok := p.k.readTCP(1500 * time.Millisecond); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <no response>"})
	}

	// The connection must still work afterwards, so ask for something.
	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	p.k.sendTCP(req.Bytes())
	if pkt, ok := p.k.awaitTCP(proto.CarList, 2*time.Second); ok {
		cap.add(packet{label: "TCP still alive", fields: []field{
			{"carListLen", fmt.Sprint(len(pkt))},
		}})
	} else {
		cap.add(packet{label: "TCP <dead after unknown id>"})
	}
	return cap
}

// probePreHandshakePacket sends a non-0x3D packet before handshaking, which the
// docs say drops the connection.
func probePreHandshakePacket(t *testing.T, tgt *target) capture {
	k := dial(t, tgt)
	defer k.close()

	req := wire.NewWriterID(proto.CarListRequest)
	req.UInt8(0)
	k.sendTCP(req.Bytes())

	var cap capture
	if pkt, ok := k.readTCP(2 * time.Second); ok {
		cap.add(decodeTCP(pkt))
	} else {
		cap.add(packet{label: "TCP <connection dropped>"})
	}
	return cap
}

// probeHTTPInfo fetches GET /INFO and captures it as a decoded packet.
func probeHTTPInfo(t *testing.T, tgt *target) capture {
	url := fmt.Sprintf("http://127.0.0.1:%d/INFO", tgt.ports.http)
	var cap capture
	resp, err := http.Get(url)
	if err != nil {
		t.Errorf("%s: GET /INFO: %v", tgt.label, err)
		return cap
	}
	defer resp.Body.Close()

	var body map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil {
		t.Errorf("%s: decode /INFO: %v", tgt.label, err)
		return cap
	}
	cap.add(decodeInfoJSON(body))
	return cap
}

// ---------------------------------------------------------------------------
// HTTP surface beyond /INFO
// ---------------------------------------------------------------------------

// probeHTTPJSON fetches the Content Manager entry-list endpoint, GET /JSON|<guid>.
func probeHTTPJSON(t *testing.T, tgt *target) capture {
	return fetchJSON(t, tgt, "/JSON|"+probeGUID, "HTTP /JSON")
}

// probeHTTPJSONOccupied fetches /JSON|<guid> with two drivers in their slots, so
// the Cars array carries real identities.
func probeHTTPJSONOccupied(t *testing.T, tgt *target) capture {
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return capture{}
	}
	defer a.k.close()
	defer b.k.close()
	return fetchJSON(t, tgt, "/JSON|"+probeGUID, "HTTP /JSON")
}

// probeHTTPInfoOccupied fetches /INFO with two drivers connected, exercising the
// live client count and driver list.
func probeHTTPInfoOccupied(t *testing.T, tgt *target) capture {
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return capture{}
	}
	defer a.k.close()
	defer b.k.close()
	return probeHTTPInfo(t, tgt)
}

// fetchText GETs a path and returns its status and body.
func fetchText(t *testing.T, tgt *target, path string) (int, string) {
	resp, err := http.Get(fmt.Sprintf("http://127.0.0.1:%d%s", tgt.ports.http, path))
	if err != nil {
		t.Errorf("%s: GET %s: %v", tgt.label, path, err)
		return 0, ""
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

// entryVolatile masks /ENTRY's clock, throughput and the session time at the
// probe's lap (its Total), which differ run to run.
var entryVolatile = regexp.MustCompile(`TIME LEFT \d+:\d+|Throughput: \d+kBs|<td>0:00:\d{3}</td>`)

// entryProbe fetches the /ENTRY HTML page (docs/05 §5.2a) with a driver
// named `name` in the world.
func entryProbe(name string) func(t *testing.T, tgt *target) capture {
	return func(t *testing.T, tgt *target) capture {
		return probeHTTPEntry(t, tgt, name)
	}
}

func probeHTTPEntry(t *testing.T, tgt *target, name string) capture {
	var cap capture
	p, _ := joinInWorld(t, tgt, name, probeGUID)
	if p == nil {
		return cap
	}
	defer p.k.close()
	p.k.sendTCP(lapTimed(120000, 90000, 1))
	time.Sleep(300 * time.Millisecond)
	status, body := fetchText(t, tgt, "/ENTRY")
	if os.Getenv("PROBE_DUMP") != "" {
		t.Logf("%s /ENTRY:\n%s", tgt.label, body)
	}
	cap.add(packet{label: "HTTP /ENTRY", fields: []field{
		{"status", fmt.Sprint(status)},
		{"body", entryVolatile.ReplaceAllString(body, "<volatile>")},
	}})
	return cap
}

// textReply records one plain-text HTTP reply, its trailing ",<seconds left>"
// masked.
func textReply(t *testing.T, tgt *target, path string) packet {
	status, body := fetchText(t, tgt, path)
	return packet{label: "HTTP " + path, fields: []field{
		{"status", fmt.Sprint(status)},
		{"body", secondsLeft.ReplaceAllString(body, ",<seconds>")},
	}}
}

var secondsLeft = regexp.MustCompile(`,\d+$`)

// probeManagerEndpoints asks a pickup server the acServerManager endpoints
// (docs/05 §5.2a): booking requests fail, and /CANJOIN and /ACAUTH answer.
func probeManagerEndpoints(t *testing.T, tgt *target) capture {
	var cap capture
	p, _ := joinInWorld(t, tgt, "Alpha", probeGUID)
	if p == nil {
		return cap
	}
	defer p.k.close()
	for _, path := range []string{
		"/SUB|" + probeCar + "|00_official|Bravo|Team|" + probeGUID2,
		"/UNSUB|" + probeGUID2,
		"/CANJOIN|" + probeGUID,
		"/CANJOIN|" + probeGUID2,
		"/CANJOIN",
		"/ACAUTH?GUID=ks1",
		"/ACAUTH?GUID=" + probeGUID,
	} {
		cap.add(textReply(t, tgt, path))
	}
	return cap
}

// probeManagerReconfig sends /RECONFIG a valid and an invalid JSON body
// (docs/05 §5.2a), which a pickup server parses and discards.
func probeManagerReconfig(t *testing.T, tgt *target) capture {
	var cap capture
	for _, path := range []string{
		`/RECONFIG?json={"Name":"x","RaceLaps":3}`,
		"/RECONFIG?json=nope",
	} {
		cap.add(textReply(t, tgt, path))
	}
	return cap
}

// cfgBooking is a booking server: pickup off, room for two bookings beyond
// the twelve entry-list slots, a one-minute [BOOK] session, then a practice.
func cfgBooking(t *testing.T, root, cfgDir string) {
	setSessions(cfgDir, t, "[BOOK]\nNAME=Booking\nTIME=1\n\n"+
		"[PRACTICE]\nNAME=Practice\nTIME=30\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "PICKUP_MODE_ENABLED", 0)
		return setINT(s, "MAX_CLIENTS", 14)
	})
}

// probeBookingFlow books, re-books, releases and joins on a booking server
// (docs/05 §5.2a): /SUB fills the lowest free session id, a booked driver who
// joins during the booking session hears 0x41, and once it is over the
// booked driver joins the booked slot.
func probeBookingFlow(t *testing.T, tgt *target) capture {
	var cap capture
	start := time.Now()
	sub := func(name, guid, car string) string {
		return "/SUB|" + car + "|00_official|" + name + "|Team|" + guid
	}
	for _, path := range []string{
		sub("Alpha", probeGUID, probeCar),
		sub("Alpha2", probeGUID, probeCar),
		sub("Bad", probeGUID3, "no_such_car"),
		"/SUB|x",
		sub("Bravo", probeGUID2, probeCar),
		"/UNSUB|" + probeGUID4,
		"/UNSUB|" + probeGUID2,
		"/CANJOIN|" + probeGUID,
	} {
		cap.add(textReply(t, tgt, path))
	}
	cap.packets = append(cap.packets, fetchJSON(t, tgt, "/JSON|"+probeGUID, "HTTP /JSON").packets...)

	k := dial(t, tgt)
	j := joinAs("Alpha")
	k.sendTCP(j.encode())
	if pkt, ok := k.readTCP(3 * time.Second); ok {
		label := fmt.Sprintf("join during booking: 0x%02X", pkt[0])
		if pkt[0] == proto.ServerRunning && len(pkt) >= 5 {
			left := binary.LittleEndian.Uint32(pkt[1:5])
			label += fmt.Sprintf(", %v ms left within the minute", left > 0 && left <= 60000)
		}
		cap.add(packet{label: label})
	} else {
		cap.add(packet{label: "join during booking: no reply"})
	}
	k.close()

	time.Sleep(time.Until(start.Add(63 * time.Second)))
	cap.add(textReply(t, tgt, "/CANJOIN|"+probeGUID))
	k = dial(t, tgt)
	defer k.close()
	k.sendTCP(joinAs("Whoever").encode())
	if pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second); ok {
		cap.add(packet{label: "booked join", fields: []field{{"sessionID", fmt.Sprint(sessionIDFromHandshake(pkt))}}})
	} else {
		cap.add(packet{label: "booked join: no 0x3E"})
	}
	return cap
}

// cfgEntryListReload: pickup off, the admin's slot reserved, one practice that
// loops.
func cfgEntryListReload(t *testing.T, root, cfgDir string) {
	// CAR_0 already has an empty GUID= line, so set that one.
	editCfg(t, cfgDir, "entry_list.ini", func(s string) string {
		return strings.Replace(s, "GUID=\n", "GUID="+probeGUID+"\n", 1)
	})
	setSessions(cfgDir, t, "[PRACTICE]\nNAME=Practice\nTIME=30\nIS_OPEN=1\n\n")
	editCfg(t, cfgDir, "server_cfg.ini", func(s string) string {
		s = setINT(s, "PICKUP_MODE_ENABLED", 0)
		s = setINT(s, "LOOP_MODE", 1)
		return setINT(s, "MAX_CLIENTS", 14)
	})
}

// probeEntryListReload edits entry_list.ini, then skips the only session so
// the sessions loop. With pickup mode off vanilla reads the file again
// (docs/05 §5.2a): the car list shows the edits, and the driver the new
// CAR_12 reserves can join it. Vanilla seats nobody as admin from the join's
// password with pickup off, so the driver becomes one with /admin. Vanilla
// orphans connected clients on a reload, so the driver's own connection is
// not read afterwards.
func probeEntryListReload(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Admin")
	if a == nil {
		return cap
	}
	defer a.k.close()
	a.k.sendTCP(chatMessage("/next_session"))
	cap.add(packet{label: "join password grants admin", fields: []field{{"0x4A", fmt.Sprint(gotTCP(a, proto.CurrentSessionUpdate))}}})
	a.k.sendTCP(chatMessage("/admin " + adminPassword))
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	// Both targets read one cfg dir, so the edit must be idempotent.
	editCfg(t, tgt.cfgDir, "entry_list.ini", func(s string) string {
		s = strings.Replace(s, "[CAR_1]\nMODEL="+probeCar+"\nSKIN=01_cup_07", "[CAR_1]\nMODEL="+probeCar+"\nSKIN=01_edited", 1)
		if strings.Contains(s, "[CAR_12]") {
			return s
		}
		return s + fmt.Sprintf("\n[CAR_12]\nMODEL=%s\nSKIN=00_official\nSPECTATOR_MODE=0\nDRIVERNAME=Added\nTEAM=\nGUID=%s\nBALLAST=0\nRESTRICTOR=0\n", probeCar, probeGUID2)
	})
	a.k.sendTCP(chatMessage("/next_session"))
	time.Sleep(time.Second)
	// Vanilla orphans the admin here and acserver-rs keeps them, so they
	// leave before the list is read.
	a.leave()
	cap.packets = append(cap.packets, fetchJSON(t, tgt, "/JSON|"+probeGUID, "HTTP /JSON").packets...)

	j := joinAs("Added")
	j.guid = probeGUID2
	k := dial(t, tgt)
	defer k.close()
	k.sendTCP(j.encode())
	if pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second); ok {
		cap.add(packet{label: "added driver joins", fields: []field{{"sessionID", fmt.Sprint(sessionIDFromHandshake(pkt))}}})
	} else {
		cap.add(packet{label: "added driver: no 0x3E"})
	}
	return cap
}

// gotTCP reports whether a packet with id arrives within the drain window.
func gotTCP(p *player, id byte) bool {
	for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == id {
			return true
		}
	}
	return false
}

// probeHTTPUnknownPath asks for a path neither server serves, comparing the
// status code rather than a body.
func probeHTTPUnknownPath(t *testing.T, tgt *target) capture {
	var cap capture
	url := fmt.Sprintf("http://127.0.0.1:%d/no/such/endpoint", tgt.ports.http)
	resp, err := http.Get(url)
	if err != nil {
		t.Errorf("%s: GET unknown path: %v", tgt.label, err)
		return cap
	}
	defer resp.Body.Close()
	io.Copy(io.Discard, resp.Body)
	cap.add(packet{
		label:  "HTTP unknown path",
		fields: []field{{"status", fmt.Sprint(resp.StatusCode)}},
	})
	return cap
}

// fetchJSON GETs a JSON endpoint and flattens it into comparable fields.
func fetchJSON(t *testing.T, tgt *target, path, label string) capture {
	var cap capture
	url := fmt.Sprintf("http://127.0.0.1:%d%s", tgt.ports.http, path)
	resp, err := http.Get(url)
	if err != nil {
		t.Errorf("%s: GET %s: %v", tgt.label, path, err)
		return cap
	}
	defer resp.Body.Close()

	var body any
	if err := json.NewDecoder(resp.Body).Decode(&body); err != nil {
		t.Errorf("%s: decode %s: %v", tgt.label, path, err)
		return cap
	}
	p := packet{label: label}
	flattenJSON("", body, &p.fields)
	sort.Slice(p.fields, func(i, j int) bool { return p.fields[i].name < p.fields[j].name })
	cap.add(p)
	return cap
}

// flattenJSON walks a decoded JSON value into dotted-path leaf fields, masking
// the volatile ones so only structural and configured differences remain.
func flattenJSON(prefix string, v any, out *[]field) {
	switch t := v.(type) {
	case map[string]any:
		keys := make([]string, 0, len(t))
		for k := range t {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			flattenJSON(joinPath(prefix, k), t[k], out)
		}
	case []any:
		*out = append(*out, field{joinPath(prefix, "len"), fmt.Sprint(len(t))})
		for i, e := range t {
			flattenJSON(fmt.Sprintf("%s[%d]", prefix, i), e, out)
		}
	default:
		leaf := prefix
		if i := strings.LastIndexAny(leaf, ".]"); i >= 0 {
			leaf = leaf[i+1:]
		}
		if jsonVolatile[leaf] {
			*out = append(*out, field{prefix, volatile})
			return
		}
		*out = append(*out, field{prefix, fmt.Sprintf("%v", v)})
	}
}

func joinPath(prefix, key string) string {
	if prefix == "" {
		return key
	}
	return prefix + "." + key
}

// jsonVolatile are JSON leaf keys tied to ephemeral ports, the server clock or
// per-run identity.
var jsonVolatile = map[string]bool{
	"port": true, "cport": true, "tport": true, "ip": true,
	"timestamp": true, "timeleft": true, "TimeLeft": true, "Time": true,
	"IsRequestedGUID": true, "Timestamp": true,
	// Session time at a lap, which a millisecond of scheduling moves.
	"TotalTime": true,
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

// probeChecksumValid joins, answers 0x44 with the digests the server asked for,
// and enters the world: the first-update burst must arrive with no kick.
func probeChecksumValid(t *testing.T, tgt *target) capture {
	return checksumProbe(t, tgt, true)
}

// probeChecksumInvalid answers 0x44 with deliberately wrong digests. Vanilla
// defers the kick until after the first update, so the burst should still
// arrive, followed by 0x68 KickCar.
func probeChecksumInvalid(t *testing.T, tgt *target) capture {
	return checksumProbe(t, tgt, false)
}

func checksumProbe(t *testing.T, tgt *target, valid bool) capture {
	var cap capture
	k := dial(t, tgt)
	defer k.close()

	k.sendTCP(handshakeRequest("Checksum"))
	pkt, ok := k.awaitTCP(proto.NewCarConnection, 3*time.Second)
	if !ok {
		t.Errorf("%s: handshake failed before checksum", tgt.label)
		return cap
	}
	p := &player{k: k, sessionID: sessionIDFromHandshake(pkt)}

	k.sendTCP(checksumPacket(t, pkt, valid))
	if _, ok := p.enterWorld(t, tgt); !ok {
		return cap
	}
	// Only the kick outcome is compared: the rest of the burst is already
	// covered by the FirstUpdate probe.
	for _, pkt := range k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if pkt[0] == proto.KickCar {
			cap.add(decodeTCP(pkt))
		}
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no kick>"})
	}
	return cap
}

// checksumPacket builds the 0x44 response to a handshake: one MD5 per requested
// path, then the car's data.acd digest. When valid is false every digest is
// zeroed so the server must reject the blob.
func checksumPacket(t *testing.T, handshake []byte, valid bool) []byte {
	var paths []string
	var model string
	for _, f := range decodeTCP(handshake).fields {
		switch {
		case strings.HasPrefix(f.name, "checksumPath["):
			paths = append(paths, f.value)
		case f.name == "carModel":
			model = f.value
		}
	}

	w := wire.NewWriterID(proto.ChecksumPacket)
	for _, p := range paths {
		w.Raw(digestOf(t, valid, filepath.Join(contentDir(), filepath.FromSlash(p))))
	}
	w.Raw(digestOf(t, valid, filepath.Join(contentDir(), "content", "cars", model, "data.acd")))
	return w.Bytes()
}

// digestOf returns the file's MD5, or 16 zero bytes when a deliberately wrong
// digest is wanted (or when the file is missing, which the servers treat as
// unverifiable either way).
func digestOf(t *testing.T, valid bool, path string) []byte {
	if !valid {
		return make([]byte, 16)
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return make([]byte, 16)
	}
	sum := md5.Sum(b)
	return sum[:]
}

// infoVolatile are /INFO keys tied to the ephemeral ports or the server clock.
var infoVolatile = map[string]bool{
	"port": true, "cport": true, "tport": true,
	"timestamp": true, "timeleft": true, "ip": true,
}

// infoSuffix is EnableServerDetails' " ℹ<port>" name suffix.
var infoSuffix = regexp.MustCompile(` ℹ\d+$`)

// decodeInfoJSON flattens the /INFO body into sorted fields, masking volatile
// keys so only structural / configured differences remain.
func decodeInfoJSON(body map[string]any) packet {
	keys := make([]string, 0, len(body))
	for k := range body {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	p := packet{label: "HTTP /INFO"}
	for _, k := range keys {
		if infoVolatile[k] {
			p.fields = append(p.fields, field{k, volatile})
			continue
		}
		// EnableServerDetails (on by default) adds " ℹ<http port>" to the
		// name; the port differs per run and the suffix is cosmetic.
		if name, ok := body[k].(string); ok && k == "name" {
			p.fields = append(p.fields, field{k, infoSuffix.ReplaceAllString(name, "")})
			continue
		}
		p.fields = append(p.fields, field{k, fmt.Sprintf("%v", body[k])})
	}
	return p
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Exploratory probes
//
// Hostile inputs, and rules this server applies that vanilla does not.
// ---------------------------------------------------------------------------

// probeChatControlChars sends chat with a newline, a bell and an ANSI escape.
func probeChatControlChars(t *testing.T, tgt *target) capture {
	return chatRelay(t, tgt, "line one\nline two\x07\x1b[31mred")
}

// probeChatWhitespaceOnly sends a message that is only whitespace. This server
// drops it (handleChat trims before deciding there is anything to say).
func probeChatWhitespaceOnly(t *testing.T, tgt *target) capture {
	return chatRelay(t, tgt, "   \t  ")
}

// probeImplausibleLap reports a 0.5 s lap. Both relay it; only vanilla makes it
// the best lap.
func probeImplausibleLap(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP(lapTimed(120000, 500, 1))
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		if len(pkt) > 0 && pkt[0] == proto.LapCompleted {
			cap.add(decodeTCP(pkt))
		}
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no leaderboard>"})
	}
	return cap
}

// probeDuplicateGUID joins twice with one guid.
func probeDuplicateGUID(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinPlayer(t, tgt, joinAs("Alpha"))
	if a == nil {
		return cap
	}
	defer a.k.close()
	if _, ok := a.enterWorld(t, tgt); !ok {
		return cap
	}
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	k := dial(t, tgt)
	defer k.close()
	cap = joinAndCapture(t, tgt, k, joinAs("AlphaAgain"))

	// Is the first connection still open? Drain it: a close ends in an error,
	// an open socket in a timeout.
	a.k.tcp.SetReadDeadline(time.Now().Add(2 * time.Second))
	closed := false
	for {
		if _, err := a.k.br.ReadByte(); err != nil {
			var ne net.Error
			closed = !(errors.As(err, &ne) && ne.Timeout())
			break
		}
	}
	cap.add(verdict("first connection closed", closed))
	return cap
}

// probeHandshakeTwice sends a second, valid 0x3D down a connection that has
// already handshaked — a slot is seated and the client is asking for another.
func probeHandshakeTwice(t *testing.T, tgt *target) capture {
	var cap capture
	p := joinPlayer(t, tgt, joinAs("Twice"))
	if p == nil {
		return cap
	}
	defer p.k.close()

	p.k.sendTCP(handshakeRequest("Twice"))
	for _, pkt := range p.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <no answer>"})
	}
	// The entry list says what the second handshake cost: whether one driver now
	// occupies two slots.
	w := wire.NewWriterID(proto.CarListRequest)
	w.UInt8(0)
	p.k.sendTCP(w.Bytes())
	if pkt, ok := p.k.awaitTCP(proto.CarList, 3*time.Second); ok {
		cap.add(decodeTCP(pkt))
	}
	return cap
}

// compareCaptures diffs two captures field for field, reporting every
// difference via t.Errorf. want is the reference, got is the candidate.
func compareCaptures(t *testing.T, probeName string, got, want capture) {
	t.Helper()
	if len(got.packets) != len(want.packets) {
		t.Errorf("%s: packet count: ours=%d %v reference=%d %v",
			probeName, len(got.packets), captureLabels(got),
			len(want.packets), captureLabels(want))
	}
	n := len(got.packets)
	if len(want.packets) < n {
		n = len(want.packets)
	}
	for i := 0; i < n; i++ {
		g, w := got.packets[i], want.packets[i]
		if g.label != w.label {
			t.Errorf("%s: packet[%d] label: ours=%q reference=%q", probeName, i, g.label, w.label)
			continue
		}
		compareFields(t, fmt.Sprintf("%s: %s", probeName, g.label), g.fields, w.fields)
	}
}

func captureLabels(c capture) []string {
	out := make([]string, len(c.packets))
	for i, packet := range c.packets {
		out[i] = packet.label
	}
	return out
}

// compareFields diffs two ordered field lists.
func compareFields(t *testing.T, ctx string, got, want []field) {
	t.Helper()
	gi := indexFields(got)
	wi := indexFields(want)
	// Fields present in one but not the other, or with differing values.
	seen := map[string]bool{}
	for _, f := range got {
		seen[f.name] = true
		wv, ok := wi[f.name]
		if !ok {
			t.Errorf("%s: field %q present in go (=%q) but absent in reference", ctx, f.name, f.value)
			continue
		}
		if f.value != wv {
			t.Errorf("%s: field %q: ours=%q reference=%q", ctx, f.name, f.value, wv)
		}
	}
	for _, f := range want {
		if !seen[f.name] {
			t.Errorf("%s: field %q present in reference (=%q) but absent in go", ctx, f.name, gi[f.name])
		}
	}
}

func indexFields(fs []field) map[string]string {
	m := make(map[string]string, len(fs))
	for _, f := range fs {
		m[f.name] = f.value
	}
	return m
}

// ---------------------------------------------------------------------------
// Test entry points
// ---------------------------------------------------------------------------

// probeSlots bounds probe concurrency. Probes are independent (own ports,
// config and servers), but each ends a capture on a quiet gap, and an
// overloaded machine produces false gaps. Most probes spend their time waiting
// on the servers' timers rather than the CPU, so the default is one per CPU.
// COMPARE_PARALLEL=1 runs serially.
var probeSlots = make(chan struct{}, probeConcurrency())

func probeConcurrency() int {
	if v := os.Getenv("COMPARE_PARALLEL"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			return n
		}
	}
	n := runtime.NumCPU()
	if n < 2 {
		return 2
	}
	if n > 12 {
		return 12
	}
	return n
}

// parallelProbe marks a probe subtest parallel and takes one of the slots above
// for the duration of the test, releasing it once the test (and its target
// shutdown) is done.
func parallelProbe(t *testing.T) {
	t.Helper()
	t.Parallel()
	probeSlots <- struct{}{}
	t.Cleanup(func() { <-probeSlots })
}

// TestCompareSelfConsistent replays every probe against two independent
// servers of ours and asserts identical captures, exercising the decoders and volatile
// masking without the reference binary.
func TestCompareSelfConsistent(t *testing.T) {
	// Parallel with TestCompareWithReference: both share probeSlots, so the
	// slow probes of one overlap the other's instead of each ending on its own
	// long tail.
	t.Parallel()
	for _, pr := range probes() {
		pr := pr
		t.Run(pr.name, func(t *testing.T) {
			parallelProbe(t)
			a := runProbe(t, pr)
			b := runProbe(t, pr)
			compareCaptures(t, pr.name, a, b)
		})
	}
}

// TestCompareWithReference replays every probe against both our server and
// the original acServer, diffing their responses. Set AC_REFERENCE_SERVER to
// enable; see COMPARE.md.
func TestCompareWithReference(t *testing.T) {
	t.Parallel() // see TestCompareSelfConsistent
	if referenceBinary() == "" {
		t.Skip("set AC_REFERENCE_SERVER to the original acServer binary to enable binary comparison")
	}
	for _, pr := range probes() {
		pr := pr
		t.Run(pr.name, func(t *testing.T) {
			parallelProbe(t)
			p := freePorts(t)
			root := t.TempDir()
			cfgDir := writeTemplatedCfg(t, root, p, pr.cfg)

			// Ours first, then reference on the same
			// ports (they never run at the same time, so the port reuse is
			// safe).
			ourCap := withTarget(t, startOurTarget(t, root, cfgDir, p), pr.run)
			refCap := withTarget(t, startRefTarget(t, root, p), pr.run)
			if os.Getenv("PROBE_DUMP") != "" {
				t.Logf("ours:\n%s\nreference:\n%s", dumpCapture(ourCap), dumpCapture(refCap))
			}

			if pr.dev != nil {
				checkDeviation(t, pr.name, pr.dev, ourCap, refCap)
				return
			}
			compareCaptures(t, pr.name, ourCap, refCap)
		})
	}
}

// dumpCapture renders a capture one packet per line, for PROBE_DUMP.
func dumpCapture(c capture) string {
	var b strings.Builder
	for _, p := range c.packets {
		b.WriteString("  " + p.label)
		for _, f := range p.fields {
			fmt.Fprintf(&b, " %s=%q", f.name, f.value)
		}
		b.WriteString("\n")
	}
	return b.String()
}

// runProbe starts a fresh server of ours on free ports, runs the probe, and
// tears it down.
func runProbe(t *testing.T, pr probe) capture {
	t.Helper()
	p := freePorts(t)
	root := t.TempDir()
	cfgDir := writeTemplatedCfg(t, root, p, pr.cfg)
	return withTarget(t, startOurTarget(t, root, cfgDir, p), pr.run)
}

// withTarget runs a probe against a target and guarantees the target is stopped
// (and its port released) before returning.
func withTarget(t *testing.T, tgt *target, run func(t *testing.T, tgt *target) capture) capture {
	t.Helper()
	defer func() {
		tgt.stop()
		// Give the OS a moment to release the shared game port before the next
		// target rebinds it.
		time.Sleep(100 * time.Millisecond)
	}()
	return run(t, tgt)
}

// ---------------------------------------------------------------------------
// Readiness, client time requests and client-sent 0x3B (docs/01 §1.5)
// ---------------------------------------------------------------------------

// probeLoadingGetsNoBroadcast joins a second player that associates UDP but
// sends no 0x46, so it is still loading. An admin's chat must not reach it.
// After it sends a UDP 0x4C Pulse, the next chat must.
func probeLoadingGetsNoBroadcast(t *testing.T, tgt *target) capture {
	var cap capture
	a := joinAdmin(t, tgt, "Alpha")
	if a == nil {
		return cap
	}
	defer a.k.close()

	second := joinAs("Loading")
	second.guid = probeGUID2
	b := joinPlayer(t, tgt, second)
	if b == nil {
		return cap
	}
	defer b.k.close()
	w := wire.NewWriterID(proto.CarConnect)
	w.UInt8(b.sessionID)
	b.k.sendUDP(w.Bytes())
	if _, ok := b.k.readUDP(3 * time.Second); !ok {
		t.Errorf("%s: no 0x4E UDP association reply", tgt.label)
		return cap
	}
	b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
	a.k.drainTCP(firstUpdateIdle, firstUpdateBudget)

	capturePhase := func(label string) {
		cap.add(packet{label: "phase: " + label})
		pkts := b.k.drainTCP(firstUpdateIdle, firstUpdateBudget)
		for _, pkt := range pkts {
			cap.add(decodeTCP(pkt))
		}
		if len(pkts) == 0 {
			cap.add(packet{label: "<none>"})
		}
	}

	a.k.sendTCP(chatMessage("while loading"))
	capturePhase("chat while loading")

	b.k.sendUDP([]byte{proto.Pulse})
	time.Sleep(200 * time.Millisecond)
	a.k.sendTCP(chatMessage("after pulse"))
	capturePhase("chat after 0x4C")
	return cap
}

// probeClientTimeRequest sends a 0xF9 with a token from a socket that never
// associated. Vanilla answers 0xF8 with the token echoed and its clock; the
// clock differs between servers, so only the shape is compared.
func probeClientTimeRequest(t *testing.T, tgt *target) capture {
	var cap capture
	k := dial(t, tgt)
	defer k.close()

	const token = 0x1122334455667788
	w := wire.NewWriterID(proto.PingUpdate)
	w.UInt64(token)
	k.sendUDP(w.Bytes())

	reply, ok := k.readUDP(2 * time.Second)
	if !ok {
		cap.add(packet{label: "UDP <no answer>"})
		return cap
	}
	r := wire.NewReader(reply[1:])
	echoed := r.UInt64() == token
	r.UInt32() // server clock
	cap.add(packet{
		label: fmt.Sprintf("UDP 0x%02X", reply[0]),
		fields: []field{
			{name: "len", value: fmt.Sprint(len(reply))},
			{name: "tokenEchoed", value: fmt.Sprint(echoed)},
			{name: "trailing", value: fmt.Sprint(r.Remaining())},
		},
	})
	return cap
}

// probeClientSentBlacklisted has an in-world player send TCP 0x3B. Vanilla
// kicks the sender: the other player sees the chat and the 0x68.
func probeClientSentBlacklisted(t *testing.T, tgt *target) capture {
	var cap capture
	a, b := twoPlayersInWorld(t, tgt)
	if a == nil || b == nil {
		return cap
	}
	defer a.k.close()
	defer b.k.close()

	b.k.sendTCP([]byte{proto.Blacklisted})
	for _, pkt := range a.k.drainTCP(firstUpdateIdle, firstUpdateBudget) {
		cap.add(decodeTCP(pkt))
	}
	if len(cap.packets) == 0 {
		cap.add(packet{label: "TCP <none>"})
	}
	return cap
}
