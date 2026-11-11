//! AI traffic in the server (`EnableAi`): which slots AI drives, spawning
//! cars around players, moving them and sending them to every client. The
//! slot rules and the spawner follow AssettoServer: `AI=fixed` slots are
//! always traffic, `AI=auto` slots are traffic until a player takes them,
//! and each AI slot can drive several cars ("states") far apart, each
//! receiver seeing the one nearest it.

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;

use anyhow::Context as _;
use rand::seq::SliceRandom as _;
use rand::{Rng, RngExt};

use super::Server;
use super::client::{Client, MAX_SESSION_SLOTS};
use super::entrycar::{EntryCar, PositionUpdate};
use super::loops::{
    Bubble, CustomBatch, MEGA_PACKET_BATCH, MegaEntry, ReadyClient, build_mega_packet,
    update_position_priorities,
};
use crate::ai::{AiSpline, AiState, BuildOptions, CarTuning, JunctionChoices, Obstacle, World};
use crate::config::{AiMode, AiParams, Config};
use crate::proto;
use crate::wire::Vec3;
use crate::wire::Writer;

/// How often AI cars look for obstacles.
pub const OBSTACLE_INTERVAL_MS: u64 = 100;

/// A player's car hitting an AI car this close stops it.
const COLLISION_RADIUS: f32 = 25.0;

/// Cars per `ai_debug` event.
const AI_DEBUG_ROWS: usize = 20;

/// The `LuaPacketType` of `ai_debug`, the hash of
/// `int16_t ClosestAiObstacles[20]; uint8_t CurrentSpeeds[20];
/// uint8_t MaxSpeeds[20]; uint8_t SessionIds[20]; uint8_t TargetSpeeds[20];`
/// (docs/04 §4.4), computed with the reference library.
const LUA_AI_DEBUG: u32 = 0xB8DC_08B3;

/// How far around a spawn point the spawner looks for neighbours.
const SPAWN_NEIGHBOUR_SEARCH: f32 = 50.0;

/// One AI state by slot and index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateRef {
    slot: u8,
    index: u16,
}

/// One entry-list slot as traffic sees it.
struct AiSlot {
    mode: AiMode,
    /// The entry's car model, for its tuning.
    model: String,
    /// AI drives the slot (no player holds it).
    controlled: bool,
    /// The cars announced to clients with a 0x5A.
    announced: bool,
    tuning: CarTuning,
    /// How many states the slot should drive (overbooking).
    target_count: usize,
    states: Vec<AiState>,
    /// The 0x46 sequence number per receiver.
    sequence: Vec<u8>,
    /// The entry's SKIN allows CSP colour changes, so each car has its own
    /// paint.
    color_changes: bool,
    /// The car (state index, spawn count) each receiver last saw.
    last_seen: Vec<Option<(usize, u8)>>,
}

/// What traffic remembers about a player between spawner passes.
#[derive(Debug, Clone, Copy, Default)]
struct PlayerTrack {
    /// The inputs last seen, to tell an active player from an AFK one.
    inputs: (u32, u8, u8),
    last_active: i64,
}

struct TrafficInner {
    slots: Vec<AiSlot>,
    /// The slowest AI car at each spline point (AssettoServer's
    /// SlowestAiStates), which the lookahead uses to find the car ahead.
    slowest: Vec<Option<StateRef>>,
    players: Vec<PlayerTrack>,
    /// Scales gaps and car counts; the hourly table may change it.
    density: f32,
}

/// The traffic system.
pub struct Traffic {
    spline: AiSpline,
    /// AiParams, swapped whole when an admin changes one (live_config.rs).
    params: parking_lot::RwLock<Arc<AiParams>>,
    inner: Mutex<TrafficInner>,
}

/// A player as the spawner and the senders see them.
#[derive(Debug, Clone, Copy)]
struct PlayerView {
    session_id: u8,
    position: Vec3,
    velocity: Vec3,
}

impl Traffic {
    /// Loads the track's AI splines and sets up the AI slots.
    pub fn new(cfg: &Config) -> anyhow::Result<Traffic> {
        let content = cfg
            .content_dir
            .as_deref()
            .context("EnableAi needs --content, for the track's ai/ folder")?;
        if !cfg.server.pickup_mode_enabled {
            anyhow::bail!("EnableAi needs PICKUP_MODE_ENABLED=1");
        }
        let params = cfg.extra.ai_params.clone();
        let ai_dir = content
            .join("content")
            .join("tracks")
            .join(&cfg.server.track)
            .join(&cfg.server.track_config)
            .join("ai");
        let ai_dir = if ai_dir.is_dir() {
            ai_dir
        } else {
            content
                .join("content")
                .join("tracks")
                .join(&cfg.server.track)
                .join("ai")
        };
        let spline = AiSpline::load(
            &ai_dir,
            &cfg.server.track,
            BuildOptions {
                lane_width: params.lane_width_meters,
                two_way_traffic: params.two_way_traffic,
                smooth_camber: params.smooth_camber,
            },
        )?;
        tracing::info!(points = spline.points.len(), junctions = spline.junctions.len(), dir = %ai_dir.display(), "AI splines loaded");

        let mut slots = Vec::with_capacity(cfg.entry_list.len());
        for entry in &cfg.entry_list {
            slots.push(AiSlot {
                mode: entry.ai_mode,
                model: entry.model.clone(),
                controlled: entry.ai_mode != AiMode::None,
                announced: false,
                tuning: CarTuning::for_model(&params, &entry.model),
                target_count: 0,
                states: Vec::new(),
                sequence: vec![0; MAX_SESSION_SLOTS],
                color_changes: allows_color_change(&entry.skin),
                last_seen: vec![None; MAX_SESSION_SLOTS],
            });
        }
        let ai_slots = slots.iter().filter(|s| s.mode != AiMode::None).count();
        if ai_slots == 0 {
            tracing::warn!("EnableAi is on but no entry-list car has AI=auto or AI=fixed");
        }
        Ok(Traffic {
            inner: Mutex::new(TrafficInner {
                slots,
                slowest: vec![None; spline.points.len()],
                players: vec![PlayerTrack::default(); MAX_SESSION_SLOTS],
                density: params.traffic_density,
            }),
            params: parking_lot::RwLock::new(Arc::new(params)),
            spline,
        })
    }

