//! The server core: every subsystem and all shared state.
//!
//! `Server` is shared as `Arc<Server>` (the Rust form of Go's `*Server`)
//! between the connection tasks, the loops and the HTTP, plugin and RCON
//! front ends. Shared state sits behind short-lived `parking_lot::Mutex`es and
//! atomics, never held across an `.await`; the core logic is synchronous,
//! and only socket I/O and timers are async.

mod admin_commands;
mod booking;
mod broadcast;
mod checksum;
mod client;
mod commands;
mod connection;
mod csp;
mod drs;
mod entrycar;
mod firstupdate;
mod handshake;
mod live_config;
mod loops;
mod moderation;
mod plugins;
mod results;
mod scripts;
mod session;
mod setup;
mod tcp_handlers;
mod traffic;
mod udp;
mod usergroups;
mod vote;
mod weather;
mod weatherfx;

use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use client::{ChecksumStatus, Client, ClientIdentity};
pub use csp::{CspFeature, csp_features};
pub use entrycar::{CarResult, EntryCar, Identity, PositionUpdate};
pub use handshake::secret_equal;
pub use plugins::{PluginRequest, PluginResponse, public_ip};
pub use session::SessionManager;
pub use weather::{WeatherSnapshot, WeatherState};

use crate::config::{self, Config};
use crate::proto;
use checksum::ChecksumInfo;
use drs::DrsZone;
use results::ResultsRecorder;
use udp::{SourceLimiter, source_limiter};
use vote::Votes;

/// The ACSP surface the core calls into: the plugin manager, if one is
/// configured. Every hook does nothing by default.
pub trait PluginBridge: Send + Sync {
    fn on_client_connected(&self, _client: &Client) {}
    fn on_client_disconnected(&self, _client: &Client) {}
    fn on_client_first_update(&self, _client: &Client) {}
    fn on_chat(&self, _session_id: u8, _message: &str) {}
    fn on_collision_with_car(
        &self,
        _session_id: u8,
        _target_id: u8,
        _speed: f32,
        _world_pos: crate::wire::Vec3,
        _rel_pos: crate::wire::Vec3,
    ) {
    }
    fn on_collision_with_env(
        &self,
        _session_id: u8,
        _speed: f32,
        _world_pos: crate::wire::Vec3,
        _rel_pos: crate::wire::Vec3,
    ) {
    }
    fn on_lap_completed(&self, _payload: &[u8]) {}
    fn on_new_session(&self) {}
    fn on_end_session(&self, _results_path: &Path) {}
}

/// The plugin bridge when no plugin is configured.
struct NoopPlugin;

impl PluginBridge for NoopPlugin {}

/// The clock value past which the next session change restarts it at 0.
/// 0x57 and 0x3E carry it as an i32, which overflows after 24.8 days, and
/// 0xF9 as a u32; 7 days leaves a session 17 more to end.
const CLOCK_REBASE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How long vanilla's Step waits between 0x54s, in µs.
const SUN_INTERVAL_US: i64 = 60_000_000;

/// The default UDP receive buffer, sized to absorb a full grid's burst while
/// the receive loop is busy.
const UDP_READ_BUFFER_BYTES: usize = 8 << 20;

/// How often the UDP and relay threads check for shutdown while idle.
const THREAD_POLL: Duration = Duration::from_millis(200);

/// What the alloc lock guards besides making slot claims atomic.
#[derive(Debug, Default)]
struct AllocState {
    /// The entry list last applied.
    loaded_entries: Vec<config::EntryCar>,
}

/// Every subsystem and all shared state. Safe for concurrent use.
pub struct Server {
    pub cfg: Config,
    entry_cars: Vec<Arc<EntryCar>>,

