//! Size-capped JSON file reads: anything missing, too big or not JSON is `None`.

use std::path::Path;

use serde::de::DeserializeOwned;

pub(crate) fn read_json<T: DeserializeOwned>(file: &Path, max: u64) -> Option<T> {
    let md = std::fs::metadata(file).ok()?;
    if md.len() > max {
        return None;
    }
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}
