//! The HTTP API: /INFO and /JSON|<guid>, the acServerManager endpoints, the
//! optional Content Manager endpoints, and the outbound lobby registration.

mod content;
mod details;
pub mod lobby;
mod manager;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};

use crate::config::{Config, SessionConfig};
use crate::proto::SessionType;
use crate::server::Server;

/// The largest request body read (only /RECONFIG's form posts have one).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Serves the HTTP API on HTTP_PORT until shutdown.
pub async fn serve(server: Arc<Server>) -> anyhow::Result<()> {
    let port = server.cfg.server.http_port;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "http api listening");
    let cors = CorsLayer::new().allow_origin(Any).allow_methods([
        Method::GET,
        Method::HEAD,
        Method::OPTIONS,
    ]);
    let app = axum::Router::new()
        .fallback(handle)
        .layer(cors)
        .with_state(Arc::clone(&server));
    let shutdown = server.shutdown.clone();
    // The peer address, for plugins that check who calls them.
    let app = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

/// The parts of a request the handlers use.
struct Incoming {
    /// The path with %XX escapes decoded (CM sends `/JSON|<guid>` either way).
    path: String,
    query: HashMap<String, String>,
}

/// Every request. Vanilla routes by path prefix, which a fixed route table
/// cannot express, so one handler dispatches.
async fn handle(State(server): State<Arc<Server>>, request: Request) -> Response {
    let raw_path = request.uri().path().to_string();
    let path = percent_encoding::percent_decode_str(&raw_path)
        .decode_utf8_lossy()
        .into_owned();
    let query: HashMap<String, String> = request
        .uri()
        .query()
        .map(|q| form_urlencoded_pairs(q.as_bytes()))
        .unwrap_or_default();
    // Useful when a registered server never appears in the list.
    tracing::debug!(method = %request.method(), path = %path, "http request");

    if path == "/api/details" {
        return details::handle_details(&server, &query);
    }
    if let Some(index) = path.strip_prefix("/api/scripts/") {
        return handle_script(&server, index);
    }
    if path == "/api/configuration" {
        let method = request.method().to_string();
        let headers = header_map(request.headers());
        let (status, body) = server.configuration_http(&method, &query, &headers);
        if status == 404 {
            return not_found();
        }
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE, "text/x-lua")],
            body,
        )
            .into_response();
    }
    if server.plugin_owns_path(&path) {
        return handle_plugin(&server, path, query, request).await;
    }
    if path.starts_with("/content/") {
        return content::handle_content(&server, &path, &query, request).await;
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_default();
    let incoming = Incoming { path, query };
    handle_root(&server, &incoming, &body).await
}

/// The largest request body a plugin route reads (ReportPlugin's clips).
const MAX_PLUGIN_BODY_BYTES: usize = 64 << 20;

/// A route an EnablePlugins plugin owns.
async fn handle_plugin(
    server: &Arc<Server>,
    path: String,
    query: HashMap<String, String>,
    request: Request,
) -> Response {
    let method = request.method().to_string();
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    let headers = header_map(request.headers());
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_PLUGIN_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let incoming = crate::server::PluginRequest {
        method,
        path,
        query,
        headers,
        peer,
        body: body.to_vec(),
    };
    let server = Arc::clone(server);
    // A plugin may write files; keep that off the async workers.
    let answer = tokio::task::spawn_blocking(move || server.plugin_http(&incoming)).await;
    match answer {
        Ok(Some(r)) => (
            StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE, r.content_type)],
            r.body,
        )
            .into_response(),
        _ => not_found(),
    }
}

/// A CSPServerScripts file as Lua, 404 for any other index (docs/05 §5.5).
fn handle_script(server: &Server, index: &str) -> Response {
    let Some((name, source)) = index
        .parse::<usize>()
        .ok()
        .and_then(|i| server.script_source(i))
    else {
        return not_found();
    };
    (
        [
            (header::CONTENT_TYPE, "text/x-lua".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", name.replace('"', "")),
            ),
        ],
        source,
    )
        .into_response()
}

/// Routes by path prefix, in vanilla's order (main.handler).
async fn handle_root(server: &Arc<Server>, request: &Incoming, body: &[u8]) -> Response {
    let path = request.path.as_str();
    if path.starts_with("/RECONFIG") {
        manager::handle_reconfig(&request.query, body)
    } else if path.starts_with("/INFO") {
        Json(build_info(server)).into_response()
    } else if path.starts_with("/SUB") {
        manager::handle_subscribe(server, path).await
    } else if path.starts_with("/UNSUB") {
        manager::handle_unsubscribe(server, path)
    } else if let Some(rest) = path.strip_prefix("/JSON") {
        // The route parameter's first character (the '|') is stripped.
        let guid = match rest.find(['|', '/']) {
            Some(i) => &rest[i + 1..],
            None => "",
        };
        Json(build_entry_list(server, guid)).into_response()
    } else if path.starts_with("/CANJOIN") {
        manager::handle_can_join(server, path)
    } else if path.starts_with("/ACAUTH") {
        manager::handle_ac_auth(&request.query)
    } else if path.starts_with("/ENTRY") {
        manager::handle_entry_page(server)
    } else {
        not_found()
    }
}

/// Decodes `a=1&b=x+y` into a map; the first value of a key wins.
fn form_urlencoded_pairs(input: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (key, value) in form_urlencoded::parse(input) {
        map.entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    map
}

/// The request headers that are text, by (lower-case) name.
fn header_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
        .collect()
}

/// A plain-text reply.
fn text(body: impl Into<String>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "404 page not found\n").into_response()
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (status, format!("{message}\n")).into_response()
}