    /// The 0x3E RandomSeed. Vanilla gives every client the same value
    /// (docs/02 0x3E), so it is fixed per run.
    random_seed: i32,
    /// Read once from the track's drs_zones.ini and sent as 0x53.
    drs_zones: Vec<DrsZone>,
    /// The encoded extra options appended to a CSP client's welcome
    /// message, empty when there are none (csp.rs).
    csp_extra: String,
    /// The CSPServerScripts entries and the plugins' scripts (scripts.rs).
    scripts: Vec<scripts::ServerScript>,
    /// Settings admins may change at runtime (live_config.rs).
    live: RwLock<live_config::Live>,
    /// The built-in AssettoServer plugins in EnablePlugins (plugins/).
    plugins: Vec<Arc<dyn plugins::Plugin>>,

    /// The game UDP socket: blocking, shared by the UDP and relay threads
    /// and every sender.
    udp_socket: OnceLock<Arc<std::net::UdpSocket>>,
    /// The async runtime, so the plain UDP and relay threads can start
    /// tasks (such as a client's first-update sequence).
    runtime: OnceLock<tokio::runtime::Handle>,
    /// A UDP endpoint to its client, without scanning every slot.
    udp_clients: RwLock<HashMap<SocketAddr, Arc<Client>>>,

    /// Pickup mode is off and a [BOOK] session is configured (booking.rs).
    booking: bool,
    /// Counts bookings and releases, for the lobby.
    booking_version: AtomicI64,
    /// Pickup mode is off, so slots can join and leave the car list
    /// (bookings, a reloaded entry list) and listings skip unlisted ones.
    dynamic: bool,

    /// When the server last had no loaded car (`None` while one is).
    idle_since: Mutex<Option<Instant>>,
    /// Vanilla's IdleMode.IsIdle (`step_idle`).
    idle: AtomicBool,

    /// Game UDP bytes sent since the last one-second sample. A plain counter,
    /// so `Relaxed` ordering is enough.
    udp_bytes: AtomicI64,
    /// That sample in kB/s (`throughput`).
    throughput_kbs: AtomicI64,

    /// Vanilla's bestLapRank: entry-car indices in leaderboard order, kept
    /// for the session and re-sorted only when vanilla sorts.
    rank: Mutex<Vec<usize>>,

    /// Uptime (µs) of the last 0x54, as vanilla's lastSunAngleUpdate; a
    /// session switch zeroes it (`step_sun`).
    sun_sent_us: AtomicI64,
    /// Uptime (µs) of the last 0x57 round (`step_race_start`).
    race_start_sent_us: AtomicI64,

    sessions: SessionManager,
    pub weather: WeatherState,
    /// The WeatherFX state with EnableWeatherFx, else `None` (weatherfx.rs).
    weatherfx: Option<weatherfx::WeatherFx>,
    /// Uptime (ms) of the last WeatherFX step.
    weatherfx_stepped_ms: AtomicI64,
    /// AI traffic with EnableAi, else `None` (traffic.rs).
    traffic: Option<traffic::Traffic>,
    /// Uptime (µs) the relay loop's next tick is due (`maybe_tick`).
    next_tick_us: AtomicI64,

    start_time: Instant,
    /// Subtracted from uptime to give the protocol clock, and moved by
    /// `rebase_clock_locked` so the clock never outgrows its i32 fields.
    clock_shift_ms: AtomicI64,
    /// Orders a rebase against code that pairs a clock reading with a stored
    /// clock offset or start time. Take it before the session lock.
    clock_lock: RwLock<()>,

    /// Bans until restart (BLACKLIST_MODE=1); the lasting ones are in the
    /// blacklist.txt files (usergroups.rs).
    session_bans: RwLock<HashSet<u64>>,
    /// The UserGroups files (usergroups.rs).
    user_groups: usergroups::UserGroups,

    /// One permit per accepted connection still awaiting a handshake.
    pending: Arc<tokio::sync::Semaphore>,
    /// Throttles 0xC8 replies per source IP.
    lobby_probes: SourceLimiter,
    /// Throttles 0xF9 replies per source IP.
    time_probes: SourceLimiter,

