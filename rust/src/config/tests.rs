// Ported from internal/config/load_test.go, plus extra_cfg.yml cases.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::load::{graphics_folder, normalise_max_contacts, solve_car_wildcard, split_list};
use super::*;
use crate::proto::MAX_CAR_SLOTS;

/// A temp dir holding server_cfg.ini and an entry_list.ini with `n`
/// identical car slots. The `TempDir` must stay alive while it is used.
fn write_cfg(server_cfg: &str, n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut server_cfg = server_cfg.to_string();
    if !server_cfg.contains("[WEATHER_0]") {
        server_cfg.push_str("\n[WEATHER_0]\nGRAPHICS=3_clear\n");
    }
    fs::write(dir.path().join("server_cfg.ini"), server_cfg).expect("write");

    let mut entries = String::new();
    for i in 0..n {
        entries.push_str(&format!("[CAR_{i}]\nMODEL=car\nSKIN=skin\n"));
    }
    fs::write(dir.path().join("entry_list.ini"), entries).expect("write");
    dir
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

fn load_dir(dir: &tempfile::TempDir) -> Result<Config, ConfigError> {
    load(path_str(dir.path()), "")
}

fn write(dir: &tempfile::TempDir, name: &str, body: &str) {
    fs::write(dir.path().join(name), body).expect("write");
}

/// Sets every [SERVER] key to a value distinct from its default, so a
/// mistyped key name shows up as a field that kept its default.
const SERVER_CFG_ALL_KEYS: &str = "
[SERVER]
NAME=testname
CARS=alpha;beta;car
TRACK=trk
CONFIG_TRACK=cfgtrk
SUN_ANGLE=33
PASSWORD=pw
ADMIN_PASSWORD=apw
UDP_PORT=1111
TCP_PORT=2222
HTTP_PORT=3333
MAX_CLIENTS=7
CLIENT_SEND_INTERVAL_HZ=24
REGISTER_TO_LOBBY=0
PICKUP_MODE_ENABLED=0
LOOP_MODE=0
MAX_BALLAST_KG=55
QUALIFY_MAX_WAIT_PERC=130
RACE_OVER_TIME=70
RESULT_SCREEN_TIME=80
RACE_GAS_PENALTY_DISABLED=1
RACE_PIT_WINDOW_START=5
RACE_PIT_WINDOW_END=10
REVERSED_GRID_RACE_POSITIONS=3
VOTING_QUORUM=90
VOTE_DURATION=25
KICK_QUORUM=95
BLACKLIST_MODE=2
MAX_CONTACTS_PER_KM=9
LEGAL_TYRES=SM;SV
START_RULE=2
RACE_EXTRA_LAP=1
TIME_OF_DAY_MULT=4
ABS_ALLOWED=2
TC_ALLOWED=2
STABILITY_ALLOWED=1
AUTOCLUTCH_ALLOWED=1
TYRE_BLANKETS_ALLOWED=1
FORCE_VIRTUAL_MIRROR=1
FUEL_RATE=120
DAMAGE_MULTIPLIER=130
TYRE_WEAR_RATE=140
ALLOWED_TYRES_OUT=4
UDP_PLUGIN_ADDRESS=127.0.0.1:12000
UDP_PLUGIN_LOCAL_PORT=12001
AUTH_PLUGIN_ADDRESS=auth.example:8080/check?x=1

[PRACTICE]
NAME=P
TIME=11
IS_OPEN=2

[RACE]
NAME=R
LAPS=12
WAIT_TIME=13
IS_OPEN=0
";

#[test]
fn load_server_cfg_every_key() {
    let dir = write_cfg(SERVER_CFG_ALL_KEYS, 8);
    let s = load_dir(&dir).expect("load").server;

    assert_eq!(s.name, "testname");
    assert_eq!(s.cars, ["alpha", "beta", "car"]);
    assert_eq!(s.track, "trk");
    assert_eq!(s.track_config, "cfgtrk");
    assert_eq!(s.sun_angle, 33.0);
    assert_eq!(s.password, "pw");
    assert_eq!(s.admin_password, "apw");
    assert_eq!(s.udp_port, 1111);
    assert_eq!(s.tcp_port, 2222);
    assert_eq!(s.http_port, 3333);
    assert_eq!(s.max_clients, 7);
    assert_eq!(s.refresh_rate_hz, 24);
    assert!(!s.register_to_lobby);
    assert!(!s.pickup_mode_enabled);
    assert!(!s.loop_mode);
    assert_eq!(s.max_ballast_kg, 55);
    assert_eq!(s.qualify_max_wait_perc, 130);
    assert_eq!(s.race_over_time, 70);
    assert_eq!(s.result_screen_time, 80);
    assert!(s.race_gas_penalty_disabled);
    assert_eq!(s.race_pit_window_start, 5);
    assert_eq!(s.race_pit_window_end, 10);
    assert_eq!(s.reversed_grid_race_positions, 3);
    assert_eq!(s.voting_quorum, 90);
    assert_eq!(s.vote_duration, Duration::from_secs(25));
    assert_eq!(s.kick_quorum, 95);
    assert_eq!(s.blacklist_mode, BlacklistMode::Ban);
    assert_eq!(s.max_contacts_per_km, 9);
    assert_eq!(s.legal_tyres, "SM;SV");
    assert_eq!(s.start_rule, 2);
    assert!(s.race_extra_lap);
    assert_eq!(s.time_of_day_mult, 4.0);
    assert_eq!(s.abs_allowed, 2);
    assert_eq!(s.tc_allowed, 2);
    assert!(s.stability_allowed);
    assert!(s.autoclutch_allowed);
    assert!(s.tyre_blankets_allowed);
    assert!(s.force_virtual_mirror);
    assert_eq!(s.fuel_rate, 120);
    assert_eq!(s.damage_multiplier, 130);
    assert_eq!(s.tyre_wear_rate, 140);
    assert_eq!(s.allowed_tyres_out, 4);
    assert_eq!(s.udp_plugin_address, "127.0.0.1:12000");
    assert_eq!(s.udp_plugin_local_port, 12001);
    assert_eq!(s.auth_plugin_address, "auth.example:8080/check?x=1");
}

#[test]
fn load_sessions_every_key() {
    let dir = write_cfg(SERVER_CFG_ALL_KEYS, 8);
    let cfg = load_dir(&dir).expect("load");

    // Sessions come out in vanilla order, skipping absent sections.
    assert_eq!(cfg.sessions.len(), 2);
    let practice = SessionConfig {
        name: "P".into(),
        time: 11,
        is_open: IsOpen::ClosesAtStart,
        ..SessionConfig::new(SessionType::Practice)
    };
    let race = SessionConfig {
        name: "R".into(),
        laps: 12,
        wait_time: 13,
        is_open: IsOpen::Closed,
        ..SessionConfig::new(SessionType::Race)
    };
    assert_eq!(cfg.sessions[0], practice);
    assert_eq!(cfg.sessions[1], race);
}

/// The acServer defaults that apply when a key is absent.
#[test]
fn load_server_cfg_defaults() {
    let dir = write_cfg("[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=10\n", 2);
    let cfg = load_dir(&dir).expect("load");
    let s = &cfg.server;

    assert_eq!(s.sun_angle, 16.0);
    assert_eq!(s.udp_port, 9600);
    assert_eq!(s.tcp_port, 9600);
    assert_eq!(s.http_port, 8081);
    assert_eq!(s.refresh_rate_hz, 18);
    assert!(s.register_to_lobby);
    assert!(s.pickup_mode_enabled);
    assert!(s.loop_mode);
    assert_eq!(s.qualify_max_wait_perc, 120);
    assert_eq!(s.race_over_time, 60);
    assert_eq!(s.result_screen_time, 60);
    assert_eq!(s.voting_quorum, 80);
    assert_eq!(s.vote_duration, Duration::from_secs(20));
    assert_eq!(s.kick_quorum, 85);
    assert_eq!(s.time_of_day_mult, 1.0);
    assert_eq!(s.abs_allowed, 1);
    assert_eq!(s.tc_allowed, 1);
    assert_eq!(s.fuel_rate, 100);
    assert_eq!(s.damage_multiplier, 100);
    assert_eq!(s.tyre_wear_rate, 100);
    assert_eq!(s.allowed_tyres_out, 2);
    assert_eq!(s.max_ballast_kg, 100);
    // An absent MAX_CONTACTS_PER_KM falls back to 5, not 0.
    assert_eq!(s.max_contacts_per_km, 5);
    // A session with no NAME takes the type's vanilla label.
    assert_eq!(cfg.sessions[0].name, "Practice");
    assert_eq!(cfg.sessions[0].is_open, IsOpen::Open);
    // An absent MAX_CLIENTS clamps to the entry-list size.
    assert_eq!(s.max_clients, 2);

    assert_eq!(
        cfg.dynamic,
        DynamicTrackConfig {
            session_start: 100,
            randomness: 0,
            session_transfer: 100,
            lap_gain: 1
        }
    );
}

#[test]
fn entry_list_slot_limit() {
    let body = "[SERVER]\nNAME=x\n\n[RACE]\nLAPS=1\n";

    let dir = write_cfg(body, MAX_CAR_SLOTS);
    let cfg = load_dir(&dir).expect("all assignable session ids should be accepted");
    assert_eq!(cfg.entry_list.len(), MAX_CAR_SLOTS);

    let dir = write_cfg(body, MAX_CAR_SLOTS + 1);
    let err = load_dir(&dir).expect_err("too many slots");
    assert_eq!(
        err.to_string(),
        "entry_list.ini contains 256 CAR_n slots (max 255)"
    );
}

#[test]
fn normalise_max_contacts_rules() {
    for (input, want) in [(0, 5), (-1, 0), (-99, 0), (7, 7), (255, 255)] {
        assert_eq!(
            normalise_max_contacts(input).expect("valid"),
            want,
            "input {input}"
        );
    }
    assert!(normalise_max_contacts(256).is_err());
}

/// Only vanilla's BASE_TEMPERATURE_* names are read; the abbreviated
/// BASE_TEMP_* spelling falls back to the defaults, as in vanilla.
#[test]
fn weather_temperature_keys() {
    let dir = write_cfg(
        "[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=10\n\n\
         [WEATHER_0]\nGRAPHICS=3_clear\nBASE_TEMPERATURE_AMBIENT=30\nBASE_TEMPERATURE_ROAD=-1\n\
         [WEATHER_1]\nGRAPHICS=3_clear\nBASE_TEMP_AMBIENT=30\nBASE_TEMP_ROAD=-1\n",
        1,
    );
    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.weathers.len(), 2);
    assert_eq!(cfg.weathers[0].base_temp_ambient, 30.0);
    assert_eq!(cfg.weathers[0].road_temp(), 29.0);
    assert_eq!(cfg.weathers[1].base_temp_ambient, 18.0);
    assert_eq!(cfg.weathers[1].road_temp(), 24.0);
}

