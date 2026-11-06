//! GET /api/details, Content Manager's extended server information.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha1::{Digest, Sha1};

use std::collections::HashMap;

use axum::Json;
use axum::response::{IntoResponse, Response};

use super::content::{ContentManifest, build_content};
use super::{InfoResponse, build_info, is_admin_guid, is_entry_list};
use crate::server::Server;

/// Fills /api/details' display-only poweredBy field.
const POWERED_BY: &str = "acserver-rs";

/// CM's salt for the anonymized player ID (docs/05 §5.3).
const ANON_SALT: &str = "antarcticfurseal";

/// The anonymized player id CM shows: SHA1 hex of the salt and guid.
fn anon_id(guid: &str) -> String {
    let digest = Sha1::digest(format!("{ANON_SALT}{guid}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One car of `players`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DetailsCar {
    driver_name: String,
    driver_nation: String,
    driver_team: String,
    #[serde(rename = "ID", skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    is_connected: bool,
    is_entry_list: bool,
    #[serde(rename = "IsRequestedGUID")]
    is_requested_guid: bool,
    model: String,
    skin: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct Players {
    cars: Option<Vec<DetailsCar>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Assists {
    abs_state: u8,
    allowed_tyres_out: i16,
    autoclutch_allowed: bool,
    damage_multiplier: u16,
    force_virtual_mirror: bool,
    fuel_rate: u16,
    stability_allowed: bool,
    tc_state: u8,
    tyre_blankets_allowed: bool,
    tyre_wear_rate: u16,
}

/// An empty JSON object.
#[derive(Debug, Serialize)]
struct Empty {}

/// The /api/details body: the /INFO fields plus Content Manager's.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Details {
    #[serde(flatten)]
    info: InfoResponse,
    ambient_temperature: f32,
    assists: Assists,
    city: String,
    content: Option<ContentManifest>,
    current_weather_id: String,
    description: String,
    extensions: Empty,
    features: Vec<String>,
    frequency: u8,
    grip: f32,
    grip_transfer: f32,
    loading_image_url: Option<String>,
    players: Players,
    powered_by: String,
    road_temperature: f32,
    track_base: String,
    until: i64,
    wind_direction: i16,
    wind_speed: i16,
    wrapped_port: u16,
}

/// GET /api/details. Always served, as AssettoServer serves it: CSP reads
/// the server's features here (docs/04 §4.6). EnableServerDetails only adds
/// the ℹ<port> name suffix.
pub(super) fn handle_details(server: &Server, query: &HashMap<String, String>) -> Response {
    let guid = query.get("guid").map(String::as_str).unwrap_or("");
    Json(build_details(server, guid)).into_response()
}

fn build_details(server: &Server, guid: &str) -> Details {
    let cfg = &server.cfg;
    let mut info = build_info(server);
    // The /INFO fields, but name has no ℹ<port> suffix.
    info.name = cfg.server.name.clone();
    let wx = server.weather.snapshot();

    // The /JSON cars plus an anonymized ID.
    let is_admin = is_admin_guid(server, guid);
    let mut cars = Vec::new();
    for car in server.listed_cars() {
        let client = car.client();
        let id = car.identity();
        cars.push(DetailsCar {
            driver_name: id.name,
            driver_nation: id.nation,
            driver_team: id.team,
            id: client.as_ref().map(|c| anon_id(&c.identity().guid)),
            is_connected: client.is_some(),
            is_entry_list: is_entry_list(server, &car, is_admin, guid),
            // Reserved for the requester, as in /JSON.
            is_requested_guid: car.reserved_for(guid),
            model: car.model(),
            skin: car.state().skin,
        });
    }

    let left = Duration::from_secs(server.sessions().time_left_seconds().max(0) as u64);
    let until = (SystemTime::now() + left)
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Details {
        info,
        ambient_temperature: wx.ambient,
        assists: Assists {
            abs_state: cfg.server.abs_allowed,
            allowed_tyres_out: cfg.server.allowed_tyres_out,
            autoclutch_allowed: cfg.server.autoclutch_allowed,
            damage_multiplier: cfg.server.damage_multiplier,
            force_virtual_mirror: cfg.server.force_virtual_mirror,
            fuel_rate: cfg.server.fuel_rate,
            stability_allowed: cfg.server.stability_allowed,
            tc_state: cfg.server.tc_allowed,
            tyre_blankets_allowed: cfg.server.tyre_blankets_allowed,
            tyre_wear_rate: cfg.server.tyre_wear_rate,
        },
        city: String::new(),
        content: build_content(server),
        current_weather_id: wx.graphics,
        description: cfg.extra.server_description.clone(),
        extensions: Empty {},
        // The CSP features: EMOJI, then those the extra options turn on.
        features: crate::server::csp_features(&cfg.extra)
            .iter()
            .map(|f| f.name.to_string())
            .collect(),
        frequency: cfg.server.refresh_rate_hz,
        grip: wx.track_grip * 100.0,
        grip_transfer: f32::from(cfg.dynamic.session_transfer),
        loading_image_url: None,
        players: Players {
            cars: (!cars.is_empty()).then_some(cars),
        },
        powered_by: POWERED_BY.to_string(),
        road_temperature: wx.road,
        track_base: cfg.server.track.clone(),
        until,
        wind_direction: wx.wind_direction,
        wind_speed: wx.wind_speed,
        wrapped_port: cfg.server.http_port,
    }
}