    checksum_cache: Mutex<HashMap<String, Arc<ChecksumInfo>>>,
    /// `checksum::shared_checksums`: the system and track entries.
    shared_checksums: OnceLock<Vec<(String, [u8; 16])>>,

    /// Makes finding and claiming a slot atomic (see `claim_slot`).
    alloc: Mutex<AllocState>,

    votes: Mutex<Votes>,
    recorder: ResultsRecorder,

    /// Set once, before `run`.
    plugin: OnceLock<Arc<dyn PluginBridge>>,
    /// Shared by the auth plugin and the lobby.
    pub http_client: reqwest::Client,

    /// Cancelled to shut everything down.
    pub shutdown: CancellationToken,
    /// Every task the server starts, so shutdown can wait for them.
    tasks: TaskTracker,
}

impl Server {
    /// Builds a server from parsed config.
    pub fn new(cfg: Config) -> anyhow::Result<Arc<Server>> {
        if cfg.entry_list.len() > proto::MAX_CAR_SLOTS {
            anyhow::bail!(
                "entry list has {} car slots (max {})",
                cfg.entry_list.len(),
                proto::MAX_CAR_SLOTS
            );
        }

        let mut entry_cars: Vec<Arc<EntryCar>> = Vec::with_capacity(cfg.entry_list.len());
        for (i, entry) in cfg.entry_list.iter().enumerate() {
            let car = EntryCar::new(i as u8, entry);
            let file = setup::setup_file(&entry.fixed_setup, entry.fixed_setup_named);
            let fixed = setup::load_fixed_setup(&cfg.base_dir, &file);
            let missing = fixed.is_empty();
            car.update(|s| s.setup = fixed);
            if entry.fixed_setup_named && missing {
                // Vanilla refuses to start when a named setup is missing (measured).
                anyhow::bail!(
                    "CAR_{i} FIXED_SETUP={:?}: setups/{file} not found",
                    entry.fixed_setup
                );
            }
            if entry.fixed_setup_set && missing {
                // Otherwise easy to miss: the slot silently gets no 0x52.
                tracing::warn!(car = i, setup = %entry.fixed_setup, "fixed setup not loaded");
            }
            entry_cars.push(Arc::new(car));
        }
        // With pickup mode off the car list can grow (bookings, a reloaded
        // entry list) up to MAX_CLIENTS, so the slots run that far; the ones
        // past the entry list start unlisted.
        let dynamic = !cfg.server.pickup_mode_enabled;
        if dynamic {
            for i in entry_cars.len()..usize::from(cfg.server.max_clients) {
                entry_cars.push(Arc::new(EntryCar::new_spare(i as u8)));
            }
        }

        match &cfg.content_dir {
            None => tracing::info!(
                reason = "no --content directory",
                "checksum verification disabled"
            ),
            Some(content) => {
                tracing::info!(content = %content.display(), "checksum verification enabled");
                // Vanilla always checks it first; without it a client may edit grip.
                if !content
                    .join("system")
                    .join("data")
                    .join("surfaces.ini")
                    .exists()
                {
                    tracing::warn!(content = %content.display(), "system/data/surfaces.ini not found under --content; it will not be checksummed");
                }
            }
        }

        let extra = &cfg.extra;
        let needs_csp = extra.enable_custom_update
            || extra.enable_weather_fx
            || !extra.csp_server_scripts.is_empty()
            || (extra.enable_ai && extra.ai_params.debug);
        if needs_csp && !extra.enable_csp {
            anyhow::bail!(
                "extra_cfg.yml: EnableCustomUpdate, EnableWeatherFx, CSPServerScripts and AiParams.Debug need EnableCSP"
            );
        }
        let plugins = plugins::load(&cfg)?;
        let mut scripts = scripts::load_scripts(&extra.csp_server_scripts, &cfg.base_dir)?;
        for plugin in &plugins {
            scripts.extend(plugin.scripts());
        }
        if extra.enable_live_configuration && extra.enable_csp {
            scripts.push(live_config::window_script());
        }
        if extra.enable_ai && extra.ai_params.debug {
            scripts.push(scripts::ServerScript::Local {
                name: "ai_debug.lua".to_string(),
                source: include_bytes!("ai_debug.lua").to_vec(),
            });
        }
        if !scripts.is_empty() {
            tracing::info!(count = scripts.len(), "csp server scripts loaded");
        }

        let mut csp_extra = String::new();
        let mut extra_ini = csp::extra_options_ini(extra, &scripts);
        for plugin in &plugins {
            extra_ini.push_str(&plugin.extra_options());
        }
        let extra_ini = extra_ini.trim();
        if extra.enable_csp && !extra_ini.is_empty() {
            let encoded = csp::encode_csp_extra_options(extra_ini).context("CSPExtraOptions")?;
            // The welcome message carries a u16 character count.
            let chars = cfg.server.welcome_message.chars().count() + encoded.chars().count();
            if chars > 0xFFFF {
                anyhow::bail!(
                    "CSPExtraOptions: welcome message plus options is {chars} characters (max 65535)"
                );
            }
            csp_extra = encoded;
        }

        let drs_zones = drs::load_drs_zones(
            cfg.content_dir.as_deref(),
            &cfg.server.track,
            &cfg.server.track_config,
        );
        if !drs_zones.is_empty() {
            tracing::info!(count = drs_zones.len(), "DRS zones loaded");
        }

        let weather = WeatherState::new(&cfg);
        let weatherfx = cfg.extra.enable_weather_fx.then(|| {
            let wx = weather.snapshot();
            weatherfx::WeatherFx::new(
                &cfg,
                wx.ambient,
                wx.road,
                wx.wind_speed,
                wx.wind_direction,
                &wx.graphics,
                wx.track_grip,
            )
        });

        let user_groups = usergroups::UserGroups::load(&cfg.extra, &cfg.base_dir)?;

        let traffic = if cfg.extra.enable_ai {
            Some(traffic::Traffic::new(&cfg).context("EnableAi")?)
        } else {
            None
        };

        let rank: Vec<usize> = (0..entry_cars.len()).collect();
        // So one host cannot exhaust file descriptors with idle sockets.
        let max_pending = (2 * entry_cars.len()).max(32);
        let server = Server {
            entry_cars,
            // Non-negative, as vanilla's.
            random_seed: rand::Rng::random_range(&mut rand::rng(), 0..=i32::MAX),
            drs_zones,
            csp_extra,
            scripts,
            live: RwLock::new(live_config::Live::from_config(&cfg)),
            plugins,
            udp_socket: OnceLock::new(),
            runtime: OnceLock::new(),
            udp_clients: RwLock::new(HashMap::new()),
            booking: cfg.booking(),
            booking_version: AtomicI64::new(0),
            dynamic,
            idle_since: Mutex::new(None),
            idle: AtomicBool::new(false),
            udp_bytes: AtomicI64::new(0),
            throughput_kbs: AtomicI64::new(0),
            rank: Mutex::new(rank),
            sun_sent_us: AtomicI64::new(0),
            race_start_sent_us: AtomicI64::new(0),
            sessions: SessionManager::new(cfg.sessions.clone()),
            weather,
            weatherfx,
            weatherfx_stepped_ms: AtomicI64::new(0),
            traffic,
            next_tick_us: AtomicI64::new(0),
            start_time: Instant::now(),
            clock_shift_ms: AtomicI64::new(0),
            clock_lock: RwLock::new(()),
            session_bans: RwLock::new(HashSet::new()),
            user_groups,
            pending: Arc::new(tokio::sync::Semaphore::new(max_pending)),
            lobby_probes: source_limiter(),
            time_probes: source_limiter(),
            checksum_cache: Mutex::new(HashMap::new()),
            shared_checksums: OnceLock::new(),
            alloc: Mutex::new(AllocState {
                loaded_entries: cfg.entry_list.clone(),
            }),
            votes: Mutex::default(),
            recorder: ResultsRecorder::default(),
            plugin: OnceLock::new(),
            http_client: reqwest::Client::new(),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
            cfg,
        };
        server.check_admin_group_safety()?;
        Ok(Arc::new(server))
    }

