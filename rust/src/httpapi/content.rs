//! Content Manager downloads (docs/05 §5.4), opt-in via
//! EnableContentDownloads. Zips are built from the --content folder, and the
//! manifest uses relative URLs, so no public IP is needed.

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha1::{Digest, Sha1};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use tower::ServiceExt as _;

use super::{not_found, plain_error};
use crate::server::Server;
use crate::server::secret_equal;

/// CM's salt for the ?password= hash.
const DOWNLOAD_PASSWORD_SALT: &str = "tanidolizedhoatzin";

/// Marks a build's temp file. `add_tree` skips these and .zip files, so a
/// car archive never includes a cached skin zip.
const ZIP_CACHE_TMP_PREFIX: &str = ".acserver-rs-zip-";

/// One downloadable item.
#[derive(Debug, Serialize)]
pub struct ContentItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    skins: Option<BTreeMap<String, ContentUrl>>,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ContentUrl {
    url: String,
}

/// The /api/details `content` manifest.
#[derive(Debug, Serialize)]
pub struct ContentManifest {
    #[serde(skip_serializing_if = "Option::is_none")]
    cars: Option<BTreeMap<String, ContentItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track: Option<ContentItem>,
}

/// <install root>/content when downloads are enabled.
fn content_root(server: &Server) -> Option<PathBuf> {
    if !server.cfg.extra.enable_content_downloads {
        return None;
    }
    Some(server.cfg.content_dir.as_ref()?.join("content"))
}

/// The manifest, listing only what is on disk, or `None` if there is
/// nothing to serve.
pub fn build_content(server: &Server) -> Option<ContentManifest> {
    let content = content_root(server)?;

    // Skins in use, per model.
    let mut skins_by_model: HashMap<String, HashSet<String>> = HashMap::new();
    for car in server.listed_cars() {
        let model = car.model();
        if model.is_empty() {
            continue;
        }
        let skins = skins_by_model.entry(model).or_default();
        let skin = car.state().skin;
        if !skin.is_empty() {
            skins.insert(skin);
        }
    }

    let mut cars: BTreeMap<String, ContentItem> = BTreeMap::new();
    for model in server.cfg.distinct_models() {
        let car_dir = content.join("cars").join(&model);
        if !has_model(&car_dir) {
            continue; // can't serve what we don't have
        }
        let mut skins: BTreeMap<String, ContentUrl> = BTreeMap::new();
        for skin in skins_by_model.get(&model).into_iter().flatten() {
            if car_dir.join("skins").join(skin).is_dir() {
                skins.insert(
                    skin.clone(),
                    ContentUrl {
                        url: format!("/content/skin/{model}/{skin}"),
                    },
                );
            }
        }
        cars.insert(
            model.clone(),
            ContentItem {
                skins: (!skins.is_empty()).then_some(skins),
                url: format!("/content/car/{model}"),
                version: ui_version(&car_dir.join("ui").join("ui_car.json")),
            },
        );
    }

    let track_dir = content.join("tracks").join(&server.cfg.server.track);
    let mut track = None;
    if has_model(&track_dir) {
        let mut ui_path = track_dir.join("ui").join("ui_track.json");
        let track_config = &server.cfg.server.track_config;
        if !track_config.is_empty() {
            let config_ui = track_dir
                .join("ui")
                .join(track_config)
                .join("ui_track.json");
            if config_ui.is_file() {
                ui_path = config_ui;
            }
        }
        track = Some(ContentItem {
            skins: None,
            url: "/content/track".to_string(),
            version: ui_version(&ui_path),
        });
    }

    if cars.is_empty() && track.is_none() {
        return None;
    }
    Some(ContentManifest {
        cars: (!cars.is_empty()).then_some(cars),
        track,
    })
}

