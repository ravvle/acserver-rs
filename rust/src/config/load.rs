//! Reading and validating the config files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::Rng;

use super::ini::IniFile;
use super::values::{parse_bool, parse_float, parse_int, parse_steam_id, scan_car_index};
use super::{
    AiMode, BlacklistMode, Config, DynamicTrackConfig, EntryCar, ExtraConfig, IsOpen, ServerConfig,
    SessionConfig, WeatherConfig,
};
use crate::proto::{MAX_CAR_SLOTS, SessionType};

/// Vanilla's `NAME` when none is set.
const DEFAULT_SERVER_NAME: &str = "Assetto Corsa Server";

/// Session sections in vanilla order.
const SESSION_SECTIONS: [(&str, SessionType); 4] = [
    ("BOOK", SessionType::Booking),
    ("PRACTICE", SessionType::Practice),
    ("QUALIFY", SessionType::Qualify),
    ("RACE", SessionType::Race),
];

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// An error inside one of the files, prefixed with its name.
    // The causes are part of each message, not separate sources, so a
    // caller printing the whole chain shows them once.
    #[error("{file}: {inner}")]
    File {
        file: &'static str,
        inner: Box<ConfigError>,
    },
    #[error("open {}: {err}", path.display())]
    Read { path: PathBuf, err: std::io::Error },
    #[error("{0}")]
    Yaml(String),
    /// A value the server refuses to start with.
    #[error("{0}")]
    Invalid(String),
}

impl ConfigError {
    fn in_file(self, file: &'static str) -> ConfigError {
        ConfigError::File {
            file,
            inner: Box::new(self),
        }
    }
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

/// Where the config files are. Empty fields default next to `server_cfg`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Files {
    /// `server_cfg.ini`
    pub server_cfg: PathBuf,
    /// `entry_list.ini`
    pub entry_list: PathBuf,
    /// `extra_cfg.yml`; optional, ignored if absent.
    pub extra_cfg: PathBuf,
    /// Root for results and the blacklist; defaults to `server_cfg`'s directory.
    pub base_dir: PathBuf,
}

/// Turns the flags into `Files`. `cfg_path` is a directory holding all three
/// files, or a `server_cfg.ini`; an empty `entry_path` / `extra_path` means
/// "next to it".
pub fn resolve(cfg_path: &str, entry_path: &str, extra_path: &str) -> Files {
    let cfg = Path::new(cfg_path);
    let (base_dir, server_cfg) = if cfg.is_dir() {
        (cfg.to_path_buf(), cfg.join("server_cfg.ini"))
    } else {
        (dir_of(cfg), cfg.to_path_buf())
    };
    let entry_list = if entry_path.is_empty() {
        base_dir.join("entry_list.ini")
    } else {
        PathBuf::from(entry_path)
    };
    let extra_cfg = if extra_path.is_empty() {
        base_dir.join("extra_cfg.yml")
    } else {
        PathBuf::from(extra_path)
    };
    Files {
        server_cfg,
        entry_list,
        extra_cfg,
        base_dir,
    }
}

/// A path's directory, `.` for a bare file name.
fn dir_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Reads `server_cfg.ini` and `entry_list.ini` from `cfg_dir` and, if present,
/// `extra_cfg.yml`. An empty `content_dir` disables content checks.
pub fn load(cfg_dir: &str, content_dir: &str) -> Result<Config, ConfigError> {
    load_files(resolve(cfg_dir, "", ""), content_dir)
}

