//! One AI car on the road: where it is on the spline, how fast it goes and
//! what it shows. The server moves it kinematically, along a Catmull-Rom
//! curve through the spline points, and every client renders it like any
//! other remote car. The driving rules follow AssettoServer's AiState.

use std::collections::HashMap;
use std::f32::consts::PI;

use rand::Rng;
use smart_default::SmartDefault;

use super::spline::AiSpline;
use crate::config::{AiParams, CarSpecificOverrides, LaneSpawnBehavior};
use crate::wire::Vec3;

pub const GRAVITY: f32 = 9.80665;

/// Status flags an AI car sets (docs/03 §3.3).
const BRAKE_LIGHTS: u32 = 0x10;
const LIGHTS: u32 = 0x20;
const HORN: u32 = 0x40;
const HAZARDS: u32 = 0x2000;
const HIGH_BEAMS_OFF: u32 = 0x4000;
const WIPER_1: u32 = 0x20_0000;
const WIPER_2: u32 = 0x40_0000;
const WIPER_3: u32 = WIPER_1 | WIPER_2;

/// Below this an AI car crawls rather than stopping for an obstacle.
const WALKING_SPEED: f32 = 10.0 / 3.6;

/// The distance needed to shed `speed` at `deceleration`.
pub fn braking_distance(speed: f32, deceleration: f32) -> f32 {
    speed * speed / (2.0 * deceleration)
}

/// The fastest speed (squared) through a corner of `radius`.
pub fn max_cornering_speed_squared(radius: f32, friction: f32) -> f32 {
    GRAVITY * friction * radius
}

/// How one AI slot's car drives: the AiParams defaults, overridden for its
/// model by CarSpecificOverrides.
#[derive(Debug, Clone, PartialEq)]
pub struct CarTuning {
    pub acceleration: f32,
    pub deceleration: f32,
    pub cornering_speed_factor: f32,
    pub cornering_brake_distance_factor: f32,
    pub cornering_brake_force_factor: f32,
    pub spline_height_offset: f32,
    pub idle_rpm: f32,
    pub max_rpm: f32,
    pub tyre_diameter: f32,
    pub max_overbooking: Option<usize>,
    pub min_spawn_protection_ms: i64,
    pub max_spawn_protection_ms: i64,
    pub min_collision_stop_ms: i64,
    pub max_collision_stop_ms: i64,
    pub vehicle_length_pre: f32,
    pub vehicle_length_post: f32,
    pub min_safety_distance: Option<i32>,
    pub max_safety_distance: Option<i32>,
    pub allowed_lanes: Option<Vec<LaneSpawnBehavior>>,
    pub min_lane_count: Option<usize>,
    pub max_lane_count: Option<usize>,
}

impl CarTuning {
    pub fn for_model(params: &AiParams, model: &str) -> CarTuning {
        let mut t = CarTuning {
            acceleration: params.default_acceleration,
            deceleration: params.default_deceleration,
            cornering_speed_factor: params.cornering_speed_factor,
            cornering_brake_distance_factor: params.cornering_brake_distance_factor,
            cornering_brake_force_factor: params.cornering_brake_force_factor,
            spline_height_offset: params.spline_height_offset_meters,
            idle_rpm: 800.0,
            max_rpm: 3000.0,
            tyre_diameter: params.tyre_diameter_meters,
            max_overbooking: None,
            min_spawn_protection_ms: i64::from(params.min_spawn_protection_time_seconds) * 1000,
            max_spawn_protection_ms: i64::from(params.max_spawn_protection_time_seconds) * 1000,
            min_collision_stop_ms: i64::from(params.min_collision_stop_time_seconds) * 1000,
            max_collision_stop_ms: i64::from(params.max_collision_stop_time_seconds) * 1000,
            vehicle_length_pre: 0.0,
            vehicle_length_post: 0.0,
            min_safety_distance: None,
            max_safety_distance: None,
            allowed_lanes: None,
            min_lane_count: None,
            max_lane_count: None,
        };
        let overrides = params
            .car_specific_overrides
            .iter()
            .filter(|o| o.model.eq_ignore_ascii_case(model));
        for o in overrides {
            t.apply(o);
        }
        t
    }

