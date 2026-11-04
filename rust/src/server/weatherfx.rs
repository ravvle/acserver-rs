//! CSP WeatherFX (`EnableWeatherFx`, docs/04 §4.2 `0xAB 0x01`): the server
//! owns the weather type, its transitions, rain and the date and time, and
//! sends them every second in place of 0x78 and 0x54. The weather types,
//! rain model and time handling follow AssettoServer, so its weather
//! settings carry over.

use parking_lot::Mutex;
use std::sync::atomic::Ordering;

use chrono::{DateTime, Datelike as _, NaiveDate, TimeZone as _, Timelike as _, Utc};
use chrono_tz::Tz;

use super::Server;
use super::client::Client;
use crate::config::Config;
use crate::proto;
use crate::wire::Writer;

/// One WeatherFX weather type: its id on the wire and the conditions it
/// brings (AssettoServer's DefaultWeatherTypeProvider).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherType {
    pub id: u8,
    pub name: &'static str,
    pub rain_intensity: f32,
    pub rain_wetness: f32,
    pub rain_water: f32,
    /// How much sun reaches the track, 0..1, which dries it.
    pub sun: f32,
    /// How much the sun warms the road (`road_temperature`).
    pub temperature_coefficient: f32,
    pub humidity: f32,
}

// A table row; one argument per column reads best in WEATHER_TYPES.
#[allow(clippy::too_many_arguments)]
const fn wt(
    id: u8,
    name: &'static str,
    rain_intensity: f32,
    rain_wetness: f32,
    rain_water: f32,
    sun: f32,
    temperature_coefficient: f32,
    humidity: f32,
) -> WeatherType {
    WeatherType {
        id,
        name,
        rain_intensity,
        rain_wetness,
        rain_water,
        sun,
        temperature_coefficient,
        humidity,
    }
}

/// Every WeatherFX type, indexed by id.
pub const WEATHER_TYPES: [WeatherType; 33] = [
    wt(0, "LightThunderstorm", 0.1, 0.0, 0.0, 0.2, 0.7, 0.6),
    wt(1, "Thunderstorm", 0.2, 0.0, 0.0, 0.1, 0.2, 0.8),
    wt(2, "HeavyThunderstorm", 0.4, 0.0, 0.0, 0.0, -0.2, 1.0),
    wt(3, "LightDrizzle", 0.05, 0.05, 0.0, 0.5, 0.1, 0.7),
    wt(4, "Drizzle", 0.15, 0.1, 0.05, 0.1, -0.1, 0.85),
    wt(5, "HeavyDrizzle", 0.25, 0.2, 0.1, 0.0, -0.3, 1.0),
    wt(6, "LightRain", 0.3, 0.3, 0.1, 0.25, 0.01, 1.0),
    wt(7, "Rain", 0.6, 0.6, 0.3, 0.05, -0.2, 1.0),
    wt(8, "HeavyRain", 1.0, 1.0, 0.5, 0.0, -0.5, 1.0),
    wt(9, "LightSnow", 0.0, 0.2, 0.05, 0.5, -0.7, 1.0),
    wt(10, "Snow", 0.0, 0.3, 0.1, 0.0, -0.8, 1.0),
    wt(11, "HeavySnow", 0.0, 0.4, 0.15, 0.0, -0.9, 1.0),
    wt(12, "LightSleet", 0.2, 0.3, 0.1, 0.1, -1.0, 1.0),
    wt(13, "Sleet", 0.5, 0.5, 0.2, 0.0, -1.0, 1.0),
    wt(14, "HeavySleet", 0.8, 0.7, 0.3, 0.0, -1.0, 1.0),
    wt(15, "Clear", 0.0, 0.0, 0.0, 1.0, 1.0, 0.0),
    wt(16, "FewClouds", 0.0, 0.0, 0.0, 1.0, 1.0, 0.0),
    wt(17, "ScatteredClouds", 0.0, 0.0, 0.0, 0.75, 0.8, 0.0),
    wt(18, "BrokenClouds", 0.0, 0.0, 0.0, 0.5, 0.1, 0.0),
    wt(19, "OvercastClouds", 0.0, 0.0, 0.0, 0.2, 0.01, 0.0),
    wt(20, "Fog", 0.0, 0.2, 0.0, 0.5, -0.3, 1.0),
    wt(21, "Mist", 0.0, 0.1, 0.0, 0.5, -0.2, 0.95),
    wt(22, "Smoke", 0.0, 0.0, 0.0, 0.0, -0.2, 0.0),
    wt(23, "Haze", 0.0, 0.0, 0.0, 1.0, 0.9, 0.8),
    wt(24, "Sand", 0.0, 0.0, 0.0, 1.0, 1.0, 0.0),
    wt(25, "Dust", 0.0, 0.0, 0.0, 1.0, 1.0, 0.5),
    wt(26, "Squalls", 0.0, 0.0, 0.0, 0.5, -0.5, 0.0),
    wt(27, "Tornado", 0.3, 0.0, 0.0, 0.0, -0.3, 0.0),
    wt(28, "Hurricane", 0.5, 0.0, 0.0, 0.0, -0.6, 0.0),
    wt(29, "Cold", 0.0, 0.0, 0.0, 1.0, 0.0, 1.0),
    wt(30, "Hot", 0.0, 0.0, 0.0, 1.0, 0.0, 0.0),
    wt(31, "Windy", 0.0, 0.0, 0.0, 0.85, 0.3, 0.0),
    wt(32, "Hail", 0.5, 0.5, 0.1, 0.3, -1.0, 1.0),
];

