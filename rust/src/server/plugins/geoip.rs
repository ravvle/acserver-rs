//! GeoIPPlugin: logs where each joining player connects from, by a MaxMind
//! GeoLite2/GeoIP2 City database.

use std::sync::Arc;

use serde::Deserialize;

use super::{Plugin, Server};
use crate::server::client::Client;

/// `!GeoIPConfiguration`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GeoIpConfiguration {
    pub database_path: String,
}

pub struct GeoIp {
    reader: maxminddb::Reader<Vec<u8>>,
}

impl GeoIp {
    pub fn new(cfg: GeoIpConfiguration) -> anyhow::Result<GeoIp> {
        let reader = maxminddb::Reader::open_readfile(&cfg.database_path)
            .map_err(|e| anyhow::anyhow!("GeoIPPlugin: {}: {e}", cfg.database_path))?;
        Ok(GeoIp { reader })
    }
}

impl Plugin for GeoIp {
    fn name(&self) -> &'static str {
        "GeoIPPlugin"
    }

    fn on_first_update(&self, _server: &Arc<Server>, client: &Arc<Client>) {
        let ip = client.peer.ip();
        let city: Option<maxminddb::geoip2::City> = self
            .reader
            .lookup(ip)
            .and_then(|result| result.decode())
            .ok()
            .flatten();
        let Some(city) = city else {
            tracing::warn!(name = client.name(), "no GeoIP result");
            return;
        };
        let name = city.country.names.english.unwrap_or("?");
        let code = city.country.iso_code.unwrap_or("?");
        let lat = city.location.latitude.unwrap_or(0.0);
        let lon = city.location.longitude.unwrap_or(0.0);
        tracing::info!(
            name = client.name(),
            country = name,
            code,
            lat,
            lon,
            "GeoIP"
        );
    }
}