    fn lock(&self) -> MutexGuard<'_, TrafficInner> {
        self.inner.lock()
    }

    /// Whether AI drives slot `id`.
    pub fn controls(&self, id: u8) -> bool {
        self.lock()
            .slots
            .get(usize::from(id))
            .is_some_and(|s| s.controlled)
    }

    /// The road network, for plugins.
    pub(crate) fn spline(&self) -> &AiSpline {
        &self.spline
    }

    /// AiParams.LaneWidthMeters.
    pub(crate) fn lane_width(&self) -> f32 {
        self.params().lane_width_meters
    }

    /// The current AiParams.
    pub(crate) fn params(&self) -> Arc<AiParams> {
        Arc::clone(&self.params.read())
    }

    /// Replaces AiParams, retuning every slot's car.
    pub(crate) fn set_params(&self, params: AiParams) {
        let params = Arc::new(params);
        let mut inner = self.lock();
        for slot in inner.slots.iter_mut() {
            slot.tuning = CarTuning::for_model(&params, &slot.model);
        }
        inner.density = params.traffic_density;
        *self.params.write() = params;
    }

    /// The AI driver's name for slot `id`.
    pub fn ai_name(&self, id: u8) -> String {
        format!("{} {id}", self.params().name_prefix)
    }
}

impl TrafficInner {
    fn state(&self, r: StateRef) -> Option<&AiState> {
        self.slots
            .get(usize::from(r.slot))?
            .states
            .get(usize::from(r.index))
    }

    /// The live car recorded at `point`, if it is still there.
    fn slowest_at(&self, point: u32) -> Option<(StateRef, &AiState)> {
        let r = self.slowest[point as usize]?;
        let state = self.state(r)?;
        (state.initialized && state.point == point).then_some((r, state))
    }

    /// Moves a car's entry from `old` to its current point, keeping the
    /// slowest car at each point.
    fn reposition(&mut self, r: StateRef, old: Option<u32>) {
        if let Some(old) = old
            && self.slowest[old as usize] == Some(r)
        {
            self.slowest[old as usize] = None;
        }
        let Some(state) = self.state(r) else { return };
        if !state.initialized {
            return;
        }
        let (point, speed) = (state.point, state.current_speed);
        let replace = match self.slowest_at(point) {
            Some((current, other)) => current != r && speed < other.current_speed,
            None => true,
        };
        if replace {
            self.slowest[point as usize] = Some(r);
        }
    }

    fn despawn(&mut self, r: StateRef) {
        let point = match self.state(r) {
            Some(s) if s.initialized => s.point,
            _ => return,
        };
        if self.slowest[point as usize] == Some(r) {
            self.slowest[point as usize] = None;
        }
        if let Some(state) = self.slots[usize::from(r.slot)]
            .states
            .get_mut(usize::from(r.index))
        {
            state.initialized = false;
        }
    }

    fn despawn_slot(&mut self, slot: usize) {
        for index in 0..self.slots[slot].states.len() {
            self.despawn(StateRef {
                slot: slot as u8,
                index: index as u16,
            });
        }
    }
}

/// A uniform pick from [0, max) weighted toward 0, as AssettoServer's
/// GetRandomWeighted favours the players furthest from traffic.
fn random_weighted(rng: &mut impl Rng, max: usize) -> usize {
    let total = max * (max + 1) / 2;
    if total == 0 {
        return 0;
    }
    let roll = rng.random_range(0..total);
    let mut target = 0;
    let mut i = max;
    while i < total {
        if roll < i {
            break;
        }
        target += 1;
        i += i.saturating_sub(1).max(1);
    }
    target.min(max - 1)
}

impl Server {
    /// Whether no player may take this slot: an `AI=fixed` slot, or any AI
    /// slot when traffic is off, as vanilla never seats a player in one.
    pub(crate) fn ai_only(&self, car: &EntryCar) -> bool {
        match car.state().ai_mode {
            AiMode::None => false,
            AiMode::Fixed => true,
            AiMode::Auto => self.traffic.is_none(),
        }
    }

    /// AiParams.MaxPlayerCount: refuse players to leave room for traffic.
    pub(crate) fn traffic_player_limit(&self) -> bool {
        let Some(traffic) = &self.traffic else {
            return false;
        };
        let max = traffic.params().max_player_count;
        max > 0 && self.connected_count() as i32 >= max
    }

    /// A player took `car`: AI stops driving it (announced with a 0x4D if
    /// its cars were out), and the traffic is shared out again.
    pub(crate) fn traffic_on_join(&self, car: &EntryCar) {
        let Some(traffic) = &self.traffic else { return };
        let id = car.session_id;
        let was_announced = {
            let mut inner = traffic.lock();
            let slot = usize::from(id);
            if slot >= inner.slots.len() || !inner.slots[slot].controlled {
                drop(inner);
                self.adjust_overbooking();
                return;
            }
            inner.despawn_slot(slot);
            let s = &mut inner.slots[slot];
            s.controlled = false;
            s.states.clear();
            s.target_count = 0;
            std::mem::replace(&mut s.announced, false)
        };
        car.update(|s| s.ai_name = None);
        tracing::debug!(car = id, "slot no longer driven by AI");
        if was_announced {
            self.broadcast_car_disconnected(id);
        }
        if traffic.params().hide_ai_cars {
            self.set_car_visibility(id, true);
        }
        self.adjust_overbooking();
    }

