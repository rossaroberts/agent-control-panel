//! fleetd: the session board as a daemon. The watcher turns file events into re-read sessions; the store keeps one
//! truthful `Session` per session and the numbered deltas between.

pub mod machine;
pub mod procscan;
pub mod sessions;
pub mod store;
pub mod watch;
