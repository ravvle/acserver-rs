//! AutoModerationPlugin: warns, then kicks, players driving without lights
//! at night, driving the wrong way, or standing on the road. CSP players see
//! a flag on screen while they are at fault.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use super::{Plugin, PluginRequest, PluginResponse, Server};
use crate::config::Config;
use crate::proto;
use crate::server::client::Client;
use crate::server::scripts::ServerScript;
use crate::wire::Writer;

/// `!AutoModerationConfiguration`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AutoModerationConfiguration {
    pub wrong_way_kick: KickRuleSettings,
    pub no_lights_kick: KickRuleSettings,
    pub blocking_road_kick: KickRuleSettings,
}

/// The three rules, resolved.
#[derive(Debug)]
struct Rules {
    wrong_way_kick: KickRule,
    no_lights_kick: KickRule,
    blocking_road_kick: KickRule,
}

/// One kick's switch and thresholds as written; a missing threshold takes
/// that rule's default (`KickRule::resolve`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct KickRuleSettings {
    pub enabled: bool,
    pub duration_seconds: Option<u32>,
    /// The wrong-way and no-lights rules apply above this speed.
    pub minimum_speed_kph: Option<f32>,
    /// The blocking-road rule applies below this speed.
    pub maximum_speed_kph: Option<f32>,
}

/// One kick's switch and thresholds.
#[derive(Debug, Clone)]
pub struct KickRule {
    pub enabled: bool,
    pub duration_seconds: u32,
    pub minimum_speed_kph: f32,
    pub maximum_speed_kph: f32,
}

impl KickRuleSettings {
    /// AssettoServer's defaults: 20 s wrong way, 60 s without lights, 30 s
    /// blocking; above 20 km/h, below 5 km/h.
    fn resolve(&self, duration: u32) -> KickRule {
        KickRule {
            enabled: self.enabled,
            duration_seconds: self.duration_seconds.unwrap_or(duration),
            minimum_speed_kph: self.minimum_speed_kph.unwrap_or(20.0),
            maximum_speed_kph: self.maximum_speed_kph.unwrap_or(5.0),
        }
    }
}

/// The flags in the `autoModerationFlag` event.
const NO_LIGHTS: u8 = 1;
const NO_PARKING: u8 = 2;
const WRONG_WAY: u8 = 4;

/// The LuaPacketType of `autoModerationFlag { uint8_t flags; }`, computed
/// with the reference xxHash3 library (docs/04 §4.4).
const LUA_AUTO_MODERATION_FLAG: u32 = 0x7909_6D99;

/// Below nautical twilight it is night.
const NIGHT_SUN_ALTITUDE: f64 = -12.0;

/// One car's counters.
#[derive(Debug, Default, Clone)]
struct Watch {
    flags: u8,
    no_lights: u32,
    no_lights_warned: bool,
    wrong_way: u32,
    wrong_way_warned: bool,
    blocking: u32,
    blocking_warned: bool,
}

pub struct AutoModeration {
    cfg: Rules,
    watches: Mutex<Vec<Watch>>,
    client_messages: bool,
}

impl AutoModeration {
    pub fn new(
        server_cfg: &Config,
        settings: AutoModerationConfiguration,
    ) -> anyhow::Result<AutoModeration> {
        let cfg = Rules {
            wrong_way_kick: settings.wrong_way_kick.resolve(20),
            no_lights_kick: settings.no_lights_kick.resolve(60),
            blocking_road_kick: settings.blocking_road_kick.resolve(30),
        };
        if (cfg.wrong_way_kick.enabled || cfg.blocking_road_kick.enabled)
            && !server_cfg.extra.enable_ai
        {
            anyhow::bail!(
                "AutoModerationPlugin: the wrong-way and blocking-road kicks need AI traffic (EnableAi), for the road"
            );
        }
        Ok(AutoModeration {
            cfg,
            watches: Mutex::new(vec![Watch::default(); 256]),
            client_messages: server_cfg.extra.enable_client_messages,
        })
    }