    /// Installs the plugin bridge. Call once, before `run`.
    pub fn set_plugin(&self, plugin: Arc<dyn PluginBridge>) {
        let _ = self.plugin.set(plugin);
    }

    /// The plugin bridge, or one that does nothing.
    fn plugin(&self) -> &dyn PluginBridge {
        match self.plugin.get() {
            Some(plugin) => plugin.as_ref(),
            None => &NoopPlugin,
        }
    }

    /// All slots.
    pub fn entry_cars(&self) -> &[Arc<EntryCar>] {
        &self.entry_cars
    }

    /// The listed car with a session id.
    pub fn car_by_id(&self, id: u8) -> Option<Arc<EntryCar>> {
        let car = self.entry_cars.get(usize::from(id))?;
        car.listed().then(|| Arc::clone(car))
    }

    /// The connected clients.
    pub fn connected_clients(&self) -> Vec<Arc<Client>> {
        self.entry_cars
            .iter()
            .filter_map(|car| car.client())
            .collect()
    }

    /// The number of occupied slots.
    pub fn connected_count(&self) -> usize {
        self.entry_cars.iter().filter(|car| car.occupied()).count()
    }

    /// The number of ready clients (vanilla GetConnectedCarsCount): the count
    /// /INFO, the lobby and the vote quorum use.
    pub fn ready_count(&self) -> usize {
        self.entry_cars
            .iter()
            .filter(|car| car.client().is_some_and(|c| c.ready()))
            .count()
    }