#[test]
fn load_dynamic_track_and_entry_list() {
    let body = "[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n\n[DYNAMIC_TRACK]\nSESSION_START=96\nRANDOMNESS=4\nSESSION_TRANSFER=70\nLAP_GAIN=30\n";
    let dir = write_cfg(body, 1);
    write(
        &dir,
        "entry_list.ini",
        "[CAR_0]\nMODEL=m0\nSKIN=s0\nDRIVERNAME=dn\nTEAM=tm\nGUID=123\nBALLAST=12.5\nRESTRICTOR=3.5\nFIXED_SETUP=fs\nAI_LEVEL=80\nSPECTATOR_MODE=1\nAI=fixed\n",
    );

    let cfg = load_dir(&dir).expect("load");
    assert_eq!(
        cfg.dynamic,
        DynamicTrackConfig {
            session_start: 96,
            randomness: 4,
            session_transfer: 70,
            lap_gain: 30
        }
    );
    let want = EntryCar {
        model: "m0".into(),
        skin: "s0".into(),
        spectator_mode: true,
        driver_name: "dn".into(),
        team: "tm".into(),
        guid: "123".into(),
        guids: vec!["123".into()],
        ballast: 12.5,
        restrictor: 3.5,
        fixed_setup: "fs".into(),
        fixed_setup_set: true,
        fixed_setup_named: true,
        ai_mode: AiMode::Fixed,
    };
    assert_eq!(cfg.entry_list[0], want);
}

