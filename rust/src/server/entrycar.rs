//! One entry-list slot: its occupant, BoP, P2P, last position and result.

use parking_lot::Mutex;
use smart_default::SmartDefault;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::client::Client;
use super::setup::SetupEntry;
use crate::config;
use crate::wire::Vec3;

/// A decoded inbound 0x46, re-encoded for the outbound one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PositionUpdate {
    pub sequence_id: u8,
    pub timestamp: u32,
    pub position: Vec3,
    pub rotation: Vec3,
    pub velocity: Vec3,
    pub tyre_angular_speed: [u8; 4],
    pub steer_angle: u8,
    pub wheel_angle: u8,
    pub engine_rpm: u16,
    pub gear: u8,
    pub status_flags: u32,
    pub performance_delta: i16,
    pub gas: u8,
    pub normalized_pos: f32,
}

/// Vanilla's best-lap value for a car with no lap. Races report total time,
/// 0 when none.
const NO_LAP_TIME: u32 = 999_999_999;

/// A slot's result in the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CarResult {
    pub best_lap: u32,
    pub total_time: u32,
    pub num_laps: u16,
    pub last_lap_time: u32,
    pub has_completed_last_lap: bool,
}

impl CarResult {
    /// The best lap, or vanilla's `NO_LAP_TIME` when there is none.
    pub fn best_lap_or_none(&self) -> u32 {
        if self.best_lap == 0 {
            NO_LAP_TIME
        } else {
            self.best_lap
        }
    }
}

/// The name, team, nation and guid a slot is listed under; also the last
/// driver seated in it (vanilla's `Car.Driver`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    pub name: String,
    pub team: String,
    pub nation: String,
    pub guid: String,
}

/// Everything about a slot except its position. Read a copy with
/// `EntryCar::state` and change it with `EntryCar::update`.
#[derive(Debug, Clone, SmartDefault)]
pub struct SlotState {
    // What the entry list sets.
    pub driver_name: String,
    pub team: String,
    pub spectator_mode: bool,
    /// The guids the slot is reserved for.
    pub guids: Vec<String>,
    pub ai_mode: config::AiMode,
    /// The entry-list BoP, restored on every join.
    pub entry_ballast: f32,
    pub entry_restrictor: f32,
    /// An entry-list car rather than a booking slot (vanilla `Car.IsEntryList`).
    pub from_entry_list: bool,

    // What a booking may set.
    pub model: String,
    pub skin: String,
    /// The locked setup sent as 0x52, empty for none.
    pub setup: Vec<SetupEntry>,

    /// The occupant, `None` if free.
    pub client: Option<Arc<Client>>,
    pub ballast: f32,
    pub restrictor: f32,
    // -1: a fresh slot's P2P count in vanilla.
    #[default(-1)]
    pub p2p_count: i16,
    pub mandatory_pit_done: bool,
    /// The last reported 0x56, shown in 0x40.
    pub damage: [f32; 5],
    /// The last accepted lap timestamp, if any.
    pub last_lap: Option<u32>,
    pub result: CarResult,
    /// Vanilla's `Car.Driver`, which outlives the connection: every listing
    /// keeps naming it, and its guid reclaims the slot (docs/01 §1.5).
    pub driver: Identity,
    /// The slot's last 0x50, sent to joiners until the next driver takes it.
    pub tyre_compound: String,
    /// The traffic driver's name while AI drives the slot (EnableAi), which
    /// every listing shows in place of the last driver.
    pub ai_name: Option<String>,
}

/// The last accepted position, if any, and the occupant's clock offset when
/// it was accepted.
#[derive(Debug, Clone, Copy, Default)]
struct PositionState {
    pos: PositionUpdate,
    offset: i64,
    valid: bool,
    /// Counts accepted updates and survives an occupant reset, so the relay
    /// can tell a new update from one it has already sent.
    version: u64,
}

/// One slot in the entry list. Its index is the session id.
#[derive(Debug)]
pub struct EntryCar {
    pub session_id: u8,
    state: Mutex<SlotState>,
    position: Mutex<PositionState>,
    /// The slot is in vanilla's car list. Every slot of a pickup or
    /// entry-list server is; a booking server lists only its entry-list and
    /// booked slots, and every car listing skips the rest.
    listed: AtomicBool,
    /// The slot was booked with the admin password, so its driver joins as
    /// admin (vanilla `Car.BookedAsAdmin`).
    pub(crate) booked_as_admin: AtomicBool,
    /// A newer position waits for the next relay pass.
    pub(crate) relay_pending: AtomicBool,
    /// The position version the relay last sent.
    relayed_version: AtomicU64,
    /// /forcelights: relay this car with its lights on.
    pub(crate) force_lights: AtomicBool,
}

