//! Ambient conditions and the dynamic track.

use parking_lot::Mutex;
use rand::Rng;

use crate::config::Config;

/// A copy of the current conditions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WeatherSnapshot {
    pub graphics: String,
    pub ambient: f32,
    pub road: f32,
    pub wind_speed: i16,
    pub wind_direction: i16,
    pub track_grip: f32,
    pub sun_angle: f32,
}

#[derive(Debug)]
struct WeatherInner {
    snap: WeatherSnapshot,
    /// The session's starting grip, before laps rubber it in. A restart
    /// returns to it (`reset_lap_grip`).
    base_grip: f32,
}

/// The current conditions, rolled from [WEATHER_n] at start and on every
/// session advance.
#[derive(Debug)]
pub struct WeatherState {
    inner: Mutex<WeatherInner>,
}

/// Vanilla's sun speed at TIME_OF_DAY_MULT=1: 0.264° per 60 s (measured on
/// v1.15), about 160° over a 10-hour day.
const SUN_DEGREES_PER_SECOND: f64 = 0.0044;

impl WeatherState {
    pub fn new(cfg: &Config) -> WeatherState {
        let state = WeatherState {
            inner: Mutex::new(WeatherInner {
                snap: WeatherSnapshot {
                    track_grip: f32::from(cfg.dynamic.session_start) / 100.0,
                    sun_angle: cfg.server.sun_angle,
                    ..WeatherSnapshot::default()
                },
                base_grip: 0.0,
            }),
        };
        state.reroll(cfg);
        // As vanilla's InitDynamicTrack: SESSION_START ± RANDOMNESS.
        state.reset_grip(cfg);
        state
    }

    /// Picks new conditions as vanilla's UpdateWeather does (at start and on
    /// session advance, not restart): a random [WEATHER_n]; ambient ±
    /// VARIATION_AMBIENT; road = ambient + BASE_TEMPERATURE_ROAD ±
    /// VARIATION_ROAD; wind speed in [MIN, MAX); direction ±
    /// rand(WIND_VARIATION_DIRECTION), kept in [0, 360].
    pub fn reroll(&self, cfg: &Config) {
        let index = rand::rng().random_range(0..cfg.weathers.len());
        self.roll(cfg, index);
    }

    /// Rolls conditions from [WEATHER_<index>], as `reroll` does from a
    /// random one (/setweather).
    pub fn roll(&self, cfg: &Config, index: usize) {
        let mut rng = rand::rng();
        let wc = &cfg.weathers[index];
        let ambient =
            wc.base_temp_ambient + (2.0 * rng.random::<f32>() - 1.0) * wc.variation_ambient;
        let road =
            ambient + wc.road_temp_offset + (2.0 * rng.random::<f32>() - 1.0) * wc.variation_road;

        let spread = wc.wind_base_speed_max - wc.wind_base_speed_min;
        // Vanilla: an empty range collapses to MAX.
        let mut speed = wc.wind_base_speed_min + spread;
        if spread > 0 {
            speed = wc.wind_base_speed_min + rng.random_range(0..spread);
        }

        let mut delta = 0;
        if wc.wind_variation_direction > 0 {
            delta = rng.random_range(0..i32::from(wc.wind_variation_direction));
        }
        let mut direction = i32::from(wc.wind_base_direction);
        if rng.random_range(0..10) % 2 == 1 {
            direction -= delta;
        } else {
            direction += delta;
        }
        if direction < 0 {
            direction += 360;
        }
        if direction > 360 {
            direction -= 360;
        }

        let mut w = self.inner.lock();
        w.snap.graphics = wc.graphics.clone();
        w.snap.ambient = ambient;
        w.snap.road = road;
        w.snap.wind_speed = i16::from(speed);
        // 0..=360: both inputs are at most 360 (checked at load).
        w.snap.wind_direction = direction as i16;
    }

    /// A new session's grip, as vanilla's OnNewSession does for any index but
    /// 0: SESSION_START blended with SESSION_TRANSFER% of the old grip
    /// (clamped at 0), ± RANDOMNESS%. Restarts skip it.
    pub fn transfer_grip(&self, cfg: &Config) {
        let start = f32::from(cfg.dynamic.session_start) / 100.0;
        let transfer = f32::from(cfg.dynamic.session_transfer) / 100.0;
        let randomness = f32::from(cfg.dynamic.randomness) / 100.0;
        let noise = 2.0 * rand::random::<f32>() - 1.0;

        let mut w = self.inner.lock();
        let old = w.snap.track_grip.max(0.0);
        w.snap.track_grip = start * (1.0 - transfer) + transfer * old + randomness * noise;
        w.base_grip = w.snap.track_grip;
    }

    /// Starts from SESSION_START ± RANDOMNESS with nothing carried over, as
    /// vanilla does for session index 0 (at start and on a LOOP_MODE wrap).
    pub fn reset_grip(&self, cfg: &Config) {
        let start = f32::from(cfg.dynamic.session_start) / 100.0;
        let randomness = f32::from(cfg.dynamic.randomness) / 100.0;
        let noise = 2.0 * rand::random::<f32>() - 1.0;

        let mut w = self.inner.lock();
        w.snap.track_grip = start + randomness * noise;
        w.base_grip = w.snap.track_grip;
    }

    /// Returns to the session's starting grip, as vanilla's RestartSession
    /// does by zeroing the lap counter.
    pub fn reset_lap_grip(&self) {
        let mut w = self.inner.lock();
        w.snap.track_grip = w.base_grip;
    }

    /// A copy of the current weather.
    pub fn snapshot(&self) -> WeatherSnapshot {
        self.inner.lock().snap.clone()
    }

    pub fn set_sun_angle(&self, angle: f32) {
        self.inner.lock().snap.sun_angle = angle;
    }

    /// Adds one lap's rubber: 1%/LAP_GAIN, capped at 1.0. LAP_GAIN=0
    /// disables it.
    pub fn add_lap_grip(&self, lap_gain: u16) {
        if lap_gain == 0 {
            return;
        }
        let mut w = self.inner.lock();
        w.snap.track_grip += 0.01 / f32::from(lap_gain);
        if w.snap.track_grip > 1.0 {
            w.snap.track_grip = 1.0;
        }
    }

    /// Sets the track grip until laps or the next session change it
    /// (/setgrip).
    pub fn set_grip(&self, grip: f32) {
        self.inner.lock().snap.track_grip = grip;
    }

    pub fn grip(&self) -> f32 {
        self.inner.lock().snap.track_grip
    }
}

/// The sun angle `session_seconds` into the current session, as vanilla's
/// Step recomputes it every pass: SUN_ANGLE plus the sun speed times the
/// multiplier per second since the session's start, clamped to -80°..80° so
/// the sun stops at sunset. Before a race starts the time is negative, so the
/// sun sits below SUN_ANGLE through the countdown.
pub fn sun_at(base: f32, multiplier: f32, session_seconds: f64) -> f32 {
    // Vanilla moves the sun even at TIME_OF_DAY_MULT=0, as at 1 (measured).
    let multiplier = if multiplier <= 0.0 { 1.0 } else { multiplier };
    let angle = f64::from(base) + session_seconds * SUN_DEGREES_PER_SECOND * f64::from(multiplier);
    angle.clamp(-80.0, 80.0) as f32
}