    fn apply(&mut self, o: &CarSpecificOverrides) {
        fn set<T: Copy>(field: &mut T, value: Option<T>) {
            if let Some(v) = value {
                *field = v;
            }
        }
        set(&mut self.acceleration, o.acceleration);
        set(&mut self.deceleration, o.deceleration);
        set(&mut self.cornering_speed_factor, o.cornering_speed_factor);
        set(
            &mut self.cornering_brake_distance_factor,
            o.cornering_brake_distance_factor,
        );
        set(
            &mut self.cornering_brake_force_factor,
            o.cornering_brake_force_factor,
        );
        set(
            &mut self.spline_height_offset,
            o.spline_height_offset_meters,
        );
        set(&mut self.idle_rpm, o.engine_idle_rpm.map(|v| v as f32));
        set(&mut self.max_rpm, o.engine_max_rpm.map(|v| v as f32));
        set(&mut self.tyre_diameter, o.tyre_diameter_meters);
        self.max_overbooking = o
            .max_overbooking
            .map(|v| v.max(0) as usize)
            .or(self.max_overbooking);
        set(
            &mut self.min_spawn_protection_ms,
            o.min_spawn_protection_time_seconds
                .map(|s| i64::from(s) * 1000),
        );
        set(
            &mut self.max_spawn_protection_ms,
            o.max_spawn_protection_time_seconds
                .map(|s| i64::from(s) * 1000),
        );
        set(
            &mut self.min_collision_stop_ms,
            o.min_collision_stop_time_seconds
                .map(|s| i64::from(s) * 1000),
        );
        set(
            &mut self.max_collision_stop_ms,
            o.max_collision_stop_time_seconds
                .map(|s| i64::from(s) * 1000),
        );
        set(&mut self.vehicle_length_pre, o.vehicle_length_pre_meters);
        set(&mut self.vehicle_length_post, o.vehicle_length_post_meters);
        self.min_safety_distance = o.min_ai_safety_distance_meters;
        self.max_safety_distance = o.max_ai_safety_distance_meters;
        self.allowed_lanes = o.allowed_lanes.clone();
        self.min_lane_count = o.min_lane_count.map(|n| n.max(0) as usize);
        self.max_lane_count = o.max_lane_count.map(|n| n.max(0) as usize);
    }

    /// Whether this car may spawn at `point`: its lane count is in range,
    /// and the lane (leftmost, middle or rightmost) is allowed.
    pub fn allows_spawn_at(&self, spline: &AiSpline, point: u32) -> bool {
        let lanes = spline.lanes_at(point).len();
        if self.min_lane_count.is_some_and(|min| lanes < min)
            || self.max_lane_count.is_some_and(|max| lanes > max)
        {
            return false;
        }
        let Some(allowed) = &self.allowed_lanes else {
            return true;
        };
        let p = spline.point(point);
        (allowed.contains(&LaneSpawnBehavior::Middle) && p.left.is_some() && p.right.is_some())
            || (allowed.contains(&LaneSpawnBehavior::Left) && p.left.is_none())
            || (allowed.contains(&LaneSpawnBehavior::Right) && p.right.is_none())
    }
}

/// Where junctions lead. A car remembers each choice so its lookahead and
/// its driving agree; the spawner's lookups choose afresh each time.
#[derive(Debug, Default)]
pub struct JunctionChoices {
    taken: Option<HashMap<u32, bool>>,
}

impl JunctionChoices {
    pub fn remembering() -> JunctionChoices {
        JunctionChoices {
            taken: Some(HashMap::new()),
        }
    }

    pub fn forgetful() -> JunctionChoices {
        JunctionChoices { taken: None }
    }

    pub fn clear(&mut self) {
        if let Some(taken) = &mut self.taken {
            taken.clear();
        }
    }

    pub fn will_take(&mut self, spline: &AiSpline, junction: u32, rng: &mut impl Rng) -> bool {
        let roll = rng.random::<f64>() < f64::from(spline.junctions[junction as usize].probability);
        match &mut self.taken {
            Some(taken) => *taken.entry(junction).or_insert(roll),
            None => roll,
        }
    }

    /// `count` points forward from `id`, through any junction it takes.
    pub fn next(
        &mut self,
        spline: &AiSpline,
        id: u32,
        count: u32,
        rng: &mut impl Rng,
    ) -> Option<u32> {
        let mut at = id;
        for _ in 0..count {
            let p = spline.point(at);
            at = match p.junction_start {
                Some(j) if self.will_take(spline, j, rng) => spline.junctions[j as usize].end,
                _ => p.next?,
            };
        }
        Some(at)
    }

    /// `count` points back from `id`. A junction is followed backwards only
    /// if it was taken, or where the lane has no point behind it.
    pub fn previous(
        &mut self,
        spline: &AiSpline,
        id: u32,
        count: u32,
        rng: &mut impl Rng,
    ) -> Option<u32> {
        let mut at = id;
        for _ in 0..count {
            let p = spline.point(at);
            at = match p.junction_end {
                Some(j) => {
                    let junction = &spline.junctions[j as usize];
                    let known = self.taken.as_ref().and_then(|t| t.get(&j).copied());
                    let taken = match known {
                        Some(taken) => taken,
                        None if p.previous.is_none() => {
                            let roll = rng.random::<f64>() < f64::from(junction.probability);
                            if let Some(t) = &mut self.taken {
                                t.insert(j, roll);
                            }
                            roll
                        }
                        None => false,
                    };
                    if taken { junction.start } else { p.previous? }
                }
                None => p.previous?,
            };
        }
        Some(at)
    }

