//! Writes vanilla's session results JSON.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use serde::Serialize;

use crate::proto::SessionType;

/// Identifies a participant.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Driver {
    pub name: String,
    pub team: String,
    pub nation: String,
    pub guid: String,
    /// `None` writes null.
    pub guids_list: Option<Vec<String>>,
}

/// One entry-list slot as recorded in the results.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Car {
    pub car_id: i64,
    pub driver: Driver,
    pub model: String,
    pub skin: String,
    #[serde(rename = "BallastKG")]
    pub ballast_kg: f32,
    pub restrictor: f32,
}

/// A final leaderboard row: one per slot, empty ones included.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResultRow {
    pub driver_name: String,
    pub driver_guid: String,
    pub car_id: i64,
    pub car_model: String,
    pub best_lap: u32,
    pub total_time: u32,
    #[serde(rename = "BallastKG")]
    pub ballast_kg: f32,
    pub restrictor: f32,
}

/// A single recorded lap.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Lap {
    pub car_id: i64,
    pub driver_name: String,
    pub driver_guid: String,
    pub car_model: String,
    pub lap_time: u32,
    pub sectors: Option<Vec<u32>>,
    pub tyre: String,
    #[serde(rename = "BallastKG")]
    pub ballast_kg: f32,
    pub restrictor: f32,
    pub cuts: i64,
    /// Server clock, ms.
    pub timestamp: u32,
}

/// A position in an `Event`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl From<glam::Vec3> for Vec3 {
    fn from(v: glam::Vec3) -> Vec3 {
        Vec3 {
            x: v.x,
            y: v.y,
            z: v.z,
        }
    }
}

/// One collision, as vanilla's onClientEvent records it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Event {
    /// COLLISION_WITH_CAR or COLLISION_WITH_ENV.
    #[serde(rename = "Type")]
    pub kind: String,
    pub car_id: i64,
    pub driver: Driver,
    /// -1 for the environment.
    pub other_car_id: i64,
    pub other_driver: Driver,
    pub impact_speed: f32,
    pub world_position: Vec3,
    pub rel_position: Vec3,
}

/// The top-level results document. `None` lists write null.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Session {
    pub track_name: String,
    pub track_config: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub duration_secs: i64,
    pub race_laps: i64,
    pub cars: Option<Vec<Car>>,
    pub result: Option<Vec<ResultRow>>,
    pub laps: Option<Vec<Lap>>,
    /// Null when there was no contact.
    pub events: Option<Vec<Event>>,
}

/// Writes `<base_dir>/results/<timestamp>_<TYPE>[_<server>].json` and returns
/// its path. The server suffix lets instances share a results directory; an
/// empty name gives vanilla's form.
pub fn write(
    base_dir: &Path,
    session_type: SessionType,
    server_name: &str,
    session: &Session,
) -> std::io::Result<PathBuf> {
    let dir = base_dir.join("results");
    std::fs::create_dir_all(&dir)?;
    let mut name = format!(
        "{}_{}",
        file_stamp(chrono::Local::now()),
        session_type.results_name()
    );
    let safe = sanitize_name(server_name);
    if !safe.is_empty() {
        name.push('_');
        name.push_str(&safe);
    }
    let path = dir.join(format!("{name}.json"));
    let bytes = serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Vanilla's results timestamp, e.g. 2026_9_23_14_22 (measured), in local
/// time.
fn file_stamp(t: chrono::DateTime<chrono::Local>) -> String {
    t.format("%Y_%-m_%-d_%-H_%-M").to_string()
}

/// Makes a server name filename-safe: ASCII letters, digits, '-' and '_'
/// stay, every other run becomes one '_', ends are trimmed, and the length is
/// capped.
fn sanitize_name(name: &str) -> String {
    const MAX_LEN: usize = 64;
    static UNSAFE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("[^A-Za-z0-9_-]+").expect("valid pattern"));
    let safe = UNSAFE.replace_all(name, "_");
    let safe = safe.trim_matches('_');
    safe[..safe.len().min(MAX_LEN)]
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_vanilla() {
        let session = Session {
            track_name: "t".into(),
            track_config: String::new(),
            kind: "RACE".into(),
            duration_secs: 0,
            race_laps: 3,
            cars: Some(vec![Car {
                car_id: 0,
                driver: Driver::default(),
                model: "m".into(),
                skin: "s".into(),
                ballast_kg: 0.0,
                restrictor: 0.0,
            }]),
            result: None,
            laps: None,
            events: None,
        };
        let text = serde_json::to_string(&session).expect("encode");
        for key in [
            "\"TrackName\"",
            "\"TrackConfig\"",
            "\"Type\"",
            "\"DurationSecs\"",
            "\"RaceLaps\"",
            "\"Cars\"",
            "\"CarId\"",
            "\"Driver\"",
            "\"GuidsList\"",
            "\"BallastKG\"",
            "\"Events\":null",
        ] {
            assert!(text.contains(key), "{key} missing from {text}");
        }
    }

    #[test]
    fn stamps_are_unpadded() {
        use chrono::TimeZone as _;
        let t = chrono::Local
            .with_ymd_and_hms(2026, 9, 3, 4, 5, 6)
            .single()
            .expect("valid local time");
        assert_eq!(file_stamp(t), "2026_9_3_4_5");
    }

    #[test]
    fn sanitizes_server_names() {
        assert_eq!(
            sanitize_name("MX-5 Cup - Brands Hatch"),
            "MX-5_Cup_-_Brands_Hatch"
        );
        assert_eq!(sanitize_name("  ###  "), "");
        assert_eq!(sanitize_name("a//b"), "a_b");
        assert_eq!(sanitize_name(&"x".repeat(70)).len(), 64);
    }
}
