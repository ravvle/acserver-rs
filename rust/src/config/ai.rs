//! `AiParams` in `extra_cfg.yml`: how AI traffic drives (server/ai). The keys
//! and defaults are AssettoServer's, so an AssettoServer `AiParams` block
//! works unchanged; keys this server does not implement are ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;

/// Tuning for AI traffic. Distances are metres, speeds km/h unless named
/// otherwise, times seconds.
#[derive(Debug, Clone, PartialEq, SmartDefault, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AiParams {
    /// AI cars within this radius of a player (offset ahead by
    /// `PlayerPositionOffsetMeters`) are never despawned.
    #[default = 200.0]
    pub player_radius_meters: f32,
    #[default = 100.0]
    pub player_position_offset_meters: f32,
    /// A player who has not moved for this long spawns no traffic.
    #[default = 10]
    pub player_afk_timeout_seconds: i64,
    /// A player further than this from every lane spawns no traffic.
    #[default = 7.0]
    pub max_player_distance_to_ai_spline_meters: f32,
    /// Spawn points are this many spline points ahead of (or behind) a player.
    #[default = 100]
    pub min_spawn_distance_points: i32,
    #[default = 400]
    pub max_spawn_distance_points: i32,
    /// The gap an AI car keeps to the car ahead, picked per spawn.
    #[default = 20]
    pub min_ai_safety_distance_meters: i32,
    #[default = 70]
    pub max_ai_safety_distance_meters: i32,
    /// Two states of one slot spawn at least this far apart...
    #[default = 1000.0]
    pub state_spawn_distance_meters: f32,
    /// ...and one is despawned once they come closer than this.
    #[default = 200.0]
    pub min_state_distance_meters: f32,
    /// No car spawns closer than this to a player.
    #[default = 150.0]
    pub spawn_safety_distance_to_player_meters: f32,
    /// A freshly spawned car cannot despawn for this long.
    #[default = 4]
    pub min_spawn_protection_time_seconds: i32,
    #[default = 8]
    pub max_spawn_protection_time_seconds: i32,
    /// How long a car stops after a player hits it.
    #[default = 1]
    pub min_collision_stop_time_seconds: i32,
    #[default = 3]
    pub max_collision_stop_time_seconds: i32,
    #[default = 80.0]
    pub max_speed_kph: f32,
    /// Added to `MaxSpeedKph` in every lane with a lane to its left.
    #[default = 10.0]
    pub right_lane_offset_kph: f32,
    /// Each car's top speed varies by this fraction around `MaxSpeedKph`.
    #[default = 0.15]
    pub max_speed_variation_percent: f32,
    /// m/s².
    #[default = 8.5]
    pub default_deceleration: f32,
    #[default = 2.5]
    pub default_acceleration: f32,
    /// The most AI cars kept alive at once, over every AI slot.
    #[default = 300]
    pub max_ai_target_count: i32,
    /// AI cars kept per connected player.
    #[default = 10]
    pub ai_per_player_target_count: i32,
    /// Refuse players once this many are connected, to leave room for
    /// traffic. 0 disables it.
    pub max_player_count: i32,
    /// Hides AI cars' name tags and map markers (CSP).
    pub hide_ai_cars: bool,
    pub spline_height_offset_meters: f32,
    /// How far apart neighbouring lanes are, for lane detection.
    #[default = 3.0]
    pub lane_width_meters: f32,
    /// Spawn traffic in lanes running against the player too.
    pub two_way_traffic: bool,
    /// Spawn traffic for a player driving the wrong way.
    #[default = true]
    pub wrong_way_traffic: bool,
    /// Lower = slower through corners.
    #[default = 0.65]
    pub cornering_speed_factor: f32,
    /// Lower = later braking for corners.
    #[default = 3.0]
    pub cornering_brake_distance_factor: f32,
    /// Times `DefaultDeceleration` when braking for a corner.
    #[default = 0.5]
    pub cornering_brake_force_factor: f32,
    /// AI cars are named "<NamePrefix> <session id>".
    #[default = "Traffic"]
    pub name_prefix: String,
    /// A car stopped this long ignores obstacles for 10 s.
    #[default = 10]
    pub ignore_obstacles_after_seconds: i32,
    /// Scales the gaps and the per-player car count. Above 1 is not useful.
    #[default = 1.0]
    pub traffic_density: f32,
    /// 24 densities, one per hour of the in-game day, replacing
    /// `TrafficDensity`.
    pub hourly_traffic_density: Option<Vec<f32>>,
    #[default = 0.65]
    pub tyre_diameter_meters: f32,
    /// Averages spline camber over five points.
    pub smooth_camber: bool,
    /// How often the spawner runs.
    #[default = 2]
    pub ai_behavior_update_interval_hz: i32,
    pub car_specific_overrides: Vec<CarSpecificOverrides>,
    /// AI cars inside these spheres ignore players, and do not stop when hit.
    pub ignore_player_obstacle_spheres: Vec<Sphere>,
    /// Gaps for cars spawned where the road has this many lanes.
    pub lane_count_specific_overrides: BTreeMap<i32, LaneCountSpecificOverrides>,
    /// Draws each AI car's speeds and gap above it (a built-in CSP script).
    pub debug: bool,
}

