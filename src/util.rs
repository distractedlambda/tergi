//! Small helpers: dates, text width, base64 for the clipboard, running
//! blocking work from async code, and numbering background requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthChar;

/// Run blocking work on Tokio's blocking pool. If `f` panics, the panic
/// carries on in the caller, so it reaches whoever is waiting on the task.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // The work was cancelled because the runtime is shutting down. The
        // caller is about to be dropped too, so just wait for that.
        Err(_) => std::future::pending().await,
    }
}

/// Numbers requests for background work where only the newest request
/// matters, such as the history walk. Each request gets a number, and a
/// result is only used if its number is still the current one. Clones share
/// the counter, so a background task can hold one and stop early once a
/// newer request replaces its own.
#[derive(Debug, Clone, Default)]
pub struct Generation(Arc<AtomicU64>);

impl Generation {
    /// Start a new request, which replaces all earlier ones.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The number of the newest request, or 0 if there hasn't been one.
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.current() == generation
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convert days since 1970-01-01 to (year, month, day). This is Howard
/// Hinnant's `civil_from_days` algorithm.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A full date like "2026-03-04 14:05 +0100" in the commit's own time zone.
pub fn format_date(secs: i64, offset: i32) -> String {
    let local = secs + i64::from(offset);
    let (y, m, d) = civil(local.div_euclid(86_400));
    let t = local.rem_euclid(86_400);
    let sign = if offset < 0 { '-' } else { '+' };
    let off = offset.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}{:02}",
        t / 3600,
        (t % 3600) / 60,
        off / 3600,
        (off % 3600) / 60
    )
}

/// A short relative date for the history list.
pub fn short_date(secs: i64, now: i64) -> String {
    let age = now - secs;
    if (0..60).contains(&age) {
        return "just now".into();
    }
    if (0..3600).contains(&age) {
        return format!("{} min ago", age / 60);
    }
    if (0..86_400).contains(&age) {
        let h = age / 3600;
        return format!("{h} hour{} ago", if h == 1 { "" } else { "s" });
    }
    if (0..7 * 86_400).contains(&age) {
        let d = age / 86_400;
        return format!("{d} day{} ago", if d == 1 { "" } else { "s" });
    }
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let (ny, _, _) = civil(now.div_euclid(86_400));
    if y == ny {
        format!("{} {d}", MONTHS[(m - 1) as usize])
    } else {
        format!("{} {d}, {y}", MONTHS[(m - 1) as usize])
    }
}

/// Display width of a string, with tabs expanded to `tab` columns.
pub fn width(s: &str, tab: usize) -> usize {
    let mut w = 0;
    for c in s.chars() {
        w += char_width(c, w, tab);
    }
    w
}

pub fn char_width(c: char, col: usize, tab: usize) -> usize {
    if c == '\t' {
        tab - col % tab
    } else {
        c.width().unwrap_or(0)
    }
}

/// Cut a string to at most `max` columns, adding an ellipsis if it was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s, 4) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c, w, 4);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Fuzzy match: every character of `needle` appears in order in `hay`.
/// Returns a score where lower is better, or `None` when it doesn't match.
pub fn fuzzy_score(needle: &str, hay: &str) -> Option<usize> {
    let hay_l = hay.to_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(pos) = hay_l.find(&needle.to_lowercase()) {
        return Some(pos);
    }
    let mut score = 100;
    let mut it = hay_l.chars().enumerate();
    let mut last = 0;
    for nc in needle.to_lowercase().chars() {
        let (i, _) = it.by_ref().find(|(_, c)| *c == nc)?;
        score += i - last;
        last = i;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(format_date(0, 0), "1970-01-01 00:00 +0000");
        assert_eq!(format_date(1_700_000_000, 3600), "2023-11-14 23:13 +0100");
        assert_eq!(format_date(1_700_000_000, -18_000), "2023-11-14 17:13 -0500");
        let now = 1_700_000_000;
        assert_eq!(short_date(now - 30, now), "just now");
        assert_eq!(short_date(now - 3600 * 5, now), "5 hours ago");
        assert_eq!(short_date(now - 86_400, now), "1 day ago");
        assert_eq!(short_date(now - 86_400 * 30, now), "Oct 15");
        assert_eq!(short_date(0, now), "Jan 1, 1970");
    }

    #[test]
    fn b64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn widths() {
        assert_eq!(width("a\tb", 4), 5);
        assert_eq!(width("日本", 4), 4);
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hi", 6), "hi");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("rw", "Reword commit").is_some());
        assert!(fuzzy_score("xyz", "Reword commit").is_none());
        assert!(fuzzy_score("rew", "Reword").unwrap() < fuzzy_score("rwd", "Reword").unwrap());
    }
}