/// Reads a configuration from explicit paths.
pub fn load_files(files: Files, content_dir: &str) -> Result<Config, ConfigError> {
    let base_dir = if files.base_dir.as_os_str().is_empty() {
        dir_of(&files.server_cfg)
    } else {
        files.base_dir.clone()
    };
    let mut cfg = Config {
        server: ServerConfig::default(),
        sessions: Vec::new(),
        weathers: Vec::new(),
        dynamic: DynamicTrackConfig::default(),
        entry_list: Vec::new(),
        extra: ExtraConfig::default(),
        base_dir,
        entry_list_path: files.entry_list.clone(),
        content_dir: normalize_content_dir(content_dir),
        warnings: Vec::new(),
        public_ip: None,
    };

    let cars_listed = cfg
        .load_server_cfg(&files.server_cfg)
        .map_err(|e| e.in_file("server_cfg.ini"))?;

    cfg.entry_list = load_entry_list(&files.entry_list, &cfg.server.cars)
        .map_err(|e| e.in_file("entry_list.ini"))?;

    if !files.extra_cfg.as_os_str().is_empty() {
        // A missing or unreadable extra_cfg.yml is simply skipped.
        if let Ok(bytes) = std::fs::read(&files.extra_cfg) {
            let text = String::from_utf8_lossy(&bytes);
            cfg.extra = ExtraConfig::from_yaml(&text)
                .map_err(|e| ConfigError::Yaml(e).in_file("extra_cfg.yml"))?;
        }
    }

    cfg.validate(cars_listed)?;
    Ok(cfg)
}

/// Reads an INI file.
fn ini_load(path: &Path) -> Result<IniFile, ConfigError> {
    let bytes = std::fs::read(path).map_err(|err| ConfigError::Read {
        path: path.to_path_buf(),
        err,
    })?;
    Ok(IniFile::parse(&String::from_utf8_lossy(&bytes)))
}

/// A setting's type, as the INI helpers parse it.
trait IniValue: Sized {
    /// The value, or `None` when the text is not one.
    fn from_ini(value: &str) -> Option<Self>;
    /// What a valid value looks like, for the error.
    fn expected() -> String;
}

impl IniValue for String {
    fn from_ini(value: &str) -> Option<String> {
        Some(value.to_string())
    }
    fn expected() -> String {
        "text".to_string()
    }
}

impl IniValue for bool {
    fn from_ini(value: &str) -> Option<bool> {
        parse_bool(value)
    }
    fn expected() -> String {
        "0 or 1".to_string()
    }
}

impl IniValue for f32 {
    fn from_ini(value: &str) -> Option<f32> {
        parse_float(value).map(|f| f as f32)
    }
    fn expected() -> String {
        "a number".to_string()
    }
}

macro_rules! int_ini_value {
    ($($t:ty),*) => {$(
        impl IniValue for $t {
            fn from_ini(value: &str) -> Option<$t> {
                parse_int(value)?.try_into().ok()
            }
            fn expected() -> String {
                format!("a whole number from {} to {}", <$t>::MIN, <$t>::MAX)
            }
        }
    )*};
}
int_ini_value!(u8, u16, u32, i16, i64);

impl IniValue for BlacklistMode {
    fn from_ini(value: &str) -> Option<BlacklistMode> {
        match parse_int(value)? {
            0 => Some(BlacklistMode::Kick),
            1 => Some(BlacklistMode::KickUntilRestart),
            2 => Some(BlacklistMode::Ban),
            _ => None,
        }
    }
    fn expected() -> String {
        "0, 1 or 2".to_string()
    }
}

impl IniValue for IsOpen {
    fn from_ini(value: &str) -> Option<IsOpen> {
        match parse_int(value)? {
            0 => Some(IsOpen::Closed),
            1 => Some(IsOpen::Open),
            2 => Some(IsOpen::ClosesAtStart),
            _ => None,
        }
    }
    fn expected() -> String {
        "0, 1 or 2".to_string()
    }
}

/// A key's value: `None` when missing or empty, an error when it is not a
/// valid value for the setting.
fn read<T: IniValue>(ini: &IniFile, section: &str, key: &str) -> Result<Option<T>, ConfigError> {
    let Some(value) = ini.get(section, key).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    match T::from_ini(&value) {
        Some(parsed) => Ok(Some(parsed)),
        None => Err(invalid(format!(
            "[{section}] {key}={value}: expected {}",
            T::expected()
        ))),
    }
}

/// Overwrites `field` with the key's value, keeping the default for a
/// missing or empty one.
fn map<T: IniValue>(
    ini: &IniFile,
    section: &str,
    key: &str,
    field: &mut T,
) -> Result<(), ConfigError> {
    if let Some(value) = read(ini, section, key)? {
        *field = value;
    }
    Ok(())
}