    /// Forward for positive `count`, back for negative.
    pub fn traverse(
        &mut self,
        spline: &AiSpline,
        id: u32,
        count: i32,
        rng: &mut impl Rng,
    ) -> Option<u32> {
        if count >= 0 {
            self.next(spline, id, count as u32, rng)
        } else {
            self.previous(spline, id, count.unsigned_abs(), rng)
        }
    }
}

/// What an AI car shows: the fields of its 0x46.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AiStatus {
    pub position: Vec3,
    pub rotation: Vec3,
    pub velocity: Vec3,
    /// Encoded as in 0x46 (value + 100).
    pub tyre_angular_speed: u8,
    pub engine_rpm: u16,
    pub status_flags: u32,
    /// Server time of the update, ms.
    pub timestamp: i64,
}

impl AiStatus {
    /// Heading in degrees, 0..360.
    pub fn heading_degrees(&self) -> f32 {
        let angle = self.rotation.x.to_degrees();
        if angle < 0.0 { angle + 360.0 } else { angle }
    }
}

/// Another car an AI car must not run into, as the obstacle check sees it.
#[derive(Debug, Clone, Copy)]
pub struct Obstacle {
    pub position: Vec3,
    pub speed: f32,
    pub target_speed: f32,
    pub length_post: f32,
}

/// What `AiState::detect_obstacles` needs to know about the world.
pub struct World<'a> {
    pub spline: &'a AiSpline,
    pub params: &'a AiParams,
    pub tuning: &'a CarTuning,
    /// Server time, ms.
    pub now: i64,
    /// The slowest AI car at each spline point, if any.
    pub slowest_at: &'a dyn Fn(u32) -> Option<Obstacle>,
    /// Players in the world.
    pub players: &'a [Obstacle],
}

/// One AI car.
#[derive(Debug, SmartDefault)]
pub struct AiState {
    pub initialized: bool,
    pub point: u32,
    pub status: AiStatus,
    pub spawn_protection_ends: i64,
    #[default(20.0 * 20.0)]
    pub safety_distance_squared: f32,
    pub acceleration: f32,
    pub current_speed: f32,
    pub target_speed: f32,
    initial_max_speed: f32,
    pub max_speed: f32,
    /// Counts spawns, so a receiver can tell a respawned car.
    pub spawn_counter: u8,
    /// The paint picked at spawn, RGBA (sent where colour changes are on).
    #[default(CAR_COLORS[0])]
    pub color: [u8; 4],
    /// The gap to the AI car ahead at the last obstacle check, -1 for none
    /// (AiParams.Debug).
    #[default(-1.0)]
    pub closest_obstacle_distance: f32,
    #[default(JunctionChoices::remembering())]
    junctions: JunctionChoices,
    start_tangent: Vec3,
    end_tangent: Vec3,
    #[default(1.0)]
    segment_length: f32,
    segment_progress: f32,
    last_tick: i64,
    stopped_for_obstacle: bool,
    stopped_since: i64,
    ignore_obstacles_until: i64,
    /// A collision stop runs from `stopped_for_collision_from` until
    /// `stopped_for_collision_until`.
    stopped_for_collision_from: i64,
    stopped_for_collision_until: i64,
    honk_start: i64,
    honk_end: i64,
    indicator: u32,
    next_junction: Option<u32>,
    junction_passed: bool,
    end_indicator_distance: f32,
    #[default(10.0)]
    min_obstacle_distance: f32,
}

/// Everyday car paints, one picked per spawn (AssettoServer's list).
const CAR_COLORS: [[u8; 4]; 18] = [
    [13, 17, 22, 255],
    [19, 24, 31, 255],
    [28, 29, 33, 255],
    [12, 13, 24, 255],
    [11, 20, 33, 255],
    [151, 154, 151, 255],
    [153, 157, 160, 255],
    [194, 196, 198, 255],
    [234, 234, 234, 255],
    [255, 255, 255, 255],
    [182, 17, 27, 255],
    [218, 25, 24, 255],
    [73, 17, 29, 255],
    [35, 49, 85, 255],
    [28, 53, 81, 255],
    [37, 58, 167, 255],
    [21, 92, 45, 255],
    [18, 46, 43, 255],
];

/// Whether `at` lies in one of IgnorePlayerObstacleSpheres.
fn ignores_players(params: &AiParams, at: Vec3) -> bool {
    params.ignore_player_obstacle_spheres.iter().any(|s| {
        let c = Vec3::new(s.center.x, s.center.y, s.center.z);
        at.distance_squared(c) < s.radius_meters * s.radius_meters
    })
}