/// The type used when GRAPHICS names none.
const DEFAULT_TYPE: u8 = 15; // Clear

/// A weather type by id or (case-insensitive) name.
pub fn find_weather_type(name_or_id: &str) -> Option<WeatherType> {
    if let Ok(id) = name_or_id.parse::<usize>() {
        return WEATHER_TYPES.get(id).copied();
    }
    WEATHER_TYPES
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name_or_id))
        .copied()
}

/// The WeatherFX type in a [WEATHER_n] GRAPHICS value: AssettoServer's
/// "3_clear_type=17" form, `_`-separated `key=value` pairs.
pub fn graphics_weather_type(graphics: &str) -> Option<WeatherType> {
    graphics
        .split('_')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "type")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .and_then(|id| WEATHER_TYPES.get(id).copied())
}

/// Seconds after midnight at which the sun stands at `sun_angle`, as
/// AssettoServer converts SUN_ANGLE: 16° per hour around 13:00.
pub fn seconds_from_sun_angle(sun_angle: f64) -> f64 {
    sun_angle * (50400.0 - 46800.0) / 16.0 + 46800.0
}

/// The road temperature at `seconds` after midnight for `ambient` under a
/// weather of `coefficient`: Content Manager's curve, which AssettoServer
/// uses (actools Game.Properties GetRoadTemperature), warmest early
/// afternoon and flat outside 08:00–18:00.
pub fn road_temperature(seconds: f64, ambient: f64, coefficient: f64) -> f64 {
    const MIN: f64 = 8.0 * 3600.0;
    const MAX: f64 = 18.0 * 3600.0;
    if !(MIN..=MAX).contains(&seconds) {
        let low = road_temperature(MIN, ambient, coefficient);
        let high = road_temperature(MAX, ambient, coefficient);
        let seconds = if seconds > MAX {
            seconds - 86400.0
        } else {
            seconds
        };
        let (from, to) = (MIN, MAX - 86400.0);
        return low + (high - low) * (seconds - from) / (to - from);
    }
    let time = (seconds / 3600.0 - 7.0) * 0.04167;
    let k = if coefficient == 0.0 { 1.0 } else { coefficient };
    ambient
        * (1.0
            + 5.33332
                * k
                * (1.0 - time)
                * ((-6.0 * time).exp() * (6.0 * time).sin() + 0.25)
                * (0.9 * time).sin())
}

