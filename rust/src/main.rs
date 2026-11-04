//! Command acserver-rs is a Rust replacement for the Kunos Assetto Corsa
//! dedicated server (protocol 202). See README.md.

use clap::Parser;
use std::io::IsTerminal;
use std::process::ExitCode;

use acserver_rs::server::Server;
use acserver_rs::{config, httpapi, plugin, rcon};
use anyhow::Context as _;

/// Command-line flags. `-c`, `-e` and `-platform` are vanilla acServer's, so
/// tools that launch it (the Kunos acServerManager) can launch this server
/// unchanged.
#[derive(Debug, Parser)]
#[command(
    name = "acserver-rs",
    about = "An Assetto Corsa multiplayer server (protocol 202)"
)]
struct Flags {
    /// server_cfg.ini, or a directory holding it (plus entry_list.ini and
    /// extra_cfg.yml).
    #[arg(short = 'c', long = "cfg", default_value = "cfg")]
    cfg_path: String,
    /// entry_list.ini (default: next to --cfg).
    #[arg(short = 'e', long = "entry")]
    entry_path: Option<String>,
    /// extra_cfg.yml (default: next to --cfg).
    #[arg(long = "extra")]
    extra_path: Option<String>,
    /// The AC install root holding content/ and system/ (its content/ dir is
    /// also accepted), for checksums. Pass "" to disable.
    #[arg(long = "content", default_value = "content")]
    content_dir: String,
    /// Enable debug logging.
    #[arg(long)]
    debug: bool,
    /// Vanilla's -platform, accepted and ignored.
    #[arg(long = "platform", hide = true)]
    _platform: Option<String>,
}

fn main() -> ExitCode {
    // Vanilla takes Go-style `-platform`; clap needs `--platform`.
    let args = std::env::args().map(|arg| {
        if arg == "-platform" || arg.starts_with("-platform=") {
            format!("-{arg}")
        } else {
            arg
        }
    });
    let flags = Flags::parse_from(args);

    let level = if flags.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(level))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(%err, "failed to start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(flags)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Loads the configuration, starts every front end and runs until SIGINT or
/// SIGTERM.
async fn run(flags: Flags) -> anyhow::Result<()> {
    let entry = flags.entry_path.as_deref().unwrap_or("");
    let extra = flags.extra_path.as_deref().unwrap_or("");
    let files = config::resolve(&flags.cfg_path, entry, extra);
    let mut cfg =
        config::load_files(files, &flags.content_dir).context("failed to load configuration")?;
    tracing::info!(
        track = %cfg.server.track,
        entries = cfg.entry_list.len(),
        sessions = cfg.sessions.len(),
        max_clients = cfg.server.max_clients,
        content = !flags.content_dir.is_empty(),
        "configuration loaded"
    );
    if cfg.server.admin_password.is_empty() {
        tracing::warn!("ADMIN_PASSWORD is empty; in-game administration is unavailable");
    }
    for warning in &cfg.warnings {
        tracing::warn!("{warning}");
    }
    cfg.public_ip = acserver_rs::server::public_ip(&cfg).await;

    let server = Server::new(cfg).context("failed to build server")?;

    let shutdown = server.shutdown.clone();
    tokio::spawn(async move {
        wait_for_signal().await;
        shutdown.cancel();
    });

    // Optional ACSP plugin API.
    match plugin::PluginManager::new(&server).await {
        Err(err) => tracing::error!("plugin API disabled: {err:#}"),
        Ok(None) => {}
        Ok(Some(manager)) => {
            server.set_plugin(manager.clone());
            tokio::spawn(manager.run());
            tracing::info!(
                listen = server.cfg.server.udp_plugin_local_port,
                send = %server.cfg.server.udp_plugin_address,
                "plugin API enabled"
            );
        }
    }

    // HTTP API.
    let http_server = std::sync::Arc::clone(&server);
    tokio::spawn(async move {
        if let Err(err) = httpapi::serve(http_server).await {
            tracing::error!("http server: {err:#}");
        }
    });

    // Outbound lobby registration.
    tokio::spawn(httpapi::lobby::run(std::sync::Arc::clone(&server)));

    // Optional RCON.
    let rcon_server = std::sync::Arc::clone(&server);
    tokio::spawn(async move {
        if let Err(err) = rcon::run(rcon_server).await {
            tracing::error!("rcon: {err:#}");
        }
    });

    server.run().await.context("server exited with error")?;
    tracing::info!("server stopped");
    Ok(())
}

/// Waits for SIGINT, or SIGTERM where there is one.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