/// Refuses a value above `max` that its type allows.
fn at_most<T: PartialOrd + std::fmt::Display>(
    key: &str,
    value: T,
    max: T,
) -> Result<(), ConfigError> {
    if value > max {
        return Err(invalid(format!("{key}={value}: expected at most {max}")));
    }
    Ok(())
}

impl Config {
    /// Reads `server_cfg.ini`. Returns whether `CARS` was given at all; an
    /// absent `CARS` later becomes the entry list's models.
    fn load_server_cfg(&mut self, path: &Path) -> Result<bool, ConfigError> {
        let ini = ini_load(path)?;
        let sc = &mut self.server;
        *sc = ServerConfig::default();
        let s = "SERVER";

        map(&ini, s, "NAME", &mut sc.name)?;
        map(&ini, s, "TRACK", &mut sc.track)?;
        map(&ini, s, "CONFIG_TRACK", &mut sc.track_config)?;
        map(&ini, s, "SUN_ANGLE", &mut sc.sun_angle)?;
        map(&ini, s, "PASSWORD", &mut sc.password)?;
        map(&ini, s, "ADMIN_PASSWORD", &mut sc.admin_password)?;
        map(&ini, s, "UDP_PORT", &mut sc.udp_port)?;
        map(&ini, s, "TCP_PORT", &mut sc.tcp_port)?;
        map(&ini, s, "HTTP_PORT", &mut sc.http_port)?;
        map(&ini, s, "MAX_CLIENTS", &mut sc.max_clients)?;
        map(&ini, s, "REGISTER_TO_LOBBY", &mut sc.register_to_lobby)?;
        map(&ini, s, "PICKUP_MODE_ENABLED", &mut sc.pickup_mode_enabled)?;
        map(&ini, s, "LOOP_MODE", &mut sc.loop_mode)?;
        map(&ini, s, "LOCKED_ENTRY_LIST", &mut sc.locked_entry_list)?;
        map(&ini, s, "SEND_BUFFER_SIZE", &mut sc.send_buffer_size)?;
        map(&ini, s, "RECV_BUFFER_SIZE", &mut sc.recv_buffer_size)?;
        map(&ini, s, "UDP_LOGGER", &mut sc.udp_logger)?;
        map(&ini, s, "MAX_BALLAST_KG", &mut sc.max_ballast_kg)?;
        map(
            &ini,
            s,
            "QUALIFY_MAX_WAIT_PERC",
            &mut sc.qualify_max_wait_perc,
        )?;
        map(&ini, s, "RACE_OVER_TIME", &mut sc.race_over_time)?;
        map(&ini, s, "RESULT_SCREEN_TIME", &mut sc.result_screen_time)?;
        map(
            &ini,
            s,
            "RACE_GAS_PENALTY_DISABLED",
            &mut sc.race_gas_penalty_disabled,
        )?;
        map(
            &ini,
            s,
            "RACE_PIT_WINDOW_START",
            &mut sc.race_pit_window_start,
        )?;
        map(&ini, s, "RACE_PIT_WINDOW_END", &mut sc.race_pit_window_end)?;
        map(
            &ini,
            s,
            "REVERSED_GRID_RACE_POSITIONS",
            &mut sc.reversed_grid_race_positions,
        )?;
        map(&ini, s, "VOTING_QUORUM", &mut sc.voting_quorum)?;
        map(&ini, s, "KICK_QUORUM", &mut sc.kick_quorum)?;
        map(&ini, s, "BLACKLIST_MODE", &mut sc.blacklist_mode)?;
        map(&ini, s, "LEGAL_TYRES", &mut sc.legal_tyres)?;
        map(&ini, s, "WELCOME_MESSAGE", &mut sc.welcome_message_path)?;
        map(&ini, s, "START_RULE", &mut sc.start_rule)?;
        map(&ini, s, "RACE_EXTRA_LAP", &mut sc.race_extra_lap)?;
        map(&ini, s, "TIME_OF_DAY_MULT", &mut sc.time_of_day_mult)?;
        map(&ini, s, "ABS_ALLOWED", &mut sc.abs_allowed)?;
        map(&ini, s, "TC_ALLOWED", &mut sc.tc_allowed)?;
        map(&ini, s, "STABILITY_ALLOWED", &mut sc.stability_allowed)?;
        map(&ini, s, "AUTOCLUTCH_ALLOWED", &mut sc.autoclutch_allowed)?;
        map(
            &ini,
            s,
            "TYRE_BLANKETS_ALLOWED",
            &mut sc.tyre_blankets_allowed,
        )?;
        map(
            &ini,
            s,
            "FORCE_VIRTUAL_MIRROR",
            &mut sc.force_virtual_mirror,
        )?;
        map(&ini, s, "FUEL_RATE", &mut sc.fuel_rate)?;
        map(&ini, s, "DAMAGE_MULTIPLIER", &mut sc.damage_multiplier)?;
        map(&ini, s, "TYRE_WEAR_RATE", &mut sc.tyre_wear_rate)?;
        map(&ini, s, "ALLOWED_TYRES_OUT", &mut sc.allowed_tyres_out)?;
        map(&ini, s, "UDP_PLUGIN_ADDRESS", &mut sc.udp_plugin_address)?;
        map(
            &ini,
            s,
            "UDP_PLUGIN_LOCAL_PORT",
            &mut sc.udp_plugin_local_port,
        )?;
        map(&ini, s, "AUTH_PLUGIN_ADDRESS", &mut sc.auth_plugin_address)?;

        // Vanilla's name when NAME is missing or empty (measured).
        if sc.name.is_empty() {
            sc.name = DEFAULT_SERVER_NAME.to_string();
        }
        // CARS accepts either separator.
        let cars = split_list(&ini.get(s, "CARS").unwrap_or_default());
        let cars_listed = cars.is_some();
        sc.cars = cars.unwrap_or_default();
        sc.idle_mode_enabled = read(&ini, "IDLE_MODE", "ENABLED")?.unwrap_or(false);
        let idle_minutes: u32 = read(&ini, "IDLE_MODE", "TIMEOUT")?.unwrap_or(0);
        sc.idle_timeout = Duration::from_secs(u64::from(idle_minutes) * 60);
        if let Some(ms) = read::<u32>(&ini, s, "SLEEP_TIME")? {
            sc.sleep_time = Duration::from_millis(ms.into());
        }
        if let Some(secs) = read::<u32>(&ini, s, "VOTE_DURATION")? {
            sc.vote_duration = Duration::from_secs(secs.into());
        }
        // Vanilla reads only CLIENT_SEND_INTERVAL_HZ; the other two are ours.
        sc.refresh_rate_hz = first_present(
            &ini,
            s,
            &["CLIENT_SEND_INTERVAL_HZ", "FPS", "REFRESH_RATE_HZ"],
        )?
        .unwrap_or(18);
        sc.max_contacts_per_km =
            normalise_max_contacts(read(&ini, s, "MAX_CONTACTS_PER_KM")?.unwrap_or(0))?;
        at_most("START_RULE", sc.start_rule, 2)?;
        at_most("ABS_ALLOWED", sc.abs_allowed, 2)?;
        at_most("TC_ALLOWED", sc.tc_allowed, 2)?;
        if sc.race_pit_window_start < 0 || sc.race_pit_window_end < 0 {
            return Err(invalid("RACE_PIT_WINDOW_START and _END cannot be negative"));
        }

        if !sc.welcome_message_path.is_empty() {
            let as_given = std::fs::read(&sc.welcome_message_path);
            let in_base = || std::fs::read(self.base_dir.join(&sc.welcome_message_path));
            if let Ok(bytes) = as_given.or_else(|_| in_base()) {
                sc.welcome_message = String::from_utf8_lossy(&bytes).into_owned();
            }
        }

        self.load_sessions(&ini)?;
        self.load_weathers(&ini)?;
        self.load_dynamic_track(&ini)?;
        Ok(cars_listed)
    }

