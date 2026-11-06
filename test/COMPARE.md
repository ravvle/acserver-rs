# Reference comparison suite

This suite runs the same scripted client exchanges ("probes") against our
`acserver-rs` (the release build of `rust/`) and the **original Kunos `acServer`
binary**, then diffs the replies
field by field. Any unintended difference from the real protocol fails a test.

## Running it

Put a Kunos dedicated-server install (the `acServer` binary plus `content/`,
`system/`, …) at **`kunosserverfiles/`** in the repo root. It is gitignored. Then:

```sh
cd test && go test . -run TestCompareWithReference -v -timeout 30m
```

| Test | What it does |
|---|---|
| `TestCompareWithReference` | Every probe against ours and the reference, diffed. Skipped without an install |
| `TestCompareSelfConsistent` | Every probe against two of our servers. Checks the decoders and masking; always runs |
| `TestSpectatorSlot` | With a `SPECTATOR_MODE=1` slot: the reference must refuse to start; ours starts, and a `0x4A` read as the game reads it misplaces StartTime |
| `TestExt*` | End-to-end tests of the AssettoServer features (README "Configuration"): custom updates, WeatherFX, server scripts and `AS_ApiKey`, AI traffic on a generated track, user groups with the extra admin commands, the automatic kicks, the network bubble and far-car `0x48`s, AI paint and debug events, six of the built-in plugins, live configuration, and (with `AI_SPLINE_AIP` and `AI_SPLINE_TRACK`) traffic on a real AI package |

The tests build `rust/` with `cargo build --release` once per run, unless
`ACSERVER_BIN` names a binary to use instead.

Each entry point takes about **3 minutes**. Probes run in parallel because each
starts fresh servers and several wait out real clocks. The whole package takes
about 13 minutes under `-race`, so always pass `-timeout 30m`. Without it, Go's
default 10-minute limit panics with `test timed out after 10m0s`, which looks
like a hang but isn't one.

| Variable | Default | Meaning |
|---|---|---|
| `ACSERVER_BIN` | `rust/target/release/acserver-rs`, built | The server binary under test |
| `COMPARE_PARALLEL` | half the cores, 2–6 | Probe concurrency. Set it to `1` first when a probe fails only under load |
| `AC_REFERENCE_SERVER` | `kunosserverfiles/acServer` | Reference binary |
| `AC_REFERENCE_DIR` | `kunosserverfiles/` | Install whose data dirs are symlinked beside the templated config |
| `AC_REFERENCE_ARGS` | — | Extra flags for the reference, split on whitespace |
| `AC_CONTENT` | `kunosserverfiles/` | `--content` for our server. It must be the install **root** (holding `content/` and `system/`) so both servers produce the same checksum paths |
| `PROBE_DUMP` | — | When set, logs both captures of every reference probe, one packet per line |

**The install is never modified.** Each run writes a port-rebound copy of `./cfg`
to a temp dir, symlinks the install's data dirs next to it, and gives the
reference fresh `logs/`, `results/` and `setups/`. `blacklist.txt` is not
symlinked, because `/ban_id` appends to it. `extra_cfg.yml` is omitted, so both
servers run with defaults.

## How comparison works

Each probe returns a `capture`: an ordered list of packets decoded into named
fields. A mismatch reads like a protocol table:

```
Handshake: TCP 0x3E NewCarConnection: field "allowedTyresOut": ours="2" reference="3"
```

Values that legitimately change between runs, such as ephemeral ports, server
clocks and random seeds, decode to `«volatile»`. Everything else must match
exactly, with floats compared to 6 significant figures. Probes that bring a
client into the world capture the whole burst, so packet **order and count** are
compared too.

## Recorded deviations

Some probes expect the two servers to differ. A deviation pins **both** answers,
so the test fails if either server changes. When both send the same packet with
different contents, `alsoDiffer` additionally fails if the captures ever become
identical.