#[test]
fn shared_guid_and_default_fixed_setup() {
    let dir = write_cfg("[SERVER]\nNAME=x\nCARS=m0\n\n[PRACTICE]\nTIME=1\n", 1);
    write(
        &dir,
        "entry_list.ini",
        "[CAR_0]\nMODEL=m0\nGUID= 111 ; 222;333 \nFIXED_SETUP=\n",
    );

    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.entry_list[0].guids, ["111", "222", "333"]);
    assert!(cfg.entry_list[0].fixed_setup_set);
    assert_eq!(cfg.entry_list[0].fixed_setup, "m0");
}

#[test]
fn max_clients_lowered_to_slots() {
    // Vanilla starts with MAX_CLIENTS set to the entry-list size (measured).
    let dir = write_cfg("[SERVER]\nNAME=x\nMAX_CLIENTS=2\n\n[PRACTICE]\nTIME=1\n", 1);
    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.server.max_clients, 1);
    assert_eq!(cfg.warnings.len(), 1);
}

#[test]
fn ballast_within_max_ballast_kg() {
    // Vanilla refuses to start on a larger entry-list ballast (measured).
    let dir = write_cfg(
        "[SERVER]\nNAME=x\nCARS=m0\nMAX_BALLAST_KG=50\n\n[PRACTICE]\nTIME=1\n",
        1,
    );
    write(&dir, "entry_list.ini", "[CAR_0]\nMODEL=m0\nBALLAST=80\n");
    let err = load_dir(&dir).expect_err("ballast");
    assert_eq!(
        err.to_string(),
        "CAR_0 BALLAST=80 exceeds MAX_BALLAST_KG=50"
    );
}

