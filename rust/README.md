# acserver-rs

The server. The main [README](../README.md) describes its configuration,
deviations from vanilla `acServer` and AssettoServer features,
`extra_cfg.yml` keys included.

## Building and testing

See the Rust section of [CLAUDE.md](../CLAUDE.md).

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo build --release
# The comparison and extension tests in ../test, which build the release
# binary themselves:
cd ../test && go test -timeout 30m .
```

`tests/` holds the in-process integration tests (joins, races, CSP,
hardening, content downloads, full-grid load). The Go suite in `../test`
drives the binary from outside and diffs it against vanilla packet by packet
(`test/COMPARE.md`).
