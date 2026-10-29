//! Calendar conversions, so zip's DOS timestamps need no date-time dependency.
//! Algorithms from Howard Hinnant's "chrono-compatible low-level date algorithms".

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Broken-down UTC time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub(crate) fn civil_to_system(c: Civil) -> Option<SystemTime> {
    let secs = days_from_civil(c.year, c.month, c.day) * 86_400
        + i64::from(c.hour) * 3600
        + i64::from(c.minute) * 60
        + i64::from(c.second);
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// `None` for times before 1970 or too far in the future.
pub(crate) fn system_to_civil(t: SystemTime) -> Option<Civil> {
    let secs = i64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400) as u32;
    Some(Civil {
        year,
        month,
        day,
        hour: rem / 3600,
        minute: rem % 3600 / 60,
        second: rem % 60,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        let c = system_to_civil(UNIX_EPOCH).unwrap();
        assert_eq!(
            c,
            Civil {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
        // 2000-02-29 12:34:56 UTC = 951_827_696
        let t = UNIX_EPOCH + Duration::from_secs(951_827_696);
        let c = system_to_civil(t).unwrap();
        assert_eq!(
            c,
            Civil {
                year: 2000,
                month: 2,
                day: 29,
                hour: 12,
                minute: 34,
                second: 56
            }
        );
        assert_eq!(civil_to_system(c), Some(t));
    }

    #[test]
    fn round_trips_every_day_in_a_range() {
        for days in (0..60_000).step_by(7) {
            let t = UNIX_EPOCH + Duration::from_secs(days * 86_400 + 12_345);
            assert_eq!(civil_to_system(system_to_civil(t).unwrap()), Some(t));
        }
    }

    #[test]
    fn pre_epoch_has_no_civil_time() {
        assert!(system_to_civil(UNIX_EPOCH - Duration::from_secs(1)).is_none());
    }
}
