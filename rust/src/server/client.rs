//! Per-connection state: identity, the TCP send queue, UDP endpoint, ping
//! and clock offset.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, Ordering};
use std::time::Instant;

use tokio::sync::mpsc;

use super::loops::MegaEntry;
use crate::proto;
use crate::wire;

/// One framed TCP packet in a client's send queue. A broadcast frames once
/// and shares the bytes across every queue.
pub type Frame = bytes::Bytes;

/// A client's file-checksum verification state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumStatus {
    Pending,
    Succeeded,
    Failed,
}

/// Who the client is, fixed at the handshake.
#[derive(Debug, Clone, Default)]
pub struct ClientIdentity {
    pub session_id: u8,
    pub guid: String,
    pub name: String,
    pub team: wire::Narrow,
    pub nation: wire::Narrow,
    pub car_model: String,
}

impl ClientIdentity {
    /// The guid as a Steam ID; 0 before the handshake, which admits only
    /// valid ones.
    pub fn steam_id(&self) -> u64 {
        self.guid.parse().unwrap_or(0)
    }
}

/// Relayed CSP client messages a client may send a second, per transport,
/// and in one burst.
const CSP_MESSAGE_BURST: u32 = 30;

/// Holds four full 255-car leaderboard rounds per client.
pub const SEND_QUEUE_DEPTH: usize = 1024;

/// Vanilla's ping-sample ring size (UpdatePingAvg).
const PING_RING_SIZE: usize = 50;

/// Covers every u8 session id.
pub const MAX_SESSION_SLOTS: usize = 256;

/// Why `send_tcp` could not queue a frame.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// The client fell `SEND_QUEUE_DEPTH` frames behind.
    QueueFull,
    /// The client is disconnecting.
    Closed,
}

/// BANDWIDTH_OPTIMIZER state, indexed by sender id, AI cars included
/// (docs/03 0x46): updates to skip between sends (`ranked_priority`), and
/// the running counter.
#[derive(Debug)]
pub struct Decimation {
    pub priority: [u8; MAX_SESSION_SLOTS],
    pub skip: [u8; MAX_SESSION_SLOTS],
}

/// A rate limit on one stream of events: up to `burst` at once, refilled
/// at `per_second`.
pub type Budget = governor::DefaultDirectRateLimiter;

pub fn budget(per_second: u32, burst: u32) -> Budget {
    let nonzero = |n: u32| NonZeroU32::new(n.max(1)).expect("at least 1");
    governor::RateLimiter::direct(
        governor::Quota::per_second(nonzero(per_second)).allow_burst(nonzero(burst)),
    )
}

/// What a client's CSP session has told the server (csp.rs).
#[derive(Debug)]
pub struct CspState {
    /// The CSP build from the handshake Features, 0 for a client without CSP.
    pub version: AtomicU32,
    /// The car the client watches plus one, 0 for none.
    pub spectating: AtomicI32,
    /// Set by the client's HandshakeOut (docs/04 §4.3).
    pub hello: AtomicBool,
    /// The client announced CUSTOM_UPDATE and the server has it on, so its
    /// positions go out as `0xAB 0x03` batches (docs/04 §4.2).
    pub custom_update: AtomicBool,
    /// Meter relayed messages per transport.
    pub tcp_budget: Budget,
    pub udp_budget: Budget,
}

impl CspState {
    /// Spends one relayed-message token from the TCP or UDP bucket.
    pub fn allow(&self, udp: bool) -> bool {
        let budget = if udp {
            &self.udp_budget
        } else {
            &self.tcp_budget
        };
        budget.check().is_ok()
    }
}

/// One connected player: a TCP control channel plus an associated UDP
/// endpoint. Shared as `Arc<Client>` between its connection tasks, its slot
/// and the server loops.
#[derive(Debug)]
pub struct Client {
    /// The TCP peer.
    pub peer: SocketAddr,
    identity: OnceLock<ClientIdentity>,

    /// Set by /admin from the client's own task while HTTP reads it for /JSON.
    is_admin: AtomicBool,

    /// Outbound TCP, drained by the writer task. `disconnect` takes the
    /// sender away; the writer then flushes what is queued and closes.
    sender: Mutex<Option<mpsc::Sender<Frame>>>,

