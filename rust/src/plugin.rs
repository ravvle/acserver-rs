//! The ACSP v4 UDP plugin API (docs/06), as a `server::PluginBridge`.

use parking_lot::Mutex;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use crate::proto::{self, SessionType};
use crate::server::{Client, PluginBridge, Server};
use crate::wire::{Reader, Vec3, Writer};

/// The ACSP protocol version.
const ACSP_VERSION: u8 = 4;

// ACSP packet ids.
const AC_NEW_SESSION: u8 = 0x32;
const AC_NEW_CONNECTION: u8 = 0x33;
const AC_CONNECTION_CLOSED: u8 = 0x34;
const AC_CAR_UPDATE: u8 = 0x35;
const AC_CAR_INFO: u8 = 0x36;
const AC_END_SESSION: u8 = 0x37;
const AC_VERSION: u8 = 0x38;
const AC_CHAT: u8 = 0x39;
const AC_CLIENT_FIRST_UPDATE: u8 = 0x3A;
const AC_SESSION_INFO: u8 = 0x3B;
const AC_ERROR: u8 = 0x3C;
const AC_CLIENT_EVENT: u8 = 0x82;

const AC_SET_REALTIME_INTERVAL: u8 = 0xC8;
const AC_GET_CAR_INFO: u8 = 0xC9;
const AC_SEND_CHAT: u8 = 0xCA;
const AC_BROADCAST_CHAT: u8 = 0xCB;
const AC_GET_SESSION_INFO: u8 = 0xCC;
const AC_SET_SESSION_INFO: u8 = 0xCD;
const AC_KICK_USER: u8 = 0xCE;
const AC_NEXT_SESSION: u8 = 0xCF;
const AC_RESTART_SESSION: u8 = 0xD0;
const AC_ADMIN_COMMAND: u8 = 0xD1;

/// Runs the plugin socket and pushes events to the configured plugin.
pub struct PluginManager {
    server: Arc<Server>,
    socket: Arc<UdpSocket>,
    /// Where events go.
    plugin_to: SocketAddr,
    /// The 0xC8 CarUpdate interval. Vanilla sends no 0x35 until a plugin sets
    /// one, and none while it is 0 (measured).
    realtime_interval: Mutex<Duration>,
    realtime_changed: Notify,
    /// 0x38 goes out before the first 0x32, exactly once.
    hello_sent: AtomicBool,
    /// Set while writes fail, so an outage warns once.
    send_down: AtomicBool,
}

impl PluginManager {
    /// Builds the manager, or `None` when UDP_PLUGIN_ADDRESS is unset.
    pub async fn new(server: &Arc<Server>) -> anyhow::Result<Option<Arc<PluginManager>>> {
        let cfg = &server.cfg.server;
        if cfg.udp_plugin_local_port == 0 || cfg.udp_plugin_address.is_empty() {
            return Ok(None);
        }
        let plugin_to = cfg
            .udp_plugin_address
            .to_socket_addrs()
            .context("UDP_PLUGIN_ADDRESS")?
            .next()
            .context("UDP_PLUGIN_ADDRESS: no address")?;
        if plugin_to.port() == cfg.udp_plugin_local_port {
            anyhow::bail!("UDP_PLUGIN_ADDRESS port must differ from UDP_PLUGIN_LOCAL_PORT");
        }
        let socket = UdpSocket::bind(("0.0.0.0", cfg.udp_plugin_local_port)).await?;
        Ok(Some(Arc::new(PluginManager {
            server: Arc::clone(server),
            socket: Arc::new(socket),
            plugin_to,
            realtime_interval: Mutex::new(Duration::ZERO),
            realtime_changed: Notify::new(),
            hello_sent: AtomicBool::new(false),
            send_down: AtomicBool::new(false),
        })))
    }