    fn load_sessions(&mut self, ini: &IniFile) -> Result<(), ConfigError> {
        for (section, kind) in SESSION_SECTIONS {
            if !ini.has_section(section) {
                continue;
            }
            let mut sess = SessionConfig::new(kind);
            map(ini, section, "NAME", &mut sess.name)?;
            map(ini, section, "TIME", &mut sess.time)?;
            map(ini, section, "LAPS", &mut sess.laps)?;
            map(ini, section, "WAIT_TIME", &mut sess.wait_time)?;
            map(ini, section, "IS_OPEN", &mut sess.is_open)?;
            if sess.kind == SessionType::Booking && sess.time == 0 {
                self.warnings
                    .push("booking session with TIME=0 ignored".to_string());
                continue;
            }
            self.sessions.push(sess);
        }
        Ok(())
    }

    fn load_weathers(&mut self, ini: &IniFile) -> Result<(), ConfigError> {
        for i in 0.. {
            let name = format!("WEATHER_{i}");
            if !ini.has_section(&name) {
                break;
            }
            let s = name.as_str();
            self.weathers.push(WeatherConfig {
                graphics: read(ini, s, "GRAPHICS")?.unwrap_or_else(|| "3_clear".to_string()),
                // Road is a delta on ambient (docs/02 0x78).
                base_temp_ambient: read(ini, s, "BASE_TEMPERATURE_AMBIENT")?.unwrap_or(18.0),
                road_temp_offset: read(ini, s, "BASE_TEMPERATURE_ROAD")?.unwrap_or(6.0),
                variation_ambient: read(ini, s, "VARIATION_AMBIENT")?.unwrap_or(0.0),
                variation_road: read(ini, s, "VARIATION_ROAD")?.unwrap_or(0.0),
                wind_base_speed_min: read(ini, s, "WIND_BASE_SPEED_MIN")?.unwrap_or(0),
                wind_base_speed_max: read(ini, s, "WIND_BASE_SPEED_MAX")?.unwrap_or(0),
                wind_base_direction: read(ini, s, "WIND_BASE_DIRECTION")?.unwrap_or(0),
                wind_variation_direction: read(ini, s, "WIND_VARIATION_DIRECTION")?.unwrap_or(0),
            });
        }
        Ok(())
    }