    udp_addr: Mutex<Option<SocketAddr>>,
    /// Throttles the "unexpected source" warning: uptime (ms) of the last.
    pub(crate) udp_source_warned_ms: AtomicI64,

    checksum: Mutex<ChecksumStatus>,

    /// Set once 0x3E is queued. Gates only 0x4A session broadcasts, which
    /// vanilla sends to every connection.
    pub(crate) handshaked: AtomicBool,
    /// Vanilla's `Conn.IsHandshakeDone`: set by the first UDP 0x46 or by a
    /// 0x4C. Gates every other broadcast, the position relay, pings, the vote
    /// quorum and the reported client count (docs/01 §1.5).
    pub(crate) ready: AtomicBool,
    pub(crate) has_sent_first_update: AtomicBool,
    pub(crate) first_update_pending: AtomicBool,
    /// The handshake time, for the load and session durations in the log.
    joined_at: OnceLock<Instant>,

    /// The last `PING_RING_SIZE` round trips (vanilla UpdatePingAvg).
    ping_samples: Mutex<VecDeque<i32>>,
    last_ping: AtomicU32,
    /// The live clock offset, slewed toward `target_offset` per 0x46.
    time_offset: AtomicI64,
    /// The latest pong's estimate.
    target_offset: AtomicI64,
    /// Server uptime (ms) of the last pong, 0 before the first.
    last_pong_ms: AtomicI64,
    /// Uptime (µs) of the last 0xF9, as vanilla's LastPingTime.
    pub(crate) ping_sent_us: AtomicI64,

    /// BANDWIDTH_OPTIMIZER tiers and counters. Only the relay thread
    /// touches them: the tick ranks, each relay or AI send advances.
    pub(crate) decimation: Mutex<Decimation>,
    /// Cars outside this client's network bubble, held for the next tick's
    /// 0x48 (`BatchFarPositions`). Only the relay thread touches it.
    pub(crate) far_positions: Mutex<Vec<MegaEntry>>,
    /// The 0x46 relay budget: half a second at the server's rate limit.
    /// Vanilla relays without limit, so one client could flood every other
    /// player; the budget sits far above any real client's send rate.
    pub(crate) position_budget: Budget,

    /// Suppresses the 0x4D: vanilla sends only the 0x68 for a kick.
    pub(crate) kicked: AtomicBool,
    pub(crate) disconnected: AtomicBool,

    /// Uptime (ms) of the last chat line, for ChatRateLimitMs.
    pub(crate) last_chat_ms: AtomicI64,
    /// Custom Shaders Patch state.
    pub csp: CspState,
    /// Wrong /admin passwords tried on this connection.
    pub(crate) admin_failures: AtomicU32,
    /// Uptime (ms) of the last input change or chat (EnableAntiAfk).
    pub(crate) last_active_ms: AtomicI64,
    /// The one-minute AFK warning has gone out.
    pub(crate) afk_warned: AtomicBool,
    /// Pongs in a row above MaxPing.
    pub(crate) high_ping_seconds: AtomicU32,
    /// 32 random hex digits a server script asks for with AS_ApiKey
    /// (scripts.rs), to authenticate its HTTP calls.
    api_key: String,

    /// When the TCP read times out; `None` for never. The handshake, then
    /// the loading timeout, then nothing once the client is in the world.
    read_deadline: Mutex<Option<Instant>>,
}

