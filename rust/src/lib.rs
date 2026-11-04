//! acserver-rs: an Assetto Corsa multiplayer server for protocol 202. It reads vanilla `server_cfg.ini` / `entry_list.ini`
//! and follows the documented protocol in `docs/`.

pub mod ai;
pub mod config;
pub mod httpapi;
pub mod plugin;
pub mod proto;
pub mod rcon;
pub mod results;
pub mod server;
pub mod wire;
