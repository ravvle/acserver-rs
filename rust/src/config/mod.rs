//! Parses `server_cfg.ini`, `entry_list.ini` and the optional `extra_cfg.yml`
//! into typed structs with acServer's defaults.
//!
//! Each setting has the type of the values it may take, so a value that
//! does not fit refuses to start.

mod ai;
mod extra;
mod ini;
mod load;
mod values;

use std::path::PathBuf;
use std::time::Duration;

use smart_default::SmartDefault;

use crate::proto::SessionType;

pub use ai::{
    AiParams, CarSpecificOverrides, LaneCountSpecificOverrides, LaneSpawnBehavior, Point, Sphere,
};
pub use extra::{
    AfkKickBehavior, ExtraConfig, IgnoreConfigurationErrors, LiveExtra, UserGroupCommandPermissions,
};
pub use ini::IniFile;
pub use load::{ConfigError, Files, load, load_files, normalize_content_dir, resolve};
pub use values::{parse_bool, parse_digits, parse_float, parse_int, parse_steam_id};

/// The fully parsed server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    /// In configured order: booking, practice, qualify, race.
    pub sessions: Vec<SessionConfig>,
    pub weathers: Vec<WeatherConfig>,
    pub dynamic: DynamicTrackConfig,
    pub entry_list: Vec<EntryCar>,
    pub extra: ExtraConfig,

    /// The config directory: root for results, the blacklist and setups.
    pub base_dir: PathBuf,
    /// `entry_list.ini`, which a server with pickup mode off reads again when
    /// its sessions loop (`reload_entry_list`).
    pub entry_list_path: PathBuf,
    /// The AC install root (holding `content/` and `system/`), since checksum
    /// paths carry those prefixes. `None` disables checksums.
    pub content_dir: Option<PathBuf>,
    /// Config problems the loader corrected rather than rejected; the caller
    /// logs them.
    pub warnings: Vec<String>,
    /// The server's public IP, looked up at start only when a plugin needs
    /// it (`server::public_ip`). `None` otherwise or when the lookup failed.
    pub public_ip: Option<std::net::IpAddr>,
}

/// `[SERVER]`. The defaults are acServer's, kept when a key is missing or
/// empty.
#[derive(Debug, Clone, PartialEq, SmartDefault)]
pub struct ServerConfig {
    pub name: String,
    /// `CARS`, split on `;` or `,`.
    pub cars: Vec<String>,
    pub track: String,
    pub track_config: String,
    #[default(16.0)]
    pub sun_angle: f32,
    pub password: String,
    pub admin_password: String,
    #[default(9600)]
    pub udp_port: u16,
    #[default(9600)]
    pub tcp_port: u16,
    #[default(8081)]
    pub http_port: u16,
    /// 0 in the file means the entry list's size.
    pub max_clients: u8,
    /// `CLIENT_SEND_INTERVAL_HZ` (or `FPS` / `REFRESH_RATE_HZ`); 0 means 18.
    pub refresh_rate_hz: u8,
    #[default(true)]
    pub register_to_lobby: bool,
    #[default(true)]
    pub pickup_mode_enabled: bool,
    #[default(true)]
    pub loop_mode: bool,
    /// Clients may only take their reserved slot.
    pub locked_entry_list: bool,
    /// Vanilla's `[IDLE_MODE] ENABLED` and `TIMEOUT` (minutes).
    pub idle_mode_enabled: bool,
    pub idle_timeout: Duration,
    /// Game UDP `SO_SNDBUF` / `SO_RCVBUF` when > 0.
    pub send_buffer_size: u32,
    pub recv_buffer_size: u32,
    /// Accepted no-op (vanilla's debug log).
    pub udp_logger: bool,
    /// `SLEEP_TIME`, between position relay passes.
    #[default(Duration::from_millis(1))]
    pub sleep_time: Duration,
    #[default(100)]
    pub max_ballast_kg: u16,
    #[default(120)]
    pub qualify_max_wait_perc: u16,
    /// Seconds.
    #[default(60)]
    pub race_over_time: u16,
    /// Seconds.
    #[default(60)]
    pub result_screen_time: u16,
    pub race_gas_penalty_disabled: bool,
    /// Laps or minutes; refused when negative.
    pub race_pit_window_start: i16,
    pub race_pit_window_end: i16,
    /// -1 reverses every finisher.
    pub reversed_grid_race_positions: i16,
    /// Percent.
    #[default(80)]
    pub voting_quorum: u16,
    #[default(Duration::from_secs(20))]
    pub vote_duration: Duration,
    /// Percent.
    #[default(85)]
    pub kick_quorum: u16,
    pub blacklist_mode: BlacklistMode,
    /// 0 in the file means 5, negative means 0 (no limit).
    pub max_contacts_per_km: u8,
    pub legal_tyres: String,
    pub welcome_message_path: String,
    /// The contents of `welcome_message_path`.
    pub welcome_message: String,
    /// 0 to 2, sent as is.
    pub start_rule: u8,
    pub race_extra_lap: bool,
    #[default(1.0)]
    pub time_of_day_mult: f32,