/// A Hermite point and its unit tangent `t` of the way from `p0` to `p1`.
fn catmull_rom(p0: Vec3, p1: Vec3, m0: Vec3, m1: Vec3, t: f32) -> (Vec3, Vec3) {
    let t2 = t * t;
    let t3 = t2 * t;
    let position = p0 * (2.0 * t3 - 3.0 * t2 + 1.0)
        + m0 * (t3 - 2.0 * t2 + t)
        + p1 * (-2.0 * t3 + 3.0 * t2)
        + m1 * (t3 - t2);
    let tangent = p0 * (6.0 * t2 - 6.0 * t)
        + m0 * (3.0 * t2 - 4.0 * t + 1.0)
        + p1 * (-6.0 * t2 + 6.0 * t)
        + m1 * (3.0 * t2 - 2.0 * t);
    (position, tangent.normalize_or_zero())
}

/// A wheel's spin rate for `speed`, encoded as in 0x46.
fn encode_tyre_speed(speed: f32, wheel_diameter: f32) -> u8 {
    let spin = speed / (PI * wheel_diameter) * 6.0;
    let encoded = ((spin + 1.0).log10() * 20.0).round() * spin.signum();
    (encoded.clamp(-100.0, 154.0) + 100.0) as u8
}

fn wipers(rain_intensity: f32) -> u32 {
    if rain_intensity < 0.05 {
        0
    } else if rain_intensity < 0.25 {
        WIPER_1
    } else if rain_intensity < 0.5 {
        WIPER_2
    } else {
        WIPER_3
    }
}

/// A random integer in [min, max), or `min` for an empty range.
fn random_between(rng: &mut impl Rng, min: i64, max: i64) -> i64 {
    if max > min {
        rng.random_range(min..max)
    } else {
        min
    }
}

impl AiState {
    /// Places the car at `point` with a fresh speed, gap and protection, and
    /// computes its first status. Returns false if the lane ends there.
    pub fn teleport(&mut self, world: &World, point: u32, rng: &mut impl Rng) -> bool {
        self.junctions.clear();
        self.point = point;
        let Some(next) = self.junctions.next(world.spline, point, 1, rng) else {
            return false;
        };
        self.segment_length = world
            .spline
            .point(next)
            .position
            .distance(world.spline.point(point).position);
        self.segment_progress = 0.0;
        self.calculate_tangents(world.spline, rng);

        let params = world.params;
        let max_speed = params.max_speed_kph / 3.6;
        let variation = max_speed * params.max_speed_variation_percent;
        let fast_lane = if world.spline.point(point).left.is_some() {
            params.right_lane_offset_kph / 3.6
        } else {
            0.0
        };
        self.initial_max_speed =
            max_speed + fast_lane - variation / 2.0 + rng.random::<f32>() * variation;
        self.current_speed = self.initial_max_speed;
        self.target_speed = self.initial_max_speed;
        self.max_speed = self.initial_max_speed;

        let (mut min, mut max) = (
            params.min_ai_safety_distance_meters,
            params.max_ai_safety_distance_meters,
        );
        let lane_count = world.spline.lanes_at(point).len() as i32;
        if let Some(o) = params.lane_count_specific_overrides.get(&lane_count) {
            min = o.min_ai_safety_distance_meters;
            max = o.max_ai_safety_distance_meters;
        }
        let min = world.tuning.min_safety_distance.unwrap_or(min);
        let max = world.tuning.max_safety_distance.unwrap_or(max);
        let scale = 1.0 / params.traffic_density;
        self.safety_distance_squared = random_between(
            rng,
            (f64::from(min * min) * f64::from(scale)).round() as i64,
            (f64::from(max * max) * f64::from(scale)).round() as i64,
        ) as f32;
        self.spawn_protection_ends = world.now
            + random_between(
                rng,
                world.tuning.min_spawn_protection_ms,
                world.tuning.max_spawn_protection_ms,
            );
        self.stopped_for_collision_from = 0;
        self.stopped_for_collision_until = 0;
        self.ignore_obstacles_until = 0;
        self.honk_start = 0;
        self.honk_end = 0;
        self.indicator = 0;
        self.next_junction = None;
        self.junction_passed = false;
        self.end_indicator_distance = 0.0;
        self.last_tick = world.now;
        self.min_obstacle_distance = rng.random_range(8..13) as f32;
        self.color = CAR_COLORS[rng.random_range(0..CAR_COLORS.len())];
        self.spawn_counter = self.spawn_counter.wrapping_add(1);
        self.initialized = true;
        self.update(world, 0.0, rng)
    }

    fn calculate_tangents(&mut self, spline: &AiSpline, rng: &mut impl Rng) {
        let here = spline.point(self.point).position;
        let Some(next) = self.junctions.next(spline, self.point, 1, rng) else {
            return;
        };
        let next_pos = spline.point(next).position;
        self.start_tangent = match self.junctions.previous(spline, self.point, 1, rng) {
            Some(prev) => (next_pos - spline.point(prev).position) * 0.5,
            None => (next_pos - here) * 0.5,
        };
        self.end_tangent = match self.junctions.next(spline, self.point, 2, rng) {
            Some(next_next) => (spline.point(next_next).position - here) * 0.5,
            None => (next_pos - here) * 0.5,
        };
    }