    /// Runs the receive loop and the realtime CarUpdate pump until shutdown.
    /// The startup 0x38 and 0x32 go out with the server's first session
    /// (`on_new_session`), so they are sent once and in order.
    pub async fn run(self: Arc<Self>) {
        tracing::info!(listen = ?self.socket.local_addr(), send = %self.plugin_to, "plugin loop started");
        tokio::spawn(Arc::clone(&self).realtime_loop());

        let shutdown = self.server.shutdown.clone();
        let mut buf = vec![0u8; 1500];
        loop {
            let Some(received) = shutdown
                .run_until_cancelled(self.socket.recv_from(&mut buf))
                .await
            else {
                return;
            };
            let (n, from) = match received {
                Ok(received) => received,
                Err(err) => {
                    tracing::debug!(%err, "plugin read failed");
                    continue;
                }
            };
            // Only accept datagrams from the configured plugin IP.
            let expected = self.plugin_to.ip().to_canonical();
            if !expected.is_unspecified() && from.ip().to_canonical() != expected {
                tracing::debug!(%from, %expected, "plugin datagram dropped: unexpected source");
                continue;
            }
            if n > 0 {
                self.handle(&buf[..n]);
            }
        }
    }

    fn send(&self, w: &Writer) {
        self.send_bytes(w.bytes());
    }

    fn send_bytes(&self, bytes: &[u8]) {
        match self.socket.try_send_to(bytes, self.plugin_to) {
            Ok(_) => {
                if self.send_down.swap(false, Ordering::SeqCst) {
                    tracing::info!(to = %self.plugin_to, "plugin reachable again");
                }
            }
            Err(err) => {
                if !self.send_down.swap(true, Ordering::SeqCst) {
                    tracing::warn!(to = %self.plugin_to, %err, "plugin write failed; events are being lost");
                } else {
                    tracing::debug!(to = %self.plugin_to, %err, "plugin write failed");
                }
            }
        }
    }