    /// The session manager.
    pub fn sessions(&self) -> &SessionManager {
        &self.sessions
    }

    /// Milliseconds since start.
    pub fn uptime_ms(&self) -> i64 {
        self.start_time.elapsed().as_millis() as i64
    }

    fn uptime_us(&self) -> i64 {
        self.start_time.elapsed().as_micros() as i64
    }

    /// The clock for every protocol timestamp: ms since the server started,
    /// or since the last rebase.
    pub fn server_time_millis(&self) -> i64 {
        self.uptime_ms() - self.clock_shift_ms.load(Ordering::SeqCst)
    }

    /// Restarts the protocol clock at 0 once it passes
    /// `CLOCK_REBASE_AFTER_MS`. Every stored clock offset moves with it, so
    /// relayed timestamps are unchanged. A pong already in flight echoes an
    /// old-clock time, so its round trip comes out negative and the ping
    /// average skips it. The caller holds the clock lock and is starting a
    /// session, whose 0x4A and 0x57 carry the new clock.
    fn rebase_clock_locked(&self) {
        let now = self.server_time_millis();
        if now < CLOCK_REBASE_AFTER_MS {
            return;
        }
        self.clock_shift_ms.fetch_add(now, Ordering::SeqCst);
        for car in &self.entry_cars {
            car.shift_relay_offset(-now);
            if let Some(client) = car.client() {
                client.shift_offsets(-now);
            }
        }
        tracing::info!(from_ms = now, "protocol clock rebased");
    }

    /// Recomputes the sun angle from the session's start and sends 0x54 once
    /// more than a minute of uptime has passed since the last one, as
    /// vanilla's Step does every pass (docs/02 0x54). The first goes out a
    /// minute after start, and a session switch zeroes the timer, so on a
    /// server up for over a minute the reset angle follows the switch at once.
    /// Microseconds, not vanilla's milliseconds: the session starts a fraction
    /// of a millisecond after the server here, so at its end the minute has
    /// strictly passed, as it has in vanilla, whose session starts later.
    fn step_sun(&self) {
        let seconds = self.elapsed_ms() as f64 / 1000.0;
        let angle = weather::sun_at(
            self.cfg.server.sun_angle,
            self.live().server.time_of_day_multiplier,
            seconds,
        );
        self.weather.set_sun_angle(angle);
        let now = self.uptime_us();
        let last = self.sun_sent_us.load(Ordering::SeqCst);
        if now - last <= SUN_INTERVAL_US {
            return;
        }
        if self
            .sun_sent_us
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.broadcast_sun_angle();
    }

