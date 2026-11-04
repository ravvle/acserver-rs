//! DRS zones from the track's drs_zones.ini, sent as 0x53.

use std::path::Path;

use super::Server;
use super::client::Client;
use crate::config::{IniFile, parse_float};
use crate::proto;
use crate::wire::Writer;

/// One DRS zone in normalised track positions. The detection point is read
/// by the client itself and not sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrsZone {
    pub start: f32,
    pub end: f32,
}

/// Reads content/tracks/<TRACK>[/<CONFIG_TRACK>]/data/drs_zones.ini as
/// vanilla's initDRSZones does: sequential [ZONE_n] sections with START and
/// END. No file means no zones and no 0x53.
pub fn load_drs_zones(root: Option<&Path>, track: &str, track_config: &str) -> Vec<DrsZone> {
    let Some(root) = root else {
        return Vec::new();
    };
    if track.is_empty() {
        return Vec::new();
    }
    let mut base = root.join("content").join("tracks").join(track);
    if !track_config.is_empty() {
        base = base.join(track_config);
    }
    let path = base.join("data").join("drs_zones.ini");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let ini = IniFile::parse(&String::from_utf8_lossy(&bytes));

    let float = |section: &str, key: &str| -> f32 {
        ini.get(section, key)
            .and_then(|v| parse_float(&v))
            .unwrap_or(0.0) as f32
    };
    let mut zones = Vec::new();
    for i in 0.. {
        let name = format!("ZONE_{i}");
        if !ini.has_section(&name) {
            break;
        }
        zones.push(DrsZone {
            start: float(&name, "START"),
            end: float(&name, "END"),
        });
    }
    zones
}

/// Encodes 0x53 DrsZonesUpdate.
pub fn build_drs_zones(zones: &[DrsZone]) -> Vec<u8> {
    let mut w = Writer::with_id(proto::DRS_ZONES_UPDATE);
    w.u8(zones.len() as u8);
    for zone in zones {
        w.f32(zone.start);
        w.f32(zone.end);
    }
    w.into_bytes()
}

impl Server {
    /// Sends 0x53, or nothing if the track has no zones.
    pub(crate) fn send_drs_zones(&self, client: &Client) {
        if self.drs_zones.is_empty() {
            return;
        }
        self.send_tcp(client, &build_drs_zones(&self.drs_zones));
    }
}
