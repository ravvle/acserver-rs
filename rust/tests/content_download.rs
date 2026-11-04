//! AssettoServer's content downloads: the /api/details manifest and the
//! /content zip endpoints.

mod common;

use std::io::Cursor;
use std::path::{Path, PathBuf};

use common::*;
use sha1::Digest as _;

/// A minimal AC install root the endpoints can zip: the sample entry list's
/// car, with two skins, and the configured track.
fn write_content_tree() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let write = |rel: &str, body: &str| {
        let path = root.path().join("content").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    write("cars/ks_mazda_mx5_cup/ks_mazda_mx5_cup.kn5", "model");
    write("cars/ks_mazda_mx5_cup/collider.kn5", "collider");
    write("cars/ks_mazda_mx5_cup/data.acd", "acd-bytes");
    write(
        "cars/ks_mazda_mx5_cup/ui/ui_car.json",
        r#"{"name":"MX5","version":"1.2"}"#,
    );
    write(
        "cars/ks_mazda_mx5_cup/skins/00_official/preview.jpg",
        "jpg0",
    );
    write("cars/ks_mazda_mx5_cup/skins/01_cup_07/preview.jpg", "jpg1");
    write("tracks/ks_brands_hatch/ks_brands_hatch.kn5", "model");
    write("tracks/ks_brands_hatch/models.ini", "[MODEL_0]");
    write(
        "tracks/ks_brands_hatch/ui/ui_track.json",
        r#"{"name":"Brands","version":"1.0"}"#,
    );
    root
}

fn start_with_content(content: &Path, downloads: bool, password: &str) -> TestServer {
    let content = PathBuf::from(content);
    let password = password.to_string();
    start(move |cfg| {
        cfg.content_dir = Some(content);
        cfg.extra.enable_server_details = true;
        cfg.extra.enable_content_downloads = downloads;
        cfg.server.password = password;
    })
}

fn status(url: &str) -> u16 {
    reqwest::blocking::get(url).unwrap().status().as_u16()
}

/// GETs a zip endpoint and returns the body.
fn download(url: &str) -> Vec<u8> {
    let resp = reqwest::blocking::get(url).unwrap();
    assert_eq!(resp.status(), 200, "GET {url}");
    assert_eq!(resp.headers()["content-type"], "application/zip");
    resp.bytes().unwrap().to_vec()
}

fn zip_names(url: &str) -> Vec<String> {
    let body = download(url);
    let archive = zip::ZipArchive::new(Cursor::new(body)).expect("a zip");
    archive
        .file_names()
        .map(|name| name.expect("entry name").into_owned())
        .collect()
}

#[test]
fn content_downloads() {
    let content = write_content_tree();
    let srv = start_with_content(content.path(), true, "");
    let base = srv.base_url();

    let details = get_json(&format!("{base}/api/details?guid=1"));
    let manifest = &details["content"];
    let car = &manifest["cars"]["ks_mazda_mx5_cup"];
    assert_eq!(car["url"], "/content/car/ks_mazda_mx5_cup");
    assert_eq!(car["version"], "1.2");
    assert!(car["skins"].get("00_official").is_some());
    assert!(car["skins"].get("01_cup_07").is_some());
    assert_eq!(manifest["track"]["url"], "/content/track");
    assert_eq!(manifest["track"]["version"], "1.0");

    let names = zip_names(&format!("{base}/content/car/ks_mazda_mx5_cup"));
    assert!(names.contains(&"ks_mazda_mx5_cup/data.acd".to_string()));
    assert!(names.contains(&"ks_mazda_mx5_cup/skins/00_official/preview.jpg".to_string()));

    // A single skin, rooted under the car.
    let skin = zip_names(&format!("{base}/content/skin/ks_mazda_mx5_cup/01_cup_07"));
    assert_eq!(skin, ["ks_mazda_mx5_cup/skins/01_cup_07/preview.jpg"]);

    let track = zip_names(&format!("{base}/content/track"));
    assert!(track.contains(&"ks_brands_hatch/models.ini".to_string()));

    // HEAD confirms without a body.
    let head = reqwest::blocking::Client::new()
        .head(format!("{base}/content/car/ks_mazda_mx5_cup"))
        .send()
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-type"], "application/zip");

    // A model not in the entry list is 404, never a filesystem read.
    assert_eq!(status(&format!("{base}/content/car/some_other_car")), 404);
    // Traversal in the skin element is rejected.
    assert_eq!(
        status(&format!(
            "{base}/content/skin/ks_mazda_mx5_cup/..%2f..%2f..%2fetc"
        )),
        404
    );
}