    /// The game socket's send rate over the last second in kB/s, as /ENTRY
    /// shows it.
    pub fn throughput(&self) -> i64 {
        self.throughput_kbs.load(Ordering::Relaxed)
    }

    /// Whether idle mode has marked the server idle.
    pub fn idle(&self) -> bool {
        self.idle.load(Ordering::SeqCst)
    }

    /// Vanilla's idle mode ([IDLE_MODE] ENABLED, TIMEOUT in minutes): once no
    /// car has been loaded for TIMEOUT the server marks itself idle, which in
    /// v1.15 only logs and sets a flag nothing reads (docs/05 §5.2a). A
    /// loaded car restarts the count, but the flag is never cleared. Called
    /// every second from the ping-timeout loop only.
    fn step_idle(&self) {
        if !self.cfg.server.idle_mode_enabled || self.idle() {
            return;
        }
        let mut since = self.idle_since.lock();
        if self.ready_count() != 0 {
            *since = None;
            return;
        }
        let Some(start) = *since else {
            tracing::info!("server is idle, start time accumulation");
            *since = Some(Instant::now());
            return;
        };
        if start.elapsed() > self.cfg.server.idle_timeout {
            tracing::info!("server is now IDLE");
            self.idle.store(true, Ordering::SeqCst);
        }
    }

    /// Runs a task that shutdown waits for.
    fn spawn_task(&self, task: impl Future<Output = ()> + Send + 'static) {
        match self.runtime.get() {
            Some(runtime) => {
                self.tasks.spawn_on(task, runtime);
            }
            None => {
                self.tasks.spawn(task);
            }
        }
    }

    /// Calls `tick` every `interval` until shutdown.
    fn spawn_ticker(&self, interval: Duration, tick: impl Fn() + Send + 'static) {
        let shutdown = self.shutdown.clone();
        self.spawn_task(async move {
            let mut ticker =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            while shutdown.run_until_cancelled(ticker.tick()).await.is_some() {
                tick();
            }
        });
    }

    /// Calls `action` once after `delay`, unless the server shuts down first.
    fn spawn_after(&self, delay: Duration, action: impl FnOnce() + Send + 'static) {
        let shutdown = self.shutdown.clone();
        self.spawn_task(async move {
            if shutdown
                .run_until_cancelled(tokio::time::sleep(delay))
                .await
                .is_some()
            {
                action();
            }
        });
    }