/// The sun's altitude in degrees at `at` seen from `latitude`, `longitude`
/// (degrees): the low-precision solar position of suncalc.
pub fn sun_altitude_degrees(at: DateTime<Utc>, latitude: f64, longitude: f64) -> f64 {
    use std::f64::consts::PI;
    let rad = PI / 180.0;
    let days = at.timestamp_millis() as f64 / 86_400_000.0 - 0.5 + 2_440_588.0 - 2_451_545.0;
    let obliquity = rad * 23.4397;
    let anomaly = rad * (357.5291 + 0.985_600_28 * days);
    let centre = rad
        * (1.9148 * anomaly.sin() + 0.02 * (2.0 * anomaly).sin() + 0.0003 * (3.0 * anomaly).sin());
    let ecliptic = anomaly + centre + rad * 102.9372 + PI;
    let declination = (ecliptic.sin() * obliquity.sin()).asin();
    let right_ascension = (ecliptic.sin() * obliquity.cos()).atan2(ecliptic.cos());
    let sidereal = rad * (280.16 + 360.985_623_5 * days) + rad * longitude;
    let hour_angle = sidereal - right_ascension;
    let phi = rad * latitude;
    (phi.sin() * declination.sin() + phi.cos() * declination.cos() * hour_angle.cos())
        .asin()
        .to_degrees()
}

/// A weather change from a plugin: a transition to `upcoming` over
/// `transition_ms` with new air conditions. Rain, wetness and puddles carry
/// on from the current state, as AssettoServer's weather plugins set them.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherChange {
    pub upcoming: WeatherType,
    pub transition_ms: f64,
    pub ambient: f32,
    pub road: f32,
    pub humidity: f32,
    pub pressure: f32,
    pub wind_speed: f32,
    pub wind_direction: f32,
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// The conditions sent in `0xAB 0x01`.
#[derive(Debug, Clone, PartialEq)]
pub struct Conditions {
    pub kind: WeatherType,
    pub upcoming: WeatherType,
    /// Progress from `kind` to `upcoming`, smoothed, 0..=65535.
    pub transition_value: u16,
    transition_progress: f64,
    transition_duration_ms: f64,
    pub ambient: f32,
    pub road: f32,
    pub wind_speed: f32,
    pub wind_direction: f32,
    pub humidity: f32,
    pub pressure: f32,
    pub rain_intensity: f32,
    pub rain_wetness: f32,
    pub rain_water: f32,
    pub track_grip: f32,
}

/// The rain model AssettoServer takes from Sol 2.2: rain wets the track and
/// fills puddles, warmth and sun dry it, and wetness and water cost grip.
#[derive(Debug, Default)]
struct RainModel {
    /// Wetness before clamping; above 1 the puddles grow.
    wetness: f64,
}

const DRYING_AIR_K: f64 = 1.0;
const DRYING_AIR_MIN: f64 = 8.0;
const DRYING_ROAD_K: f64 = 1.0;
const DRYING_ROAD_MIN: f64 = 12.0;
const DRYING_SUN_K: f64 = 3.0;
const RAIN_WETTING_K: f64 = 75.0;
const HUMIDITY_WETTING_K: f64 = 1.0;
const PUDDLES_DRYING_K: f64 = 1.0;
const PUDDLES_RAIN_K: f64 = 6.0;
const PUDDLES_DRAIN: f64 = 0.25;
const TIME_K: f64 = 0.001;

/// 1 at `base`, rising to 2 at 40° and falling to 0 at 0°, clamped.
fn temperature_offset(base: f64, temp: f64) -> f64 {
    let result = if temp < base {
        1.0 - (temp - base).abs() / base
    } else if temp > base {
        1.0 + (temp - base).abs() / (40.0 - base)
    } else {
        1.0
    };
    result.clamp(0.0, 2.0)
}

fn temperature_lerp(temp: f64, base: f64, cold: f64, hot: f64) -> f64 {
    lerp(cold, hot, temperature_offset(base, temp) * 0.5)
}

