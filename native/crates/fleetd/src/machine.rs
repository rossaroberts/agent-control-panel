//! This machine's name on the board (Ruling R3): `FLEETD_MACHINE`, else the `self` of the panel's `fleet.json`, else the
//! short host name. The panel's file is only read.

use std::path::Path;

pub fn machine_name(home: &Path) -> String {
    if let Some(m) = std::env::var("FLEETD_MACHINE")
        .ok()
        .filter(|m| !m.is_empty())
    {
        return m;
    }
    let panel = home.join("Library/Application Support/Agent Panel/fleet.json");
    if let Some(m) = std::fs::read(panel)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("self")?.as_str().map(str::to_string))
        .filter(|m| !m.is_empty())
    {
        return m;
    }
    short_hostname()
}

fn short_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is 256 bytes and the call is told so; it NUL-terminates what fits.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]);
    name.split('.').next().unwrap_or_default().to_string()
}