    /// Starts all listeners and loops, and runs until `shutdown` is
    /// cancelled.
    pub async fn run(self: &Arc<Self>) -> anyhow::Result<()> {
        let tcp = tokio::net::TcpListener::bind(("0.0.0.0", self.cfg.server.tcp_port))
            .await
            .context("tcp")?;
        tracing::debug!(addr = ?tcp.local_addr(), "tcp listening");
        let udp = self.bind_udp().context("udp")?;
        let _ = self.udp_socket.set(Arc::clone(&udp));
        let _ = self.runtime.set(tokio::runtime::Handle::current());

        self.spawn_task(Arc::clone(self).accept_loop(tcp));

        self.start_sessions();

        // The two hot loops run on their own plain threads rather than as
        // tasks: each then wakes only for its own work, instead of Tokio
        // moving the busiest traffic between worker threads (measured at
        // tens of thousands of extra context switches a second).
        let server = Arc::clone(self);
        let udp_thread = std::thread::Builder::new()
            .name("udp-receive".to_string())
            .spawn(move || server.udp_receive_loop(&udp))
            .context("start the UDP thread")?;
        let server = Arc::clone(self);
        let relay_thread = std::thread::Builder::new()
            .name("relay".to_string())
            .spawn(move || server.relay_loop())
            .context("start the relay thread")?;

        let server = Arc::clone(self);
        self.spawn_ticker(Duration::from_secs(1), move || server.ping_timeouts());
        let server = Arc::clone(self);
        self.spawn_ticker(Duration::from_secs(1), move || server.expire_stale_vote());
        self.start_traffic();
        self.start_user_groups();
        self.start_plugins();
        if self.weatherfx.is_some() {
            self.weatherfx_stepped_ms
                .store(self.uptime_ms(), Ordering::SeqCst);
            let server = Arc::clone(self);
            self.spawn_ticker(
                Duration::from_millis(weatherfx::WEATHERFX_INTERVAL_MS as u64),
                move || server.step_weatherfx(),
            );
        }

        tracing::info!(
            name = %self.cfg.server.name,
            tcp = self.cfg.server.tcp_port,
            udp = self.cfg.server.udp_port,
            http = self.cfg.server.http_port,
            slots = self.entry_cars.len(),
            client_hz = self.cfg.server.refresh_rate_hz,
            "server running"
        );

        self.shutdown.cancelled().await;
        tracing::info!("shutting down: waiting for connections and loops to drain");
        self.tasks.close();
        self.tasks.wait().await;
        // Both threads check for shutdown at least every `THREAD_POLL`.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = udp_thread.join();
            let _ = relay_thread.join();
        })
        .await;
        tracing::info!("shutdown complete");
        Ok(())
    }

    /// Opens the game UDP socket. RECV_BUFFER_SIZE and SEND_BUFFER_SIZE apply
    /// when > 0, as in vanilla. Our default receive buffer is larger; the
    /// kernel clamps either request, so it is harmless.
    fn bind_udp(&self) -> anyhow::Result<Arc<std::net::UdpSocket>> {
        let std_socket = std::net::UdpSocket::bind(("0.0.0.0", self.cfg.server.udp_port))?;
        // Blocking, with a timeout so the receive thread notices shutdown.
        std_socket.set_read_timeout(Some(THREAD_POLL))?;
        let sock = socket2::SockRef::from(&std_socket);
        let read_buffer = match self.cfg.server.recv_buffer_size {
            size if size > 0 => size as usize,
            _ => UDP_READ_BUFFER_BYTES,
        };
        if let Err(err) = sock.set_recv_buffer_size(read_buffer) {
            // Not fatal, but a full-grid burst may now drop datagrams.
            tracing::warn!(%err, "udp set read buffer failed");
        }
        if self.cfg.server.send_buffer_size > 0
            && let Err(err) = sock.set_send_buffer_size(self.cfg.server.send_buffer_size as usize)
        {
            tracing::warn!(%err, "udp set write buffer failed");
        }
        tracing::debug!(addr = ?std_socket.local_addr(), "udp listening");
        Ok(Arc::new(std_socket))
    }

    /// Receives datagrams until shutdown, on its own thread.
    fn udp_receive_loop(self: Arc<Self>, socket: &std::net::UdpSocket) {
        let mut buf = vec![0u8; 1500];
        while !self.shutdown.is_cancelled() {
            match socket.recv_from(&mut buf) {
                Ok((0, _)) => {}
                Ok((n, addr)) => self.handle_udp(&buf[..n], addr),
                // The read timeout: go round and check for shutdown.
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => tracing::debug!(%err, "udp read failed"),
            }
        }
    }
}

/// Whole milliseconds from `from` to `to`, negative when `to` is earlier.
pub(crate) fn signed_millis(from: Instant, to: Instant) -> i64 {
    match to.checked_duration_since(from) {
        Some(d) => d.as_millis() as i64,
        None => -(from.duration_since(to).as_millis() as i64),
    }
}