impl RainModel {
    fn update_water(&mut self, c: &mut Conditions, sun: f64, dt: f64) {
        let ambient = f64::from(c.ambient);
        let road = f64::from(c.road);
        let drying = (DRYING_AIR_K * temperature_lerp(ambient, DRYING_AIR_MIN, -1.0, 2.0)).max(0.0)
            + (DRYING_ROAD_K * temperature_lerp(road, DRYING_ROAD_MIN, -1.0, 2.0)).max(0.0)
            + DRYING_SUN_K * sun;
        let intensity = f64::from(c.rain_intensity);
        let wetting = dt
            * TIME_K
            * (-drying
                + RAIN_WETTING_K * intensity.powf(1.7)
                + (HUMIDITY_WETTING_K
                    * f64::from(c.humidity)
                    * temperature_lerp(ambient, DRYING_AIR_MIN, 1.0, -1.0))
                .max(0.0));
        self.wetness += wetting;
        let drain = PUDDLES_DRAIN * temperature_lerp(road, DRYING_ROAD_MIN, 0.0, 3.0);
        let mut water = f64::from(c.rain_water);
        if wetting > 0.0 {
            if self.wetness >= 1.0 {
                let room = (intensity - water).max(0.0);
                let growth = room.powf(3.0 - 2.0 * intensity.powi(4)).clamp(0.0, 1.0);
                water = (water
                    + dt * TIME_K * PUDDLES_RAIN_K * (self.wetness - 1.0) * intensity * growth)
                    .clamp(0.0, 1.0);
            }
            water = (water - dt * TIME_K * drain).clamp(0.0, 1.0);
        } else if wetting < 0.0 {
            water = (water + dt * TIME_K * (PUDDLES_DRYING_K * (self.wetness - 1.0) - drain))
                .clamp(0.0, 1.0);
        }
        c.rain_water = water as f32;
        c.rain_wetness = (self.wetness + (water * 5.0).min(0.1)).clamp(0.0, 1.0) as f32;
        self.wetness = self.wetness.clamp(0.0, 3.0);
        if wetting < 0.0 && self.wetness >= 1.0 {
            self.wetness = 1.0;
        }
    }

    /// Advances transitions and rain by `dt_ms`, then sets the grip from
    /// `base_grip` less what the water takes.
    fn update(&mut self, c: &mut Conditions, base_grip: f64, reduction: f64, dt_ms: f64) {
        if c.kind.id != c.upcoming.id {
            c.transition_progress += dt_ms / c.transition_duration_ms.max(1.0);
            if c.transition_progress >= 1.0 {
                c.kind = c.upcoming;
                c.transition_progress = 0.0;
                c.transition_value = 0;
                c.rain_intensity = c.kind.rain_intensity;
            } else {
                c.transition_value = (smoothstep(c.transition_progress) * 65535.0) as u16;
                c.rain_intensity = lerp(
                    f64::from(c.kind.rain_intensity),
                    f64::from(c.upcoming.rain_intensity),
                    c.transition_progress,
                ) as f32;
            }
        }
        let sun = lerp(
            f64::from(c.kind.sun),
            f64::from(c.upcoming.sun),
            c.transition_progress,
        );
        self.update_water(c, sun, dt_ms / 1000.0);
        c.track_grip = (base_grip
            - lerp(0.0, reduction * 0.3, f64::from(c.rain_wetness))
            - lerp(0.0, reduction * 0.7, f64::from(c.rain_water))) as f32;
    }
}

/// The WeatherFX state: conditions, the rain model and the in-game clock.
pub struct WeatherFx {
    inner: Mutex<FxInner>,
    tz: Tz,
    /// The track's latitude and longitude, if known (`track_position`).
    position: Option<(f64, f64)>,
    /// A time speed set at runtime (TimeDilationPlugin), over
    /// TIME_OF_DAY_MULT.
    time_mult: Mutex<Option<f64>>,
}

