//! The one place jarvisd turns a `SystemTime` into the RFC 3339 string every
//! DTO's timestamp fields carry (F9.9: was reimplemented at 8 call sites in
//! 3 divergent failure behaviours).

use std::time::SystemTime;
use time::format_description::well_known::Rfc3339;

/// Format `t` as RFC 3339 UTC. Never panics: `OffsetDateTime::format` can
/// only fail for a year outside 0000-9999, which cannot happen for
/// `SystemTime::now()` but is not provably impossible for a timestamp read
/// back from storage — a corrupted persisted value must degrade to a
/// recognizable sentinel, never crash the request handler that happened to
/// read it. (Chosen over the two other behaviours this call had accreted:
/// `.expect(...)` panicking the handler, and `.unwrap_or_default()` silently
/// returning `""`, which is indistinguishable from a missing field.)
pub fn rfc3339(t: SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// Round `t` down to microsecond precision — Postgres `timestamptz`'s native
/// resolution, so a value written and read back compares equal instead of
/// silently losing its trailing nanoseconds on the round trip.
pub fn truncate_to_micros(t: SystemTime) -> SystemTime {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => std::time::UNIX_EPOCH + std::time::Duration::from_micros(d.as_micros() as u64),
        // Pre-epoch clock: leave untouched, storage will reject it.
        Err(_) => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_unix_epoch() {
        assert_eq!(rfc3339(std::time::UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn truncates_sub_microsecond_precision() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1_500_500_999);
        let truncated = truncate_to_micros(t);
        assert_eq!(
            truncated.duration_since(std::time::UNIX_EPOCH).unwrap(),
            std::time::Duration::from_micros(1_500_500)
        );
    }

    #[test]
    fn a_pre_epoch_time_is_left_untouched() {
        let before_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(truncate_to_micros(before_epoch), before_epoch);
    }
}