    /// One second's check of every loaded, non-admin player.
    fn check(&self, server: &Server) {
        let night = match &server.weatherfx {
            Some(fx) => fx.sun_altitude(),
            // Without WeatherFX the vanilla sun angle stands in for it.
            None => Some(f64::from(server.weather.snapshot().sun_angle)),
        }
        .is_some_and(|a| a < NIGHT_SUN_ALTITUDE);
        let lane_radius2 = server
            .traffic
            .as_ref()
            .map_or(0.0, |t| (t.lane_width() / 2.0 * 1.25).powi(2));
        let mut watches = self.watches.lock();
        for client in server.connected_clients() {
            if client.is_admin()
                || !client
                    .has_sent_first_update
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                continue;
            }
            let Some(pos) = server.entry_cars[usize::from(client.session_id())].last_position()
            else {
                continue;
            };
            let w = &mut watches[usize::from(client.session_id())];
            let old = w.flags;
            let speed2 = pos.velocity.length_squared();
            let on_road = server.traffic.as_ref().and_then(|t| {
                t.spline()
                    .world_to_spline(pos.position)
                    .filter(|(_, d2)| *d2 < lane_radius2)
                    .map(|(id, _)| t.spline().forward(id).dot(pos.velocity) < 0.0)
            });

            let rule = &self.cfg.no_lights_kick;
            let fault = rule.enabled
                && night
                && pos.status_flags & 0x20 == 0
                && speed2 > (rule.minimum_speed_kph / 3.6).powi(2);
            if step(
                server,
                &client,
                rule,
                fault,
                &mut w.no_lights,
                &mut w.no_lights_warned,
                "It is currently night, please turn on your lights or you will be kicked.",
                "driving without lights",
            ) {
                w.flags |= NO_LIGHTS;
            } else {
                w.flags &= !NO_LIGHTS;
            }

            let rule = &self.cfg.wrong_way_kick;
            let fault = rule.enabled
                && on_road == Some(true)
                && speed2 > (rule.minimum_speed_kph / 3.6).powi(2);
            if step(
                server,
                &client,
                rule,
                fault,
                &mut w.wrong_way,
                &mut w.wrong_way_warned,
                "You are driving the wrong way! Turn around or you will be kicked.",
                "driving the wrong way",
            ) {
                w.flags |= WRONG_WAY;
            } else {
                w.flags &= !WRONG_WAY;
            }

            let rule = &self.cfg.blocking_road_kick;
            let fault = rule.enabled
                && on_road.is_some()
                && speed2 < (rule.maximum_speed_kph / 3.6).powi(2);
            if step(
                server,
                &client,
                rule,
                fault,
                &mut w.blocking,
                &mut w.blocking_warned,
                "You are blocking the road! Please move or teleport to pits, or you will be kicked.",
                "blocking the road",
            ) {
                w.flags |= NO_PARKING;
            } else {
                w.flags &= !NO_PARKING;
            }

            if self.client_messages && client.is_csp() && old != w.flags {
                let mut m = Writer::with_id(proto::EXTENDED);
                m.u8(proto::EXT_TCP_CLIENT_MESSAGE);
                m.u8(proto::SERVER_SESSION_ID);
                m.u16(proto::CSP_LUA_MESSAGE);
                m.u32(LUA_AUTO_MODERATION_FLAG);
                m.u8(w.flags);
                server.send_tcp(&client, m.bytes());
            }
        }
    }
}

/// Counts a second at fault: warns at half the rule's duration, kicks past
/// it, and resets once the fault is gone. Returns whether it is at fault.
#[allow(clippy::too_many_arguments)]
fn step(
    server: &Server,
    client: &Arc<Client>,
    rule: &KickRule,
    fault: bool,
    seconds: &mut u32,
    warned: &mut bool,
    warning: &str,
    reason: &str,
) -> bool {
    if !fault {
        *seconds = 0;
        *warned = false;
        return false;
    }
    *seconds += 1;
    if *seconds > rule.duration_seconds {
        server.kick_for(client, reason);
    } else if !*warned && *seconds > rule.duration_seconds / 2 {
        *warned = true;
        server.send_chat_to(client, warning);
    }
    true
}

impl Plugin for AutoModeration {
    fn name(&self) -> &'static str {
        "AutoModerationPlugin"
    }

    fn scripts(&self) -> Vec<ServerScript> {
        if !self.client_messages {
            return Vec::new();
        }
        vec![ServerScript::Local {
            name: "automoderation.lua".to_string(),
            source: include_bytes!("auto_moderation/automoderation.lua").to_vec(),
        }]
    }

    fn start(self: Arc<Self>, server: &Arc<Server>) {
        let s = Arc::clone(server);
        server.spawn_ticker(Duration::from_secs(1), move || self.check(&s));
    }

    fn on_disconnect(&self, _server: &Server, client: &Client) {
        let mut watches = self.watches.lock();
        watches[usize::from(client.session_id())] = Watch::default();
    }

    fn owns_path(&self, path: &str) -> bool {
        path.starts_with("/automoderation/")
    }

    fn http(&self, _server: &Arc<Server>, request: &PluginRequest) -> Option<PluginResponse> {
        let body: &[u8] = match request.path.as_str() {
            "/automoderation/no_lights.png" => include_bytes!("auto_moderation/no_lights.png"),
            "/automoderation/no_parking.png" => include_bytes!("auto_moderation/no_parking.png"),
            "/automoderation/wrong_way.png" => include_bytes!("auto_moderation/wrong_way.png"),
            _ => return None,
        };
        Some(PluginResponse {
            status: 200,
            content_type: "image/png",
            body: body.to_vec(),
        })
    }
}