/// The vanilla /INFO body.
#[derive(Debug, Serialize)]
struct InfoResponse {
    cars: Vec<String>,
    clients: usize,
    country: Vec<String>,
    cport: u16,
    durations: Vec<u16>,
    extra: bool,
    inverted: i16,
    ip: String,
    json: Option<String>,
    l: bool,
    maxclients: u8,
    name: String,
    pass: bool,
    pickup: bool,
    pit: bool,
    port: u16,
    session: usize,
    sessiontypes: Vec<u8>,
    timed: bool,
    timeleft: i64,
    timeofday: i64,
    timestamp: i64,
    tport: u16,
    track: String,
}

fn build_info(server: &Server) -> InfoResponse {
    let cfg = &server.cfg;
    let (_, current) = server.sessions().current();
    let sessions = server.sessions().all();
    let weather = server.weather.snapshot();

    let timed = timed_race(&sessions);
    let mut durations = Vec::with_capacity(sessions.len());
    let mut session_types = Vec::with_capacity(sessions.len());
    for session in &sessions {
        session_types.push(session.kind.as_u8());
        // Minutes, or laps for a lap race (AssettoServer sends seconds).
        if lap_race(session, timed) {
            durations.push(session.laps);
        } else {
            durations.push(session.time);
        }
    }

    InfoResponse {
        cars: cfg.distinct_models(),
        clients: server.ready_count(),
        country: server_country(&cfg.extra.country),
        cport: cfg.server.http_port,
        durations,
        extra: cfg.server.race_extra_lap,
        inverted: cfg.server.reversed_grid_race_positions,
        ip: String::new(),
        json: None,
        l: false,
        maxclients: cfg.server.max_clients,
        name: server_name(cfg),
        pass: !cfg.server.password.is_empty(),
        pickup: cfg.server.pickup_mode_enabled,
        pit: cfg.server.race_pit_window_end != 0,
        port: cfg.server.udp_port,
        // Vanilla sends the current session *index*, not its type.
        session: current,
        sessiontypes: session_types,
        timed,
        timeleft: server.sessions().time_left_seconds(),
        // Truncated toward zero.
        timeofday: weather.sun_angle as i64,
        timestamp: 0,
        tport: cfg.server.tcp_port,
        track: track_name(cfg),
    }
}

/// The Country override if it is a pair, else ["na", "na"].
fn server_country(country: &[String]) -> Vec<String> {
    if country.len() == 2 {
        return country.to_vec();
    }
    vec!["na".to_string(), "na".to_string()]
}

fn server_name(cfg: &Config) -> String {
    if cfg.extra.enable_server_details {
        return format!("{} ℹ{}", cfg.server.name, cfg.server.http_port);
    }
    cfg.server.name.clone()
}

fn track_name(cfg: &Config) -> String {
    if cfg.server.track_config.is_empty() {
        return cfg.server.track.clone();
    }
    format!("{}-{}", cfg.server.track, cfg.server.track_config)
}

/// Vanilla's ServerOptions.TimedRace, which /INFO and the lobby report as
/// "timed": the race runs on TIME because its LAPS is 0. Practice and
/// qualifying do not count, so a server with no race reports false (measured).
pub(crate) fn timed_race(sessions: &[SessionConfig]) -> bool {
    sessions
        .iter()
        .any(|s| s.kind == SessionType::Race && s.laps == 0)
}

/// Whether vanilla gives a session's duration in laps: only a race when the
/// race is not timed.
pub(crate) fn lap_race(session: &SessionConfig, timed: bool) -> bool {
    session.kind == SessionType::Race && !timed
}

/// One /JSON car, matching vanilla's body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct EntryCarJson {
    model: String,
    skin: String,
    driver_name: String,
    driver_team: String,
    driver_nation: String,
    is_connected: bool,
    #[serde(rename = "IsRequestedGUID")]
    is_requested_guid: bool,
    is_entry_list: bool,
}

/// The /JSON body: only Cars, as vanilla sends.
#[derive(Debug, Serialize)]
struct EntryListResponse {
    #[serde(rename = "Cars")]
    cars: Vec<EntryCarJson>,
}

/// GET /JSON|<guid>.
fn build_entry_list(server: &Server, guid: &str) -> EntryListResponse {
    let guid = guid.trim();
    let is_admin = is_admin_guid(server, guid);
    let mut cars = Vec::new();
    for car in server.listed_cars() {
        // The occupant, else the slot's last driver.
        let id = car.identity();
        cars.push(EntryCarJson {
            model: car.model(),
            skin: car.state().skin,
            driver_name: id.name,
            driver_team: id.team,
            driver_nation: id.nation,
            is_connected: car.occupied(),
            // "Reserved for the requester", not "occupied by" (docs/05 §5.2).
            is_requested_guid: car.reserved_for(guid),
            is_entry_list: is_entry_list(server, &car, is_admin, guid),
        });
    }
    EntryListResponse { cars }
}

/// /JSON's IsEntryList. With pickup mode on, slot availability for the
/// requesting guid. With it off, vanilla's Car.IsEntryList: every entry-list
/// car, and no booked one (probes BookingFlow, EntryListReload).
fn is_entry_list(
    server: &Server,
    car: &crate::server::EntryCar,
    is_admin: bool,
    guid: &str,
) -> bool {
    if !server.cfg.server.pickup_mode_enabled {
        return car.state().from_entry_list;
    }
    is_admin || !car.reserved() || car.reserved_for(guid) || car.occupied()
}

/// Whether the guid belongs to a connected admin.
fn is_admin_guid(server: &Server, guid: &str) -> bool {
    let Some(number) = crate::config::parse_steam_id(guid) else {
        return false;
    };
    server
        .connected_clients()
        .iter()
        .any(|c| c.identity().steam_id() == number && c.is_admin())
}