| Probe | Ours | Vanilla |
|---|---|---|
| `RejectNoGUID` | `0x6F` | admits a client with no guid |
| `RejectEmptyName` | `0x6F` | admits a blank name |
| `TruncatedHandshake` | `0x6F` | `0x45` (the same blank-name case, reached through the empty car model) |
| `DuplicateGUID` | `0x3E`, the old connection closed | seats the same guid twice |
| `HandshakeTwice` | ignores the second `0x3D` | seats the connection in a second slot |
| `SessionClosed` | `0x6E`, per the official guide | admits in pickup mode despite `IS_OPEN=0` |
| `CarListPageOutOfRange` | empty page | reads past its array (`count=59`) |
| `ChatSpoofedSender` | relays the real sender | relays the id the client wrote |
| `ChatControlChars` | strips control characters except tab | relays newline and escapes verbatim |
| `ChatWhitespaceOnly` | drops the line | relays it |
| `PositionImplausible` | drops NaN coordinates | relays them to every car |
| `ImplausibleLap` | records a 0.5 s lap but not as the best | takes it as the best lap |
| `VoteKickSelf`, `VoteKickEmptySlot` | nothing broadcast | opens and announces the vote |
| `CmdKickIdEmptySlot` | `ERROR: Cannot find driver id 2` | "kicks" the empty slot with a blank name and a `0x68` |
| `CmdClientList`, `CmdClientListTwo` | one `0x47` per driver | `Command:/client_list not recognized` |
| `HTTPUnknownPath` | `404` | `200`, empty body |
| `HTTPEntryEscaped` | `/ENTRY` HTML-escapes driver names, teams and the session name | writes them verbatim |
| `ManagerReconfig` | discards the settings; serde's error text, and `400` for a raw `"` in the query | discards the settings |

## Probes

### Handshake and joining

| Probe | Exchange |
|---|---|
| `UnsupportedProtocol` | wrong version → `0x42` |
| `Handshake` | valid join → full `0x3E` |
| `HandshakeAltRules` | every driving rule at a non-default value |
| `HandshakeFixedSetup` | `0x3E` with a two-model entry list |
| `RejectUnknownCar` | model not in the entry list → `0x45` |
| `RejectNoSlots` | full grid → `0x45` |
| `RejectWrongPassword` / `AcceptPassword` / `AcceptAdminPassword` | join password handling |
| `RejectBlacklisted` | guid in `blacklist.txt` → `0x3B` |
| `RejectNoGUID` / `RejectEmptyName` / `DuplicateGUID` / `HandshakeTwice` | deviations |
| `ReservedSlotOwner` / `ReservedSlotOther` / `ReservedSlotJson` | entry-list `GUID=` over TCP and `/JSON` |
| `PickupModeOff` / `SessionClosed` | `PICKUP_MODE_ENABLED=0`; `IS_OPEN=0` |
| `MaxClientsCap` | `MAX_CLIENTS=1` with free slots left |
| `KickThenRejoin` / `BanThenRejoin` | rejoin after `/kick_id` and `/ban_id` |
| `UnicodeName` / `UnicodeJoinBroadcast` | accents, CJK and a non-BMP emoji through `string8` and `wstring8` |

### Loading and the first update

| Probe | Exchange |
|---|---|
| `UDPAssociation` | `0x4E` → reply |
| `FirstUpdate` | the whole first-update burst, in order |
| `WelcomeMessage` | the same with `WELCOME_MESSAGE` set |
| `FirstUpdateEntryBoP` / `BoPBeforeJoin` | entry-list BoP; admin BoP on an empty slot |
| `FirstUpdateRaceState` | a newcomer sees another car's P2P and pit state |
| `SecondClientJoin` | what a loaded client hears when another joins |
| `ChecksumValid` / `ChecksumInvalid` | real MD5s → no kick; zeroed → `0x68` after the snapshot |
| `ChatWhileLoading` | chat before entering the world |

### Car list, chat and relays