    /// A player left `car`: an AI slot is traffic again.
    pub(crate) fn traffic_on_leave(&self, car: &EntryCar) {
        let Some(traffic) = &self.traffic else { return };
        let id = car.session_id;
        let slot = usize::from(id);
        let becomes_ai = {
            let mut inner = traffic.lock();
            match inner.slots.get_mut(slot) {
                Some(s) if s.mode != AiMode::None && !s.controlled => {
                    s.controlled = true;
                    s.last_seen.fill(None);
                    s.states.clear();
                    s.target_count = 0;
                    s.announced = false;
                    true
                }
                _ => false,
            }
        };
        if becomes_ai {
            car.update(|s| s.ai_name = Some(traffic.ai_name(id)));
            tracing::debug!(car = id, "slot driven by AI");
            if traffic.params().hide_ai_cars {
                self.set_car_visibility(id, false);
            }
        }
        self.adjust_overbooking();
    }

    /// Names every AI-driven slot at start.
    pub(crate) fn traffic_init_slots(&self) {
        let Some(traffic) = &self.traffic else { return };
        for car in &self.entry_cars {
            if traffic.controls(car.session_id) {
                car.update(|s| s.ai_name = Some(traffic.ai_name(car.session_id)));
            }
        }
    }

    /// Tells a joining client which cars are hidden AI (HideAiCars).
    pub(crate) fn send_traffic_visibility(&self, client: &Client) {
        let Some(traffic) = &self.traffic else { return };
        if !traffic.params().hide_ai_cars || !client.is_csp() || !self.cfg.extra.enable_csp {
            return;
        }
        for car in &self.entry_cars {
            if traffic.controls(car.session_id) {
                self.send_car_visibility(client, car.session_id, false);
            }
        }
    }

    /// Shares the AI cars out: AiPerPlayerTargetCount per player, at most
    /// MaxAiTargetCount, spread over the free AI slots, each capped by its
    /// MaxOverbooking.
    pub(crate) fn adjust_overbooking(&self) {
        let Some(traffic) = &self.traffic else { return };
        let players = self.connected_count();
        let mut inner = traffic.lock();
        let density = inner.density;
        let ai_slots: Vec<usize> = (0..inner.slots.len())
            .filter(|&i| inner.slots[i].controlled)
            .collect();
        if ai_slots.is_empty() {
            return;
        }
        let per_player = ((traffic.params().ai_per_player_target_count as f32 * density).round()
            as usize)
            .min(ai_slots.len());
        let target =
            (players * per_player).min(traffic.params().max_ai_target_count.max(0) as usize);
        let each = target / ai_slots.len();
        let rest = target % ai_slots.len();
        tracing::debug!(players, slots = ai_slots.len(), target, "AI overbooking");
        for (n, &i) in ai_slots.iter().enumerate() {
            let slot = &mut inner.slots[i];
            let mut count = if n < rest { each + 1 } else { each };
            if let Some(max) = slot.tuning.max_overbooking {
                count = count.min(max);
            }
            while slot.states.len() < count {
                slot.states.push(AiState::default());
            }
            slot.target_count = count;
        }
    }

    /// /setaioverbooking: every AI slot drives `count` cars, until the next
    /// player joins or leaves.
    pub(crate) fn set_ai_overbooking(&self, count: usize) {
        let Some(traffic) = &self.traffic else { return };
        let mut inner = traffic.lock();
        for slot in inner.slots.iter_mut().filter(|s| s.controlled) {
            let count = slot
                .tuning
                .max_overbooking
                .map_or(count, |max| count.min(max));
            while slot.states.len() < count {
                slot.states.push(AiState::default());
            }
            slot.target_count = count;
        }
    }

    /// The players traffic plays around, with their last positions.
    fn traffic_players(&self) -> Vec<PlayerView> {
        let mut players = Vec::new();
        for car in &self.entry_cars {
            let Some(client) = car.client() else { continue };
            if !client.ready() {
                continue;
            }
            if let Some(pos) = car.last_position() {
                players.push(PlayerView {
                    session_id: car.session_id,
                    position: pos.position,
                    velocity: pos.velocity,
                });
            }
        }
        players
    }

