# Running under systemd (AlmaLinux 10)

## One server

```sh
# 1. Build a static binary and install it (needs musl-gcc and musl-libc-static)
(cd rust && CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl)
sudo install -m 0755 rust/target/x86_64-unknown-linux-musl/release/acserver-rs /usr/local/bin/acserver-rs
sudo restorecon -v /usr/local/bin/acserver-rs           # SELinux label bin_t

# 2. Service account
sudo useradd --system --no-create-home --shell /sbin/nologin acserver-rs

# 3. Config. StateDirectory= creates /var/lib/acserver-rs on start, but the
#    config must be in place first.
sudo mkdir -p /var/lib/acserver-rs/cfg
sudo cp cfg/server_cfg.ini cfg/entry_list.ini /var/lib/acserver-rs/cfg/
sudo chown -R acserver-rs:acserver-rs /var/lib/acserver-rs
sudo chmod 750 /var/lib/acserver-rs

# 4. Checksum content (skip to run unverified; see Notes)
sudo cp -r /path/to/assettocorsa/content /var/lib/acserver-rs/content
sudo chown -R acserver-rs:acserver-rs /var/lib/acserver-rs/content

# 5. Unit and optional overrides
sudo install -m 0644 deploy/acserver-rs.service /etc/systemd/system/
sudo install -m 0644 deploy/acserver-rs.sysconfig /etc/sysconfig/acserver-rs
sudo systemctl daemon-reload
sudo systemctl enable --now acserver-rs

# 6. Firewall (ports from cfg/server_cfg.ini)
sudo firewall-cmd --permanent --add-port=9600/tcp --add-port=9600/udp --add-port=8081/tcp
sudo firewall-cmd --reload
```

Check it with `systemctl status acserver-rs`, `journalctl -u acserver-rs -f` and
`curl localhost:8081/INFO`.

## Several servers

Use the template unit `acserver-rs@.service`. Instance `<i>` runs
`cfg/server_cfg_<i>.ini` with `cfg/entry_list_<i>.ini`, and all instances share
one flat `cfg/` directory. Each needs its own `TCP_PORT`, `UDP_PORT` and
`HTTP_PORT`.

```sh
sudo mkdir -p /var/lib/acserver-rs/cfg
for i in prac dtm; do
  sudo cp cfg/server_cfg.ini /var/lib/acserver-rs/cfg/server_cfg_$i.ini
  sudo cp cfg/entry_list.ini /var/lib/acserver-rs/cfg/entry_list_$i.ini
done
sudo cp cfg/extra_cfg.yml /var/lib/acserver-rs/cfg/        # shared
# Now give each server_cfg_<i>.ini distinct ports.
sudo chown -R acserver-rs:acserver-rs /var/lib/acserver-rs

sudo install -m 0644 deploy/acserver-rs@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now acserver-rs@prac acserver-rs@dtm

sudo firewall-cmd --permanent --add-port=9600/tcp --add-port=9600/udp --add-port=8081/tcp  # prac
sudo firewall-cmd --permanent --add-port=9601/tcp --add-port=9601/udp --add-port=8082/tcp  # dtm
sudo firewall-cmd --reload
```

Manage each instance separately: `systemctl status acserver-rs@dtm`,
`journalctl -u acserver-rs@prac -f`. To break the file-name convention, copy
`deploy/acserver-rs-instance.sysconfig` to `/etc/sysconfig/acserver-rs-<i>` and edit it.

## Notes

- **Writable config directory.** Results go to `<cfg dir>/results/`, so the
  `acserver-rs` user must be able to write there. `/var/lib/acserver-rs` is the one
  writable path under `ProtectSystem=strict`.
- **Checksums** are on by default, against `/var/lib/acserver-rs/content`. To run
  without them, set `ACSERVER_CONTENT_FLAG=` (empty) in `/etc/sysconfig/acserver-rs`.
- **Ban list.** At startup the server merges `blacklist.txt` from its working
  directory (`/var/lib/acserver-rs`, where vanilla keeps it) with
  `<cfg dir>/blacklist.txt`. `/ban_id`, and kicks under `BLACKLIST_MODE=2`,
  append to the cfg-dir copy. All instances in the template setup share both.
- **Results** are named after the server, so instances can share `results/`. To
  separate everything, give an instance its own directory via `ACSERVER_CFG`.
- **Ports below 1024** need the `CAP_NET_BIND_SERVICE` lines in the unit uncommented.
- **SELinux:** the unit expects its files under `/var/lib/acserver-rs`. Anything
  elsewhere may need relabelling.
- **Shutdown:** `SIGTERM` closes connections cleanly; `TimeoutStopSec=20s`
  covers the drain. Results are written at session changes only, so the
  running session's are not saved.
- Check the unit with `systemd-analyze verify /etc/systemd/system/acserver-rs.service`
  and review its hardening with `systemd-analyze security acserver-rs`.
