//! `extra_cfg.yml`, the opt-in extensions (README.md lists each key).

use serde::{Deserialize, Serialize};
use serde_norway::Value;
use smart_default::SmartDefault;

use std::collections::BTreeMap;

use super::ai::AiParams;

/// The parsed `extra_cfg.yml`. Every key is optional: one that is absent or
/// left empty (`Key:`) keeps its default, and unknown keys are ignored.
///
/// The defaults are vanilla behaviour plus what no vanilla client can
/// observe: position validation and the CSP extensions, which only ever
/// reach a client that announced a CSP build.
#[derive(Debug, Clone, PartialEq, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ExtraConfig {
    pub enable_server_details: bool,
    pub server_description: String,
    /// Serves `/content/*` zips built from `--content` and lists them in
    /// `/api/details` (docs/05 §5.4).
    pub enable_content_downloads: bool,
    pub enable_rcon: bool,
    pub rcon_port: i64,
    #[serde(rename = "EnableUnlimitedP2P")]
    pub enable_unlimited_p2p: bool,
    /// AssettoServer hardening, on by default.
    #[default = true]
    pub validate_position_updates: bool,
    /// Requires `0x4E` from the TCP peer's /24 (IPv6 /48). Off by default:
    /// CGNAT can split a real client's flows, and that player would load
    /// until timed out.
    #[serde(rename = "ValidateUDPSource")]
    pub validate_udp_source: bool,
    /// Vanilla behaviour.
    #[default = true]
    pub write_result_json: bool,
    #[default = 5]
    pub player_loading_timeout_minutes: i64,
    /// The `[name, ISO]` pair for `/INFO`, which vanilla gets from GeoIP.
    /// Empty reports `["na", "na"]`.
    pub country: Vec<String>,

    /// Turns on the CSP extensions that need no further switch: the `0xAB`
    /// kick message, car visibility, SpectateCar and CSPExtraOptions
    /// (docs/04). Off, `0xAB` is ignored as vanilla ignores it.
    #[default = true]
    #[serde(rename = "EnableCSP")]
    pub enable_csp: bool,
    /// Relays `0xAB 0x03` between CSP clients over TCP and sends the CSP
    /// handshake.
    #[default = true]
    pub enable_client_messages: bool,
    /// Relays `0xAB 0x05` over UDP.
    #[default = true]
    pub enable_udp_client_messages: bool,
    /// Rejects a join whose CSP build is older, or absent, with `0x6F`.
    /// 0 admits everyone.
    #[serde(rename = "MinimumCSPVersion")]
    pub minimum_csp_version: i64,
    /// An INI document appended to the welcome message for CSP clients
    /// (docs/04 §4.5).
    #[serde(rename = "CSPExtraOptions")]
    pub csp_extra_options: String,

    // AssettoServer features, each off unless set, except EnableCustomUpdate
    // and the live NetworkBubbleDistance (README.md).
    /// Sends CSP's compact position batch (`0xAB 0x03`) to clients that
    /// announce CUSTOM_UPDATE (docs/04 §4.2). Needs `EnableCSP`. On by
    /// default, unlike AssettoServer.
    #[default = true]
    pub enable_custom_update: bool,
    /// Lua scripts every CSP client loads: paths relative to the cfg
    /// directory, served at /api/scripts/<n>, or http(s) URLs passed on as
    /// they are (docs/04 §4.5, docs/05 §5.5). Needs `EnableCSP`.
    #[serde(rename = "CSPServerScripts")]
    pub csp_server_scripts: Vec<String>,
    /// Runs CSP WeatherFX (`0xAB 0x01`) in place of 0x78 and 0x54, and
    /// refuses clients without it. Needs `EnableCSP`.
    pub enable_weather_fx: bool,
    /// The track's IANA time zone for WeatherFX, such as "Asia/Tokyo".
    /// Empty: `data_track_params.ini` in the cfg directory, else UTC.
    pub track_timezone: String,
    /// Drives `AI=auto|fixed` entry-list slots as traffic along the track's
    /// `ai/fast_lane*.ai` splines. Needs `--content`.
    pub enable_ai: bool,
    pub ai_params: AiParams,

    /// Named lists of Steam IDs: group name → file, one id per line,
    /// relative to the working directory and re-read when it changes.
    pub user_groups: BTreeMap<String, String>,
    /// The group whose members join as admin. Empty: none.
    pub admin_user_group: String,
    /// The group allowed to join, when it lists anyone. Empty: none.
    pub whitelist_user_group: String,
    /// A group banned on top of blacklist.txt; /ban adds to it. Empty: none.
    pub blacklist_user_group: String,
    /// Admin commands a group may run without being admin.
    pub user_group_command_permissions: Vec<UserGroupCommandPermissions>,
    /// AssettoServer's chat commands (/ban with a reason, /whois, /pit,
    /// /forcelights, /setweather, …, and /ping and /time for everyone).
    /// Off: the vanilla commands only.
    pub enable_admin_commands: bool,
    pub ignore_configuration_errors: IgnoreConfigurationErrors,

    /// The built-in AssettoServer plugins to run, by name
    /// ("WordFilterPlugin"); see `plugin_configs`.
    pub enable_plugins: Vec<String>,
    /// Each plugin's settings document, by its tag without the '!'
    /// ("WordFilterConfiguration"); read from the later YAML documents.
    #[serde(skip)]
    pub plugin_configs: BTreeMap<String, Value>,
    /// The track's position, for the sun's altitude (TimeDilationPlugin,
    /// the no-lights kick) and live weather. `None`: data_track_params.ini.
    pub track_latitude: Option<f64>,
    pub track_longitude: Option<f64>,
    /// `/set`, `/api/configuration` and CSP's in-game configuration window
    /// for admins (live_config.rs).
    pub enable_live_configuration: bool,

    /// The settings an admin may change while the server runs; these are
    /// their starting values.
    #[serde(flatten)]
    pub live: LiveExtra,
}

