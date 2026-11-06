//! fleetd: the session board as a daemon. The watcher turns file events into re-read sessions; the store keeps one
//! truthful `Session` per session and the numbered deltas between.

#[cfg(target_os = "macos")]
pub mod kq;
pub mod machine;
pub mod procscan;
pub mod reads;
pub mod sessions;
pub mod store;
pub mod watch;
