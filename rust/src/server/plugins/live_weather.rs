//! LiveWeatherPlugin: follows the real weather at the track's position from
//! OpenWeatherMap, every UpdateIntervalMinutes.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use super::{Plugin, Server, change_weather, require_weatherfx};
use crate::config::Config;
use crate::server::weatherfx::{WeatherType, find_weather_type};

/// `!LiveWeatherConfiguration`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LiveWeatherConfiguration {
    pub open_weather_map_api_key: String,
    #[serde(default = "default_interval")]
    pub update_interval_minutes: u64,
}

fn default_interval() -> u64 {
    10
}

pub struct LiveWeather {
    cfg: LiveWeatherConfiguration,
}

impl LiveWeather {
    pub fn new(server_cfg: &Config, cfg: LiveWeatherConfiguration) -> anyhow::Result<LiveWeather> {
        require_weatherfx(server_cfg, "LiveWeatherPlugin")?;
        if cfg.open_weather_map_api_key.trim().is_empty() || cfg.update_interval_minutes < 1 {
            anyhow::bail!(
                "LiveWeatherPlugin: OpenWeatherMapApiKey is required, UpdateIntervalMinutes >= 1"
            );
        }
        Ok(LiveWeather { cfg })
    }

    async fn update(&self, server: &Server, lat: f64, lon: f64) -> anyhow::Result<()> {
        let url = format!(
            "https://api.openweathermap.org/data/2.5/weather?appid={}&units=metric&lat={lat}&lon={lon}",
            self.cfg.open_weather_map_api_key
        );
        let response = server.http_client.get(&url).send().await?;
        let ok = response.status().is_success();
        let json: serde_json::Value = response.json().await?;
        if !ok {
            anyhow::bail!(
                "OpenWeatherMap returned error {}: {}",
                json["cod"],
                json["message"]
            );
        }
        let id = json["weather"][0]["id"].as_i64().unwrap_or(800);
        let kind = weather_for(id);
        let ambient = json["main"]["temp"].as_f64().unwrap_or(15.0).max(0.0) as f32;
        let pressure = json["main"]["pressure"].as_f64().unwrap_or(1013.0) as f32;
        let humidity = json["main"]["humidity"].as_f64().unwrap_or(50.0) as f32 / 100.0;
        let wind_speed = json["wind"]["speed"].as_f64().unwrap_or(0.0) as f32;
        let wind_direction = json["wind"]["deg"].as_f64().unwrap_or(0.0) as f32;
        tracing::debug!(weather = kind.name, ambient, "live weather");
        change_weather(
            server,
            kind,
            120_000.0,
            Some(ambient),
            humidity,
            Some(pressure),
            Some((wind_speed, wind_direction)),
        );
        Ok(())
    }
}

/// An OpenWeatherMap condition code as a WeatherFX type (Content Manager's
/// mapping, which AssettoServer uses).
pub fn weather_for(code: i64) -> WeatherType {
    let name = match code {
        200..=202 | 211 | 221 | 230..=232 => "Thunderstorm",
        210 => "LightThunderstorm",
        212 | 901 => "HeavyThunderstorm",
        300 | 310 => "LightDrizzle",
        301 | 311 | 321 => "Drizzle",
        302 | 312 => "HeavyDrizzle",
        500 | 520 => "LightRain",
        501 | 511 | 313 | 521 | 531 => "Rain",
        502..=504 | 314 | 522 => "HeavyRain",
        600 | 620 => "LightSnow",
        601 | 621 => "Snow",
        602 | 622 => "HeavySnow",
        615 => "LightSleet",
        616 | 611 => "Sleet",
        612 => "HeavySleet",
        701 => "Mist",
        711 => "Smoke",
        721 => "Haze",
        751 | 731 => "Sand",
        761 | 762 => "Dust",
        741 => "Fog",
        771 => "Squalls",
        781 | 900 => "Tornado",
        800 | 951 | 952 => "Clear",
        801 | 953 | 954 => "FewClouds",
        802 => "ScatteredClouds",
        803 => "BrokenClouds",
        804 => "OvercastClouds",
        902 | 958..=962 => "Hurricane",
        903 => "Cold",
        904 => "Hot",
        905 | 955..=957 => "Windy",
        906 => "Hail",
        _ => "Clear",
    };
    find_weather_type(name).expect("every mapped name is a weather type")
}

impl Plugin for LiveWeather {
    fn name(&self) -> &'static str {
        "LiveWeatherPlugin"
    }

    fn start(self: Arc<Self>, server: &Arc<Server>) {
        let Some((lat, lon)) = server.weatherfx.as_ref().and_then(|fx| fx.track_position()) else {
            tracing::error!(
                "LiveWeatherPlugin needs the track's position (TrackLatitude/TrackLongitude or data_track_params.ini); not started"
            );
            return;
        };
        let s = Arc::clone(server);
        let shutdown = server.shutdown.clone();
        let interval = Duration::from_secs(self.cfg.update_interval_minutes * 60);
        server.spawn_task(async move {
            loop {
                if let Err(err) = self.update(&s, lat, lon).await {
                    tracing::error!(%err, "live weather update failed");
                }
                if shutdown
                    .run_until_cancelled(tokio::time::sleep(interval))
                    .await
                    .is_none()
                {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_map_to_weather() {
        assert_eq!(weather_for(800).name, "Clear");
        assert_eq!(weather_for(502).name, "HeavyRain");
        assert_eq!(weather_for(957).name, "Windy");
        assert_eq!(weather_for(958).name, "Hurricane");
        assert_eq!(weather_for(12345).name, "Clear");
    }
}