    fn load_dynamic_track(&mut self, ini: &IniFile) -> Result<(), ConfigError> {
        let d = &mut self.dynamic;
        *d = DynamicTrackConfig::default();
        let s = "DYNAMIC_TRACK";
        map(ini, s, "SESSION_START", &mut d.session_start)?;
        map(ini, s, "RANDOMNESS", &mut d.randomness)?;
        map(ini, s, "SESSION_TRANSFER", &mut d.session_transfer)?;
        map(ini, s, "LAP_GAIN", &mut d.lap_gain)?;
        Ok(())
    }

    /// Checks the loaded configuration as vanilla does at startup.
    /// `cars_listed` says whether `CARS` was present.
    fn validate(&mut self, cars_listed: bool) -> Result<(), ConfigError> {
        if self.server.pickup_mode_enabled
            && self.sessions.iter().any(|s| s.kind == SessionType::Booking)
        {
            // Vanilla's initServerCfg refuses to start too.
            return Err(invalid("pickup mode servers cannot have a [BOOK] session"));
        }
        // A booking server fills its car list from bookings, so it may start empty.
        if self.entry_list.is_empty() && !self.booking() {
            return Err(invalid("entry_list.ini contains no CAR_n slots"));
        }
        if self.entry_list.len() > MAX_CAR_SLOTS {
            return Err(invalid(format!(
                "entry_list.ini contains {} CAR_n slots (max {MAX_CAR_SLOTS})",
                self.entry_list.len()
            )));
        }
        // At most MAX_CAR_SLOTS (255), checked above.
        let entries = self.entry_list.len() as u8;
        if self.server.max_clients == 0 {
            self.server.max_clients = entries;
        }
        if !self.server.pickup_mode_enabled {
            // Vanilla lowers MAX_CLIENTS only in pickup mode. Without it,
            // bookings and a reloaded entry list take ids below MAX_CLIENTS.
            if entries > self.server.max_clients {
                self.warnings.push(format!(
                    "entry list has {entries} CAR_n slots, more than MAX_CLIENTS={}",
                    self.server.max_clients
                ));
            }
        } else if self.server.max_clients > entries {
            // Vanilla lowers MAX_CLIENTS to the entry-list size and starts (measured).
            self.warnings.push(format!(
                "MAX_CLIENTS={} exceeds the {entries} CAR_n slots; using {entries}",
                self.server.max_clients
            ));
            self.server.max_clients = entries;
        }
        if self.sessions.is_empty() {
            return Err(invalid("server_cfg.ini defines no sessions"));
        }
        if self.server.refresh_rate_hz == 0 {
            self.server.refresh_rate_hz = 18;
        }
        if !cars_listed {
            self.server.cars = self.distinct_models();
        }
        validate_entries(&self.server, &self.entry_list)?;
        // Vanilla's grid is the first MAX_CLIENTS entries: slots past it appear
        // nowhere, not in 0x40, 0x49, 0x5B, 0x70, /JSON or the results (measured).
        self.entry_list
            .truncate(usize::from(self.server.max_clients));
        if self.weathers.is_empty() {
            return Err(invalid("server_cfg.ini requires a WEATHER_0 section"));
        }
        for (i, weather) in self.weathers.iter().enumerate() {
            if weather.wind_base_speed_max < weather.wind_base_speed_min
                || weather.wind_base_speed_max > 40
            {
                return Err(invalid(format!(
                    "WEATHER_{i} wind speed range must satisfy 0 <= MIN <= MAX <= 40"
                )));
            }
            at_most("WIND_BASE_DIRECTION", weather.wind_base_direction, 360)?;
            at_most(
                "WIND_VARIATION_DIRECTION",
                weather.wind_variation_direction,
                360,
            )?;
        }
        self.validate_pit_window();
        self.validate_content()
    }