    /// Moves every AI car to now, ranks every car for each receiver, then
    /// sends each receiver the car of each AI slot nearest it that its tier
    /// lets through, batched with the far players in `far` (`ready`'s order),
    /// after releasing the traffic lock. Runs on the relay loop's tick
    /// (`maybe_tick`).
    pub(crate) fn step_traffic(&self, ready: &[ReadyClient], far: Vec<Vec<MegaEntry>>) {
        let Some(traffic) = &self.traffic else { return };
        let params = traffic.params();
        let now = self.server_time_millis();
        let rain = self
            .weatherfx
            .as_ref()
            .map_or(0.0, |fx| fx.conditions().rain_intensity);
        let mut rng = rand::rng();
        let mut inner = traffic.lock();
        // Move.
        for slot in 0..inner.slots.len() {
            if !inner.slots[slot].controlled {
                continue;
            }
            for index in 0..inner.slots[slot].states.len() {
                let r = StateRef {
                    slot: slot as u8,
                    index: index as u16,
                };
                let (old, alive) = {
                    let s = &mut inner.slots[slot];
                    let tuning = s.tuning.clone();
                    let state = &mut s.states[index];
                    if !state.initialized {
                        continue;
                    }
                    let old = state.point;
                    let world = World {
                        spline: &traffic.spline,
                        params: &params,
                        tuning: &tuning,
                        now,
                        slowest_at: &|_| None,
                        players: &[],
                    };
                    (old, state.update(&world, rain, &mut rng))
                };
                if alive {
                    inner.reposition(r, Some(old));
                } else {
                    tracing::trace!(car = slot, "AI car reached the end of its lane");
                    inner.slowest[old as usize] = inner.slowest[old as usize].filter(|x| *x != r);
                    inner.slots[slot].states[index].initialized = false;
                }
            }
        }
        // Send.
        let receivers: Vec<(usize, PositionUpdate)> = ready
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.view.map(|p| (i, p)))
            .collect();
        // What each receiver gets this tick, sent once the lock is dropped.
        let mut outgoing: Vec<Vec<MegaEntry>> = Vec::new();
        outgoing.resize_with(ready.len(), Vec::new);
        let bubble = Bubble::new(&self.live().extra);
        let two_way = traffic.params().two_way_traffic;
        let min_state_d2 = traffic.params().min_state_distance_meters.powi(2);
        // The players' cars, ranked with each receiver's AI cars below.
        let snaps: Vec<_> = self
            .position_snapshots()
            .into_iter()
            .filter(|s| {
                !inner
                    .slots
                    .get(usize::from(s.id))
                    .is_some_and(|slot| slot.controlled)
            })
            .collect();
        for (ri, receiver_pos) in &receivers {
            let receiver = &ready[*ri];
            if receiver.addr.is_none() {
                continue;
            }
            let to = usize::from(receiver.session_id);
            // Each slot's car this receiver sees.
            let seen_cars: Vec<(usize, usize, f32)> = inner
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.controlled)
                .filter_map(|(slot_id, slot)| {
                    let index = best_state_for(&slot.states, receiver_pos, two_way, min_state_d2)?;
                    let d2 = slot.states[index]
                        .status
                        .position
                        .distance_squared(receiver_pos.position);
                    Some((slot_id, index, d2))
                })
                .collect();
            // One ranking of every car, players' and AI, sets both rates.
            let ai: Vec<(u8, f32)> = seen_cars
                .iter()
                .map(|&(slot_id, _, d2)| (slot_id as u8, d2))
                .collect();
            update_position_priorities(
                receiver.session_id,
                Some(receiver_pos.position),
                &mut receiver.client.decimation.lock().priority,
                &snaps,
                &ai,
                bubble,
            );
            for &(slot_id, index, _) in &seen_cars {
                let slot = &mut inner.slots[slot_id];
                let state = &slot.states[index];
                let status = state.status;
                // A receiver seeing a different car of this slot gets its paint.
                let seen = Some((index, state.spawn_counter));
                if slot.color_changes && slot.last_seen[to] != seen {
                    slot.last_seen[to] = seen;
                    self.send_car_color(&receiver.client, slot_id as u8, state.color);
                }
                // The number advances on a skipped update too, so the game
                // reads the gap as the time that passed: it paces a state's
                // timestamp by gap x send interval.
                let sequence = slot.sequence[to];
                slot.sequence[to] = sequence.wrapping_add(1);
                if receiver.client.skip_position(slot_id as u8) {
                    continue;
                }
                let pos = PositionUpdate {
                    sequence_id: sequence,
                    timestamp: 0,
                    position: status.position,
                    rotation: status.rotation,
                    velocity: status.velocity,
                    tyre_angular_speed: [status.tyre_angular_speed; 4],
                    steer_angle: 127,
                    wheel_angle: 127,
                    engine_rpm: status.engine_rpm,
                    gear: 2,
                    status_flags: status.status_flags,
                    performance_delta: 0,
                    gas: 0,
                    normalized_pos: 0.0,
                };
                outgoing[*ri].push(MegaEntry {
                    session_id: slot_id as u8,
                    // Server clock → receiver clock (docs/03 0x46).
                    timestamp: (status.timestamp - receiver.offset) as u32,
                    ping: 0,
                    pos,
                });
            }
        }
        drop(inner);
        // Each receiver's far players (`BatchFarPositions`) ride in its AI
        // cars' 0x48s; with no AI to share, they go out on their own.
        for ((receiver, entries), held) in ready.iter().zip(&mut outgoing).zip(far) {
            if entries.is_empty() {
                self.send_far_positions(receiver, &held, now);
            } else {
                entries.extend(held);
            }
        }
        self.send_traffic_batches(ready, &outgoing, now);
    }

    /// Sends each receiver its AI cars in batches of 20, as AssettoServer
    /// does: `0xAB 0x03` to a custom-update client, else 0x48 (docs/03).
    fn send_traffic_batches(&self, ready: &[ReadyClient], outgoing: &[Vec<MegaEntry>], now: i64) {
        let mut custom = CustomBatch::default();
        for (receiver, entries) in ready.iter().zip(outgoing) {
            let Some(addr) = receiver.addr else { continue };
            if receiver.custom_update {
                for e in entries {
                    self.add_custom_update(
                        receiver,
                        &mut custom,
                        e.session_id,
                        e.ping,
                        e.timestamp,
                        &e.pos,
                    );
                }
                self.flush_custom_batches(
                    std::slice::from_ref(receiver),
                    std::slice::from_mut(&mut custom),
                );
                continue;
            }
            let server_time = (now - receiver.offset) as u32;
            let ping = receiver.client.ping();
            for chunk in entries.chunks(MEGA_PACKET_BATCH) {
                self.send_udp(&build_mega_packet(server_time, ping, chunk), addr);
            }
        }
    }

    /// Picks each AI car's target speed from what is ahead of it.
    pub(crate) fn traffic_obstacles(&self) {
        let Some(traffic) = &self.traffic else { return };
        let params = traffic.params();
        let now = self.server_time_millis();
        let players: Vec<Obstacle> = self
            .traffic_players()
            .iter()
            .map(|p| Obstacle {
                position: p.position,
                speed: p.velocity_length(),
                target_speed: p.velocity_length(),
                length_post: 0.0,
            })
            .collect();
        let mut rng = rand::rng();
        let mut inner = traffic.lock();
        // A copy of what each point's slowest car looks like from behind, so
        // one car can be changed while the others are read. Keyed by point,
        // since only the few occupied points matter.
        let mut views: std::collections::HashMap<u32, Obstacle> = std::collections::HashMap::new();
        for slot in &inner.slots {
            for state in slot.states.iter().filter(|s| s.initialized) {
                if let Some((r, s)) = inner.slowest_at(state.point) {
                    let tuning = &inner.slots[usize::from(r.slot)].tuning;
                    views.insert(state.point, s.as_obstacle(tuning));
                }
            }
        }
        let slowest_at = |p: u32| views.get(&p).copied();
        for slot in inner.slots.iter_mut() {
            if !slot.controlled {
                continue;
            }
            let tuning = slot.tuning.clone();
            for state in slot.states.iter_mut() {
                let world = World {
                    spline: &traffic.spline,
                    params: &params,
                    tuning: &tuning,
                    now,
                    slowest_at: &slowest_at,
                    players: &players,
                };
                state.detect_obstacles(&world, &mut rng);
            }
        }
        if traffic.params().debug {
            self.send_traffic_debug(&inner);
        }
    }

    /// AiParams.Debug: each CSP player gets, for every AI slot, the speeds
    /// and gap of its car nearest them, as the `ai_debug` Lua event that
    /// the built-in ai_debug.lua draws.
    fn send_traffic_debug(&self, inner: &TrafficInner) {
        if !self.cfg.extra.enable_client_messages {
            return;
        }
        let kmh = |ms: f32| (ms * 3.6).clamp(0.0, 255.0) as u8;
        for player in self.traffic_players() {
            let Some(client) = self.entry_cars[usize::from(player.session_id)].client() else {
                continue;
            };
            if !client.is_csp() {
                continue;
            }
            let mut rows: Vec<(u8, u8, u8, u8, i16)> = Vec::new();
            for (id, slot) in inner.slots.iter().enumerate().filter(|(_, s)| s.controlled) {
                let nearest = slot.states.iter().filter(|s| s.initialized).min_by(|a, b| {
                    let da = a.status.position.distance_squared(player.position);
                    let db = b.status.position.distance_squared(player.position);
                    da.total_cmp(&db)
                });
                if let Some(s) = nearest {
                    let gap = s.closest_obstacle_distance.clamp(-1.0, f32::from(i16::MAX)) as i16;
                    rows.push((
                        id as u8,
                        kmh(s.current_speed),
                        kmh(s.target_speed),
                        kmh(s.max_speed),
                        gap,
                    ));
                }
            }
            for chunk in rows.chunks(AI_DEBUG_ROWS) {
                let mut ids = [255u8; AI_DEBUG_ROWS];
                let mut current = [0u8; AI_DEBUG_ROWS];
                let mut target = [0u8; AI_DEBUG_ROWS];
                let mut max = [0u8; AI_DEBUG_ROWS];
                let mut gaps = [0i16; AI_DEBUG_ROWS];
                for (i, row) in chunk.iter().enumerate() {
                    (ids[i], current[i], target[i], max[i], gaps[i]) = *row;
                }
                // Fields in the Lua struct's packed order (docs/04 §4.4).
                let mut w = Writer::with_capacity(9 + AI_DEBUG_ROWS * 6);
                w.u8(proto::EXTENDED);
                w.u8(proto::EXT_TCP_CLIENT_MESSAGE);
                w.u8(proto::SERVER_SESSION_ID);
                w.u16(proto::CSP_LUA_MESSAGE);
                w.u32(LUA_AI_DEBUG);
                for gap in gaps {
                    w.i16(gap);
                }
                w.raw(&current);
                w.raw(&max);
                w.raw(&ids);
                w.raw(&target);
                self.send_tcp(&client, w.bytes());
            }
        }
    }

    /// The spawner, at AiBehaviorUpdateIntervalHz: despawns cars far from
    /// every player and respawns them ahead of (or behind) players, weighted
    /// toward the players with the least traffic near them.
    pub(crate) fn traffic_spawn(&self) {
        let Some(traffic) = &self.traffic else { return };
        let now = self.server_time_millis();
        let params = traffic.params();
        let spline = &traffic.spline;
        let views = self.traffic_players();
        let mut rng = rand::rng();
        let mut inner = traffic.lock();
        self.update_traffic_density(traffic, &mut inner);

        // Players eligible to spawn traffic: active, and (unless configured
        // otherwise) driving with their lane.
        let mut players: Vec<PlayerView> = Vec::new();
        for p in &views {
            let entry = self.entry_cars[usize::from(p.session_id)].last_position();
            let track = &mut inner.players[usize::from(p.session_id)];
            if let Some(pos) = entry {
                let inputs = (pos.status_flags, pos.gas, pos.steer_angle);
                if inputs != track.inputs || track.last_active == 0 {
                    track.inputs = inputs;
                    track.last_active = now;
                }
            }
            if now - track.last_active >= params.player_afk_timeout_seconds * 1000 {
                continue;
            }
            let right_way = spline
                .world_to_spline(p.position)
                .is_some_and(|(id, _)| spline.forward(id).dot(p.velocity) > 0.0);
            if params.two_way_traffic || params.wrong_way_traffic || right_way {
                players.push(*p);
            }
        }

        // Despawn unsafe duplicates, sort out the free states.
        let min_state_d2 = params.min_state_distance_meters.powi(2);
        let mut alive: Vec<StateRef> = Vec::new();
        let mut free: Vec<StateRef> = Vec::new();
        for slot in 0..inner.slots.len() {
            if !inner.slots[slot].controlled {
                continue;
            }
            let states = &inner.slots[slot].states;
            let mut close: Vec<usize> = Vec::new();
            for i in 0..states.len() {
                for j in 0..states.len() {
                    let (a, b) = (&states[i], &states[j]);
                    if i != j
                        && a.initialized
                        && b.initialized
                        && a.status.position.distance_squared(b.status.position) < min_state_d2
                        && (params.two_way_traffic
                            || a.status.velocity.dot(b.status.velocity) > 0.0)
                    {
                        close.push(i);
                    }
                }
            }
            for i in close {
                inner.despawn(StateRef {
                    slot: slot as u8,
                    index: i as u16,
                });
            }
            for (i, s) in inner.slots[slot].states.iter().enumerate() {
                let r = StateRef {
                    slot: slot as u8,
                    index: i as u16,
                };
                if s.initialized {
                    alive.push(r);
                } else {
                    free.push(r);
                }
            }
        }

        // Each live car's distance to its nearest player (looking ahead of
        // the player), and each player's to its nearest car.
        let offsets: Vec<Vec3> = players
            .iter()
            .map(|p| {
                if p.velocity == Vec3::default() {
                    p.position
                } else {
                    p.position
                        + p.velocity.normalize_or_zero() * params.player_position_offset_meters
                }
            })
            .collect();
        let mut car_to_player: Vec<(StateRef, f32)> = Vec::new();
        let mut player_to_car: Vec<f32> = vec![f32::MAX; players.len()];
        for &r in &alive {
            let Some(state) = inner.state(r) else {
                continue;
            };
            let mut nearest = f32::MAX;
            for (j, offset) in offsets.iter().enumerate() {
                let d2 = state.status.position.distance_squared(*offset);
                nearest = nearest.min(d2);
                player_to_car[j] = player_to_car[j].min(d2);
            }
            car_to_player.push((r, nearest));
        }
        // Furthest first, so the cars nobody sees are reused first.
        car_to_player.sort_by(|a, b| b.1.total_cmp(&a.1));
        let radius2 = params.player_radius_meters.powi(2);
        for (r, d2) in car_to_player {
            let protected = inner
                .state(r)
                .is_some_and(|s| now <= s.spawn_protection_ends);
            if d2 > radius2 && !protected {
                free.push(r);
            }
        }
        if !alive.is_empty() && !players.is_empty() {
            // Players far from any traffic first.
            let mut order: Vec<usize> = (0..players.len()).collect();
            order.sort_by(|&a, &b| player_to_car[b].total_cmp(&player_to_car[a]));
            players = order.into_iter().map(|i| players[i]).collect();
        } else {
            players.shuffle(&mut rng);
        }

        // Free cars above their slot's target are retired, not respawned.
        let over_target = |inner: &TrafficInner, r: &StateRef| {
            usize::from(r.index) >= inner.slots[usize::from(r.slot)].target_count
        };
        for r in &free {
            if over_target(&inner, r) {
                inner.despawn(*r);
            }
        }
        free.retain(|r| !over_target(&inner, r));

        let mut choices = JunctionChoices::forgetful();
        while !players.is_empty() && !free.is_empty() {
            let mut found = None;
            while found.is_none() && !players.is_empty() {
                let pick = random_weighted(&mut rng, players.len());
                let player = players.remove(pick);
                found = spawn_point(traffic, &inner, &mut choices, &player, &views, &mut rng);
            }
            let Some(point) = found else { continue };
            if choices.next(spline, point, 1, &mut rng).is_none() {
                continue;
            }
            let behind = nearest_on_road(traffic, &inner, &mut choices, point, false, &mut rng);
            let ahead = nearest_on_road(traffic, &inner, &mut choices, point, true, &mut rng);
            let mut spawned = None;
            for (n, &r) in free.iter().enumerate() {
                if !can_spawn(traffic, &inner, r, point, behind, ahead) {
                    continue;
                }
                let slot = usize::from(r.slot);
                let tuning = inner.slots[slot].tuning.clone();
                let old = inner
                    .state(r)
                    .and_then(|s| s.initialized.then_some(s.point));
                inner.despawn(r);
                let world = World {
                    spline,
                    params: &params,
                    tuning: &tuning,
                    now,
                    slowest_at: &|_| None,
                    players: &[],
                };
                let ok = inner.slots[slot].states[usize::from(r.index)]
                    .teleport(&world, point, &mut rng);
                if ok {
                    inner.reposition(r, old);
                } else {
                    inner.slots[slot].states[usize::from(r.index)].initialized = false;
                }
                spawned = Some(n);
                break;
            }
            if let Some(n) = spawned {
                free.remove(n);
            }
        }
        self.trim_states(traffic, &mut inner);
    }

    /// Drops free states above each slot's target, and announces or retires
    /// the slot's car to clients as it gains or loses its last state.
    fn trim_states(&self, traffic: &Traffic, inner: &mut TrafficInner) {
        let mut connect: Vec<u8> = Vec::new();
        let mut disconnect: Vec<u8> = Vec::new();
        for (id, slot) in inner.slots.iter_mut().enumerate() {
            if !slot.controlled {
                continue;
            }
            while slot.states.len() > slot.target_count
                && slot.states.last().is_some_and(|s| !s.initialized)
            {
                slot.states.pop();
            }
            let any = slot.states.iter().any(|s| s.initialized);
            if any && !slot.announced {
                slot.announced = true;
                connect.push(id as u8);
            } else if !any && slot.announced && slot.states.is_empty() {
                slot.announced = false;
                disconnect.push(id as u8);
            }
        }
        for id in connect {
            self.broadcast_named_car_connected(id, &traffic.ai_name(id));
        }
        for id in disconnect {
            self.broadcast_car_disconnected(id);
        }
    }

    /// AiParams.HourlyTrafficDensity, by the in-game hour.
    fn update_traffic_density(&self, traffic: &Traffic, inner: &mut TrafficInner) {
        let params = traffic.params();
        let Some(hourly) = &params.hourly_traffic_density else {
            return;
        };
        let hour = match &self.weatherfx {
            Some(fx) => fx.hour_of_day(),
            None => {
                let sun = f64::from(self.weather.snapshot().sun_angle);
                super::weatherfx::seconds_from_sun_angle(sun) / 3600.0
            }
        }
        .rem_euclid(24.0);
        let low = hour.floor() as usize % 24;
        let high = (low + 1) % 24;
        let t = (hour - hour.floor()) as f32;
        inner.density = hourly[low] + (hourly[high] - hourly[low]) * t;
    }

    /// A player's car hit AI slot `target`: its car nearest the player stops
    /// for a moment, after a short reaction delay.
    pub(crate) fn traffic_collision(&self, sender: &EntryCar, target: u8) {
        let Some(traffic) = &self.traffic else { return };
        let Some(from) = sender.last_position() else {
            return;
        };
        let now = self.server_time_millis();
        let mut rng = rand::rng();
        let mut inner = traffic.lock();
        let Some(slot) = inner.slots.get_mut(usize::from(target)) else {
            return;
        };
        if !slot.controlled {
            return;
        }
        let tuning = slot.tuning.clone();
        let nearest = slot
            .states
            .iter_mut()
            .filter(|s| s.initialized)
            .map(|s| (s.status.position.distance_squared(from.position), s))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((d2, state)) = nearest
            && d2 < COLLISION_RADIUS * COLLISION_RADIUS
        {
            let delay = rng.random_range(100..500);
            state.stop_for_collision(&traffic.params(), &tuning, now + delay, &mut rng);
        }
    }

    /// Starts the obstacle and spawner loops.
    pub(crate) fn start_traffic(self: &Arc<Self>) {
        let Some(traffic) = &self.traffic else { return };
        self.traffic_init_slots();
        let server = Arc::clone(self);
        self.spawn_ticker(
            std::time::Duration::from_millis(OBSTACLE_INTERVAL_MS),
            move || server.traffic_obstacles(),
        );
        let hz = traffic.params().ai_behavior_update_interval_hz.max(1) as u64;
        let server = Arc::clone(self);
        self.spawn_ticker(std::time::Duration::from_millis(1000 / hz), move || {
            server.traffic_spawn()
        });
    }
}

