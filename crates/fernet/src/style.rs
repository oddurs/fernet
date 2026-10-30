//! Output style: lowercase, quiet and exact. Colour only on a TTY, and
//! `NO_COLOR` is respected.

use jiff::tz::TimeZone;
use jiff::{Timestamp, Unit};

/// Whether to use colour on stdout.
pub(crate) fn colour(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Dims `s` when colour is on.
pub(crate) fn dim(s: &str, colour: bool) -> String {
    if colour {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

/// A mark drawn as a rule: `──────────── tephra · name ────────────`.
pub(crate) fn tephra(name: &str) -> String {
    let side = "─".repeat(12);
    format!("{side} tephra · {name} {side}")
}

/// A short label for an instant relative to `now`: `-45s`, `-3h` within a
/// day, then `oct 28`, then `oct 28 2025` in other years.
pub(crate) fn when(nanos: i64, now: Timestamp, tz: &TimeZone) -> String {
    let t = fernet_fs_core::time::timestamp(nanos);
    let ago = now.as_second() - t.as_second();
    match ago {
        i64::MIN..0 => short_date(t, now, tz),
        0..60 => format!("-{ago}s"),
        60..3600 => format!("-{}m", ago / 60),
        3600..86_400 => format!("-{}h", ago / 3600),
        _ => short_date(t, now, tz),
    }
}

fn short_date(t: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let z = t.to_zoned(tz.clone());
    let fmt = if z.year() == now.to_zoned(tz.clone()).year() {
        "%b %-d"
    } else {
        "%b %-d %Y"
    };
    z.strftime(fmt).to_string().to_lowercase()
}

/// A full local timestamp, for `fernet log`: `2026-10-30 16:45:12`.
pub(crate) fn stamp(nanos: i64, tz: &TimeZone) -> String {
    let t = fernet_fs_core::time::timestamp(nanos);
    t.to_zoned(tz.clone()).round(Unit::Second).map_or_else(
        |_| t.to_string(),
        |z| z.strftime("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// Human sizes: `512 b`, `1.2 kib`, `3.4 mib`.
pub(crate) fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // Display only.
    let b = bytes as f64;
    match bytes {
        0..1024 => format!("{bytes} b"),
        1024..1_048_576 => format!("{:.1} kib", b / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} mib", b / 1_048_576.0),
        _ => format!("{:.1} gib", b / 1_073_741_824.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let tz = TimeZone::UTC;
        let now: Timestamp = "2026-10-30T16:00:00Z".parse().unwrap();
        let n = |s: &str| fernet_fs_core::time::nanos(s.parse().unwrap());
        assert_eq!(when(n("2026-10-30T15:59:15Z"), now, &tz), "-45s");
        assert_eq!(when(n("2026-10-30T13:00:00Z"), now, &tz), "-3h");
        assert_eq!(when(n("2026-10-28T09:00:00Z"), now, &tz), "oct 28");
        assert_eq!(when(n("2025-10-28T09:00:00Z"), now, &tz), "oct 28 2025");
        assert_eq!(size(2048), "2.0 kib");
        assert_eq!(tephra("x"), "──────────── tephra · x ────────────");
    }
}