    /// Reads `entry_list.ini` again and checks it as `load` does, returning at
    /// most `MAX_CLIENTS` cars. Vanilla's NextSession does this when the
    /// sessions loop with pickup mode off, so the file can change between
    /// rounds. `self` is left unchanged.
    pub fn reload_entry_list(&self) -> Result<Vec<EntryCar>, ConfigError> {
        let mut entries = load_entry_list(&self.entry_list_path, &self.server.cars)?;
        validate_entries(&self.server, &entries)?;
        entries.truncate(usize::from(self.server.max_clients));
        Ok(entries)
    }

    /// Zeroes a pit window that cannot be served, as vanilla does (docs/02
    /// 0x3E); otherwise clients enforce an impossible stop.
    pub(crate) fn validate_pit_window(&mut self) {
        let start = self.server.race_pit_window_start;
        let end = self.server.race_pit_window_end;
        if start == 0 && end == 0 {
            return; // no window configured: nothing to validate
        }
        let mut valid = start < end;
        if valid {
            valid = false;
            for s in &self.sessions {
                if s.kind != SessionType::Race {
                    continue;
                }
                // end is non-negative, checked at load.
                let end = end as u16;
                if s.laps > 0 {
                    valid = end <= s.laps;
                } else if s.time > 0 {
                    valid = end <= s.time;
                }
            }
        }
        if !valid {
            self.server.race_pit_window_start = 0;
            self.server.race_pit_window_end = 0;
        }
    }

    /// Checks track and car names against the content tree, if there is one.
    fn validate_content(&self) -> Result<(), ConfigError> {
        let Some(root) = &self.content_dir else {
            return Ok(());
        };
        let track_root = root.join("content").join("tracks").join(&self.server.track);
        require_dir("track", &track_root)?;

        let track_config = &self.server.track_config;
        if !track_config.is_empty() {
            // A game install has ui/<CONFIG_TRACK>; packed server content may
            // have only models_<CONFIG_TRACK>.ini. Validate when either exists.
            let candidates = [
                track_root.join("ui").join(track_config),
                track_root.join(track_config),
                track_root.join(format!("models_{track_config}.ini")),
                track_root.join(format!("models_layout_{track_config}.ini")),
            ];
            let found = candidates.iter().any(|path| path.exists());
            if dir_has_entries(&track_root) && !found {
                return Err(invalid(format!(
                    "track configuration content not found for {track_config:?}"
                )));
            }
        }

        let ui_path = if track_config.is_empty() {
            track_root.join("ui").join("ui_track.json")
        } else {
            track_root
                .join("ui")
                .join(track_config)
                .join("ui_track.json")
        };
        let pitboxes = read_pitboxes(&ui_path);
        if pitboxes > 0 && i64::from(self.server.max_clients) > pitboxes {
            return Err(invalid(format!(
                "MAX_CLIENTS={} exceeds track pit boxes={pitboxes}",
                self.server.max_clients
            )));
        }

        for (i, entry) in self.entry_list.iter().enumerate() {
            let car_root = root.join("content").join("cars").join(&entry.model);
            require_dir(&format!("CAR_{i} model"), &car_root)?;
            if !entry.skin.is_empty() {
                let skins_root = car_root.join("skins");
                if skins_root.exists() {
                    require_dir(&format!("CAR_{i} skin"), &skins_root.join(&entry.skin))?;
                }
            }
        }

        let weather_root = root.join("content").join("weather");
        if weather_root.exists() {
            for (i, weather) in self.weathers.iter().enumerate() {
                require_dir(
                    &format!("WEATHER_{i} graphics"),
                    &weather_root.join(graphics_folder(&weather.graphics)),
                )?;
            }
        }
        Ok(())
    }
}