    /// Moves `progress` metres along the current segment, onto later ones as
    /// needed. False at the end of the road.
    fn advance(&mut self, spline: &AiSpline, progress: f32, rng: &mut impl Rng) -> bool {
        let mut progress = progress;
        let mut moved = false;
        while progress > self.segment_length {
            progress -= self.segment_length;
            let Some(next) = self.junctions.next(spline, self.point, 1, rng) else {
                return false;
            };
            let Some(next_next) = self.junctions.next(spline, next, 1, rng) else {
                return false;
            };
            self.point = next;
            self.segment_length = spline
                .point(next_next)
                .position
                .distance(spline.point(next).position);
            moved = true;
            if self.junction_passed {
                self.end_indicator_distance -= self.segment_length;
                if self.end_indicator_distance < 0.0 {
                    self.indicator = 0;
                    self.junction_passed = false;
                    self.end_indicator_distance = 0.0;
                }
            }
            if let Some(j) = self.next_junction
                && spline.point(self.point).junction_end == Some(j)
            {
                self.junction_passed = true;
                self.end_indicator_distance = spline.junctions[j as usize].indicate_distance_post;
                self.next_junction = None;
            }
        }
        if moved {
            self.calculate_tangents(spline, rng);
        }
        self.segment_progress = progress;
        true
    }

    /// Advances the car to `world.now` and recomputes its status. False when
    /// it has reached the end of the road and must despawn.
    pub fn update(&mut self, world: &World, rain_intensity: f32, rng: &mut impl Rng) -> bool {
        if !self.initialized {
            return true;
        }
        let dt = (world.now - self.last_tick) as f32 / 1000.0;
        self.last_tick = world.now;
        if self.acceleration != 0.0 {
            self.current_speed += self.acceleration * dt;
            if (self.acceleration < 0.0 && self.current_speed < self.target_speed)
                || (self.acceleration > 0.0 && self.current_speed > self.target_speed)
            {
                self.current_speed = self.target_speed;
                self.acceleration = 0.0;
            }
        }
        let spline = world.spline;
        if !self.advance(spline, self.segment_progress + dt * self.current_speed, rng) {
            return false;
        }
        let Some(next) = self.junctions.next(spline, self.point, 1, rng) else {
            return false;
        };
        let t = if self.segment_length > 0.0 {
            self.segment_progress / self.segment_length
        } else {
            0.0
        };
        let (position, tangent) = catmull_rom(
            spline.point(self.point).position,
            spline.point(next).position,
            self.start_tangent,
            self.end_tangent,
            t,
        );
        let flat = (tangent.z * tangent.z + tangent.x * tangent.x).sqrt();
        let tuning = world.tuning;
        let now = world.now;
        let honking = self.stopped_for_obstacle && now > self.honk_start && now < self.honk_end;
        let max_speed = world.params.max_speed_kph / 3.6;
        let rpm =
            tuning.idle_rpm + (tuning.max_rpm - tuning.idle_rpm) * (self.current_speed / max_speed);
        let mut flags = LIGHTS | HIGH_BEAMS_OFF | wipers(rain_intensity) | self.indicator;
        if self.stopped_for_collision(now) || self.current_speed < 20.0 / 3.6 {
            flags |= HAZARDS;
        }
        if self.current_speed == 0.0 || self.acceleration < 0.0 {
            flags |= BRAKE_LIGHTS;
        }
        if honking {
            flags |= HORN;
        }
        self.status = AiStatus {
            position: Vec3::new(
                position.x,
                position.y + tuning.spline_height_offset,
                position.z,
            ),
            rotation: Vec3::new(
                tangent.z.atan2(tangent.x) - PI / 2.0,
                -(flat.atan2(tangent.y) - PI / 2.0),
                spline.camber(self.point, t),
            ),
            velocity: tangent * self.current_speed,
            tyre_angular_speed: encode_tyre_speed(self.current_speed, tuning.tyre_diameter),
            engine_rpm: rpm.clamp(0.0, f32::from(u16::MAX)) as u16,
            status_flags: flags,
            timestamp: now,
        };
        true
    }