#[test]
fn model_must_be_legal() {
    let dir = write_cfg("[SERVER]\nNAME=x\nCARS=legal\n\n[PRACTICE]\nTIME=1\n", 1);
    let err = load_dir(&dir).expect_err("illegal model");
    assert_eq!(
        err.to_string(),
        r#"CAR_0 MODEL="car" is not listed in [SERVER] CARS"#
    );
}

#[test]
fn restrictor_range() {
    let dir = write_cfg("[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n", 1);
    write(
        &dir,
        "entry_list.ini",
        "[CAR_0]\nMODEL=car\nRESTRICTOR=101\n",
    );
    let err = load_dir(&dir).expect_err("restrictor");
    assert_eq!(
        err.to_string(),
        "CAR_0 RESTRICTOR must be between 0 and 100"
    );
}

#[test]
fn weather_is_mandatory() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        &dir,
        "server_cfg.ini",
        "[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n",
    );
    write(&dir, "entry_list.ini", "[CAR_0]\nMODEL=car\n");
    let err = load_dir(&dir).expect_err("no weather");
    assert_eq!(
        err.to_string(),
        "server_cfg.ini requires a WEATHER_0 section"
    );
}

#[test]
fn wind_maximum() {
    let body = "[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n\n\
                [WEATHER_0]\nGRAPHICS=3_clear\nWIND_BASE_SPEED_MIN=5\nWIND_BASE_SPEED_MAX=41\n";
    let dir = write_cfg(body, 1);
    let err = load_dir(&dir).expect_err("wind");
    assert_eq!(
        err.to_string(),
        "WEATHER_0 wind speed range must satisfy 0 <= MIN <= MAX <= 40"
    );
}

#[test]
fn content_and_pitbox_validation() {
    let content = tempfile::tempdir().expect("tempdir");
    let root = content.path();
    fs::create_dir_all(root.join("content/tracks/trk/ui")).expect("mkdir");
    fs::create_dir_all(root.join("content/cars/car/skins/skin")).expect("mkdir");
    fs::create_dir_all(root.join("content/weather/3_clear")).expect("mkdir");
    fs::write(
        root.join("content/tracks/trk/ui/ui_track.json"),
        r#"{"pitboxes":1}"#,
    )
    .expect("write");

    let dir = write_cfg(
        "[SERVER]\nNAME=x\nTRACK=trk\nMAX_CLIENTS=2\n\n[PRACTICE]\nTIME=1\n",
        2,
    );
    let err = load(path_str(dir.path()), path_str(root)).expect_err("pitboxes");
    assert_eq!(err.to_string(), "MAX_CLIENTS=2 exceeds track pit boxes=1");

    // Some tracks (SRP) store the count as a string.
    fs::write(
        root.join("content/tracks/trk/ui/ui_track.json"),
        r#"{"pitboxes":"1"}"#,
    )
    .expect("write");
    let err = load(path_str(dir.path()), path_str(root)).expect_err("string pitboxes");
    assert_eq!(err.to_string(), "MAX_CLIENTS=2 exceeds track pit boxes=1");
}

