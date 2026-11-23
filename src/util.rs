//! Small helpers: dates, base64 for the clipboard, running blocking work
//! from async code, numbering background requests, and turning git's paths
//! into file system paths.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
#[derive(Debug, Clone)]
pub struct Generation(Arc<tokio::sync::watch::Sender<u64>>);

impl Default for Generation {
    fn default() -> Self {
        Generation(Arc::new(tokio::sync::watch::Sender::new(0)))
    }
}

impl Generation {
    /// Start a new request, which replaces all earlier ones.
    pub fn next(&self) -> u64 {
        let mut next = 0;
        self.0.send_modify(|g| {
            *g += 1;
            next = *g;
        });
        next
    }

    /// The number of the newest request, or 0 if there hasn't been one.
    pub fn current(&self) -> u64 {
        *self.0.borrow()
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.current() == generation
    }

    /// Wait until a newer request replaces `generation`.
    pub async fn replaced(&self, generation: u64) {
        // This can only fail once the sender is gone, and we hold it.
        let _ = self.0.subscribe().wait_for(|&g| g != generation).await;
    }
}

/// A path as git prints it, as a path for the file system. Git stores the
/// bytes of a file name as they are, and so does Unix, so nothing is lost
/// even when the name isn't UTF-8.
pub fn fs_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// The bytes of a file system path, as git would print them.
pub fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// A byte string as an argument or environment value for a child process.
pub fn os_str(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
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

/// Split a timestamp into days since 1970-01-01 and seconds into that
/// day, in the time zone `offset` seconds east of UTC. A crafted commit
/// can have a timestamp near `i64::MAX`, so we add the offset to the
/// seconds within the day rather than to the timestamp itself.
fn local_day(secs: i64, offset: i32) -> (i64, i64) {
    let t = secs.rem_euclid(86_400) + i64::from(offset);
    (
        secs.div_euclid(86_400) + t.div_euclid(86_400),
        t.rem_euclid(86_400),
    )
}

/// A full date like "2026-03-04 14:05 +0100" in the commit's own time zone.
pub fn format_date(secs: i64, offset: i32) -> String {
    let (days, t) = local_day(secs, offset);
    let (y, m, d) = civil(days);
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
    let (y, m, d) = civil(local_day(secs, offset).0);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A short relative date for the history list.
pub fn short_date(secs: i64, now: i64) -> String {
    let age = now.saturating_sub(secs);
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

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
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
        assert_eq!(
            format_date(1_700_000_000, -18_000),
            "2023-11-14 17:13 -0500"
        );
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
    fn extreme_dates() {
        assert_eq!(format_date(i64::MAX, 0), "292277026596-12-04 15:30 +0000");
        assert_eq!(
            format_date(i64::MAX - 100, 50_400),
            "292277026596-12-05 05:28 +1400"
        );
        assert_eq!(
            format_date(i64::MIN, -43_200),
            "-292277022657-01-26 20:29 -1200"
        );
        assert_eq!(format_day(i64::MAX, 50_400), "292277026596-12-05");
        assert_eq!(format_day(i64::MIN, -43_200), "-292277022657-01-26");
        assert_eq!(format_day(i64::MAX, i32::MAX), "292277026664-12-23");
        assert_eq!(format_day(i64::MIN, i32::MIN), "-292277022725-01-08");
        let now = 1_700_000_000;
        assert_eq!(short_date(i64::MIN, now), "Jan 27, -292277022657");
        assert_eq!(short_date(i64::MAX, now), "Dec 4, 292277026596");
        assert_eq!(short_date(now, i64::MIN), "Nov 14, 2023");
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
        fn offset_matches_wide_arithmetic(secs in any::<i64>(), offset in any::<i32>()) {
            let local = i128::from(secs) + i128::from(offset);
            let days = i64::try_from(local.div_euclid(86_400)).expect("days fit in i64");
            let t = i64::try_from(local.rem_euclid(86_400)).expect("seconds fit in i64");
            prop_assert_eq!(local_day(secs, offset), (days, t));
        }

        // A commit can claim any timestamp and offset, and debug builds
        // abort on overflow, so none of these may overflow.
        #[test]
        fn dates_never_panic(secs in any::<i64>(), offset in any::<i32>(), now in any::<i64>()) {
            format_date(secs, offset);
            format_day(secs, offset);
            short_date(secs, now);
        }

        #[test]
        fn fuzzy_matches_letters_in_order(needle in "[abAB]{0,4}", hay in "[abAB ]{0,10}") {
            let mut hay_chars = hay.to_lowercase().chars().collect::<Vec<_>>().into_iter();
            let in_order = needle.to_lowercase().chars().all(|n| hay_chars.any(|h| h == n));
            prop_assert_eq!(fuzzy_score(&needle, &hay).is_some(), in_order);
        }
    }
}