    /// Looks ahead along the road for corners, junctions and the nearest AI
    /// car: the closest one, its gap, and the speed the corners allow.
    fn look_ahead(&mut self, world: &World, rng: &mut impl Rng) -> (Option<Obstacle>, f32, f32) {
        let spline = world.spline;
        let tuning = world.tuning;
        let max_distance = braking_distance(self.current_speed, tuning.deceleration) * 2.0 + 20.0;
        let mut closest: Option<Obstacle> = None;
        let mut closest_distance = f32::MAX;
        let mut junction_found = false;
        let mut travelled = 0.0;
        let mut at = self.point;
        let mut max_speed = f32::MAX;
        let speed_squared = self.current_speed * self.current_speed;
        while travelled < max_distance {
            travelled += spline.point(at).length;
            let Some(next) = self.junctions.next(spline, at, 1, rng) else {
                break;
            };
            at = next;
            let point = spline.point(at);
            if !junction_found
                && let Some(j) = point.junction_start
                && travelled < spline.junctions[j as usize].indicate_distance_pre
            {
                let junction = &spline.junctions[j as usize];
                let indicator = if self.junctions.will_take(spline, j, rng) {
                    junction.indicate_when_taken
                } else {
                    junction.indicate_when_not_taken
                };
                if indicator != 0 {
                    self.indicator = indicator;
                    self.next_junction = Some(j);
                    junction_found = true;
                }
            }
            if closest.is_none()
                && let Some(other) = (world.slowest_at)(at)
            {
                closest_distance = (self.status.position.distance(other.position)
                    - tuning.vehicle_length_pre
                    - other.length_post)
                    .max(0.0);
                closest = Some(other);
            }
            let corner = max_cornering_speed_squared(point.radius, tuning.cornering_speed_factor);
            if corner < speed_squared {
                let corner_speed = corner.sqrt();
                let brake = braking_distance(
                    self.current_speed - corner_speed,
                    tuning.deceleration * tuning.cornering_brake_force_factor,
                ) * tuning.cornering_brake_distance_factor;
                if brake > travelled {
                    max_speed = max_speed.min(corner_speed);
                }
            }
        }
        (closest, closest_distance, max_speed)
    }

    /// The nearest player in front, within a 28° cone.
    fn player_ahead(&self, world: &World) -> Option<(Obstacle, f32)> {
        if ignores_players(world.params, self.status.position) {
            return None;
        }
        let mut best: Option<(Obstacle, f32)> = None;
        for player in world.players {
            let d2 = player.position.distance_squared(self.status.position);
            let angle = self.angle_to(player.position);
            if angle > 166.0 && angle < 194.0 && best.is_none_or(|(_, b)| d2 < b) {
                best = Some((*player, d2));
            }
        }
        best.map(|(p, d2)| (p, d2.sqrt()))
    }

    /// The bearing of `target` relative to this car's heading, degrees.
    fn angle_to(&self, target: Vec3) -> f32 {
        let p = self.status.position;
        let mut angle = (p.x - target.x).atan2(p.z - target.z).to_degrees();
        if angle < 0.0 {
            angle += 360.0;
        }
        (angle + self.status.heading_degrees()) % 360.0
    }

    fn set_target_speed(&mut self, speed: f32, deceleration: f32, acceleration: f32) {
        self.target_speed = speed;
        self.acceleration = if speed < self.current_speed {
            -deceleration
        } else if speed > self.current_speed {
            acceleration
        } else {
            0.0
        };
    }

    /// Picks a target speed from what is ahead: stop for something very
    /// close, match a slower car or player within braking range, slow for
    /// corners. A car stuck for `IgnoreObstaclesAfterSeconds` drives on
    /// regardless for 10 s.
    pub fn detect_obstacles(&mut self, world: &World, rng: &mut impl Rng) {
        if !self.initialized {
            return;
        }
        let tuning = world.tuning;
        let now = world.now;
        if now < self.ignore_obstacles_until {
            self.set_target_speed(self.max_speed, tuning.deceleration, tuning.acceleration);
            return;
        }
        if self.stopped_for_collision(now) {
            self.set_target_speed(0.0, tuning.deceleration, tuning.acceleration);
            return;
        }
        let mut target = self.initial_max_speed;
        let mut has_obstacle = false;
        let (ai_ahead, ai_distance, corner_limit) = self.look_ahead(world, rng);
        let player = self.player_ahead(world);
        let player_distance = player.map_or(f32::MAX, |(_, d)| d);
        self.closest_obstacle_distance = if ai_ahead.is_some() {
            ai_distance
        } else {
            -1.0
        };

        if player_distance < self.min_obstacle_distance || ai_distance < self.min_obstacle_distance
        {
            target = 0.0;
            has_obstacle = true;
        } else if player_distance < ai_distance
            && let Some((p, _)) = player
        {
            let mut speed = p.speed;
            if speed < 0.1 {
                speed = 0.0;
            }
            if (speed < self.current_speed || speed == 0.0)
                && player_distance
                    < braking_distance(self.current_speed - speed, tuning.deceleration) * 2.0 + 20.0
            {
                target = WALKING_SPEED.max(speed);
                has_obstacle = true;
            }
        } else if let Some(other) = ai_ahead {
            let speed = other.speed.min(other.target_speed);
            if (speed < self.current_speed || other.speed == 0.0)
                && ai_distance
                    < braking_distance(self.current_speed - speed, tuning.deceleration) * 2.0 + 20.0
            {
                target = WALKING_SPEED.max(speed);
                has_obstacle = true;
            }
        }
        target = target.min(corner_limit);

        if self.current_speed == 0.0 && !self.stopped_for_obstacle {
            self.stopped_for_obstacle = true;
            self.stopped_since = now;
            self.honk_start = now + rng.random_range(3000..7000);
            self.honk_end = self.honk_start + rng.random_range(500..1500);
        } else if self.current_speed > 0.0 && self.stopped_for_obstacle {
            self.stopped_for_obstacle = false;
        } else if self.stopped_for_obstacle
            && now - self.stopped_since
                > i64::from(world.params.ignore_obstacles_after_seconds) * 1000
        {
            self.ignore_obstacles_until = now + 10_000;
        }

        let mut deceleration = tuning.deceleration;
        if !has_obstacle {
            deceleration *= tuning.cornering_brake_force_factor;
        }
        self.max_speed = self.initial_max_speed;
        self.set_target_speed(target, deceleration, tuning.acceleration);
    }

