//! RandomWeatherPlugin: moves to a randomly picked weather type at random
//! intervals, weighted by WeatherWeights.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use serde::Deserialize;
use smart_default::SmartDefault;

use super::{Plugin, Server, change_weather, require_weatherfx};
use crate::config::Config;
use crate::server::weatherfx::{WEATHER_TYPES, WeatherType, find_weather_type};

/// `!RandomWeatherConfiguration`.
#[derive(Debug, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct RandomWeatherConfiguration {
    /// Weight per weather type name; types left out weigh 1.
    pub weather_weights: HashMap<String, f32>,
    #[default(5)]
    pub min_weather_duration_minutes: u64,
    #[default(30)]
    pub max_weather_duration_minutes: u64,
    #[default(120)]
    pub min_transition_duration_seconds: u64,
    #[default(600)]
    pub max_transition_duration_seconds: u64,
}

pub struct RandomWeather {
    cfg: RandomWeatherConfiguration,
    /// Each type with weight > 0 and its cumulative share, ascending.
    choices: Vec<(WeatherType, f32)>,
}

impl RandomWeather {
    pub fn new(
        server_cfg: &Config,
        cfg: RandomWeatherConfiguration,
    ) -> anyhow::Result<RandomWeather> {
        require_weatherfx(server_cfg, "RandomWeatherPlugin")?;
        RandomWeather::from_settings(cfg)
    }

    /// The plugin without the WeatherFX check, for tests.
    fn from_settings(cfg: RandomWeatherConfiguration) -> anyhow::Result<RandomWeather> {
        if cfg.min_weather_duration_minutes > cfg.max_weather_duration_minutes
            || cfg.min_transition_duration_seconds > cfg.max_transition_duration_seconds
        {
            anyhow::bail!("RandomWeatherPlugin: a Min duration is above its Max");
        }
        let mut weights: Vec<(WeatherType, f32)> =
            WEATHER_TYPES.iter().map(|t| (*t, 1.0)).collect();
        for (name, weight) in &cfg.weather_weights {
            let Some(kind) = find_weather_type(name) else {
                anyhow::bail!("RandomWeatherPlugin: no weather type {name}");
            };
            if *weight < 0.0 {
                anyhow::bail!("RandomWeatherPlugin: the weight of {name} is negative");
            }
            weights[usize::from(kind.id)].1 = *weight;
        }
        let total: f32 = weights.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            anyhow::bail!("RandomWeatherPlugin: every weather weighs 0");
        }
        let mut sum = 0.0;
        let mut choices = Vec::new();
        for (kind, weight) in weights {
            if weight > 0.0 {
                sum += weight / total;
                choices.push((kind, sum));
            }
        }
        Ok(RandomWeather { cfg, choices })
    }

    fn pick(&self, roll: f32) -> WeatherType {
        self.choices
            .iter()
            .find(|(_, share)| roll < *share)
            .or(self.choices.last())
            .map(|(kind, _)| *kind)
            .expect("at least one weather has weight")
    }

    /// Picks the next weather, starts the move to it, and returns how long
    /// until the next change.
    fn change(&self, server: &Server) -> Duration {
        let mut rng = rand::rng();
        let weather_ms = 60_000
            * rng.random_range(
                self.cfg.min_weather_duration_minutes..=self.cfg.max_weather_duration_minutes,
            );
        let transition_ms = 1000
            * rng.random_range(
                self.cfg.min_transition_duration_seconds..=self.cfg.max_transition_duration_seconds,
            );
        let next = self.pick(rng.random::<f32>());
        tracing::info!(
            weather = next.name,
            transition_s = transition_ms / 1000,
            duration_min = weather_ms / 60_000,
            "random weather"
        );
        change_weather(
            server,
            next,
            transition_ms as f64,
            None,
            next.humidity,
            None,
            None,
        );
        Duration::from_millis(weather_ms + transition_ms)
    }
}

impl Plugin for RandomWeather {
    fn name(&self) -> &'static str {
        "RandomWeatherPlugin"
    }

    fn start(self: Arc<Self>, server: &Arc<Server>) {
        let s = Arc::clone(server);
        let shutdown = server.shutdown.clone();
        server.spawn_task(async move {
            loop {
                let wait = self.change(&s);
                if shutdown
                    .run_until_cancelled(tokio::time::sleep(wait))
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
    fn weights_shape_the_pick() {
        let mut settings = RandomWeatherConfiguration::default();
        for t in WEATHER_TYPES.iter() {
            settings.weather_weights.insert(t.name.to_string(), 0.0);
        }
        settings.weather_weights.insert("Rain".into(), 1.0);
        settings.weather_weights.insert("Clear".into(), 3.0);
        let plugin = RandomWeather::from_settings(settings).expect("valid");
        assert_eq!(plugin.pick(0.1).name, "Rain");
        assert_eq!(plugin.pick(0.5).name, "Clear");
        assert_eq!(plugin.pick(0.99).name, "Clear");
    }
}