/// A WeatherFX GRAPHICS value ("3_clear_type=15") is checked against its
/// folder name, without the key=value pairs.
#[test]
fn weather_graphics_folder_ignores_weatherfx_pairs() {
    assert_eq!(graphics_folder("3_clear"), "3_clear");
    assert_eq!(graphics_folder("3_clear_type=15"), "3_clear");
    assert_eq!(
        graphics_folder("sol_05_broken_clouds_time=0_mult=20"),
        "sol_05_broken_clouds"
    );

    let content = tempfile::tempdir().expect("tempdir");
    let root = content.path();
    fs::create_dir_all(root.join("content/tracks/trk")).expect("mkdir");
    fs::create_dir_all(root.join("content/cars/car")).expect("mkdir");
    fs::create_dir_all(root.join("content/weather/3_clear")).expect("mkdir");
    let dir = write_cfg(
        "[SERVER]\nNAME=x\nTRACK=trk\nMAX_CLIENTS=1\n\n[PRACTICE]\nTIME=1\n\n\
         [WEATHER_0]\nGRAPHICS=3_clear_type=15\n",
        1,
    );
    load(path_str(dir.path()), path_str(root)).expect("load");
}

/// CAR_n sections sort numerically, not lexically: CAR_10 after CAR_9.
#[test]
fn entry_list_ordering() {
    let dir = write_cfg("[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n", 1);
    let mut body = String::new();
    for i in 0..11 {
        body.push_str(&format!("[CAR_{i}]\nMODEL=m{i}\n"));
    }
    write(&dir, "entry_list.ini", &body);

    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.entry_list.len(), 11);
    for (i, ec) in cfg.entry_list.iter().enumerate() {
        assert_eq!(ec.model, format!("m{i}"), "slot {i}");
    }
}

/// The reference SolveCarWildcard grammar: "*N" is a 1-based index into
/// CARS, an out-of-range N picks some legal car, and a non-wildcard or
/// unparseable MODEL stays as written.
#[test]
fn solve_car_wildcard_grammar() {
    let cars: Vec<String> = ["alpha", "beta", "gamma"].map(String::from).to_vec();

    assert_eq!(solve_car_wildcard("beta", &cars), None);
    assert_eq!(solve_car_wildcard("*1", &cars).as_deref(), Some("alpha"));
    assert_eq!(solve_car_wildcard("*3", &cars).as_deref(), Some("gamma"));

    for model in ["*0", "*4", "*99"] {
        let got = solve_car_wildcard(model, &cars).expect(model);
        assert!(cars.contains(&got), "{model}");
    }
    for model in ["*", "*x", "*1a"] {
        assert_eq!(solve_car_wildcard(model, &cars), None, "{model}");
    }
    assert_eq!(solve_car_wildcard("*1", &[]), None);
}

#[test]
fn entry_list_wildcard_resolution() {
    let dir = write_cfg(
        "[SERVER]\nNAME=x\nCARS=alpha;beta;gamma\n\n[PRACTICE]\nTIME=1\n",
        1,
    );
    write(
        &dir,
        "entry_list.ini",
        "[CAR_0]\nMODEL=*2\n[CAR_1]\nMODEL=beta\n",
    );

    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.entry_list.len(), 2);
    assert_eq!(cfg.entry_list[0].model, "beta", "*2 is the 2nd CARS entry");
    assert_eq!(
        cfg.entry_list[1].model, "beta",
        "a plain model is unchanged"
    );

    write(&dir, "entry_list.ini", "[CAR_0]\nMODEL=*bad\n");
    let err = load_dir(&dir).expect_err("unresolvable");
    assert_eq!(
        err.to_string(),
        r#"CAR_0 MODEL="*bad" is not listed in [SERVER] CARS"#
    );
}

#[test]
fn split_list_separators() {
    assert_eq!(split_list("a;b"), Some(vec!["a".into(), "b".into()]));
    assert_eq!(split_list("a,b"), Some(vec!["a".into(), "b".into()]));
    assert_eq!(split_list(" a ; b "), Some(vec!["a".into(), "b".into()]));
    assert_eq!(split_list(""), None);
    assert_eq!(split_list(";"), Some(vec![]));
}