/// The `extra_cfg.yml` settings that may change at runtime
/// (live_config.rs). They sit at the top level of the file.
#[derive(Debug, Clone, Copy, PartialEq, SmartDefault, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LiveExtra {
    /// Forces every car's lights on for everyone else.
    pub force_lights: bool,
    /// Kicks a player whose inputs have not changed for MaxAfkTimeMinutes,
    /// after a warning a minute before. Admins are exempt.
    pub enable_anti_afk: bool,
    #[default = 10]
    pub max_afk_time_minutes: i64,
    /// What counts as activity: any input change, or only one made above
    /// 20 km/h.
    pub afk_kick_behavior: AfkKickBehavior,
    /// Kicks a player whose ping stays above this many ms for more than
    /// MaxPingSeconds. 0 disables it.
    pub max_ping: i64,
    #[default = 10]
    pub max_ping_seconds: i64,
    /// Kicks a player who has not answered the checksum request this many
    /// seconds after loading. 0 disables it.
    pub player_checksum_timeout_seconds: i64,
    /// Beyond this distance a car's positions reach a player at the
    /// optimizer's lowest tier, every fifth update. 0 disables it for
    /// players' cars; AI traffic then uses 500 m. AssettoServer's default too,
    /// though it sends 4 Hz outside it.
    #[default = 500.0]
    pub network_bubble_distance: f32,
    /// Sends a player's cars outside their network bubble once per
    /// CLIENT_SEND_INTERVAL_HZ tick, batched in 0x48s, instead of as single
    /// 0x46s. On by default; vanilla never sends a 0x48.
    #[default = true]
    pub batch_far_positions: bool,
    /// WeatherFX follows the wall clock in the track's time zone.
    pub enable_real_time: bool,
    /// WeatherFX keeps today's date while TIME_OF_DAY_MULT runs the clock.
    #[default = true]
    pub lock_server_date: bool,
    /// The grip a fully wet track loses, as a fraction (AssettoServer's
    /// name).
    pub rain_track_grip_reduction_percent: f64,
    /// Drops chat sent this soon after the sender's last. 0 is vanilla;
    /// AssettoServer uses 1000.
    pub chat_rate_limit_ms: i64,
    #[default = true]
    pub enable_session_voting: bool,
    #[default = true]
    pub enable_kick_voting: bool,
}

/// `AfkKickBehavior`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum AfkKickBehavior {
    /// Any change of lights, gas or steering is activity.
    #[default]
    PlayerInput,
    /// Only a change made above 20 km/h is.
    MinimumSpeed,
}

/// One `UserGroupCommandPermissions` entry.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserGroupCommandPermissions {
    pub user_group: String,
    pub commands: Vec<String>,
}

/// `IgnoreConfigurationErrors`; only the key this server checks.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct IgnoreConfigurationErrors {
    /// Starts with an admin group although Steam IDs are not verified, so
    /// anyone can claim an admin's id.
    pub unsafe_admin_whitelist: bool,
}

impl ExtraConfig {
    /// Parses a YAML document over the defaults.
    pub fn from_yaml(text: &str) -> Result<ExtraConfig, String> {
        // As AssettoServer's file: the first document is these settings,
        // each later one a plugin's, tagged with its configuration type
        // ("--- !WordFilterConfiguration").
        let mut documents = Vec::new();
        for document in serde_norway::Deserializer::from_str(text) {
            documents.push(Value::deserialize(document).map_err(|e| e.to_string())?);
        }
        let mut documents = documents.into_iter();
        let mut root = documents.next().unwrap_or(Value::Null);
        // An empty `Key:` keeps the default, as an absent one does.
        if let Value::Mapping(mapping) = &mut root {
            mapping.retain(|_, value| !value.is_null());
        }
        let mut extra = if root.is_null() {
            ExtraConfig::default() // an empty document
        } else {
            serde_norway::from_value(root).map_err(|e| e.to_string())?
        };
        for document in documents {
            match document {
                Value::Tagged(tagged) => {
                    let tag = tagged.tag.to_string();
                    let name = tag.trim_start_matches('!').to_string();
                    extra.plugin_configs.insert(name, tagged.value);
                }
                Value::Null => {}
                _ => return Err("a plugin configuration document needs a tag, such as \"--- !WordFilterConfiguration\"".to_string()),
            }
        }
        extra.validate()?;
        Ok(extra)
    }

    fn validate(&self) -> Result<(), String> {
        self.ai_params.validate()?;
        if self.live.enable_anti_afk && self.live.max_afk_time_minutes <= 0 {
            return Err("MaxAfkTimeMinutes must be above 0".to_string());
        }
        for group in [
            &self.admin_user_group,
            &self.whitelist_user_group,
            &self.blacklist_user_group,
        ]
        .into_iter()
        .chain(
            self.user_group_command_permissions
                .iter()
                .map(|p| &p.user_group),
        ) {
            if !group.is_empty() && !self.user_groups.contains_key(group) {
                return Err(format!("user group {group:?} is not in UserGroups"));
            }
        }
        Ok(())
    }
}