impl EntryCar {
    /// A slot for entry-list car `entry`.
    pub fn new(session_id: u8, entry: &config::EntryCar) -> EntryCar {
        let car = EntryCar::new_spare(session_id);
        car.configure(entry);
        car.update(|s| {
            s.ballast = entry.ballast;
            s.restrictor = entry.restrictor;
            s.driver = initial_driver(entry, !s.guids.is_empty());
        });
        car
    }

    /// A slot past the entry list, out of the car list until a booking or a
    /// reloaded entry list fills it.
    pub fn new_spare(session_id: u8) -> EntryCar {
        EntryCar {
            session_id,
            state: Mutex::new(SlotState::default()),
            position: Mutex::new(PositionState::default()),
            listed: AtomicBool::new(false),
            booked_as_admin: AtomicBool::new(false),
            relay_pending: AtomicBool::new(false),
            relayed_version: AtomicU64::new(0),
            force_lights: AtomicBool::new(false),
        }
    }

    /// A copy of the slot's state.
    pub fn state(&self) -> SlotState {
        self.state.lock().clone()
    }

    /// Changes the slot's state under its lock. Do not call back into the
    /// slot from `change`.
    pub fn update<R>(&self, change: impl FnOnce(&mut SlotState) -> R) -> R {
        change(&mut self.state.lock())
    }

    /// Makes the slot entry-list car `entry` and lists it. Its BoP, driver
    /// and results are the caller's to reset.
    pub fn configure(&self, entry: &config::EntryCar) {
        self.update(|s| {
            s.driver_name = entry.driver_name.clone();
            s.team = entry.team.clone();
            s.spectator_mode = entry.spectator_mode;
            s.guids = entry.guids.clone();
            s.ai_mode = entry.ai_mode;
            s.entry_ballast = entry.ballast;
            s.entry_restrictor = entry.restrictor;
            s.from_entry_list = true;
            s.model = entry.model.clone();
            s.skin = entry.skin.clone();
        });
        self.listed.store(true, Ordering::SeqCst);
    }

    /// Clears the slot's entry-list attributes, driver and identity (unlist).
    pub fn clear_listing(&self) {
        self.listed.store(false, Ordering::SeqCst);
        self.booked_as_admin.store(false, Ordering::SeqCst);
        self.update(|s| {
            s.driver_name.clear();
            s.team.clear();
            s.spectator_mode = false;
            s.guids.clear();
            s.ai_mode = config::AiMode::None;
            s.entry_ballast = 0.0;
            s.entry_restrictor = 0.0;
            s.from_entry_list = false;
            s.driver = Identity::default();
            s.model.clear();
            s.skin.clear();
            s.setup.clear();
        });
    }

    /// The slot's car model folder name.
    pub fn model(&self) -> String {
        self.state.lock().model.clone()
    }

    /// Whether the slot is in the car list (see `EntryCar::listed`).
    pub fn listed(&self) -> bool {
        self.listed.load(Ordering::SeqCst)
    }

    pub fn set_listed(&self, listed: bool) {
        self.listed.store(listed, Ordering::SeqCst);
    }

    pub fn reserved(&self) -> bool {
        !self.state.lock().guids.is_empty()
    }

    pub fn reserved_for(&self, guid: &str) -> bool {
        self.state
            .lock()
            .guids
            .iter()
            .any(|allowed| allowed == guid)
    }

    /// The current occupant, if any.
    pub fn client(&self) -> Option<Arc<Client>> {
        self.state.lock().client.clone()
    }

    /// Clears the occupant only if it is still `client`. Returns whether it was.
    pub fn release_client(&self, client: &Arc<Client>) -> bool {
        let mut s = self.state.lock();
        match &s.client {
            Some(current) if Arc::ptr_eq(current, client) => {
                s.client = None;
                true
            }
            _ => false,
        }
    }

    /// Whether a live client holds this slot.
    pub fn occupied(&self) -> bool {
        self.state.lock().client.is_some()
    }

    /// The occupant's name, team, nation and guid, else those of the slot's
    /// last driver, which vanilla keeps after they leave and lists in 0x5B,
    /// 0x40, /JSON and the results file. A slot nobody has used is blank
    /// unless the entry list reserves it.
    pub fn identity(&self) -> Identity {
        let s = self.state.lock();
        if let Some(client) = &s.client {
            return client_identity(client);
        }
        if let Some(name) = &s.ai_name {
            return Identity {
                name: name.clone(),
                ..Identity::default()
            };
        }
        s.driver.clone()
    }

    /// A slot's driver before anyone takes it, from its entry-list car.
    pub fn reset_driver(&self, entry: &config::EntryCar) {
        self.update(|s| s.driver = initial_driver(entry, !s.guids.is_empty()));
    }