| Probe | Exchange |
|---|---|
| `CarList` / `CarListOccupied` / `CarListMultiModel` / `CarListWindow` | `0x40` variants, including a non-zero start page |
| `CarListAfterDamage` / `CarListPageOutOfRange` | damage in `0x40`; bad page |
| `ChatBroadcast` / `ChatBurst` | relay; three in a row (vanilla rate-limits nothing) |
| `UnicodeChat` / `ChatMaxLength` | Unicode chat; a 255-character message |
| `ChatSpoofedSender` / `ChatControlChars` / `ChatWhitespaceOnly` | deviations |
| `RelaySectorSplit` / `RelaySectorSplitOOB` | `0x58`, including an index past the sector count |
| `RelayTyreCompound` / `RelayIllegalTyre` | `0x50`, including a compound not in `LEGAL_TYRES` |
| `RelayDamage` / `RelayP2PActivate` / `RelayMandatoryPit` | `0x56`, `0x0D`, `0x0E` |
| `RelayJumpStart` | `0x82` jump start → broadcast |
| `RelayCollisionEnv` / `RelayCollisionCar` | collisions reach only the plugin |
| `EchoDamage` / `EchoP2PQuery` / `EchoTyreCompound` | does the sender hear its own relay? |
| `EchoSectorSplit`, `EchoP2PActivate`, `EchoMandatoryPit`, `EchoJumpStart`, `EchoCollision*`, `EchoIllegalTyre`, `EchoLapWithCut`, `EchoSectorSplitOOB` | the same for every other relayed packet |
| `CarDisconnect` | `0x43` → `0x4D` |

### Laps and leaderboards

| Probe | Exchange |
|---|---|
| `RelayLapCompleted` / `RelayLapWithCut` / `RelayLapNoSplits` | `0x49`; with cuts (lap count, grip); with no splits |
| `EchoLap` | does the lapping car get its own leaderboard? |
| `RepeatedLap` | a lap whose timestamp doesn't advance is dropped |
| `LeaderboardOrder` | the faster car ranks first |
| `BestLapProgression` | time tracks the *fastest* lap, count tracks laps run |
| `LeaderboardSyncAfterLap` | a late joiner's sync carries the standings |
| `LeaderboardAfterDisconnect` / `SlotReuseResult` | results stay with the slot |
| `ImplausibleLap` | deviation |

### Positions and UDP