/// An `IgnorePlayerObstacleSpheres` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Sphere {
    pub center: Point,
    pub radius_meters: f32,
}

/// A position in `extra_cfg.yml`: `{X: 1, Y: 2, Z: 3}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Point {
    #[serde(rename = "X")]
    pub x: f32,
    #[serde(rename = "Y")]
    pub y: f32,
    #[serde(rename = "Z")]
    pub z: f32,
}

/// A `LaneCountSpecificOverrides` value.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LaneCountSpecificOverrides {
    pub min_ai_safety_distance_meters: i32,
    pub max_ai_safety_distance_meters: i32,
}

/// Which lanes of a multi-lane road a car may spawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum LaneSpawnBehavior {
    Left,
    Middle,
    Right,
}

/// Per-model replacements for the defaults above.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CarSpecificOverrides {
    pub model: String,
    pub spline_height_offset_meters: Option<f32>,
    pub engine_idle_rpm: Option<i32>,
    pub engine_max_rpm: Option<i32>,
    pub acceleration: Option<f32>,
    pub deceleration: Option<f32>,
    pub cornering_speed_factor: Option<f32>,
    pub cornering_brake_distance_factor: Option<f32>,
    pub cornering_brake_force_factor: Option<f32>,
    pub tyre_diameter_meters: Option<f32>,
    /// The most AI states this slot may drive at once.
    pub max_overbooking: Option<i32>,
    pub min_spawn_protection_time_seconds: Option<i32>,
    pub max_spawn_protection_time_seconds: Option<i32>,
    pub min_collision_stop_time_seconds: Option<i32>,
    pub max_collision_stop_time_seconds: Option<i32>,
    /// The car's length ahead of and behind its position, for gaps.
    pub vehicle_length_pre_meters: Option<f32>,
    pub vehicle_length_post_meters: Option<f32>,
    pub min_ai_safety_distance_meters: Option<i32>,
    pub max_ai_safety_distance_meters: Option<i32>,
    /// The lanes the car may spawn in; all when absent.
    pub allowed_lanes: Option<Vec<LaneSpawnBehavior>>,
    /// Spawn only where the road has at least / at most this many lanes.
    pub min_lane_count: Option<i32>,
    pub max_lane_count: Option<i32>,
}

impl AiParams {
    /// Rejects values the traffic code cannot run with.
    pub fn validate(&self) -> Result<(), String> {
        if self.ai_behavior_update_interval_hz <= 0 {
            return Err("AiParams.AiBehaviorUpdateIntervalHz must be above 0".to_string());
        }
        if self.lane_width_meters < 2.1 {
            return Err("AiParams.LaneWidthMeters cannot be below 2.1".to_string());
        }
        if self.min_spawn_distance_points > self.max_spawn_distance_points
            || self.min_ai_safety_distance_meters > self.max_ai_safety_distance_meters
            || self.min_spawn_protection_time_seconds > self.max_spawn_protection_time_seconds
            || self.min_collision_stop_time_seconds > self.max_collision_stop_time_seconds
        {
            return Err("AiParams: a Min value is above its Max".to_string());
        }
        if self.traffic_density <= 0.0 {
            return Err("AiParams.TrafficDensity must be above 0".to_string());
        }
        if let Some(hourly) = &self.hourly_traffic_density
            && hourly.len() != 24
        {
            return Err("AiParams.HourlyTrafficDensity must have 24 entries".to_string());
        }
        Ok(())
    }
}