#[test]
fn resolve_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();

    // Directory form: everything is looked up inside the directory.
    assert_eq!(
        resolve(path_str(d), "", ""),
        Files {
            server_cfg: d.join("server_cfg.ini"),
            entry_list: d.join("entry_list.ini"),
            extra_cfg: d.join("extra_cfg.yml"),
            base_dir: d.to_path_buf(),
        }
    );

    // File form: the base dir and sibling files come from the file's directory.
    let srv = d.join("server_cfg_dtm.ini");
    fs::write(&srv, "x").expect("write");
    assert_eq!(
        resolve(path_str(&srv), "", ""),
        Files {
            server_cfg: srv.clone(),
            entry_list: d.join("entry_list.ini"),
            extra_cfg: d.join("extra_cfg.yml"),
            base_dir: d.to_path_buf(),
        }
    );

    // Explicit --entry / --extra win over the defaults.
    let got = resolve(path_str(&srv), "/e/dtm.ini", "/x/dtm.yml");
    assert_eq!(got.entry_list, PathBuf::from("/e/dtm.ini"));
    assert_eq!(got.extra_cfg, PathBuf::from("/x/dtm.yml"));
}

/// A server_cfg and entry_list in one flat directory under other names.
#[test]
fn load_files_split_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        &dir,
        "server_cfg_dtm.ini",
        "[SERVER]\nNAME=dtm\n\n[PRACTICE]\nTIME=1\n\n[WEATHER_0]\nGRAPHICS=3_clear\n",
    );
    write(
        &dir,
        "entry_list_dtm.ini",
        "[CAR_0]\nMODEL=m0\n[CAR_1]\nMODEL=m1\n",
    );

    let files = resolve(
        path_str(&dir.path().join("server_cfg_dtm.ini")),
        path_str(&dir.path().join("entry_list_dtm.ini")),
        "",
    );
    let cfg = load_files(files, "").expect("load");
    assert_eq!(cfg.server.name, "dtm");
    assert_eq!(cfg.entry_list.len(), 2);
    assert_eq!(cfg.base_dir, dir.path());
}

/// The four windows vanilla zeroes (inverted, past the last lap, past the
/// clock, no laps or clock) and the ones it keeps.
#[test]
fn pit_window_validation() {
    let cases = [
        ("fits in a lap race", 2, 4, 5, 0, 2, 4),
        ("ends on the last lap", 2, 5, 5, 0, 2, 5),
        ("ends past the last lap", 2, 8, 5, 0, 0, 0),
        ("fits in a timed race", 20, 60, 0, 60, 20, 60),
        ("ends past the clock", 20, 61, 0, 60, 0, 0),
        ("opens after it closes", 30, 10, 5, 0, 0, 0),
        ("race with no length", 1, 5, 0, 0, 0, 0),
    ];
    let dir = write_cfg("[SERVER]\nNAME=x\n\n[RACE]\nLAPS=1\n", 1);
    let base = load_dir(&dir).expect("load");
    for (name, start, end, laps, mins, want_start, want_end) in cases {
        let mut c = base.clone();
        c.server.race_pit_window_start = start;
        c.server.race_pit_window_end = end;
        c.sessions = vec![SessionConfig {
            laps,
            time: mins,
            ..SessionConfig::new(SessionType::Race)
        }];
        c.validate_pit_window();
        assert_eq!(c.server.race_pit_window_start, want_start, "{name}");
        assert_eq!(c.server.race_pit_window_end, want_end, "{name}");
    }
}

/// --content given the install root or its content/ both resolve to the root.
#[test]
fn normalize_content_dir_forms() {
    let install = tempfile::tempdir().expect("tempdir");
    let root = install.path();
    fs::create_dir_all(root.join("content/tracks")).expect("mkdir");

    assert_eq!(normalize_content_dir(path_str(root)).as_deref(), Some(root));
    let content = root.join("content");
    assert_eq!(
        normalize_content_dir(path_str(&content)).as_deref(),
        Some(root)
    );
    let trailing = format!("{}/", path_str(&content));
    assert_eq!(normalize_content_dir(&trailing).as_deref(), Some(root));
    assert_eq!(normalize_content_dir(""), None);

    // A root named "content" that holds a content/ subdir is an install root.
    let other = tempfile::tempdir().expect("tempdir");
    let nested = other.path().join("content");
    fs::create_dir_all(nested.join("content")).expect("mkdir");
    assert_eq!(
        normalize_content_dir(path_str(&nested)).as_deref(),
        Some(nested.as_path())
    );

    // load applies it, so later code sees one spelling.
    let dir = write_cfg(
        "[SERVER]\nNAME=x\nTRACK=trk\nMAX_CLIENTS=1\n\n[PRACTICE]\nTIME=1\n",
        1,
    );
    fs::create_dir_all(root.join("content/tracks/trk")).expect("mkdir");
    fs::create_dir_all(root.join("content/cars/car")).expect("mkdir");
    fs::create_dir_all(root.join("content/weather/3_clear")).expect("mkdir");
    let cfg = load(path_str(dir.path()), path_str(&content)).expect("load");
    assert_eq!(cfg.content_dir.as_deref(), Some(root));
}