    async fn realtime_loop(self: Arc<Self>) {
        let shutdown = self.server.shutdown.clone();
        loop {
            let interval = *self.realtime_interval.lock();
            // A zero interval waits only for a change.
            let tick = async {
                if interval.is_zero() {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(interval).await;
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = self.realtime_changed.notified() => {}
                _ = tick => self.push_car_updates(),
            }
        }
    }

    /// Applies a 0xC8 and restarts the CarUpdate timer.
    fn set_realtime_interval(&self, interval: Duration) {
        *self.realtime_interval.lock() = interval;
        self.realtime_changed.notify_one();
    }

    /// Sends a 0x35 for every loaded car, as vanilla's udpStepPlugin does: one
    /// ready only by a 0x4C has no position yet and goes out zeroed.
    fn push_car_updates(&self) {
        for car in self.server.entry_cars() {
            if !car.client().is_some_and(|c| c.ready()) {
                continue;
            }
            let pos = car.last_position().unwrap_or_default();
            let mut w = Writer::with_id(AC_CAR_UPDATE);
            w.u8(car.session_id);
            w.vec3(pos.position);
            w.vec3(pos.velocity);
            w.u8(pos.gear);
            w.u16(pos.engine_rpm);
            w.f32(pos.normalized_pos);
            self.send(&w);
        }
    }

    /// Pushes 0x38.
    fn send_version(&self) {
        let mut w = Writer::with_id(AC_VERSION);
        w.u8(ACSP_VERSION);
        self.send(&w);
    }

    /// Builds a 0x32 or 0x3B body for a session index.
    fn build_session_info(&self, id: u8, session_index: i64) -> Writer {
        let cfg = &self.server.cfg.server;
        let sessions = self.server.sessions().all();
        let (_, current) = self.server.sessions().current();
        let wx = self.server.weather.snapshot();

        let index = if session_index < 0 || session_index >= sessions.len() as i64 {
            current
        } else {
            session_index as usize
        };
        let session = &sessions[index];

        let mut w = Writer::with_id(id);
        w.u8(ACSP_VERSION);
        w.u8(index as u8);
        w.u8(current as u8);
        w.u8(sessions.len() as u8);
        w.wstring8(&cfg.name);
        w.string8(&cfg.track);
        w.string8(&cfg.track_config);
        w.string8(&session.name);
        w.u8(session.kind.as_u8());
        w.u16(session.time);
        w.u16(session.laps);
        w.u16(session.wait_time);
        w.temperature(wx.ambient);
        w.temperature(wx.road);
        w.string8(&wx.graphics); // string8, not wstring8 (measured)
        w.i32(self.server.elapsed_ms() as i32);
        w
    }

    fn send_connection(&self, id: u8, client: &Client) {
        let ident = client.identity();
        let car = &self.server.entry_cars()[usize::from(ident.session_id)];
        let mut w = Writer::with_id(id);
        w.wstring8(&ident.name);
        w.wstring8(&ident.guid);
        w.u8(ident.session_id);
        w.string8(&car.model());
        w.string8(&car.state().skin);
        self.send(&w);
    }

    fn collision(
        &self,
        kind: u8,
        session_id: u8,
        target: Option<u8>,
        speed: f32,
        world: Vec3,
        rel: Vec3,
    ) {
        let mut w = Writer::with_id(AC_CLIENT_EVENT);
        w.u8(kind);
        w.u8(session_id);
        if let Some(target) = target {
            w.u8(target);
        }
        w.f32(speed);
        w.vec3(world);
        w.vec3(rel);
        self.send(&w);
    }

    /// Dispatches an inbound plugin request.
    fn handle(&self, packet: &[u8]) {
        let (&id, body) = packet
            .split_first()
            .expect("caller passes a non-empty packet");
        let mut r = Reader::new(body);
        match id {
            AC_VERSION => self.send_version(),
            AC_SET_REALTIME_INTERVAL => {
                let ms = r.u16();
                self.set_realtime_interval(Duration::from_millis(u64::from(ms)));
                tracing::debug!(ms, "plugin realtime interval set");
            }
            AC_GET_CAR_INFO => self.send_car_info(r.u8()),
            AC_SEND_CHAT => {
                let session_id = r.u8();
                let msg = r.wstring8();
                if let Some(client) = self
                    .server
                    .car_by_id(session_id)
                    .and_then(|car| car.client())
                {
                    self.server.send_chat_to(&client, &msg);
                }
            }
            AC_BROADCAST_CHAT => self.server.broadcast_server_chat(&r.wstring8()),
            AC_GET_SESSION_INFO => self.send_session_info(i64::from(r.i16())),
            AC_SET_SESSION_INFO => self.handle_set_session_info(&mut r),
            AC_KICK_USER => {
                let session_id = r.u8();
                if self.server.car_by_id(session_id).is_none() {
                    self.send_error(&format!(
                        "ERROR: ACSP_KICK_USER index {session_id} out of bound"
                    ));
                } else {
                    self.server.kick_car("plugin", session_id);
                }
            }
            AC_NEXT_SESSION => {
                self.server.next_session();
            }
            AC_RESTART_SESSION => {
                self.server.restart_session();
            }
            AC_ADMIN_COMMAND => {
                // Vanilla runs these; AssettoServer ignores them.
                self.server.execute_admin_command("plugin", &r.wstring8());
            }
            _ => tracing::debug!(id, "unhandled plugin packet"),
        }
    }

    fn send_car_info(&self, session_id: u8) {
        let mut w = Writer::with_id(AC_CAR_INFO);
        w.u8(session_id);
        let Some(car) = self.server.car_by_id(session_id) else {
            w.bool(false);
            for _ in 0..5 {
                w.wstring8("");
            }
            self.send(&w);
            return;
        };
        w.bool(car.occupied());
        w.wstring8(&car.model());
        w.wstring8(&car.state().skin);
        let id = car.identity();
        w.wstring8(&id.name);
        w.wstring8(&id.team);
        w.wstring8(&id.guid);
        self.send(&w);
    }

    fn send_session_info(&self, index: i64) {
        let count = self.server.sessions().count() as i64;
        if index != -1 && (index < 0 || index >= count) {
            self.send_error(&format!(
                "ERROR: ACSP_GET_SESSION_INFO index {index} out of bound"
            ));
            return;
        }
        self.send(&self.build_session_info(AC_SESSION_INFO, index));
    }

    fn handle_set_session_info(&self, r: &mut Reader) {
        let index = r.u8();
        let name = r.wstring8();
        let kind = SessionType::from_u8(r.u8());
        let laps = r.i32();
        let time_ms = r.i32();
        let wait_ms = r.u32();
        // Out-of-range values saturate.
        let fit = |n: i64| u16::try_from(n.max(0)).unwrap_or(u16::MAX);

        // Time on the wire is milliseconds; sessions store minutes.
        let updated = self.server.sessions().set_session_info(
            usize::from(index),
            &name,
            kind,
            fit(laps.into()),
            fit((time_ms / 60000).into()),
            fit((wait_ms / 1000).into()),
        );
        if updated {
            self.send(&self.build_session_info(AC_SESSION_INFO, i64::from(index)));
        } else {
            self.send_error(&format!(
                "ERROR: ACSP_SET_SESSION_INFO out of bounds: {index}"
            ));
        }
    }

    fn send_error(&self, msg: &str) {
        let mut w = Writer::with_id(AC_ERROR);
        w.wstring8(msg);
        self.send(&w);
    }
}

impl PluginBridge for PluginManager {
    /// Pushes 0x32, preceded by 0x38 the first time (docs/06 §6.1).
    fn on_new_session(&self) {
        if !self.hello_sent.swap(true, Ordering::SeqCst) {
            self.send_version();
        }
        let (_, index) = self.server.sessions().current();
        self.send(&self.build_session_info(AC_NEW_SESSION, index as i64));
    }