/// Where to spawn for `player`: a random distance ahead (or behind, if
/// they drive against the lane), on a random lane, moved on until no car
/// or player is too close.
fn spawn_point(
    traffic: &Traffic,
    inner: &TrafficInner,
    choices: &mut JunctionChoices,
    player: &PlayerView,
    players: &[PlayerView],
    rng: &mut impl Rng,
) -> Option<u32> {
    let spline = &traffic.spline;
    let params = traffic.params();
    let (nearest, d2) = spline.world_to_spline(player.position)?;
    if spline.point(nearest).next.is_none()
        || d2 > params.max_player_distance_to_ai_spline_meters.powi(2)
    {
        return None;
    }
    let mut direction = if spline.forward(nearest).dot(player.velocity) > 0.0 {
        1
    } else {
        -1
    };
    let distance =
        rng.random_range(params.min_spawn_distance_points..=params.max_spawn_distance_points);
    let mut point = choices.traverse(spline, nearest, distance * direction, rng)?;
    point = random_lane(spline, point, rng);
    if spline.point(point).next.is_some() {
        direction = if spline.forward(point).dot(player.velocity) > 0.0 {
            1
        } else {
            -1
        };
    }
    let mut steps = 0;
    while !position_safe(traffic, inner, players, point) {
        point = choices.traverse(spline, point, direction * 5, rng)?;
        steps += 1;
        if steps > 1000 {
            return None;
        }
    }
    Some(random_lane(spline, point, rng))
}