/// `MAX_CONTACTS_PER_KM` rules: 0 becomes 5, negative becomes 0 (no limit),
/// above 255 refuses to start.
pub(crate) fn normalise_max_contacts(value: i64) -> Result<u8, ConfigError> {
    match value {
        0 => Ok(5),
        ..0 => Ok(0),
        _ => u8::try_from(value).map_err(|_| {
            invalid(format!(
                "MAX_CONTACTS_PER_KM out of bound (max 255): {value}"
            ))
        }),
    }
}

/// Reads the `[CAR_n]` sections in ascending numeric order (CAR_10 after CAR_9).
fn load_entry_list(path: &Path, legal_cars: &[String]) -> Result<Vec<EntryCar>, ConfigError> {
    let ini = ini_load(path)?;
    let mut cars: Vec<(i64, &str)> = Vec::new();
    for section in ini.sections() {
        if let Some(index) = scan_car_index(section.name()) {
            cars.push((index, section.name()));
        }
    }
    // Stable, so two names with the same number keep file order.
    cars.sort_by_key(|&(index, _)| index);

    let mut entries = Vec::with_capacity(cars.len());
    for (_, s) in cars {
        let mut ec = EntryCar::default();
        map(&ini, s, "MODEL", &mut ec.model)?;
        map(&ini, s, "SKIN", &mut ec.skin)?;
        map(&ini, s, "SPECTATOR_MODE", &mut ec.spectator_mode)?;
        map(&ini, s, "DRIVERNAME", &mut ec.driver_name)?;
        map(&ini, s, "TEAM", &mut ec.team)?;
        map(&ini, s, "GUID", &mut ec.guid)?;
        map(&ini, s, "BALLAST", &mut ec.ballast)?;
        map(&ini, s, "RESTRICTOR", &mut ec.restrictor)?;
        map(&ini, s, "FIXED_SETUP", &mut ec.fixed_setup)?;

        if let Some(resolved) = solve_car_wildcard(&ec.model, legal_cars) {
            ec.model = resolved;
        }
        // AI is a mode name rather than a flag.
        let ai = ini.get(s, "AI").unwrap_or_default();
        ec.ai_mode = if ai.eq_ignore_ascii_case("fixed") {
            AiMode::Fixed
        } else if ai.eq_ignore_ascii_case("auto") {
            AiMode::Auto
        } else {
            AiMode::None
        };
        ec.guids = split_semicolon_list(&ec.guid);
        ec.fixed_setup_set = ini.has_key(s, "FIXED_SETUP");
        ec.fixed_setup_named = !ec.fixed_setup.trim().is_empty();
        if ec.fixed_setup_set && !ec.fixed_setup_named {
            ec.fixed_setup = ec.model.clone();
        }
        entries.push(ec);
    }
    Ok(entries)
}