    // Assists and rates.
    /// 0 to 2, sent as is.
    #[default(1)]
    pub abs_allowed: u8,
    #[default(1)]
    pub tc_allowed: u8,
    pub stability_allowed: bool,
    pub autoclutch_allowed: bool,
    pub tyre_blankets_allowed: bool,
    pub force_virtual_mirror: bool,
    /// Percent.
    #[default(100)]
    pub fuel_rate: u16,
    /// Percent.
    #[default(100)]
    pub damage_multiplier: u16,
    /// Percent.
    #[default(100)]
    pub tyre_wear_rate: u16,
    #[default(2)]
    pub allowed_tyres_out: i16,

    // Plugin API.
    /// `ip:port` the server sends events to.
    pub udp_plugin_address: String,
    /// The port the server listens on for plugin requests.
    pub udp_plugin_local_port: u16,
    pub auth_plugin_address: String,

    // Vanilla's timing floors in seconds (measured; docs/01 §1.6, docs/02
    // "End of a race"). Not settable from the .ini; tests shorten them.
    #[default(20)]
    pub min_race_wait: u16,
    #[default(30)]
    pub min_race_over: u16,
    #[default(10)]
    pub min_result_screen: u16,
}

/// `BLACKLIST_MODE`, which is also the kick reason sent to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum BlacklistMode {
    /// The driver may rejoin.
    #[default]
    Kick = 0,
    /// Blacklisted until the server restarts.
    KickUntilRestart = 1,
    /// Added to `blacklist.txt`.
    Ban = 2,
}

/// A session's `IS_OPEN` (the official guide's rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsOpen {
    Closed,
    Open,
    /// A race closes to joins 20 s before its start.
    ClosesAtStart,
}

/// One `[BOOK]`, `[PRACTICE]`, `[QUALIFY]` or `[RACE]` section.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    pub kind: SessionType,
    pub name: String,
    /// Minutes; 0 for a lap race.
    pub time: u16,
    pub laps: u16,
    /// Seconds.
    pub wait_time: u16,
    pub is_open: IsOpen,
}

impl SessionConfig {
    /// A session of `kind` with vanilla's defaults.
    pub fn new(kind: SessionType) -> SessionConfig {
        SessionConfig {
            kind,
            name: kind.default_name().to_string(),
            time: 0,
            laps: 0,
            wait_time: 0,
            is_open: IsOpen::Open,
        }
    }
}

/// One `[WEATHER_n]` section.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherConfig {
    pub graphics: String,
    pub base_temp_ambient: f32,
    /// `BASE_TEMPERATURE_ROAD`, a delta on ambient (see `road_temp`).
    pub road_temp_offset: f32,
    pub variation_ambient: f32,
    pub variation_road: f32,
    /// 0 <= MIN <= MAX <= 40.
    pub wind_base_speed_min: u8,
    pub wind_base_speed_max: u8,
    /// Degrees, at most 360.
    pub wind_base_direction: u16,
    pub wind_variation_direction: u16,
}

impl WeatherConfig {
    /// The absolute road temperature.
    pub fn road_temp(&self) -> f32 {
        self.base_temp_ambient + self.road_temp_offset
    }
}

/// `[DYNAMIC_TRACK]`.
#[derive(Debug, Clone, PartialEq, Eq, SmartDefault)]
pub struct DynamicTrackConfig {
    /// Percent.
    #[default(100)]
    pub session_start: u16,
    /// Percent.
    pub randomness: u16,
    /// Percent.
    #[default(100)]
    pub session_transfer: u16,
    /// Laps per 1% of grip; 0 means none.
    #[default(1)]
    pub lap_gain: u16,
}

/// One `[CAR_n]` slot from `entry_list.ini`.
#[derive(Debug, Clone, PartialEq, SmartDefault)]
pub struct EntryCar {
    pub model: String,
    pub skin: String,
    pub spectator_mode: bool,
    pub driver_name: String,
    pub team: String,
    pub guid: String,
    /// `guid` split on `;` for shared or reserved slots.
    pub guids: Vec<String>,
    pub ballast: f32,
    pub restrictor: f32,
    pub fixed_setup: String,
    /// The key is present; an empty `FIXED_SETUP` means `setups/<MODEL>.ini`.
    pub fixed_setup_set: bool,
    /// `FIXED_SETUP` names a file, which must then exist.
    pub fixed_setup_named: bool,
    /// `AI=`: vanilla never seats a player in a `fixed` or `auto` slot.
    pub ai_mode: AiMode,
}

/// An entry-list slot's `AI=` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiMode {
    #[default]
    None,
    /// AI traffic that a player may take over (with EnableAi).
    Auto,
    /// Always AI traffic.
    Fixed,
}

impl EntryCar {
    /// `AI=fixed` or `AI=auto`.
    pub fn is_ai(&self) -> bool {
        self.ai_mode != AiMode::None
    }
}

impl Config {
    /// Whether the server books its cars: pickup mode is off and a `[BOOK]`
    /// session is configured (vanilla's booking mode, docs/05 §5.2a).
    pub fn booking(&self) -> bool {
        if self.server.pickup_mode_enabled {
            return false;
        }
        self.sessions.iter().any(|s| s.kind == SessionType::Booking)
    }

    /// The unique car models in the entry list, in first-seen order.
    pub fn distinct_models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for entry in &self.entry_list {
            if !entry.model.is_empty() && !out.contains(&entry.model) {
                out.push(entry.model.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