struct FxInner {
    conditions: Conditions,
    rain: RainModel,
    /// The in-game instant.
    now: DateTime<Utc>,
}

impl WeatherFx {
    /// Starts from the first rolled weather, at SUN_ANGLE's time of day today
    /// in the track's time zone.
    pub fn new(
        cfg: &Config,
        ambient: f32,
        road: f32,
        wind_speed: i16,
        wind_direction: i16,
        graphics: &str,
        grip: f32,
    ) -> WeatherFx {
        let tz = track_timezone(cfg);
        let kind = graphics_weather_type(graphics).unwrap_or_else(|| {
            tracing::warn!(
                graphics,
                "GRAPHICS names no WeatherFX type (\"..._type=N\"); using Clear"
            );
            WEATHER_TYPES[usize::from(DEFAULT_TYPE)]
        });
        let today = Utc::now().with_timezone(&tz).date_naive();
        let now = local_time_on(
            tz,
            today,
            seconds_from_sun_angle(f64::from(cfg.server.sun_angle)),
        );
        tracing::info!(timezone = %tz, weather = kind.name, time = %now.with_timezone(&tz), "weatherfx started");
        WeatherFx {
            inner: Mutex::new(FxInner {
                conditions: Conditions {
                    kind,
                    upcoming: kind,
                    transition_value: 0,
                    transition_progress: 0.0,
                    transition_duration_ms: 1.0,
                    ambient,
                    road,
                    wind_speed: f32::from(wind_speed),
                    wind_direction: f32::from(wind_direction),
                    humidity: 0.0,
                    pressure: 0.0,
                    rain_intensity: kind.rain_intensity,
                    rain_wetness: kind.rain_wetness,
                    rain_water: kind.rain_water,
                    track_grip: grip,
                },
                rain: RainModel::default(),
                now,
            }),
            tz,
            position: track_position(cfg),
            time_mult: Mutex::new(None),
        }
    }