/// The first AI car within 50 m along the road from `point`.
fn nearest_on_road(
    traffic: &Traffic,
    inner: &TrafficInner,
    choices: &mut JunctionChoices,
    point: u32,
    forward: bool,
    rng: &mut impl Rng,
) -> Option<StateRef> {
    let spline = &traffic.spline;
    let mut travelled = 0.0;
    let mut at = point;
    while travelled < SPAWN_NEIGHBOUR_SEARCH {
        travelled += spline.point(at).length;
        at = if forward {
            choices.next(spline, at, 1, rng)?
        } else {
            choices.previous(spline, at, 1, rng)?
        };
        if let Some((r, _)) = inner.slowest_at(at) {
            return Some(r);
        }
    }
    None
}

/// Whether free state `r` may spawn at `point`: within its slot's
/// target, far from its slot's other cars, and keeping both its own and
/// its neighbours' gaps.
fn can_spawn(
    traffic: &Traffic,
    inner: &TrafficInner,
    r: StateRef,
    point: u32,
    behind: Option<StateRef>,
    ahead: Option<StateRef>,
) -> bool {
    let slot = &inner.slots[usize::from(r.slot)];
    if usize::from(r.index) >= slot.target_count
        || !slot.tuning.allows_spawn_at(&traffic.spline, point)
    {
        return false;
    }
    let Some(me) = slot.states.get(usize::from(r.index)) else {
        return false;
    };
    let at = traffic.spline.point(point).position;
    let spawn_d2 = traffic.params().state_spawn_distance_meters.powi(2);
    for (i, other) in slot.states.iter().enumerate() {
        if i != usize::from(r.index)
            && other.initialized
            && at.distance_squared(other.status.position) < spawn_d2
        {
            return false;
        }
    }
    let gap_ok = |n: Option<StateRef>, my_len: f32, their_len: fn(&CarTuning) -> f32| -> bool {
        let Some(n) = n.filter(|n| *n != r) else {
            return true;
        };
        let Some(other) = inner.state(n) else {
            return true;
        };
        let their = their_len(&inner.slots[usize::from(n.slot)].tuning);
        let gap = (at.distance_squared(other.status.position).sqrt() - their - my_len).max(0.0);
        let gap2 = gap * gap;
        gap2 >= other.safety_distance_squared && gap2 >= me.safety_distance_squared
    };
    gap_ok(behind, slot.tuning.vehicle_length_post, |t| {
        t.vehicle_length_pre
    }) && gap_ok(ahead, slot.tuning.vehicle_length_pre, |t| {
        t.vehicle_length_post
    })
}