| Probe | Exchange |
|---|---|
| `PositionBroadcast` | `0x46` in → `0x46` out |
| `PositionStaleTimestamp` | a backwards timestamp is dropped |
| `MaxClientsGrid` | with `MAX_CLIENTS=3` of 12 entries, every slot list has 3 rows |
| `PositionTimestampRebase` | a relayed timestamp lands in the receiver's clock |
| `PositionOffsetSlew` | after a pong 100 ms off, relayed timestamps move 3 ms per update |
| `PositionRelayEachUpdate` | each `0x46` relayed once, with no visible delay at 60 and 500 Hz (a relay pass every `SLEEP_TIME=1` ms looks immediate); nothing once the sender stops |
| `PositionBurstMerges` | five back-to-back updates from one car are relayed as fewer, the newest among them (one relay per car per pass) |
| `UDPReassociation` | repeated `0x4E` answered; a new port takes over |
| `PingsAfterLoad` | no `0xF9` until the first `0x46` |
| `PingAverageNoRamp` | the reported ping is the mean of the samples so far |
| `HandshakeSeedAndClock` | `0x3E` seed shared by all clients; `CurrentTime` is the server clock |
| `PositionImplausible` | deviation |
| `PingUpdate` | periodic `0xF9` |
| `LobbyCheck` | `0xC8` → reply |
| `SessionRequestResync` / `SessionRequestMatching` | `0x4F` wrong type → `0x4A`; right type → nothing |
| `RaceStart` | `0x57` on a race (re)start |
| `RaceLapRules` | race laps: none before the start, server-counted, finish on the next lap after the leader |
| `RaceOverCap` | `0x4B` `max(RACE_OVER_TIME, 30)` s after the leader finishes |
| `PracticeLapCount` | practice counts uncut laps itself |
| `ResultScreenFloor` | `0x54` every 60 s; next session `max(RESULT_SCREEN_TIME, 10)` s after `0x4B`, then `0x54` |
| `RaceStartCountdown` | `0x57` every second of the countdown, 20 s minimum wait |
| `PracticeIgnoresWaitTime` | practice starts at once despite `WAIT_TIME` |
| `SessionClockOffset` | `0x4A` and `0x57` times are in the receiver's clock (server clock − ping offset) |
| `SunOnSessionSwitch` | past a minute of uptime, a restart and a skip each send the reset sun after `0x4A`/`0x78` (~65 s per target) |
| `SlotHandover` | a returning guid reclaims its slot and laps; a newcomer clears the slot it takes; departed names and tyres stay in the snapshot |
| `LeaderboardHoldsOrder` | the leaderboard order is re-sorted only on a counted lap |
| `VoteDurationExact` | a new vote's `0x64` time is exactly `VOTE_DURATION` ms |
| `DepartedDriverListings` | a departed driver stays named in `0x40`, `/JSON` and the results file |
| `StepTimers` | the first `0xF9` follows loading, and a switch's first `0x57` its `0x4A`, within 200 ms |
| `TimedRaceFinish` / `TimedRaceExtraLap` | a timed race finishes the leader on its first crossing after time-up (one lap later with the extra lap), others after it (~90 s each) |
| `SunInCountdown` | the sun sits below `SUN_ANGLE` during a race countdown |
| `ReversedRaceRepeat` | a last race with `REVERSED_GRID_RACE_POSITIONS` runs again on a reversed grid |
| `EmptyRaceSkipped` | a closed race with one loaded car is skipped with no `0x4B` |
| `NoLoopShutsDown` | `LOOP_MODE=0` stops the server after the last session |
| `FirstUpdateExtras` | welcome, fixed setup and DRS in vanilla's order; a section without `VALUE` sent as 0 |
| `ManagerEndpoints` | `/SUB`, `/UNSUB`, `/CANJOIN`, `/ACAUTH` on a pickup server |
| `ManagerReconfig` | `/RECONFIG` with valid and invalid JSON (deviation) |
| `BookingFlow` | book, re-book, release and join on a booking server (~65 s per target) |
| `HTTPEntry` | `/ENTRY`'s HTML, clock and throughput masked |
| `HTTPEntryEscaped` | deviation: a driver named `<b>A&B</b>` |
| `EntryListReload` | pickup off: a loop re-reads `entry_list.ini`; the join password grants no admin |
| `BoPResetOnJoin` | a join restores the slot's entry-list BoP and, if changed, broadcasts `0x70` after the `0x5A` |

### Chat commands

| Probe | Exchange |
|---|---|
| `CmdHelp` / `CmdUnknown` / `CmdUpperCase` / `CmdTrailingSpace` | reply text; exact matching |
| `CmdAdminOK` / `CmdAdminWrong` / `CmdAdminNoArgs` | `/admin` |
| `CmdAdminOnlyAsPlayer` | admin command from a non-admin |
| `CmdBallast` / `CmdRestrictor` | reply and the every-slot `0x70` |
| `CmdBallastBadCar` / `CmdBallastNoArgs` / `CmdRestrictorNoArgs` | error messages |
| `CmdBallastOutOfRange` / `CmdRestrictorNegative` | out-of-range values are refused |
| `CmdBallastNonNumericID` / `CmdBallastNonNumericValue` / `CmdBallastExtraArg` / `CmdRestrictorNonNumeric` | every malformed shape → the syntax error |
| `CmdKickByName` / `CmdKickByID` / `CmdBanByID` | the full kick sequence |
| `CmdKickUnknownName` / `CmdKickIdOutOfRange` / `CmdKickIdNegative` | kick targets that don't exist |
| `CmdKickIdEmptySlot` / `CmdClientList` / `CmdClientListTwo` | deviations |

### Sessions

