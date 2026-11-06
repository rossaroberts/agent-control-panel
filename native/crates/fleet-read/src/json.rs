//! Size-capped JSON file reads: anything missing, too big or not JSON is `None`.

use std::io::Read;
use std::path::Path;

use serde::de::DeserializeOwned;

/// Reads at most `max` bytes: a file that grows past it while being read is refused as well.
pub(crate) fn read_json<T: DeserializeOwned>(file: &Path, max: u64) -> Option<T> {
    let mut buf = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(max + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > max {
        return None;
    }
    serde_json::from_slice(&buf).ok()
}
