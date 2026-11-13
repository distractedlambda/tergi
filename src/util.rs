//! Small helpers: dates, text width, base64 for the clipboard, running
//! blocking work from async code, and numbering background requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// The day, like "2026-03-04", in the commit's own time zone.
pub fn format_day(secs: i64, offset: i32) -> String {
    let (y, m, d) = civil((secs + i64::from(offset)).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
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
    } else if c.is_control() {
        // We draw other control characters as a replacement character.
        1
    } else {
        c.width().unwrap_or(0)
    }
}

/// Split a line into the pieces that each start a new terminal cell, with
/// the byte offset of each one.
///
/// A character and the zero-width characters after it, such as combining
/// accents, stay together in one piece. Some graphemes are wider in a
/// terminal than the sum of their characters, which is how `width`
/// measures them. Emoji joined by zero-width joiners are an example. We
/// split those into their characters, so that the terminal draws them in
/// as many columns as we count. Zero-width characters that end up on
/// their own are dropped. That keeps format characters such as bidi
/// overrides away from the terminal.
pub fn cells(line: &str) -> impl Iterator<Item = (usize, &str)> {
    line.grapheme_indices(true)
        .flat_map(|(gi, g)| {
            let whole = !g.contains(char::is_control)
                && g.width() == g.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
            g.char_indices().filter_map(move |(ci, c)| {
                if whole {
                    (ci == 0).then_some((gi, g))
                } else {
                    Some((gi + ci, &g[ci..ci + c.len_utf8()]))
                }
            })
        })
        .filter(|(_, s)| width(s, 1) > 0)
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
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn dates() {
        assert_eq!(format_date(0, 0), "1970-01-01 00:00 +0000");
        assert_eq!(format_date(1_700_000_000, 3600), "2023-11-14 23:13 +0100");
        assert_eq!(format_date(1_700_000_000, -18_000), "2023-11-14 17:13 -0500");
        assert_eq!(format_day(1_700_000_000, 3600), "2023-11-14");
        assert_eq!(format_day(1_699_999_000, 36_000), "2023-11-15");
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
        assert_eq!(width("a\x07\0b", 4), 4);
        let pieces = |s| cells(s).collect::<Vec<_>>();
        assert_eq!(pieces("e\u{301}\u{323}x"), [(0, "e\u{301}\u{323}"), (5, "x")]);
        assert_eq!(pieces("\u{301}a\u{202e}"), [(2, "a")]);
        assert_eq!(pieces("❤\u{fe0f}"), [(0, "❤")]);
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("hi", 6), "hi");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("rw", "Reword commit").is_some());
        assert!(fuzzy_score("xyz", "Reword commit").is_none());
        assert!(fuzzy_score("rew", "Reword").unwrap() < fuzzy_score("rwd", "Reword").unwrap());
    }

    fn days_in_month(y: i64, m: u32) -> u32 {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        match m {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        }
    }

    proptest! {
        #[test]
        fn civil_days_follow_each_other(days in -1_000_000i64..1_000_000) {
            let (y, m, d) = civil(days);
            let next = if d < days_in_month(y, m) {
                (y, m, d + 1)
            } else if m < 12 {
                (y, m + 1, 1)
            } else {
                (y + 1, 1, 1)
            };
            prop_assert_eq!(civil(days + 1), next);
        }

        #[test]
        fn cells_cover_the_width(s in "[a\t\u{7}é日❤👨\u{301}\u{200b}\u{200d}\u{202e}\u{fe0f}]{0,20}") {
            let mut end = 0;
            let mut total = 0;
            for (i, piece) in cells(&s) {
                prop_assert!(i >= end && s[i..].starts_with(piece), "{:?} at {} in {:?}", piece, i, s);
                end = i + piece.len();
                total += width(piece, 1);
            }
            prop_assert_eq!(total, width(&s, 1));
        }

        #[test]
        fn truncate_fits(s in "[ab \t日é]{0,20}", max in 0usize..25) {
            let t = truncate(&s, max);
            prop_assert!(width(&t, 4) <= max, "{:?} is wider than {}", t, max);
            if width(&s, 4) <= max {
                prop_assert_eq!(t, s);
            } else if max > 0 {
                let kept = t.strip_suffix('…');
                prop_assert!(kept.is_some_and(|k| s.starts_with(k)), "{:?} from {:?}", t, s);
            }
        }

        #[test]
        fn fuzzy_matches_letters_in_order(needle in "[abAB]{0,4}", hay in "[abAB ]{0,10}") {
            let mut hay_chars = hay.to_lowercase().chars().collect::<Vec<_>>().into_iter();
            let in_order = needle.to_lowercase().chars().all(|n| hay_chars.any(|h| h == n));
            prop_assert_eq!(fuzzy_score(&needle, &hay).is_some(), in_order);
        }
    }
}