| Probe | Exchange |
|---|---|
| `CmdNextSession` / `CmdKsns` / `CmdRestartSession` / `CmdKsrs` | `0x4A`, `0x78`, reply; the aliases |
| `NextSessionWhileLoading` | a switch stalls while someone loads |
| `QualifyToRaceGrid` | qualifying order becomes the race grid |
| `NextSessionClearsLeaderboard` / `RestartClearsLeaderboard` | old times (and rubber) are gone |
| `SessionChangeResetsCarState` | P2P and pit reset; BoP stays |
| `SessionExpiry` | practice runs out: `0x4B`, then `0x4A` (~40 s per target) |

### Votes

| Probe | Exchange |
|---|---|
| `VoteNextSessionSolo` / `VoteRestartSessionSolo` | a lone vote: broadcast, nothing happens yet |
| `VoteNoQuorum` / `VoteKickNoQuorum` | what the grid is told |
| `VotePassesAtExpiry` / `VoteExpiry` | pass → session change; fail → `0x67` |
| `VoteKickPasses` | target's view, announcement, `0x68` |
| `VoteAgainst` / `VoteAgainstAfterYes` / `VoteChangedMind` | `LastVoter` semantics; one vote per id |
| `VoteTallyTwoYes` / `VoteQuorumFourDrivers` / `VoteKickQuorumThree` | tallies and quorum truncation |
| `VoteQuorumAfterLeave` | quorum recomputed after a driver leaves |
| `VoteDuration` | `VOTE_DURATION` countdown |
| `VoteCompetingKind` / `VoteKickRetarget` | a second vote while one runs |
| `VoteKickThenRejoin` / `VoteAcrossSessionChange` | after a kick vote; across a session change |
| `VoteKickSelf` / `VoteKickEmptySlot` | deviations |

### HTTP and malformed input

| Probe | Exchange |
|---|---|
| `HTTPInfo` / `HTTPInfoOccupied` / `HTTPInfoPracticeOnly` / `HTTPInfoTimedRace` | `/INFO`, key by key; `timed` is the race running on `TIME` |
| `HTTPJson` / `HTTPJsonOccupied` | `/JSON\|<guid>`, leaf by leaf |
| `HTTPUnknownPath` | deviation |
| `TruncatedHandshake` | deviation |
| `UnknownPacketID` | an unallocated id; the connection must survive |
| `PreHandshakePacket` | a non-`0x3D` before handshaking |

### Not probed

- **Spectator slots.** Vanilla refuses to start with `SPECTATOR_MODE=1`.
- **Finishing a race on laps.** Vanilla ignores laps from a car that never
  properly took the start, so a scripted client gets no `0x49` or `0x4B`.
  `SessionExpiry` covers the end-of-session path instead.
- **Lobby registration.** The lobby address is hard-coded. To capture it, give
  each server a network namespace that owns that IP:

  ```sh
  unshare -rn sh -c '
    ip link set lo up
    ip addr add 93.57.10.21/32 dev lo
    python3 fake_lobby.py &          # logs the request line, replies "OK"
    cd rundir && ./acServer          # or: ./acserver-rs --cfg cfg
  '
  ```

  `rundir` needs `cfg/` with `REGISTER_TO_LOBBY=1`, plus `content/` and `system/`.
  Diffing the two captures shows the `durations` and `max_clients` differences
  in docs/05 §5.8.

## Adding a probe

1. Write `func(t *testing.T, tgt *target) capture` using the `kit` helpers
   (`dial`, `sendTCP`, `awaitTCP`, `sendUDP`, `readUDP`, `drainTCP`) or
   `joinPlayer` / `enterWorld` / `twoPlayersInWorld`. For a non-default config,
   add a `cfg` tweak; it runs before either server starts.
2. Decode each reply in `compare_decode_test.go`, adding a decoder if needed.
   Mask run-dependent fields with `markVol` / `volatile`.
3. Register the probe in `probes()`.

| File | Contents |
|---|---|
| `compare_harness_test.go` | launchers, config templating, TCP/UDP kit |
| `compare_decode_test.go` | per-packet decoders and volatile masking |
| `compare_probes_test.go` | probes, comparator, test entry points |