    fn lock(&self) -> parking_lot::MutexGuard<'_, FxInner> {
        self.inner.lock()
    }

    /// Advances the clock and the weather by `dt_ms` of server time.
    pub fn step(&self, live: &super::live_config::Live, base_grip: f32, dt_ms: i64) {
        let mut fx = self.lock();
        if live.extra.enable_real_time {
            fx.now = Utc::now();
        } else {
            // TIME_OF_DAY_MULT=0 runs as 1, as the vanilla sun does.
            let configured = if live.server.time_of_day_multiplier <= 0.0 {
                1.0
            } else {
                f64::from(live.server.time_of_day_multiplier)
            };
            let mult = self.time_mult.lock().unwrap_or(configured);
            fx.now += chrono::Duration::milliseconds((dt_ms as f64 * mult) as i64);
            if live.extra.lock_server_date {
                let real_day = Utc::now().with_timezone(&self.tz).date_naive();
                let local = fx.now.with_timezone(&self.tz);
                if local.date_naive() != real_day {
                    let seconds = f64::from(local.num_seconds_from_midnight());
                    fx.now = local_time_on(self.tz, real_day, seconds);
                }
            }
        }
        let FxInner {
            conditions, rain, ..
        } = &mut *fx;
        rain.update(
            conditions,
            f64::from(base_grip),
            live.extra.rain_track_grip_reduction_percent,
            dt_ms as f64,
        );
    }

    /// Applies a plugin's weather change.
    pub fn apply(&self, change: &WeatherChange) {
        let mut fx = self.lock();
        let c = &mut fx.conditions;
        c.upcoming = change.upcoming;
        c.transition_value = 0;
        c.transition_progress = 0.0;
        c.transition_duration_ms = change.transition_ms;
        c.ambient = change.ambient;
        c.road = change.road;
        c.humidity = change.humidity;
        c.pressure = change.pressure;
        c.wind_speed = change.wind_speed;
        c.wind_direction = change.wind_direction;
    }

    /// Sets the time speed at runtime; `None` returns to TIME_OF_DAY_MULT.
    pub fn set_time_mult(&self, mult: Option<f64>) {
        *self.time_mult.lock() = mult;
    }

    /// Seconds after local midnight, in game.
    pub fn seconds_of_day(&self) -> f64 {
        let local = self.lock().now.with_timezone(&self.tz);
        f64::from(local.num_seconds_from_midnight())
    }

    /// The sun's altitude in degrees now, in game; `None` without the
    /// track's position.
    pub fn sun_altitude(&self) -> Option<f64> {
        let (lat, lon) = self.position?;
        Some(sun_altitude_degrees(self.lock().now, lat, lon))
    }

    /// The track's latitude and longitude, if known.
    pub fn track_position(&self) -> Option<(f64, f64)> {
        self.position
    }

    /// Starts a transition to `upcoming` over `seconds`.
    pub fn set_upcoming(&self, upcoming: WeatherType, seconds: u32) {
        let mut fx = self.lock();
        let c = &mut fx.conditions;
        c.upcoming = upcoming;
        c.transition_value = 0;
        c.transition_progress = 0.0;
        c.transition_duration_ms = f64::from(seconds) * 1000.0;
        if seconds == 0 {
            c.kind = upcoming;
            c.rain_intensity = upcoming.rain_intensity;
        }
    }

    /// Replaces the conditions at once, as /setweather does.
    pub fn set_weather(
        &self,
        kind: WeatherType,
        ambient: f32,
        road: f32,
        wind_speed: f32,
        wind_direction: f32,
    ) {
        let mut fx = self.lock();
        let c = &mut fx.conditions;
        c.kind = kind;
        c.upcoming = kind;
        c.transition_value = 0;
        c.transition_progress = 0.0;
        c.ambient = ambient;
        c.road = road;
        c.wind_speed = wind_speed;
        c.wind_direction = wind_direction;
        c.rain_intensity = kind.rain_intensity;
        c.rain_wetness = kind.rain_wetness;
        c.rain_water = kind.rain_water;
    }

    /// Sets rain, wetness and puddles directly (/setrain); the rain model
    /// carries on from there.
    pub fn set_rain(&self, intensity: f32, wetness: f32, water: f32) {
        let mut fx = self.lock();
        fx.conditions.rain_intensity = intensity.clamp(0.0, 1.0);
        fx.conditions.rain_wetness = wetness.clamp(0.0, 1.0);
        fx.conditions.rain_water = water.clamp(0.0, 1.0);
        fx.rain.wetness = f64::from(fx.conditions.rain_wetness);
    }

    /// Sets the time of day, keeping the date.
    pub fn set_time_of_day(&self, seconds: f64) {
        let mut fx = self.lock();
        let day = fx.now.with_timezone(&self.tz).date_naive();
        fx.now = local_time_on(self.tz, day, seconds);
    }

    /// A copy of the conditions.
    pub fn conditions(&self) -> Conditions {
        self.lock().conditions.clone()
    }

    /// The in-game local time of day as hours, 0..24.
    pub fn hour_of_day(&self) -> f64 {
        let local = self.lock().now.with_timezone(&self.tz);
        f64::from(local.num_seconds_from_midnight()) / 3600.0
    }

    /// The in-game local date and time, for messages.
    pub fn local_time(&self) -> String {
        let local = self.lock().now.with_timezone(&self.tz);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute()
        )
    }

    /// Encodes `0xAB 0x01` (docs/04 §4.2).
    pub fn build_update(&self) -> Vec<u8> {
        let fx = self.lock();
        let c = &fx.conditions;
        let mut w = Writer::with_capacity(28);
        w.u8(proto::EXTENDED);
        w.u8(proto::EXT_UDP_WEATHER_UPDATE);
        w.u64(fx.now.timestamp().max(0) as u64);
        w.u8(c.kind.id);
        w.u8(c.upcoming.id);
        w.u16(c.transition_value);
        w.f16(c.ambient);
        w.f16(c.road);
        w.f16(c.track_grip);
        w.f16(c.wind_direction);
        w.f16(c.wind_speed);
        w.f16(c.humidity);
        w.f16(c.pressure);
        w.f16(c.rain_intensity);
        w.f16(c.rain_wetness);
        w.f16(c.rain_water);
        w.into_bytes()
    }
}

