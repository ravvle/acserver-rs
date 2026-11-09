# Changelog

Keep a section short: what an operator or player would notice, a few bullets at
most. The reasoning behind a change belongs in its commit message, not here.

## v1.0.2

- **Non-ASCII team and nation names show correctly** to other players, as
  with vanilla, instead of turning into `�`.
- **Editing `entry_list.ini` with pickup mode off no longer confuses
  connected players.** If a loop back changes the car list, everyone is
  disconnected so they rejoin with the new list; an edit that leaves the list
  alone (BoP, for one) keeps them connected.

## v1.0.1

- **CSP clients get the server's features.** `/api/details` is always served
  and lists `CLIENT_MESSAGES`, `CLIENT_UDP_MESSAGES` and `EMOJI`, so CSP
  scripts exchange events directly (UDP included) instead of through chat,
  and emoji in names and chat no longer turn into `?`.
- **`EnableServerDetails` is on by default,** as in AssettoServer: the server
  name gets its `ℹ<port>` suffix, so Content Manager finds the details page
  from the lobby list.
- **CSP script messages reach their sender too,** as CSP scripts expect.
- **Distant AI cars move more smoothly** when their updates are thinned out.

## v1.0.0

First release of acserver-rs, a drop-in replacement for the Kunos `acServer`
(protocol 202, ACSP v4), written in Rust. It behaves like vanilla v1.15
wherever a client can tell, checked packet by packet against the original.

- **Runs your existing setup:** vanilla `server_cfg.ini`, `entry_list.ini`,
  `results/*.json` and the `-c`/`-e`/`-platform` flags, so the Kunos
  acServerManager can launch it. A config value that does not fit its setting
  stops the server at startup, naming the key.
- **The full vanilla feature set,** hardened where vanilla is open to abuse
  (README "How it differs from the original").
- **AssettoServer's features,** each opt-in in `extra_cfg.yml`: AI traffic,
  CSP WeatherFX and server scripts, user groups and admin commands, automatic
  kicks, live configuration, the bundled plugins, content downloads and RCON.
- **A quarter of vanilla's CPU** on a 31-car grid, a third on 64
  (`test/PERFORMANCE.md`).
- **Static binaries and ready-to-run bundles** for Linux (amd64, arm64,
  armv7), Windows (amd64, arm64) and macOS (amd64, arm64).
