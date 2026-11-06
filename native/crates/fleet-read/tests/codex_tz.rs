//! The date folders a Codex home keeps are the machine's LOCAL Y/MM/DD, as `new Date()` gives in Node. This test alone sets
//! `TZ` (a POSIX string, so no tz database is needed), so it has a test binary of its own.

use fleet_read::codex::recent_rollouts;
use std::fs::{self, FileTimes, OpenOptions};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// 2026-11-06T23:30:00Z: the 7th at UTC+14, the 6th at UTC-10.
const NOW: i64 = 1_794_007_800_000;

fn rollout_in(home: &Path, day: &str) {
    let d = home.join("sessions").join(day);
    fs::create_dir_all(&d).unwrap();
    let f = d.join("rollout-x.jsonl");
    fs::write(&f, "{\"payload\":{\"id\":\"x\"}}\n").unwrap();
    OpenOptions::new()
        .write(true)
        .open(&f)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(NOW as u64 + 1000)),
        )
        .unwrap();
}

#[test]
fn day_folders_are_local_time() {
    // within 0 ms the folders of the local day and the day before are looked in, and the file is newer than the window
    let ahead = tempfile::tempdir().unwrap();
    rollout_in(ahead.path(), "2026/11/07");
    let behind = tempfile::tempdir().unwrap();
    rollout_in(behind.path(), "2026/11/05");

    std::env::set_var("TZ", "XXX-14"); // UTC+14: the 7th, then the 6th
    assert_eq!(recent_rollouts(ahead.path(), 0, NOW).len(), 1);
    assert_eq!(recent_rollouts(behind.path(), 0, NOW).len(), 0);
    std::env::set_var("TZ", "XXX10"); // UTC-10: the 6th, then the 5th
    assert_eq!(recent_rollouts(ahead.path(), 0, NOW).len(), 0);
    assert_eq!(recent_rollouts(behind.path(), 0, NOW).len(), 1);
}