    /// Restores the entry-list ballast and restrictor, as vanilla's
    /// createNewCarConnection does for every join, and reports whether they
    /// changed.
    pub fn reset_bop(&self) -> bool {
        self.update(|s| {
            let changed = s.ballast != s.entry_ballast || s.restrictor != s.entry_restrictor;
            s.ballast = s.entry_ballast;
            s.restrictor = s.entry_restrictor;
            changed
        })
    }

    /// A copy of the accumulated result.
    pub fn result(&self) -> CarResult {
        self.state.lock().result
    }

    /// The last accepted position, if any.
    pub fn last_position(&self) -> Option<PositionUpdate> {
        let st = self.position.lock();
        st.valid.then_some(st.pos)
    }

    /// The last position plus the clock offset stored with it.
    pub fn relay_position(&self) -> Option<(PositionUpdate, i64)> {
        let st = self.position.lock();
        st.valid.then_some((st.pos, st.offset))
    }

    /// The latest position and its offset if it is newer than the last one
    /// relayed, recording it as relayed. Only the relay pass calls it, so an
    /// update that lands between the pending flag being cleared and this read
    /// is sent once, not again on the next pass.
    pub fn pending_relay(&self) -> Option<(PositionUpdate, i64)> {
        let st = self.position.lock();
        if !st.valid || st.version == self.relayed_version.load(Ordering::SeqCst) {
            return None;
        }
        self.relayed_version.store(st.version, Ordering::SeqCst);
        Some((st.pos, st.offset))
    }

    /// The clock offset stored with the last accepted update, 0 if none.
    pub fn relay_offset(&self) -> i64 {
        self.position.lock().offset
    }

    /// Moves the stored clock offset with a clock rebase.
    pub fn shift_relay_offset(&self, delta: i64) {
        self.position.lock().offset += delta;
    }

    /// Stores `pos` only if its timestamp is newer, as vanilla does, so a
    /// reordered datagram cannot move a car backwards. `offset` is the
    /// occupant's current clock offset, which the relay uses until the next
    /// accepted update.
    pub fn set_position(&self, pos: PositionUpdate, offset: i64) -> bool {
        let mut st = self.position.lock();
        if st.valid && pos.timestamp <= st.pos.timestamp {
            return false;
        }
        st.pos = pos;
        st.offset = offset;
        st.valid = true;
        st.version += 1;
        true
    }

    /// Vanilla's lap replay gate: timestamps must increase.
    pub fn accept_lap_timestamp(&self, timestamp: u32) -> bool {
        self.update(|s| {
            if s.last_lap.is_some_and(|last| timestamp <= last) {
                return false;
            }
            s.last_lap = Some(timestamp);
            true
        })
    }

    /// Records `client` as the slot's driver and clears its tyre compound, as
    /// every vanilla pickup does.
    pub fn seat(&self, client: &Client) {
        self.update(|s| {
            s.driver = client_identity(client);
            s.tyre_compound.clear();
        });
    }

    /// Clears the slot's laps for a new driver (vanilla ResetCarResults).
    /// Session state such as `has_completed_last_lap` is kept.
    pub fn reset_laps(&self) {
        self.update(|s| {
            s.result.best_lap = 0;
            s.result.total_time = 0;
            s.result.num_laps = 0;
            s.result.last_lap_time = 0;
        });
    }

    /// Clears the slot for a new session. BoP is kept.
    pub fn reset_result(&self) {
        self.update(|s| {
            s.result = CarResult::default();
            s.p2p_count = -1;
            s.mandatory_pit_done = false;
        });
        self.reset_occupant_state();
    }

    /// Clears the timestamp gates for a new occupant. Results stay with the
    /// slot, as in vanilla.
    pub fn reset_occupant_state(&self) {
        {
            let mut st = self.position.lock();
            let version = st.version;
            *st = PositionState {
                version,
                ..PositionState::default()
            };
        }
        self.relay_pending.store(false, Ordering::SeqCst);
        self.update(|s| s.last_lap = None);
    }
}

/// How a slot lists `client`.
fn client_identity(client: &Client) -> Identity {
    let id = client.identity();
    Identity {
        name: id.name.clone(),
        team: id.team.clone(),
        nation: id.nation.clone(),
        guid: id.guid.clone(),
    }
}

/// A slot's driver before anyone takes it. Vanilla names an unused slot only
/// when it is reserved: /JSON shows "" for an open slot even with TEAM= set
/// (measured).
fn initial_driver(entry: &config::EntryCar, reserved: bool) -> Identity {
    if !reserved {
        return Identity::default();
    }
    Identity {
        name: entry.driver_name.clone(),
        team: entry.team.clone(),
        nation: String::new(),
        guid: String::new(),
    }
}
