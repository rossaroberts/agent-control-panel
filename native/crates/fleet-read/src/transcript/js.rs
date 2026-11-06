//! The bits of JavaScript semantics `readTranscript` leans on, so the port gives Node's answers: whitespace and `trim`,
//! `slice` in UTF-16 units, truthiness, `String(v)`, `Date.parse` (ISO forms), `path.dirname`/`basename`, a URL's host,
//! and `JSON.parse`'s tolerance of lone surrogates.

use serde_json::Value;

/// The characters JS's `\s` and `String.prototype.trim` treat as white space (WhiteSpace and LineTerminator).
pub const WS: &str = "\\t\\n\\x0B\\x0C\\r \\u{A0}\\u{1680}\\u{2000}-\\u{200A}\\u{2028}\\u{2029}\\u{202F}\\u{205F}\\u{3000}\\u{FEFF}";

pub fn is_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub fn trim(s: &str) -> &str {
    s.trim_matches(is_ws)
}

/// `s.slice(0, n)`: the first `n` UTF-16 code units. A surrogate pair the cut would split is left out (JS keeps half).
pub fn slice16(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// `oneLine`: white space runs as one space, trimmed, at most `n` UTF-16 units.
pub fn one_line(s: &str, n: usize) -> String {
    let mut out = String::with_capacity(s.len().min(n * 4));
    let mut gap = false;
    for c in trim(s).chars() {
        if is_ws(c) {
            gap = true;
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(c);
    }
    let cut = slice16(&out, n).len();
    out.truncate(cut);
    out
}

pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `v ?? fallback` over JSON: absent and `null` fall through.
pub fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// `String(v)` (and template interpolation) for JSON values.
pub fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(xs) => join(xs, ","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `xs.join(sep)`: `null` elements are empty.
pub fn join(xs: &[Value], sep: &str) -> String {
    xs.iter()
        .map(|x| {
            if x.is_null() {
                String::new()
            } else {
                string(x)
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}

/// `${v ?? fallback}`.
pub fn tmpl(v: Option<&Value>, fallback: &str) -> String {
    present(v).map_or_else(|| fallback.to_string(), string)
}

/// `Date.parse(s) || null` for the ISO 8601 forms V8 reads (`YYYY[-MM[-DD]]`, then `T` or a space, `HH:mm[:ss[.f…]]`,
/// then `Z` or `±HH:mm`), plus `±YYYYYY` years. A date-time with no offset is local time in JS: `None` here, as is any
/// other form (V8's legacy parser) and an answer of 0.
pub fn date_parse(s: &str) -> Option<i64> {
    let b = s.trim_matches(is_ws).as_bytes();
    let mut i = 0;
    let num = |i: &mut usize, n: usize| -> Option<i64> {
        let d = b.get(*i..*i + n)?;
        if !d.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *i += n;
        Some(d.iter().fold(0, |a, &c| a * 10 + i64::from(c - b'0')))
    };
    let year = match b.first()? {
        b'+' | b'-' => {
            let neg = b[0] == b'-';
            i = 1;
            let y = num(&mut i, 6)?;
            if neg {
                -y
            } else {
                y
            }
        }
        _ => num(&mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = num(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = num(&mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * 86_400_000;
    if i < b.len() {
        if !matches!(b[i], b'T' | b't' | b' ') {
            return None;
        }
        i += 1;
        let h = num(&mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        let m = num(&mut i, 2)?;
        let (mut sec, mut frac) = (0, 0);
        if b.get(i) == Some(&b':') {
            i += 1;
            sec = num(&mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let d = &b[start..i.min(start + 3)];
                frac = d.iter().fold(0, |a, &c| a * 10 + i64::from(c - b'0'))
                    * 10_i64.pow(3 - d.len() as u32);
            }
        }
        if h > 24 || m > 59 || sec > 59 || (h == 24 && (m, sec, frac) != (0, 0, 0)) {
            return None;
        }
        ms += ((h * 60 + m) * 60 + sec) * 1000 + frac;
        match b.get(i) {
            Some(b'Z' | b'z') if i + 1 == b.len() => {}
            Some(&sign @ (b'+' | b'-')) => {
                i += 1;
                let oh = num(&mut i, 2)?;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = num(&mut i, 2)?;
                if i != b.len() || oh > 23 || om > 59 {
                    return None;
                }
                let off = (oh * 60 + om) * 60_000;
                ms += if sign == b'+' { -off } else { off };
            }
            _ => return None, // no offset: local time
        }
    }
    (ms != 0).then_some(ms)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (a day past the month's end rolls over, as in JS).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Node's posix `path.dirname`.
pub fn dirname(p: &str) -> &str {
    let b = p.as_bytes();
    if b.is_empty() {
        return ".";
    }
    let root = b[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => {
            if root {
                "/"
            } else {
                "."
            }
        }
        Some(1) if root => "//",
        Some(e) => &p[..e],
    }
}

/// Node's posix `path.basename`.
pub fn basename(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or("")
}

/// `new URL(u).host`, or `None` where the constructor throws. Covers the URLs a fetch names: a special scheme's host
/// (lower case, default port dropped, user info dropped); other schemes without an authority have an empty host.
pub fn url_host(u: &str) -> Option<String> {
    let u = u.trim_matches(|c: char| c <= ' ');
    let colon = u.find(':')?;
    let scheme = &u[..colon];
    let mut sc = scheme.chars();
    if !sc.next()?.is_ascii_alphabetic()
        || !sc.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let rest = &u[colon + 1..];
    let default_port = match scheme.as_str() {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        "ftp" => Some("21"),
        "file" => None,
        _ => {
            return Some(match rest.strip_prefix("//") {
                Some(a) => authority_host(a, None)?,
                None => String::new(),
            })
        }
    };
    if scheme == "file" {
        return Some(
            rest.strip_prefix("//")
                .and_then(|a| authority_host(a, None))
                .unwrap_or_default(),
        );
    }
    let host = authority_host(rest.trim_start_matches(['/', '\\']), default_port)?;
    (!host.is_empty()).then_some(host)
}

fn authority_host(a: &str, default_port: Option<&str>) -> Option<String> {
    let auth = &a[..a.find(['/', '?', '#', '\\']).unwrap_or(a.len())];
    let hp = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let (host, port) = match hp.rfind(':') {
        Some(i) if !hp.ends_with(']') => (&hp[..i], Some(&hp[i + 1..])),
        _ => (hp, None),
    };
    let host = host.to_lowercase();
    match port {
        Some("") | None => Some(host),
        Some(p) if p.bytes().all(|c| c.is_ascii_digit()) => {
            let p = p.trim_start_matches('0');
            let p = if p.is_empty() { "0" } else { p };
            if p.len() > 5 || p.parse::<u32>().ok()? > 65_535 {
                return None;
            }
            Some(if Some(p) == default_port {
                host
            } else {
                format!("{host}:{p}")
            })
        }
        Some(_) => None,
    }
}

/// `JSON.parse` of one line as a `serde_json::Value`. JS takes a lone surrogate escape (`"\ud83d"`, what cutting an
/// emoji in half writes) where serde_json refuses the line: such escapes become U+FFFD and the line is read.
pub fn parse(line: &[u8]) -> Option<Value> {
    let s = String::from_utf8_lossy(line);
    match serde_json::from_str(&s) {
        Ok(v) => Some(v),
        Err(_) if s.contains("\\u") => serde_json::from_str(&mend_surrogates(&s)).ok(),
        Err(_) => None,
    }
}

fn mend_surrogates(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| -> Option<u16> {
        let h = b.get(i..i + 4)?;
        u16::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    };
    let esc = |i: usize| -> Option<u16> {
        (b.get(i) == Some(&b'\\') && b.get(i + 1) == Some(&b'u'))
            .then(|| hex(i + 2))
            .flatten()
    };
    let mut out = String::with_capacity(s.len());
    let (mut i, mut from) = (0, 0);
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        match esc(i) {
            Some(0xD800..=0xDBFF) if matches!(esc(i + 6), Some(0xDC00..=0xDFFF)) => i += 12,
            Some(0xD800..=0xDFFF) => {
                out.push_str(&s[from..i]);
                out.push_str("\\ufffd");
                i += 6;
                from = i;
            }
            _ => i += 2,
        }
    }
    out.push_str(&s[from..]);
    out
}
