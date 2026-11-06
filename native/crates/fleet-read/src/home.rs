//! The home folder fleet reads under: `FLEETD_HOME` (for fixtures), else `HOME`.

use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    std::env::var_os("FLEETD_HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}