/// `seconds` after local midnight of `day` in `tz`; the earlier instant if
/// the time is ambiguous, an hour later if it falls in a DST gap.
fn local_time_on(tz: Tz, day: NaiveDate, seconds: f64) -> DateTime<Utc> {
    let midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
    let naive = midnight + chrono::Duration::milliseconds((seconds * 1000.0) as i64);
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => t.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(early, _) => early.with_timezone(&Utc),
        chrono::LocalResult::None => {
            let later = naive + chrono::Duration::hours(1);
            tz.from_local_datetime(&later)
                .earliest()
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
        }
    }
}

/// The track's section of `<cfg dir>/data_track_params.ini`
/// (AssettoServer's file), if present.
fn track_params(cfg: &Config) -> Option<(crate::config::IniFile, String)> {
    let bytes = std::fs::read(cfg.base_dir.join("data_track_params.ini")).ok()?;
    let ini = crate::config::IniFile::parse(&String::from_utf8_lossy(&bytes));
    let track = std::path::Path::new(&cfg.server.track)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some((ini, track))
}

/// The track's latitude and longitude: TrackLatitude and TrackLongitude,
/// else LATITUDE and LONGITUDE in data_track_params.ini.
fn track_position(cfg: &Config) -> Option<(f64, f64)> {
    if let (Some(lat), Some(lon)) = (cfg.extra.track_latitude, cfg.extra.track_longitude) {
        return Some((lat, lon));
    }
    let (ini, track) = track_params(cfg)?;
    let lat = ini.get(&track, "LATITUDE")?.trim().parse().ok()?;
    let lon = ini.get(&track, "LONGITUDE")?.trim().parse().ok()?;
    Some((lat, lon))
}

/// The track's time zone: `TrackTimezone`, else the TIMEZONE of the track's
/// section in `<cfg dir>/data_track_params.ini` (AssettoServer's file), else
/// UTC.
fn track_timezone(cfg: &Config) -> Tz {
    let mut name = cfg.extra.track_timezone.trim().to_string();
    if name.is_empty()
        && let Some((ini, track)) = track_params(cfg)
    {
        name = ini.get(&track, "TIMEZONE").unwrap_or_default();
    }
    if name.is_empty() {
        tracing::warn!(
            "no track time zone (TrackTimezone or data_track_params.ini); WeatherFX uses UTC"
        );
        return Tz::UTC;
    }
    name.parse::<Tz>().unwrap_or_else(|_| {
        tracing::warn!(timezone = %name, "unknown time zone; WeatherFX uses UTC");
        Tz::UTC
    })
}

/// How often the weather goes out.
pub const WEATHERFX_INTERVAL_MS: i64 = 1000;

impl Server {
    /// Sends the current WeatherFX conditions to one client.
    pub(crate) fn send_weatherfx(&self, client: &Client) {
        if let Some(fx) = &self.weatherfx {
            self.send_udp_to_client(client, &fx.build_update());
        }
    }