impl PlayerView {
    fn velocity_length(&self) -> f32 {
        self.velocity.length()
    }
}

/// No AI car keeps its gap over `point`, and no player is near it.
fn position_safe(
    traffic: &Traffic,
    inner: &TrafficInner,
    players: &[PlayerView],
    point: u32,
) -> bool {
    let spline = &traffic.spline;
    let at = spline.point(point).position;
    for slot in &inner.slots {
        if !slot.controlled {
            continue;
        }
        for s in &slot.states {
            if s.initialized
                && s.status.position.distance_squared(at) < s.safety_distance_squared
                && spline.same_direction(s.point, point)
            {
                return false;
            }
        }
    }
    let safety = traffic
        .params()
        .spawn_safety_distance_to_player_meters
        .powi(2);
    players
        .iter()
        .all(|p| p.position.distance_squared(at) >= safety)
}

/// CSP driver options packed after a '/' in SKIN: base64 of [0, flags,
/// flags ^ 0x17]. Flag 0x10 allows colour changes.
pub fn allows_color_change(skin: &str) -> bool {
    use base64::Engine as _;
    let Some((_, packed)) = skin.rsplit_once('/') else {
        return false;
    };
    let Ok(bytes) =
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(packed.trim_end_matches('='))
    else {
        return false;
    };
    bytes.len() == 3 && bytes[0] == 0 && bytes[2] == bytes[1] ^ 0x17 && bytes[1] & 0x10 != 0
}