impl Client {
    /// A new connection whose frames go to `sender`, allowed
    /// `position_rate` 0x46 updates a second.
    pub fn new(peer: SocketAddr, sender: mpsc::Sender<Frame>, position_rate: u32) -> Client {
        Client {
            peer,
            identity: OnceLock::new(),
            is_admin: AtomicBool::new(false),
            sender: Mutex::new(Some(sender)),
            udp_addr: Mutex::new(None),
            udp_source_warned_ms: AtomicI64::new(i64::MIN),
            checksum: Mutex::new(ChecksumStatus::Pending),
            handshaked: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            has_sent_first_update: AtomicBool::new(false),
            first_update_pending: AtomicBool::new(false),
            joined_at: OnceLock::new(),
            ping_samples: Mutex::new(VecDeque::with_capacity(PING_RING_SIZE)),
            last_ping: AtomicU32::new(0),
            time_offset: AtomicI64::new(0),
            target_offset: AtomicI64::new(0),
            last_pong_ms: AtomicI64::new(0),
            ping_sent_us: AtomicI64::new(0),
            decimation: Mutex::new(Decimation {
                priority: [0; MAX_SESSION_SLOTS],
                skip: [0; MAX_SESSION_SLOTS],
            }),
            far_positions: Mutex::new(Vec::new()),
            position_budget: budget(position_rate, position_rate / 2),
            kicked: AtomicBool::new(false),
            disconnected: AtomicBool::new(false),
            // Never: a first chat is never rate-limited.
            last_chat_ms: AtomicI64::new(i64::MIN),
            csp: CspState {
                version: AtomicU32::new(0),
                spectating: AtomicI32::new(0),
                hello: AtomicBool::new(false),
                custom_update: AtomicBool::new(false),
                tcp_budget: budget(CSP_MESSAGE_BURST, CSP_MESSAGE_BURST),
                udp_budget: budget(CSP_MESSAGE_BURST, CSP_MESSAGE_BURST),
            },
            admin_failures: AtomicU32::new(0),
            last_active_ms: AtomicI64::new(0),
            afk_warned: AtomicBool::new(false),
            high_ping_seconds: AtomicU32::new(0),
            api_key: format!("{:032x}", rand::random::<u128>()),
            read_deadline: Mutex::new(None),
        }
    }

    /// The key a server script authenticates with (scripts.rs).
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Fixes the client's identity at the handshake. Called once, before the
    /// client is published in a slot.
    pub fn set_identity(&self, identity: ClientIdentity) {
        let _ = self.identity.set(identity);
        let _ = self.joined_at.set(Instant::now());
    }

    /// The identity from the handshake; empty (session id 0xFF) before it.
    pub fn identity(&self) -> &ClientIdentity {
        static NONE: OnceLock<ClientIdentity> = OnceLock::new();
        match self.identity.get() {
            Some(identity) => identity,
            None => NONE.get_or_init(|| ClientIdentity {
                session_id: proto::SERVER_SESSION_ID,
                ..ClientIdentity::default()
            }),
        }
    }

    /// The client's slot, or 0xFF before the handshake assigns one.
    pub fn session_id(&self) -> u8 {
        self.identity().session_id
    }

    pub fn name(&self) -> &str {
        &self.identity().name
    }

    pub fn is_admin(&self) -> bool {
        self.is_admin.load(Ordering::SeqCst)
    }

    pub fn set_admin(&self, admin: bool) {
        self.is_admin.store(admin, Ordering::SeqCst);
    }

    pub fn checksum_status(&self) -> ChecksumStatus {
        *self.checksum.lock()
    }

    pub fn set_checksum_status(&self, status: ChecksumStatus) {
        *self.checksum.lock() = status;
    }

    /// The associated UDP endpoint, `None` until the 0x4E association.
    pub fn udp_addr(&self) -> Option<SocketAddr> {
        *self.udp_addr.lock()
    }

    pub fn set_udp_addr(&self, addr: Option<SocketAddr>) {
        *self.udp_addr.lock() = addr;
    }

    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn is_csp(&self) -> bool {
        self.csp_version() > 0
    }

    pub fn csp_version(&self) -> u32 {
        self.csp.version.load(Ordering::SeqCst)
    }

    /// Seconds since the handshake, rounded, for log lines.
    pub fn seconds_since_join(&self) -> u64 {
        match self.joined_at.get() {
            Some(at) => at.elapsed().as_secs_f64().round() as u64,
            None => 0,
        }
    }

    pub fn read_deadline(&self) -> Option<Instant> {
        *self.read_deadline.lock()
    }

    pub fn set_read_deadline(&self, deadline: Option<Instant>) {
        *self.read_deadline.lock() = deadline;
    }

    /// Frames a payload and queues it. The caller handles `QueueFull` by
    /// dropping the client (`Server::send_tcp`).
    pub fn queue_payload(&self, payload: &[u8]) -> Result<(), SendError> {
        self.queue_frame(wire::frame_tcp(payload))
    }