    /// Stops for a while from `start` after a player hit it, unless it is
    /// in an IgnorePlayerObstacleSpheres sphere.
    pub fn stop_for_collision(
        &mut self,
        params: &AiParams,
        tuning: &CarTuning,
        start: i64,
        rng: &mut impl Rng,
    ) {
        if ignores_players(params, self.status.position) {
            return;
        }
        self.stopped_for_collision_from = start;
        self.stopped_for_collision_until = start
            + random_between(
                rng,
                tuning.min_collision_stop_ms,
                tuning.max_collision_stop_ms,
            );
    }

    fn stopped_for_collision(&self, now: i64) -> bool {
        now >= self.stopped_for_collision_from && now < self.stopped_for_collision_until
    }

    /// What another car's obstacle check sees of this one.
    pub fn as_obstacle(&self, tuning: &CarTuning) -> Obstacle {
        Obstacle {
            position: self.status.position,
            speed: self.current_speed,
            target_speed: self.target_speed,
            length_post: tuning.vehicle_length_post,
        }
    }

    pub fn speed(&self) -> f32 {
        self.status.velocity.length()
    }
}

#[cfg(test)]
mod tests {
    use super::super::spline::tests::two_lane_road;
    use super::*;

    fn world<'a>(
        spline: &'a AiSpline,
        params: &'a AiParams,
        tuning: &'a CarTuning,
        now: i64,
        players: &'a [Obstacle],
    ) -> World<'a> {
        World {
            spline,
            params,
            tuning,
            now,
            slowest_at: &|_| None,
            players,
        }
    }

    #[test]
    fn a_car_drives_along_its_lane() {
        let spline = two_lane_road();
        let params = AiParams::default();
        let tuning = CarTuning::for_model(&params, "car");
        let mut rng = rand::rng();
        let mut car = AiState::default();
        assert!(car.teleport(&world(&spline, &params, &tuning, 0, &[]), 10, &mut rng));
        let start = car.status.position;
        assert!(car.update(&world(&spline, &params, &tuning, 1000, &[]), 0.0, &mut rng));
        let moved = car.status.position.x - start.x;
        assert!(
            (moved - car.current_speed).abs() < 0.5,
            "moved {moved} at {}",
            car.current_speed
        );
        // Heading +x: AC's yaw for +x is -pi/2.
        assert!((car.status.rotation.x + PI / 2.0).abs() < 1e-3);
        assert_eq!(car.status.status_flags & LIGHTS, LIGHTS);
    }

    #[test]
    fn a_car_stops_for_a_player_ahead() {
        let spline = two_lane_road();
        let params = AiParams::default();
        let tuning = CarTuning::for_model(&params, "car");
        let mut rng = rand::rng();
        let mut car = AiState::default();
        assert!(car.teleport(&world(&spline, &params, &tuning, 0, &[]), 10, &mut rng));
        let ahead = car.status.position + Vec3::new(5.0, 0.0, 0.0);
        let players = [Obstacle {
            position: ahead,
            speed: 0.0,
            target_speed: 0.0,
            length_post: 0.0,
        }];
        car.detect_obstacles(&world(&spline, &params, &tuning, 0, &players), &mut rng);
        assert_eq!(car.target_speed, 0.0);
        assert!(car.acceleration < 0.0);
    }

    #[test]
    fn a_car_ends_with_its_lane() {
        let spline = two_lane_road();
        let params = AiParams::default();
        let tuning = CarTuning::for_model(&params, "car");
        let mut rng = rand::rng();
        let mut car = AiState::default();
        assert!(car.teleport(&world(&spline, &params, &tuning, 0, &[]), 190, &mut rng));
        // 400 m of road at ~25 m/s: gone well within a minute.
        assert!(!car.update(
            &world(&spline, &params, &tuning, 60_000, &[]),
            0.0,
            &mut rng
        ));
    }

    #[test]
    fn lane_rules_limit_spawns() {
        let spline = two_lane_road();
        let mut tuning = CarTuning::for_model(&AiParams::default(), "car");
        assert!(tuning.allows_spawn_at(&spline, 50));
        tuning.min_lane_count = Some(3);
        assert!(!tuning.allows_spawn_at(&spline, 50), "a two-lane road");
        tuning.min_lane_count = None;
        let a = spline.point(50);
        let edge = if a.left.is_none() {
            LaneSpawnBehavior::Left
        } else {
            LaneSpawnBehavior::Right
        };
        tuning.allowed_lanes = Some(vec![edge]);
        assert!(tuning.allows_spawn_at(&spline, 50));
        tuning.allowed_lanes = Some(vec![LaneSpawnBehavior::Middle]);
        assert!(
            !tuning.allows_spawn_at(&spline, 50),
            "two lanes have no middle"
        );
    }

    #[test]
    fn spheres_make_cars_ignore_players() {
        let spline = two_lane_road();
        let mut params = AiParams::default();
        let tuning = CarTuning::for_model(&params, "car");
        let mut rng = rand::rng();
        let mut car = AiState::default();
        assert!(car.teleport(&world(&spline, &params, &tuning, 0, &[]), 10, &mut rng));
        let p = car.status.position;
        params
            .ignore_player_obstacle_spheres
            .push(crate::config::Sphere {
                center: crate::config::Point {
                    x: p.x,
                    y: p.y,
                    z: p.z,
                },
                radius_meters: 50.0,
            });
        let players = [Obstacle {
            position: p + Vec3::new(5.0, 0.0, 0.0),
            speed: 0.0,
            target_speed: 0.0,
            length_post: 0.0,
        }];
        car.detect_obstacles(&world(&spline, &params, &tuning, 0, &players), &mut rng);
        assert!(car.target_speed > 0.0, "the player is ignored");
        car.stop_for_collision(&params, &tuning, 0, &mut rng);
        assert!(!car.stopped_for_collision(1));
    }

    /// Drives cars over a real network (AI_SPLINE_DIR, AI_SPLINE_TRACK) and
    /// reports any jump between updates.
    #[test]
    fn real_spline_drives_smoothly() {
        let Ok(dir) = std::env::var("AI_SPLINE_DIR") else {
            return;
        };
        let track = std::env::var("AI_SPLINE_TRACK").unwrap_or_default();
        let options = crate::ai::BuildOptions {
            lane_width: 3.0,
            two_way_traffic: false,
            smooth_camber: false,
        };
        let spline = AiSpline::load(std::path::Path::new(&dir), &track, options).expect("load");
        let params = AiParams::default();
        let tuning = CarTuning::for_model(&params, "car");
        let mut rng = rand::rng();
        let mut worst = 0.0f32;
        let mut jumps = 0;
        for start in (0..spline.points.len() as u32).step_by(5000) {
            let mut car = AiState::default();
            if !car.teleport(&world(&spline, &params, &tuning, 0, &[]), start, &mut rng) {
                continue;
            }
            let mut last = car.status.position;
            for tick in 1..=2400 {
                let before = car.point;
                if !car.update(
                    &world(&spline, &params, &tuning, tick * 50, &[]),
                    0.0,
                    &mut rng,
                ) {
                    break;
                }
                let step = last.distance(car.status.position);
                worst = worst.max(step);
                if step > 5.0 {
                    jumps += 1;
                    let p = spline.point(before);
                    println!(
                        "jump {step:.1} m: point {before} -> {} (junction start {:?}, next {:?})",
                        car.point, p.junction_start, p.next
                    );
                }
                last = car.status.position;
            }
        }
        println!("worst step {worst:.2} m, jumps {jumps}");
    }

    #[test]
    fn tyre_speed_encoding() {
        assert_eq!(encode_tyre_speed(0.0, 0.65), 100);
        assert!(encode_tyre_speed(25.0, 0.65) > 100);
    }

    #[test]
    fn overrides_apply_by_model() {
        let mut params = AiParams::default();
        params.car_specific_overrides.push(CarSpecificOverrides {
            model: "truck".into(),
            acceleration: Some(1.0),
            max_overbooking: Some(2),
            ..CarSpecificOverrides::default()
        });
        assert_eq!(CarTuning::for_model(&params, "truck").acceleration, 1.0);
        assert_eq!(
            CarTuning::for_model(&params, "truck").max_overbooking,
            Some(2)
        );
        assert_eq!(CarTuning::for_model(&params, "car").acceleration, 2.5);
    }
}