/// A random lane beside `point`, or `point` itself if it has none.
fn random_lane(spline: &AiSpline, point: u32, rng: &mut impl Rng) -> u32 {
    let lanes = spline.lanes_at(point);
    if lanes.is_empty() {
        point
    } else {
        lanes[rng.random_range(0..lanes.len())]
    }
}

/// The car of `states` a receiver at `receiver` should see: the nearest,
/// preferring one travelling its way among those within MinStateDistance
/// (AssettoServer's GetBestStateForPlayer).
fn best_state_for(
    states: &[AiState],
    receiver: &PositionUpdate,
    two_way: bool,
    min_state_d2: f32,
) -> Option<usize> {
    let mut best: Option<&AiState> = None;
    let mut best_index = None;
    let mut best_d2 = f32::MAX;
    let fast_enough = receiver.velocity.dot(receiver.velocity) > 1.0;
    for (index, state) in states.iter().enumerate().filter(|(_, s)| s.initialized) {
        let d2 = state.status.position.distance_squared(receiver.position);
        if two_way {
            if d2 < best_d2 {
                best = Some(state);
                best_index = Some(index);
                best_d2 = d2;
            }
            continue;
        }
        let best_same = best.is_some_and(|b| b.status.velocity.dot(receiver.velocity) > 0.0);
        let same = state.status.velocity.dot(receiver.velocity) > 0.0;
        let tie_break = best_d2 < min_state_d2 && d2 < min_state_d2 && fast_enough;
        if (tie_break && same && (d2 < best_d2 || !best_same)) || (!tie_break && d2 < best_d2) {
            best = Some(state);
            best_index = Some(index);
            best_d2 = d2;
        }
    }
    best_index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_options_in_skin() {
        use base64::Engine as _;
        let packed =
            base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8, 0x10, 0x10 ^ 0x17]);
        assert!(allows_color_change(&format!("00_white/{packed}")));
        let other =
            base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8, 0x20, 0x20 ^ 0x17]);
        assert!(!allows_color_change(&format!("00_white/{other}")));
        assert!(!allows_color_change("00_white"));
    }

    #[test]
    fn weighted_pick_stays_in_range() {
        let mut rng = rand::rng();
        for max in 1..20 {
            for _ in 0..200 {
                assert!(random_weighted(&mut rng, max) < max);
            }
        }
    }
}
