//! /api/details: the Content Manager fields and the players.Cars shape.

mod common;

use common::*;

/// The CM-only top-level fields are present, and a free slot's driver
/// strings are "" (never null, docs/05 §5.2/§5.3). The page is served even
/// without EnableServerDetails, which only adds the ℹ<port> name suffix.
#[test]
fn details_fields() {
    let srv = start(|cfg| cfg.extra.enable_server_details = false);
    let d = get_json(&format!("{}/api/details?guid=1", srv.base_url()));
    let name = d["name"].as_str().expect("name");
    assert!(!name.contains('ℹ'), "no ℹ suffix when off: {name}");
    for key in [
        "wrappedPort",
        "poweredBy",
        "extensions",
        "city",
        "loadingImageUrl",
        "trackBase",
        "ip",
        "json",
        "l",
        "timestamp",
        "until",
        "content",
        "assists",
        "features",
    ] {
        assert!(d.get(key).is_some(), "field {key:?} present");
    }
    assert_eq!(d["poweredBy"], "acserver-rs");
    assert!(d["loadingImageUrl"].is_null());
    assert_eq!(d["extensions"], serde_json::json!({}));
    assert!(d["content"].is_null(), "no content downloads configured");
    let features = d["features"].as_array().expect("features");
    for feature in ["EMOJI", "CLIENT_MESSAGES", "CLIENT_UDP_MESSAGES"] {
        assert!(
            features.iter().any(|f| f == feature),
            "{feature} is advertised by default"
        );
    }

    let cars = d["players"]["Cars"].as_array().expect("players.Cars");
    assert!(!cars.is_empty());
    for car in cars {
        for key in ["DriverName", "DriverTeam", "DriverNation"] {
            assert_eq!(car[key], "", "{key} is an empty string for a free slot");
        }
        assert_eq!(car["IsConnected"], false);
        assert!(car.get("IsRequestedGUID").is_some());
        assert!(car.get("IsEntryList").is_some());
        assert!(car.get("ID").is_none(), "no anonymous ID for a free slot");
    }
}