    /// The WeatherFX loop: advances time and rain and sends the conditions
    /// to every ready client, once a second.
    pub(crate) fn step_weatherfx(&self) {
        let Some(fx) = &self.weatherfx else { return };
        let now = self.uptime_ms();
        let last = self.weatherfx_stepped_ms.swap(now, Ordering::SeqCst);
        fx.step(&self.live(), self.weather.grip(), now - last);
        let packet = fx.build_update();
        for client in self.broadcast_clients() {
            self.send_udp_to_client(&client, &packet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_type_parses_assettoserver_form() {
        assert_eq!(
            graphics_weather_type("3_clear_type=17").map(|t| t.id),
            Some(17)
        );
        assert_eq!(graphics_weather_type("3_clear"), None);
        assert_eq!(graphics_weather_type("3_clear_type=99"), None);
        assert_eq!(find_weather_type("heavyrain").map(|t| t.id), Some(8));
        assert_eq!(find_weather_type("7").map(|t| t.name), Some("Rain"));
    }

    #[test]
    fn sun_angle_16_is_two_pm() {
        assert_eq!(seconds_from_sun_angle(0.0), 46800.0);
        assert_eq!(seconds_from_sun_angle(16.0), 50400.0);
    }

    fn dry(kind: WeatherType) -> Conditions {
        Conditions {
            kind,
            upcoming: kind,
            transition_value: 0,
            transition_progress: 0.0,
            transition_duration_ms: 1.0,
            ambient: 20.0,
            road: 25.0,
            wind_speed: 0.0,
            wind_direction: 0.0,
            humidity: 0.0,
            pressure: 0.0,
            rain_intensity: kind.rain_intensity,
            rain_wetness: 0.0,
            rain_water: 0.0,
            track_grip: 1.0,
        }
    }

    #[test]
    fn heavy_rain_wets_the_track_and_costs_grip() {
        let mut c = dry(WEATHER_TYPES[8]);
        let mut rain = RainModel::default();
        for _ in 0..600 {
            rain.update(&mut c, 0.98, 0.1, 1000.0);
        }
        assert!(c.rain_wetness > 0.9, "wetness {}", c.rain_wetness);
        assert!(c.rain_water > 0.0, "water {}", c.rain_water);
        assert!(
            c.track_grip < 0.98 && c.track_grip > 0.88,
            "grip {}",
            c.track_grip
        );
    }

    #[test]
    fn clear_sky_stays_dry() {
        let mut c = dry(WEATHER_TYPES[15]);
        let mut rain = RainModel::default();
        for _ in 0..60 {
            rain.update(&mut c, 0.98, 0.1, 1000.0);
        }
        assert_eq!(c.rain_wetness, 0.0);
        assert_eq!(c.track_grip, 0.98);
    }

    #[test]
    fn transition_reaches_the_upcoming_type() {
        let mut c = dry(WEATHER_TYPES[15]);
        c.upcoming = WEATHER_TYPES[7];
        c.transition_duration_ms = 10_000.0;
        let mut rain = RainModel::default();
        rain.update(&mut c, 1.0, 0.0, 5000.0);
        assert_eq!(c.transition_value, 32767);
        assert!((c.rain_intensity - 0.3).abs() < 1e-6);
        rain.update(&mut c, 1.0, 0.0, 5000.0);
        assert_eq!(c.kind.id, 7);
        assert_eq!(c.transition_value, 0);
    }

    #[test]
    fn sun_altitude_noon_and_midnight() {
        // Tokyo at the June solstice: about 78° at noon, well below at midnight.
        let noon = Utc
            .with_ymd_and_hms(2026, 6, 21, 2, 45, 0)
            .single()
            .expect("valid");
        let alt = sun_altitude_degrees(noon, 35.68, 139.69);
        assert!((alt - 77.8).abs() < 1.5, "noon {alt}");
        let midnight = Utc
            .with_ymd_and_hms(2026, 6, 21, 14, 45, 0)
            .single()
            .expect("valid");
        assert!(sun_altitude_degrees(midnight, 35.68, 139.69) < -20.0);
    }

    #[test]
    fn road_is_warmer_than_air_at_midday() {
        let noon = road_temperature(13.0 * 3600.0, 20.0, 1.0);
        assert!(noon > 25.0 && noon < 45.0, "noon road {noon}");
        let night = road_temperature(2.0 * 3600.0, 20.0, 1.0);
        assert!(night < noon);
    }

    #[test]
    fn local_time_honours_the_zone() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 15).expect("valid date");
        let t = local_time_on(chrono_tz::Asia::Tokyo, day, 14.0 * 3600.0);
        assert_eq!(t.to_rfc3339(), "2026-01-15T05:00:00+00:00");
    }
}