/// A value is cut at "//", and a missing or empty NAME becomes "Assetto
/// Corsa Server".
#[test]
fn slash_comments_and_default_name() {
    let dir = write_cfg(
        "[SERVER]\nNAME=\nMAX_CLIENTS=5 // five\n\n[PRACTICE]\nTIME=1\n",
        5,
    );
    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.server.name, "Assetto Corsa Server");
    assert_eq!(cfg.server.max_clients, 5);
}

#[test]
fn extra_cfg_defaults_and_overrides() {
    let dir = write_cfg("[SERVER]\nNAME=x\n\n[PRACTICE]\nTIME=1\n", 1);
    assert_eq!(load_dir(&dir).expect("load").extra, ExtraConfig::default());

    write(
        &dir,
        "extra_cfg.yml",
        "# comment\nEnableCSP: false\nRconPort: 9999\nEnableRcon: true\nServerDescription:\n\
         Country: [Germany, DE]\nChatRateLimitMs: 1000\nUnknownKey: 1\n",
    );
    let extra = load_dir(&dir).expect("load").extra;
    assert!(!extra.enable_csp);
    assert_eq!(extra.rcon_port, 9999);
    assert!(extra.enable_rcon);
    assert_eq!(extra.server_description, "");
    assert_eq!(extra.country, ["Germany", "DE"]);
    assert_eq!(extra.live.chat_rate_limit_ms, 1000);
    assert!(extra.validate_position_updates, "untouched default");
    assert!(
        extra.enable_server_details,
        "on by default, as AssettoServer"
    );

    write(&dir, "extra_cfg.yml", "");
    assert_eq!(
        load_dir(&dir).expect("empty file").extra,
        ExtraConfig::default()
    );

    write(&dir, "extra_cfg.yml", "EnableCSP: maybe\n");
    let err = load_dir(&dir).expect_err("bad bool");
    assert!(err.to_string().starts_with("extra_cfg.yml: "), "{err}");
}

#[test]
fn values_that_do_not_fit_refuse_to_start() {
    let base = "[SERVER]\nCARS=car\n[PRACTICE]\nTIME=10\n";
    for (line, needle) in [
        ("UDP_PORT=70000", "UDP_PORT=70000"),
        ("MAX_CLIENTS=-1", "MAX_CLIENTS=-1"),
        ("FUEL_RATE=abc", "FUEL_RATE=abc"),
        ("BLACKLIST_MODE=3", "BLACKLIST_MODE=3"),
        ("ABS_ALLOWED=5", "ABS_ALLOWED=5"),
        ("RACE_PIT_WINDOW_START=-2", "RACE_PIT_WINDOW"),
        ("LOCKED_ENTRY_LIST=maybe", "LOCKED_ENTRY_LIST=maybe"),
    ] {
        let dir = write_cfg(
            &base.replace("[SERVER]\n", &format!("[SERVER]\n{line}\n")),
            1,
        );
        let err = load_dir(&dir).expect_err(line).to_string();
        assert!(err.contains(needle), "{line}: {err}");
    }
    let dir = write_cfg(&format!("{base}IS_OPEN=7\n"), 1);
    assert!(load_dir(&dir).is_err(), "IS_OPEN=7");
    let dir = write_cfg(
        &format!("{base}[WEATHER_0]\nGRAPHICS=3_clear\nWIND_BASE_DIRECTION=400\n"),
        1,
    );
    assert!(load_dir(&dir).is_err(), "WIND_BASE_DIRECTION=400");
}

#[test]
fn empty_values_keep_the_default() {
    let dir = write_cfg(
        "[SERVER]\nCARS=car\nUDP_PORT=\nVOTE_DURATION=\n[PRACTICE]\nTIME=10\n",
        1,
    );
    let cfg = load_dir(&dir).expect("load");
    assert_eq!(cfg.server.udp_port, 9600);
    assert_eq!(cfg.server.vote_duration, Duration::from_secs(20));
}