/// The first request builds a deflate-compressed zip and caches it next to
/// the content; later requests are served from the cache.
#[test]
fn content_download_caching() {
    let content = write_content_tree();
    let srv = start_with_content(content.path(), true, "");
    let base = srv.base_url();
    let cache = content.path().join("content/cars/ks_mazda_mx5_cup.zip");
    assert!(!cache.exists(), "no cache before the first request");

    let body = download(&format!("{base}/content/car/ks_mazda_mx5_cup"));
    assert!(cache.exists(), "the cache is written next to the content");
    let mut archive = zip::ZipArchive::new(Cursor::new(body)).unwrap();
    for i in 0..archive.len() {
        let file = archive.by_index(i).unwrap();
        assert_eq!(
            file.compression(),
            zip::CompressionMethod::Deflated,
            "{} is deflated, as AssettoServer's",
            file.name().unwrap()
        );
    }

    // A new source file must not appear: the cached archive is served.
    std::fs::write(
        content
            .path()
            .join("content/cars/ks_mazda_mx5_cup/NEW_FILE.txt"),
        "x",
    )
    .unwrap();
    let names = zip_names(&format!("{base}/content/car/ks_mazda_mx5_cup"));
    assert!(!names.contains(&"ks_mazda_mx5_cup/NEW_FILE.txt".to_string()));
    // The cache never nests inside the archive it caches.
    assert!(!names.contains(&"ks_mazda_mx5_cup/ks_mazda_mx5_cup.zip".to_string()));
}

/// Content holding only checksum data (no model) offers nothing: the zips
/// would be a car and a track that cannot load.
#[test]
fn content_downloads_need_models() {
    let content = write_content_tree();
    std::fs::remove_file(
        content
            .path()
            .join("content/cars/ks_mazda_mx5_cup/ks_mazda_mx5_cup.kn5"),
    )
    .unwrap();
    std::fs::remove_file(
        content
            .path()
            .join("content/tracks/ks_brands_hatch/ks_brands_hatch.kn5"),
    )
    .unwrap();
    let srv = start_with_content(content.path(), true, "");
    let base = srv.base_url();
    let details = get_json(&format!("{base}/api/details?guid=1"));
    assert!(
        details["content"].is_null(),
        "nothing to offer without a model"
    );
    for path in [
        "/content/car/ks_mazda_mx5_cup",
        "/content/skin/ks_mazda_mx5_cup/00_official",
        "/content/track",
    ] {
        assert_eq!(status(&format!("{base}{path}")), 404, "{path}");
    }
}

/// With the flag off the manifest is null and the endpoints 404.
#[test]
fn content_downloads_disabled() {
    let content = write_content_tree();
    let srv = start_with_content(content.path(), false, "");
    let base = srv.base_url();
    let details = get_json(&format!("{base}/api/details?guid=1"));
    assert!(details["content"].is_null());
    assert_eq!(status(&format!("{base}/content/car/ks_mazda_mx5_cup")), 404);
}

/// A join password gates downloads on the SHA1(salt + password) token.
#[test]
fn content_download_password() {
    let content = write_content_tree();
    let srv = start_with_content(content.path(), true, "hunter2");
    let base = srv.base_url();
    assert_eq!(status(&format!("{base}/content/car/ks_mazda_mx5_cup")), 401);
    let token: String = sha1::Sha1::digest(b"tanidolizedhoatzinhunter2")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        status(&format!(
            "{base}/content/car/ks_mazda_mx5_cup?password={token}"
        )),
        200
    );
}