    /// Pushes 0x37 with the results file's path, relative to the working
    /// directory when the config dir is (measured: "results/<name>.json").
    fn on_end_session(&self, results_path: &Path) {
        let mut w = Writer::with_id(AC_END_SESSION);
        w.wstring8(&results_path.to_string_lossy());
        self.send(&w);
    }

    fn on_client_connected(&self, client: &Client) {
        self.send_connection(AC_NEW_CONNECTION, client);
    }

    fn on_client_disconnected(&self, client: &Client) {
        self.send_connection(AC_CONNECTION_CLOSED, client);
    }

    fn on_client_first_update(&self, client: &Client) {
        let mut w = Writer::with_id(AC_CLIENT_FIRST_UPDATE);
        w.u8(client.session_id());
        self.send(&w);
    }

    fn on_chat(&self, session_id: u8, message: &str) {
        let mut w = Writer::with_id(AC_CHAT);
        w.u8(session_id);
        w.wstring8(message);
        self.send(&w);
    }

    fn on_collision_with_car(
        &self,
        session_id: u8,
        target_id: u8,
        speed: f32,
        world_pos: Vec3,
        rel_pos: Vec3,
    ) {
        self.collision(
            proto::EVENT_COLLISION_WITH_CAR,
            session_id,
            Some(target_id),
            speed,
            world_pos,
            rel_pos,
        );
    }

    fn on_collision_with_env(&self, session_id: u8, speed: f32, world_pos: Vec3, rel_pos: Vec3) {
        self.collision(
            proto::EVENT_COLLISION_WITH_ENV,
            session_id,
            None,
            speed,
            world_pos,
            rel_pos,
        );
    }

    /// Relays the game-protocol leaderboard packet verbatim (0x49).
    fn on_lap_completed(&self, payload: &[u8]) {
        let _ = self.socket.try_send_to(payload, self.plugin_to);
    }
}