/// GET/HEAD /content/{car/{id}|skin/{id}/{skin}|track}.
pub(super) async fn handle_content(
    server: &Server,
    path: &str,
    query: &HashMap<String, String>,
    request: Request,
) -> Response {
    let Some(content) = content_root(server) else {
        return not_found();
    };
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if !check_download_password(server, query) {
        return plain_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    let parts: Vec<&str> = path.trim_start_matches("/content/").split('/').collect();
    let target = match parts.as_slice() {
        ["car", model] => car_zip(server, &content, model),
        ["skin", model, skin] => skin_zip(server, &content, model, skin),
        ["track"] => track_zip(server, &content),
        _ => None,
    };
    let Some((dir, cache, prefix)) = target else {
        return not_found();
    };
    serve_cached_zip(dir, cache, prefix, request).await
}

/// (source dir, cache path, archive prefix)
type ZipTarget = (PathBuf, PathBuf, String);

fn car_zip(server: &Server, content: &Path, model: &str) -> Option<ZipTarget> {
    // Constrained to the entry list, never arbitrary directories.
    if !is_configured_model(server, model) {
        return None;
    }
    let dir = content.join("cars").join(model);
    if !has_model(&dir) {
        return None;
    }
    let cache = content.join("cars").join(format!("{model}.zip"));
    Some((dir, cache, model.to_string()))
}

fn skin_zip(server: &Server, content: &Path, model: &str, skin: &str) -> Option<ZipTarget> {
    if !is_configured_model(server, model) || !safe_element(skin) {
        return None;
    }
    let car_dir = content.join("cars").join(model);
    let dir = car_dir.join("skins").join(skin);
    if !has_model(&car_dir) || !dir.is_dir() {
        return None;
    }
    // Root the archive at <model>/skins/<skin> so it extracts into the car.
    let cache = car_dir.join("skins").join(format!("{skin}.zip"));
    Some((dir, cache, format!("{model}/skins/{skin}")))
}

fn track_zip(server: &Server, content: &Path) -> Option<ZipTarget> {
    let track = &server.cfg.server.track;
    let dir = content.join("tracks").join(track);
    if track.is_empty() || !has_model(&dir) {
        return None;
    }
    let cache = content.join("tracks").join(format!("{track}.zip"));
    Some((dir, cache, track.clone()))
}

/// Serialises zip builds, so concurrent first requests build each one
/// once. Builds are few (one per car, skin and track), so one lock will do.
static ZIP_BUILD: Mutex<()> = Mutex::new(());

/// The file name a download is offered under.
fn download_name(prefix: &str) -> String {
    let base = prefix.rsplit('/').next().unwrap_or(prefix);
    format!("{base}.zip")
}

/// Serves `cache`, building it from `dir` on first use. A cache that
/// cannot be written is a server error. Builds are bounded by the entry
/// list: one per car, skin and track.
async fn serve_cached_zip(
    dir: PathBuf,
    cache: PathBuf,
    prefix: String,
    request: Request,
) -> Response {
    let disposition = format!("attachment; filename=\"{}\"", download_name(&prefix));
    let built = {
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // Another build need not hold up a download that is cached.
            if cache.is_file() {
                return Ok(());
            }
            let _guard = ZIP_BUILD.lock();
            if cache.is_file() {
                return Ok(()); // built while we waited
            }
            build_zip_file(&cache, &dir, &prefix)
        })
        .await
    };
    if !matches!(built, Ok(Ok(()))) {
        tracing::error!(path = %cache.display(), result = ?built, "content zip could not be built");
        return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "content unavailable");
    }
    // ServeFile handles Range, HEAD and If-Modified-Since.
    let Ok(served) = tower_http::services::ServeFile::new(&cache)
        .oneshot(request)
        .await;
    let mut response = served.map(Body::new);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// Zips `src` to a temp file and renames it into place, so no reader sees a
/// partial archive.
fn build_zip_file(cache: &Path, src: &Path, prefix: &str) -> std::io::Result<()> {
    let parent = cache.parent().unwrap_or(Path::new("."));
    let tmp = parent.join(format!("{ZIP_CACHE_TMP_PREFIX}{}", std::process::id()));
    let result = (|| {
        let file = std::fs::File::create(&tmp)?;
        let mut zip = zip::ZipWriter::new(file);
        add_tree(&mut zip, src, prefix)?;
        zip.finish().map_err(std::io::Error::other)?;
        std::fs::rename(&tmp, cache)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Deflates every file under `root` into the archive at `prefix`, skipping
/// cached zips and temp files, in name order.
fn add_tree<W: std::io::Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    root: &Path,
    prefix: &str,
) -> std::io::Result<()> {
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        let is_zip = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
        if name.starts_with(ZIP_CACHE_TMP_PREFIX) || is_zip {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel = rel.to_string_lossy().replace('\\', "/");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(format!("{prefix}/{rel}"), options)
            .map_err(std::io::Error::other)?;
        std::io::copy(&mut std::fs::File::open(path)?, zip)?;
    }
    Ok(())
}

/// Requires the CM password when the server has one.
fn check_download_password(server: &Server, query: &HashMap<String, String>) -> bool {
    let password = &server.cfg.server.password;
    if password.is_empty() {
        return true;
    }
    let digest = Sha1::digest(format!("{DOWNLOAD_PASSWORD_SALT}{password}").as_bytes());
    let want: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let given = query.get("password").map(String::as_str).unwrap_or("");
    secret_equal(given, &want)
}

fn is_configured_model(server: &Server, model: &str) -> bool {
    safe_element(model) && server.cfg.distinct_models().iter().any(|m| m == model)
}

/// Rejects anything that isn't a single, traversal-free path element.
fn safe_element(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// Whether `dir` holds a model, a .kn5 other than collider.kn5. A content
/// folder trimmed to checksum data has none, and zipping it would hand a
/// client a car or track that cannot load, so nothing is offered for it.
fn has_model(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".kn5") && name != "collider.kn5" {
            return true;
        }
    }
    false
}

/// "version" from ui_car.json / ui_track.json, or `None`. AC ui json can have
/// a BOM, so it is trimmed first.
fn ui_version(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    let doc: serde_json::Value = serde_json::from_str(text).ok()?;
    match doc.get("version")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => n.as_f64().map(|f| format!("{f}")),
        _ => None,
    }
}
