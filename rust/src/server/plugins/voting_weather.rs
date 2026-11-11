//! VotingWeatherPlugin: every VotingIntervalMinutes players vote with
//! `/w <n>` among NumChoices random weathers; the winner fades in over two
//! minutes.

use parking_lot::Mutex;
use smart_default::SmartDefault;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rand::RngExt;
use rand::seq::SliceRandom as _;
use serde::Deserialize;

use super::{Plugin, Server, change_weather, require_weatherfx};
use crate::config::Config;
use crate::server::client::Client;
use crate::server::weatherfx::{WEATHER_TYPES, WeatherType, find_weather_type};

/// `!VotingWeatherConfiguration`.
#[derive(Debug, SmartDefault, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct VotingWeatherConfiguration {
    /// Weather types never offered, by name.
    pub blacklisted_weathers: Vec<String>,
    #[default(3)]
    pub num_choices: usize,
    #[default(10)]
    pub voting_interval_minutes: u64,
    #[default(30)]
    pub voting_duration_seconds: u64,
}

#[derive(Default)]
struct Vote {
    open: bool,
    choices: Vec<(WeatherType, u32)>,
    /// Guids that have voted.
    voted: HashSet<u64>,
}

pub struct VotingWeather {
    cfg: VotingWeatherConfiguration,
    weathers: Vec<WeatherType>,
    vote: Mutex<Vote>,
}

impl VotingWeather {
    pub fn new(
        server_cfg: &Config,
        cfg: VotingWeatherConfiguration,
    ) -> anyhow::Result<VotingWeather> {
        require_weatherfx(server_cfg, "VotingWeatherPlugin")?;
        if cfg.num_choices < 2 || cfg.voting_interval_minutes < 1 || cfg.voting_duration_seconds < 1
        {
            anyhow::bail!(
                "VotingWeatherPlugin: NumChoices >= 2, VotingIntervalMinutes >= 1, VotingDurationSeconds >= 1"
            );
        }
        if cfg.voting_duration_seconds >= cfg.voting_interval_minutes * 60 {
            anyhow::bail!("VotingWeatherPlugin: the vote must be shorter than the interval");
        }
        let mut blacklist = HashSet::new();
        for name in &cfg.blacklisted_weathers {
            let kind = find_weather_type(name)
                .ok_or_else(|| anyhow::anyhow!("VotingWeatherPlugin: no weather type {name}"))?;
            blacklist.insert(kind.id);
        }
        let weathers: Vec<WeatherType> = WEATHER_TYPES
            .iter()
            .filter(|t| !blacklist.contains(&t.id))
            .copied()
            .collect();
        Ok(VotingWeather {
            cfg,
            weathers,
            vote: Mutex::new(Vote::default()),
        })
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, Vote> {
        self.vote.lock()
    }

    fn open_vote(&self, server: &Server) {
        let mut pool = self.weathers.clone();
        pool.shuffle(&mut rand::rng());
        pool.truncate(self.cfg.num_choices);
        let names: Vec<&str> = pool.iter().map(|k| k.name).collect();
        tracing::debug!(choices = ?names, "weather vote opened");
        server.broadcast_server_chat("Vote for next weather:");
        for (i, kind) in pool.iter().enumerate() {
            server.broadcast_server_chat(&format!(" /w {i} - {}", kind.name));
        }
        let mut vote = self.lock();
        vote.choices = pool.into_iter().map(|k| (k, 0)).collect();
        vote.voted.clear();
        vote.open = true;
    }

    fn close_vote(&self, server: &Server) {
        let winner = {
            let mut vote = self.lock();
            vote.open = false;
            let most = vote.choices.iter().map(|(_, n)| *n).max().unwrap_or(0);
            let tied: Vec<WeatherType> = vote
                .choices
                .iter()
                .filter(|(_, n)| *n == most)
                .map(|(k, _)| *k)
                .collect();
            if tied.is_empty() {
                return;
            }
            let votes: Vec<String> = vote
                .choices
                .iter()
                .map(|(k, n)| format!("{}={n}", k.name))
                .collect();
            let winner = tied[rand::rng().random_range(0..tied.len())];
            tracing::debug!(winner = winner.name, ?votes, "weather vote ended");
            winner
        };
        server.broadcast_server_chat(&format!(
            "Weather vote ended. Next weather: {}",
            winner.name
        ));
        change_weather(server, winner, 120_000.0, None, winner.humidity, None, None);
    }
}

impl Plugin for VotingWeather {
    fn name(&self) -> &'static str {
        "VotingWeatherPlugin"
    }

    fn start(self: Arc<Self>, server: &Arc<Server>) {
        let s = Arc::clone(server);
        let shutdown = server.shutdown.clone();
        let duration = Duration::from_secs(self.cfg.voting_duration_seconds);
        let pause = Duration::from_secs(self.cfg.voting_interval_minutes * 60) - duration;
        server.spawn_task(async move {
            loop {
                self.open_vote(&s);
                if shutdown
                    .run_until_cancelled(tokio::time::sleep(duration))
                    .await
                    .is_none()
                {
                    return;
                }
                self.close_vote(&s);
                if shutdown
                    .run_until_cancelled(tokio::time::sleep(pause))
                    .await
                    .is_none()
                {
                    return;
                }
            }
        });
    }

    fn on_command(
        &self,
        _server: &Arc<Server>,
        client: &Arc<Client>,
        cmd: &str,
        args: &[&str],
    ) -> Option<String> {
        if cmd != "w" {
            return None;
        }
        let mut vote = self.lock();
        if !vote.open {
            return Some("There is no ongoing weather vote.".to_string());
        }
        let Some(choice) = args
            .first()
            .and_then(|a| a.parse::<usize>().ok())
            .filter(|c| *c < vote.choices.len())
        else {
            return Some("Invalid choice.".to_string());
        };
        if !vote.voted.insert(client.identity().steam_id()) {
            return Some("You voted already.".to_string());
        }
        vote.choices[choice].1 += 1;
        tracing::debug!(
            name = client.name(),
            weather = vote.choices[choice].0.name,
            "weather vote cast"
        );
        Some(format!(
            "Your vote for {} has been counted.",
            vote.choices[choice].0.name
        ))
    }
}
