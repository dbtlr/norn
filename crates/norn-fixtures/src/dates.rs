//! Dependency-free proleptic-Gregorian calendar math (Howard Hinnant's
//! `days_from_civil` / `civil_from_days` algorithms — public domain, no
//! external crate needed) plus ISO formatting for the two datetime shapes
//! the expansion generator emits.

/// Days since 1970-01-01 for a given (year, month, day).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A resolved point in time, decomposed for ISO formatting.
pub struct Moment {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
}

/// Base 2024-01-01T00:00 plus `minutes_offset` minutes, wall-clock (no
/// timezone math — offsets are small enough to never need it beyond simple
/// day rollover).
pub fn from_base_plus_minutes(minutes_offset: i64) -> Moment {
    let base_day = days_from_civil(2024, 1, 1);
    let day_offset = minutes_offset.div_euclid(1440);
    let minute_of_day = minutes_offset.rem_euclid(1440);
    let (year, month, day) = civil_from_days(base_day + day_offset);
    Moment {
        year,
        month,
        day,
        hour: minute_of_day / 60,
        minute: minute_of_day % 60,
    }
}

impl Moment {
    /// `YYYY-MM-DD`.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `YYYY-MM-DDTHH:MM:00Z`.
    pub fn datetime_z(&self) -> String {
        format!("{}T{:02}:{:02}:00Z", self.date(), self.hour, self.minute)
    }

    /// `YYYY-MM-DD HH:MM:00` (space-separated form).
    pub fn datetime_space(&self) -> String {
        format!("{} {:02}:{:02}:00", self.date(), self.hour, self.minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_new_years_day() {
        let m = from_base_plus_minutes(0);
        assert_eq!(m.date(), "2024-01-01");
        assert_eq!(m.datetime_z(), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn rolls_over_days() {
        // 1500 minutes = 1 day, 60 minutes.
        let m = from_base_plus_minutes(1500);
        assert_eq!(m.date(), "2024-01-02");
        assert_eq!(m.hour, 1);
        assert_eq!(m.minute, 0);
    }

    #[test]
    fn handles_leap_day() {
        // 2024 is a leap year; Feb has 29 days. 31 (Jan) + 29 = 60 days to
        // reach March 1.
        let m = from_base_plus_minutes(60 * 1440);
        assert_eq!(m.date(), "2024-03-01");
    }
}
