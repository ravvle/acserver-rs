//! TimeDilationPlugin: sets the speed of in-game time from the sun's
//! altitude by a lookup table, so nights can pass faster than days.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use super::{Plugin, Server, require_weatherfx};
use crate::config::Config;

/// `!TimeDilationConfiguration`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TimeDilationConfiguration {
    pub lookup_table: Vec<LutEntry>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LutEntry {
    pub sun_angle: f64,
    pub time_mult: f64,
}

pub struct TimeDilation {
    /// Sorted by sun angle.
    table: Vec<LutEntry>,
}

/// The table's value at `x`, linear between entries and clamped at its ends.
pub fn lookup(table: &[LutEntry], x: f64) -> f64 {
    let (Some(first), Some(last)) = (table.first(), table.last()) else {
        return 1.0;
    };
    if x <= first.sun_angle {
        return first.time_mult;
    }
    if x >= last.sun_angle {
        return last.time_mult;
    }
    for pair in table.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if x <= b.sun_angle {
            let t = (x - a.sun_angle) / (b.sun_angle - a.sun_angle);
            return a.time_mult + (b.time_mult - a.time_mult) * t;
        }
    }
    last.time_mult
}

impl TimeDilation {
    pub fn new(cfg: &Config, settings: TimeDilationConfiguration) -> anyhow::Result<TimeDilation> {
        require_weatherfx(cfg, "TimeDilationPlugin")?;
        if settings.lookup_table.is_empty()
            || settings.lookup_table.iter().any(|e| e.time_mult < 0.0)
        {
            anyhow::bail!("TimeDilationPlugin: LookupTable needs entries with TimeMult >= 0");
        }
        let mut table = settings.lookup_table;
        table.sort_by(|a, b| a.sun_angle.total_cmp(&b.sun_angle));
        Ok(TimeDilation { table })
    }
}

impl Plugin for TimeDilation {
    fn name(&self) -> &'static str {
        "TimeDilationPlugin"
    }

    fn start(self: Arc<Self>, server: &Arc<Server>) {
        let Some(fx) = &server.weatherfx else { return };
        if fx.track_position().is_none() {
            tracing::error!(
                "TimeDilationPlugin needs the track's position (TrackLatitude/TrackLongitude or data_track_params.ini); not started"
            );
            return;
        }
        let s = Arc::clone(server);
        server.spawn_ticker(Duration::from_secs(1), move || {
            if let Some(fx) = &s.weatherfx
                && let Some(altitude) = fx.sun_altitude()
            {
                fx.set_time_mult(Some(lookup(&self.table, altitude)));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_interpolates_and_clamps() {
        let table = [
            LutEntry {
                sun_angle: -10.0,
                time_mult: 8.0,
            },
            LutEntry {
                sun_angle: 10.0,
                time_mult: 1.0,
            },
        ];
        assert_eq!(lookup(&table, -30.0), 8.0);
        assert_eq!(lookup(&table, 30.0), 1.0);
        assert_eq!(lookup(&table, 0.0), 4.5);
    }
}