/// Checks each car as vanilla does at startup.
fn validate_entries(server: &ServerConfig, entries: &[EntryCar]) -> Result<(), ConfigError> {
    let legal_cars: Vec<String> = server.cars.iter().map(|m| m.to_lowercase()).collect();
    for (i, entry) in entries.iter().enumerate() {
        if !legal_cars.contains(&entry.model.to_lowercase()) {
            return Err(invalid(format!(
                "CAR_{i} MODEL={:?} is not listed in [SERVER] CARS",
                entry.model
            )));
        }
        for guid in &entry.guids {
            if !parse_steam_id(guid).is_some_and(|n| n != 0) {
                return Err(invalid(format!(
                    "CAR_{i} GUID contains invalid Steam64 ID {guid:?}"
                )));
            }
        }
        if entry.restrictor < 0.0 || entry.restrictor > 100.0 {
            return Err(invalid(format!(
                "CAR_{i} RESTRICTOR must be between 0 and 100"
            )));
        }
        // Vanilla refuses to start on a ballast over MAX_BALLAST_KG (measured).
        if entry.ballast > f32::from(server.max_ballast_kg) {
            return Err(invalid(format!(
                "CAR_{i} BALLAST={} exceeds MAX_BALLAST_KG={}",
                entry.ballast, server.max_ballast_kg
            )));
        }
    }
    Ok(())
}

/// Resolves `MODEL=*N` against `CARS=` (reference SolveCarWildcard, docs/08
/// §8.1). `None` for a plain model or one that cannot resolve; validation then
/// rejects it as an illegal car.
pub(crate) fn solve_car_wildcard(model: &str, legal_cars: &[String]) -> Option<String> {
    let rest = model.strip_prefix('*')?;
    if legal_cars.is_empty() {
        return None;
    }
    let n = parse_int(rest)?;
    if n >= 1 && n <= legal_cars.len() as i64 {
        return Some(legal_cars[(n - 1) as usize].clone());
    }
    let pick = rand::rng().random_range(0..legal_cars.len());
    Some(legal_cars[pick].clone())
}

/// Maps `--content` to the install root, accepting either the root or its
/// `content/` directory (which would otherwise make every checksum file
/// silently missing). `None` for an empty flag.
pub fn normalize_content_dir(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        return None;
    }
    // Rebuilding from components drops "." parts and trailing separators.
    let clean: PathBuf = Path::new(dir).components().collect();
    if clean.join("content").is_dir() {
        return Some(clean);
    }
    if clean.file_name().is_some_and(|name| name == "content") {
        return Some(dir_of(&clean));
    }
    Some(clean)
}

/// The weather folder a GRAPHICS value names: the value up to its first
/// `_key=value` pair, so AssettoServer's "3_clear_type=15" is `3_clear`.
pub(crate) fn graphics_folder(graphics: &str) -> String {
    graphics
        .split('_')
        .take_while(|part| !part.contains('='))
        .collect::<Vec<_>>()
        .join("_")
}

fn require_dir(label: &str, path: &Path) -> Result<(), ConfigError> {
    if path.is_dir() {
        return Ok(());
    }
    Err(invalid(format!(
        "{label} content directory not found: {}",
        path.display()
    )))
}

fn dir_has_entries(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// `pitboxes` from a track's `ui_track.json`, or 0 (skip the check) when
/// the file, the field or a numeric value is missing.
fn read_pitboxes(path: &Path) -> i64 {
    #[derive(serde::Deserialize)]
    struct UiTrack {
        // A number in some tracks, a string in others ("170").
        #[serde(default)]
        pitboxes: Option<serde_json::Value>,
    }
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let Ok(UiTrack {
        pitboxes: Some(value),
    }) = serde_json::from_slice(&bytes)
    else {
        return 0;
    };
    match value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Splits `CARS`: on `;` if there is one, otherwise on `,`. `None` for an
/// empty value, which means "use the entry list's models".
pub(crate) fn split_list(s: &str) -> Option<Vec<String>> {
    if s.is_empty() {
        return None;
    }
    let separator = if s.contains(';') { ';' } else { ',' };
    let parts: Vec<String> = s
        .split(separator)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    Some(parts)
}

fn split_semicolon_list(s: &str) -> Vec<String> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// The value of the first of `keys` that is present.
fn first_present<T: IniValue>(
    ini: &IniFile,
    section: &str,
    keys: &[&str],
) -> Result<Option<T>, ConfigError> {
    match keys.iter().find(|key| ini.has_key(section, key)) {
        Some(key) => read(ini, section, key),
        None => Ok(None),
    }
}
