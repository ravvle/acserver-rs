// Parts of this module and its plugins are translated from AssettoServer's
// plugins. Copyright (C) 2023 Niewiarowski, compujuckel. AGPL-3.0-or-later.

//! AssettoServer's bundled plugins, built in: each is enabled by naming it
//! in `EnablePlugins` and configured by its tagged document in
//! extra_cfg.yml, as in AssettoServer. None runs by default.

mod auto_moderation;
mod geoip;
mod live_weather;
mod race_challenge;
mod random_weather;
mod report;
mod time_dilation;
mod voting_weather;
mod word_filter;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::de::DeserializeOwned;

use super::Server;
use super::client::Client;
use super::entrycar::PositionUpdate;
use super::scripts::ServerScript;
use crate::config::Config;

/// An HTTP request a plugin may answer.
pub struct PluginRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// Header names in lower case.
    pub headers: HashMap<String, String>,
    pub peer: Option<IpAddr>,
    pub body: Vec<u8>,
}

/// A plugin's HTTP answer.
pub struct PluginResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl PluginResponse {
    pub fn status(status: u16) -> PluginResponse {
        PluginResponse {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }
}

/// One built-in plugin. Every hook has a default that does nothing, so a
/// plugin implements only what it uses.
pub trait Plugin: Send + Sync {
    /// Its EnablePlugins name.
    fn name(&self) -> &'static str;
    /// INI text added to the CSP extra options.
    fn extra_options(&self) -> String {
        String::new()
    }
    /// Lua scripts every CSP client loads.
    fn scripts(&self) -> Vec<ServerScript> {
        Vec::new()
    }
    /// Starts its loops, once the server runs.
    fn start(self: Arc<Self>, _server: &Arc<Server>) {}
    /// A reason to refuse a joining driver by name.
    fn reject_join(&self, _name: &str) -> Option<String> {
        None
    }
    /// Whether a chat line may go out; false drops it.
    fn on_chat(&self, _server: &Arc<Server>, _client: &Arc<Client>, _message: &str) -> bool {
        true
    }
    /// Answers a chat command ("w", "race", …) for any player; `None` when
    /// it is not this plugin's.
    fn on_command(
        &self,
        _server: &Arc<Server>,
        _client: &Arc<Client>,
        _cmd: &str,
        _args: &[&str],
    ) -> Option<String> {
        None
    }
    /// A player finished loading.
    fn on_first_update(&self, _server: &Arc<Server>, _client: &Arc<Client>) {}
    /// A player left.
    fn on_disconnect(&self, _server: &Server, _client: &Client) {}
    /// A player's position update was accepted; `previous` is the last one.
    fn on_position(
        &self,
        _server: &Arc<Server>,
        _client: &Arc<Client>,
        _previous: Option<PositionUpdate>,
        _position: &PositionUpdate,
    ) {
    }
    /// Answers an HTTP request it owns.
    fn http(&self, _server: &Arc<Server>, _request: &PluginRequest) -> Option<PluginResponse> {
        None
    }
    /// Whether `path` is one of its HTTP routes, so the request body is
    /// read for it.
    fn owns_path(&self, _path: &str) -> bool {
        false
    }
}

/// The server's public IP, when a plugin needs it before the server is
/// built: ReportPlugin without a ServerAddress. `None` when none does or the
/// lookup fails.
pub async fn public_ip(cfg: &Config) -> Option<std::net::IpAddr> {
    if !cfg.extra.enable_plugins.iter().any(|p| p == "ReportPlugin") {
        return None;
    }
    let report: report::ReportConfiguration = settings(cfg, "ReportConfiguration")
        .ok()
        .flatten()
        .unwrap_or_default();
    if report.server_address.is_some() {
        return None;
    }
    report::lookup_public_ip().await
}

/// A plugin's settings document, by tag; `None` if the file has none.
fn settings<T: DeserializeOwned>(cfg: &Config, tag: &str) -> anyhow::Result<Option<T>> {
    match cfg.extra.plugin_configs.get(tag) {
        Some(value) => Ok(Some(
            serde_norway::from_value(value.clone()).with_context(|| format!("!{tag}"))?,
        )),
        None => Ok(None),
    }
}

/// A plugin's settings document, which it cannot run without.
fn required_settings<T: DeserializeOwned>(
    cfg: &Config,
    plugin: &str,
    tag: &str,
) -> anyhow::Result<T> {
    settings(cfg, tag)?.with_context(|| {
        format!("{plugin} needs its settings: add a \"--- !{tag}\" document to extra_cfg.yml")
    })
}

/// Builds every plugin in EnablePlugins, checking its settings.
pub fn load(cfg: &Config) -> anyhow::Result<Vec<Arc<dyn Plugin>>> {
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    for name in &cfg.extra.enable_plugins {
        let plugin: Arc<dyn Plugin> = match name.as_str() {
            "AutoModerationPlugin" => Arc::new(auto_moderation::AutoModeration::new(
                cfg,
                settings(cfg, "AutoModerationConfiguration")?.unwrap_or_default(),
            )?),
            "GeoIPPlugin" => Arc::new(geoip::GeoIp::new(required_settings(
                cfg,
                name,
                "GeoIPConfiguration",
            )?)?),
            "LiveWeatherPlugin" => Arc::new(live_weather::LiveWeather::new(
                cfg,
                required_settings(cfg, name, "LiveWeatherConfiguration")?,
            )?),
            "RaceChallengePlugin" => Arc::new(race_challenge::RaceChallenge::new(cfg)),
            "RandomWeatherPlugin" => Arc::new(random_weather::RandomWeather::new(
                cfg,
                settings(cfg, "RandomWeatherConfiguration")?.unwrap_or_default(),
            )?),
            "ReportPlugin" => Arc::new(report::Report::new(
                cfg,
                settings(cfg, "ReportConfiguration")?.unwrap_or_default(),
            )?),
            "TimeDilationPlugin" => Arc::new(time_dilation::TimeDilation::new(
                cfg,
                required_settings(cfg, name, "TimeDilationConfiguration")?,
            )?),
            "VotingWeatherPlugin" => Arc::new(voting_weather::VotingWeather::new(
                cfg,
                settings(cfg, "VotingWeatherConfiguration")?.unwrap_or_default(),
            )?),
            "WordFilterPlugin" => Arc::new(word_filter::WordFilter::new(
                settings(cfg, "WordFilterConfiguration")?.unwrap_or_default(),
            )?),
            "DiscordAuditPlugin" => {
                bail!("DiscordAuditPlugin is not built in; leave it out of EnablePlugins")
            }
            other => bail!("no plugin named {other}"),
        };
        tracing::info!(plugin = plugin.name(), "plugin loaded");
        plugins.push(plugin);
    }
    Ok(plugins)
}

/// Starts a WeatherFX transition to `next`, as AssettoServer's weather
/// plugins do: the road temperature follows the time of day and the new
/// weather; the rest of the air keeps its values unless given.
#[allow(clippy::too_many_arguments)]
pub(crate) fn change_weather(
    server: &Server,
    next: super::weatherfx::WeatherType,
    transition_ms: f64,
    ambient: Option<f32>,
    humidity: f32,
    pressure: Option<f32>,
    wind: Option<(f32, f32)>,
) {
    let Some(fx) = &server.weatherfx else { return };
    let now = fx.conditions();
    let ambient = ambient.unwrap_or(now.ambient);
    let road = super::weatherfx::road_temperature(
        fx.seconds_of_day(),
        f64::from(ambient),
        f64::from(next.temperature_coefficient),
    ) as f32;
    let (wind_speed, wind_direction) = wind.unwrap_or((now.wind_speed, now.wind_direction));
    fx.apply(&super::weatherfx::WeatherChange {
        upcoming: next,
        transition_ms,
        ambient,
        road,
        humidity,
        pressure: pressure.unwrap_or(now.pressure),
        wind_speed,
        wind_direction,
    });
}

/// Weather plugins drive WeatherFX.
fn require_weatherfx(cfg: &Config, plugin: &str) -> anyhow::Result<()> {
    if !cfg.extra.enable_weather_fx {
        bail!("{plugin} needs EnableWeatherFx");
    }
    Ok(())
}

impl Server {
    /// Starts every plugin.
    pub(crate) fn start_plugins(self: &Arc<Self>) {
        for plugin in &self.plugins {
            Arc::clone(plugin).start(self);
        }
    }