    /// Queues an already-framed packet.
    pub fn queue_frame(&self, frame: Frame) -> Result<(), SendError> {
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }
        let guard = self.sender.lock();
        let Some(sender) = guard.as_ref() else {
            return Err(SendError::Closed);
        };
        match sender.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SendError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SendError::Closed),
        }
    }

    /// Takes the sender away, so the writer flushes what is queued and closes.
    pub fn close_queue(&self) {
        self.sender.lock().take();
    }

    pub fn ping(&self) -> u16 {
        self.last_ping.load(Ordering::SeqCst) as u16
    }

    pub fn time_offset(&self) -> i64 {
        self.time_offset.load(Ordering::SeqCst)
    }

    /// Moves both offsets by `delta` (a clock rebase).
    pub fn shift_offsets(&self, delta: i64) {
        self.time_offset.fetch_add(delta, Ordering::SeqCst);
        self.target_offset.fetch_add(delta, Ordering::SeqCst);
    }

    /// Moves the live clock offset toward the latest pong's estimate, as
    /// vanilla's Car.OnMessage does on every 0x46 (docs/03 0xF8): a gap over
    /// 1 s or under 3 ms is taken at once, anything between moves 3 ms. One
    /// pong's jitter then reaches relayed timestamps gradually.
    pub fn slew_offset(&self) {
        let target = self.target_offset.load(Ordering::SeqCst);
        let current = self.time_offset.load(Ordering::SeqCst);
        let gap = target - current;
        if !(-1000..=1000).contains(&gap) || (gap > -3 && gap < 3) {
            self.time_offset.store(target, Ordering::SeqCst);
        } else if gap > 0 {
            self.time_offset.store(current + 3, Ordering::SeqCst);
        } else {
            self.time_offset.store(current - 3, Ordering::SeqCst);
        }
    }

    /// Adds one 0xF8 round trip to the ping ring and recomputes the averaged
    /// ping and the target clock offset, as vanilla's UpdatePingAvg does
    /// (docs/03 0xF8): the mean of the samples so far, the last 50 once the
    /// ring is full, so there is no ramp from zero. A negative sample is kept
    /// but left out of the mean. The live offset follows on the next 0x46.
    pub fn record_pong(&self, raw: i32, now: i32, client_time: i32, uptime_ms: i64) {
        let mut samples = self.ping_samples.lock();
        if samples.len() == PING_RING_SIZE {
            samples.pop_front();
        }
        samples.push_back(raw);
        let valid = samples.iter().filter(|&&s| s >= 0).map(|&s| i64::from(s));
        let (sum, count) = valid.fold((0, 0), |(sum, count), s| (sum + s, count + 1));
        // count is 0 only if every sample is negative; vanilla would divide by
        // zero.
        let average = if count > 0 { sum / count } else { 0 };
        self.last_ping
            .store(u32::from(average as u16), Ordering::SeqCst);
        self.target_offset.store(
            i64::from(now) - average / 2 - i64::from(client_time),
            Ordering::SeqCst,
        );
        self.last_pong_ms.store(uptime_ms, Ordering::SeqCst);
    }

    /// Uptime (ms) of the last pong, 0 before the first.
    pub fn last_pong_ms(&self) -> i64 {
        self.last_pong_ms.load(Ordering::SeqCst)
    }

    /// Whether to drop `sender`'s update for this client under
    /// BANDWIDTH_OPTIMIZER, so every priority+1th update is sent (reference
    /// BroadcastCarUpdate). Advances the counter: call once per relayed
    /// update or AI tick.
    pub fn skip_position(&self, sender: u8) -> bool {
        self.relay_tier(sender).is_none()
    }

    /// `skip_position` that also returns the tier `sender`'s update goes
    /// out at, or `None` to drop it.
    pub fn relay_tier(&self, sender: u8) -> Option<u8> {
        let mut d = self.decimation.lock();
        let i = usize::from(sender);
        if d.skip[i] < d.priority[i] {
            d.skip[i] += 1;
            return None;
        }
        d.skip[i] = 0;
        Some(d.priority[i])
    }
}
