//! fleetd: the session board as a daemon. The watcher turns file events into re-read sessions; the store keeps one
//! truthful `Session` per session and the numbered deltas between; the socket serves both to clients.

pub mod alarm;
pub mod attach;
pub mod attach_cmd;
pub mod bench;
pub mod board_meta;
pub mod client;
pub mod config;
pub mod health;
#[cfg(target_os = "macos")]
pub mod kq;
pub mod machine;
pub mod net;
pub mod out;
pub mod procscan;
pub mod pty;
pub mod reads;
pub mod serve;
pub mod service;
pub mod sessions;
pub mod store;
pub mod tailscale;
pub mod terms;
pub mod watch;