    /// The first plugin's reason to refuse `name`, if any.
    pub(crate) fn plugin_rejects_join(&self, name: &str) -> Option<String> {
        self.plugins.iter().find_map(|p| p.reject_join(name))
    }

    /// Whether every plugin lets a chat line through.
    pub(crate) fn plugins_allow_chat(
        self: &Arc<Self>,
        client: &Arc<Client>,
        message: &str,
    ) -> bool {
        self.plugins
            .iter()
            .all(|p| p.on_chat(self, client, message))
    }

    /// A plugin's answer to a player's chat command.
    pub(crate) fn plugin_command(
        self: &Arc<Self>,
        client: &Arc<Client>,
        cmd: &str,
        args: &[&str],
    ) -> Option<String> {
        self.plugins
            .iter()
            .find_map(|p| p.on_command(self, client, cmd, args))
    }

    pub(crate) fn plugins_first_update(self: &Arc<Self>, client: &Arc<Client>) {
        for plugin in &self.plugins {
            plugin.on_first_update(self, client);
        }
    }

    pub(crate) fn plugins_disconnect(&self, client: &Client) {
        for plugin in &self.plugins {
            plugin.on_disconnect(self, client);
        }
    }

    pub(crate) fn plugins_position(
        self: &Arc<Self>,
        client: &Arc<Client>,
        previous: Option<PositionUpdate>,
        position: &PositionUpdate,
    ) {
        for plugin in &self.plugins {
            plugin.on_position(self, client, previous, position);
        }
    }

    /// Whether a plugin owns this HTTP path.
    pub fn plugin_owns_path(&self, path: &str) -> bool {
        self.plugins.iter().any(|p| p.owns_path(path))
    }

    /// The owning plugin's answer to an HTTP request.
    pub fn plugin_http(self: &Arc<Self>, request: &PluginRequest) -> Option<PluginResponse> {
        self.plugins.iter().find_map(|p| p.http(self, request))
    }

    /// A connected client by session id.
    fn client_in(&self, id: u8) -> Option<Arc<Client>> {
        self.entry_cars.get(usize::from(id))?.client()
    }
}

/// The bearing of `target` from a car at `from` heading `heading_degrees`,
/// as AssettoServer's plugins measure it: 180 is straight ahead.
pub(crate) fn bearing(
    from: crate::wire::Vec3,
    heading_degrees: f32,
    target: crate::wire::Vec3,
) -> f32 {
    let mut angle = (from.x - target.x).atan2(from.z - target.z).to_degrees();
    if angle < 0.0 {
        angle += 360.0;
    }
    (angle + heading_degrees) % 360.0
}

/// A 0x46 rotation's heading in degrees, 0..360.
pub(crate) fn heading(p: &PositionUpdate) -> f32 {
    let angle = p.rotation.x.to_degrees();
    if angle < 0.0 { angle + 360.0 } else { angle }
}
