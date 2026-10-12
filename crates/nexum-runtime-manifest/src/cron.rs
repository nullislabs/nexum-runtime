//! Five-field cron expressions for `schedule` triggers, evaluated in UTC.
//!
//! The fields are minute, hour, day of month, month and day of week. A
//! field is `*`, a number, a range `a-b`, a step `*/n` or `a-b/n`, or a
//! comma list of those. Nothing else parses: a seconds or year field,
//! an `@` macro, a timezone prefix, a name, and the `L`, `W`, `#` and
//! `?` extensions are refused rather than misread.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use thiserror::Error;

const MINUTES_PER_DAY: u64 = 24 * 60;

/// One Gregorian cycle: weekdays and leap days repeat exactly, so a
/// schedule naming a real date matches inside it.
const SEARCH_DAYS: u64 = 146_097;

/// One of the five fields, named as the refusals name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronField {
    /// 0-59.
    Minute,
    /// 0-23.
    Hour,
    /// 1-31.
    DayOfMonth,
    /// 1-12.
    Month,
    /// 0-7, where 0 and 7 are both Sunday.
    DayOfWeek,
}

impl CronField {
    const ALL: [Self; 5] = [
        Self::Minute,
        Self::Hour,
        Self::DayOfMonth,
        Self::Month,
        Self::DayOfWeek,
    ];

    const fn bounds(self) -> (u32, u32) {
        match self {
            Self::Minute => (0, 59),
            Self::Hour => (0, 23),
            Self::DayOfMonth => (1, 31),
            Self::Month => (1, 12),
            Self::DayOfWeek => (0, 7),
        }
    }
}

impl fmt::Display for CronField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::DayOfMonth => "day-of-month",
            Self::Month => "month",
            Self::DayOfWeek => "day-of-week",
        })
    }
}

/// Why a cron expression was refused; the wording is operator-pinned.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CronError {
    /// An `@daily`-style macro.
    #[error("`@` macros are not supported; write the five fields")]
    Macro,
    /// Not five whitespace-separated fields.
    #[error(
        "expected 5 fields (minute hour day-of-month month day-of-week), found {found}; \
         a seconds field, a year field and a timezone are not supported"
    )]
    FieldCount {
        /// Fields the expression carries.
        found: usize,
    },
    /// A name, or one of the `L`, `W`, `#` and `?` extensions.
    #[error(
        "{field} field {token:?}: names and the `L`, `W`, `#` and `?` extensions \
         are not supported; use numbers"
    )]
    Unsupported {
        /// The field carrying the token.
        field: CronField,
        /// The list element as written.
        token: String,
    },
    /// Not `*`, a number, a range or a step.
    #[error(
        "{field} field {token:?} is not `*`, a number, a range `a-b`, \
         or a step `*/n` or `a-b/n`"
    )]
    Malformed {
        /// The field carrying the token.
        field: CronField,
        /// The list element as written.
        token: String,
    },
    /// A value outside the field's bounds.
    #[error("{field} field value {value} in {token:?} is outside {min}-{max}")]
    OutOfRange {
        /// The field carrying the token.
        field: CronField,
        /// The list element as written.
        token: String,
        /// The offending value.
        value: u32,
        /// Lowest value the field takes.
        min: u32,
        /// Highest value the field takes.
        max: u32,
    },
    /// A range whose start is after its end; a range does not wrap.
    #[error("{field} field range {token:?} runs backwards; a range does not wrap")]
    ReversedRange {
        /// The field carrying the token.
        field: CronField,
        /// The list element as written.
        token: String,
    },
    /// A step of zero, or one past the field's highest value.
    #[error("{field} field step in {token:?} must be between 1 and {max}")]
    InvalidStep {
        /// The field carrying the token.
        field: CronField,
        /// The list element as written.
        token: String,
        /// Highest step the field takes.
        max: u32,
    },
    /// The day-of-month and month fields name no real date, such as
    /// 30 February.
    #[error("the day-of-month and month fields name no real date, so the schedule never fires")]
    NeverFires,
}

/// A validated five-field cron expression, evaluated in UTC.
///
/// When day-of-month and day-of-week are both restricted, a day matches
/// when either does; otherwise both must. A field is unrestricted when
/// it starts with `*`, so `*/2` leaves it unrestricted, as in Vixie cron.
#[derive(Clone, PartialEq, Eq)]
pub struct CronSchedule {
    /// Bit `n` set: minute `n` matches.
    minutes: u64,
    /// Bit `n` set: hour `n` matches.
    hours: u64,
    /// Bit `n` set: day `n` of the month matches.
    days_of_month: u64,
    /// Bit `n` set: month `n` matches.
    months: u64,
    /// Bit `n` set: weekday `n` matches, 0 being Sunday.
    days_of_week: u64,
    /// Both day fields restricted: either one matching admits the day.
    either_day: bool,
    expression: String,
}

impl CronSchedule {
    /// The expression as written, trimmed.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// The first matching minute strictly after `after`; both are
    /// durations since the Unix epoch, UTC.
    ///
    /// `None` only past the search bound of one Gregorian cycle, which a
    /// validated expression never reaches.
    pub fn next_after(&self, after: Duration) -> Option<Duration> {
        let first_minute = (after.as_secs() / 60).checked_add(1)?;
        let mut day = first_minute / MINUTES_PER_DAY;
        let mut minute_of_day = first_minute % MINUTES_PER_DAY;
        let last_day = day.checked_add(SEARCH_DAYS)?;
        while day <= last_day {
            let date = Date::from_days(day);
            if self.months & (1 << date.month) == 0 {
                day = date.first_of_next_month();
                minute_of_day = 0;
                continue;
            }
            if self.day_matches(date.day, weekday(day))
                && let Some(minute) = self.minute_of_day_from(minute_of_day)
            {
                let minutes = day.checked_mul(MINUTES_PER_DAY)?.checked_add(minute)?;
                return minutes.checked_mul(60).map(Duration::from_secs);
            }
            day += 1;
            minute_of_day = 0;
        }
        None
    }

    fn day_matches(&self, day_of_month: u64, weekday: u64) -> bool {
        let by_date = self.days_of_month & (1 << day_of_month) != 0;
        let by_weekday = self.days_of_week & (1 << weekday) != 0;
        if self.either_day {
            by_date || by_weekday
        } else {
            by_date && by_weekday
        }
    }

    /// The first matching minute of a day at or after `from`.
    fn minute_of_day_from(&self, from: u64) -> Option<u64> {
        let (from_hour, from_minute) = (from / 60, from % 60);
        (from_hour..24).find_map(|hour| {
            if self.hours & (1 << hour) == 0 {
                return None;
            }
            let start = if hour == from_hour { from_minute } else { 0 };
            let remaining = self.minutes >> start;
            (remaining != 0).then(|| hour * 60 + start + u64::from(remaining.trailing_zeros()))
        })
    }
}

impl fmt::Debug for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CronSchedule({:?})", self.expression)
    }
}

impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.expression)
    }
}

impl FromStr for CronSchedule {
    type Err = CronError;

    fn from_str(expression: &str) -> Result<Self, CronError> {
        let expression = expression.trim();
        if expression.starts_with('@') {
            return Err(CronError::Macro);
        }
        let fields: Vec<&str> = expression.split_ascii_whitespace().collect();
        let fields = <[&str; 5]>::try_from(fields).map_err(|fields| CronError::FieldCount {
            found: fields.len(),
        })?;
        let mut masks = [0u64; 5];
        for ((field, text), mask) in CronField::ALL.into_iter().zip(fields).zip(&mut masks) {
            *mask = parse_field(field, text)?;
        }
        let [minutes, hours, days_of_month, months, days_of_week] = masks;
        let [_, _, day_of_month, _, day_of_week] = fields;
        let either_day = !day_of_month.starts_with('*') && !day_of_week.starts_with('*');
        // Only the day-of-month can leave a schedule without a real date:
        // under the either rule the weekday admits a day in every month.
        let dated = (1..=12u64)
            .filter(|month| months & (1 << month) != 0)
            .any(|month| days_of_month & ((1 << (longest_month(month) + 1)) - 1) != 0);
        if !dated && !either_day {
            return Err(CronError::NeverFires);
        }
        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            either_day,
            expression: expression.to_owned(),
        })
    }
}

/// One field's match set as a bit mask; weekday 7 folds onto 0.
fn parse_field(field: CronField, text: &str) -> Result<u64, CronError> {
    let (min, max) = field.bounds();
    let mut bits = 0u64;
    for token in text.split(',') {
        let malformed = || CronError::Malformed {
            field,
            token: token.to_owned(),
        };
        if token
            .chars()
            .any(|c| c.is_alphabetic() || c == '#' || c == '?')
        {
            return Err(CronError::Unsupported {
                field,
                token: token.to_owned(),
            });
        }
        let (range, step) = match token.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (token, None),
        };
        let number = |digits: &str| -> Result<u32, CronError> {
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed());
            }
            // Only an overflow fails here, and that is out of every range.
            let value = digits.parse::<u32>().unwrap_or(u32::MAX);
            if !(min..=max).contains(&value) {
                return Err(CronError::OutOfRange {
                    field,
                    token: token.to_owned(),
                    value,
                    min,
                    max,
                });
            }
            Ok(value)
        };
        let (low, high) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((low, high)) => (number(low)?, number(high)?),
            // A step needs a span to walk: `5/15` is refused, not guessed.
            None if step.is_some() => return Err(malformed()),
            None => {
                let value = number(range)?;
                (value, value)
            }
        };
        if low > high {
            return Err(CronError::ReversedRange {
                field,
                token: token.to_owned(),
            });
        }
        let step = match step {
            None => 1,
            Some(digits) => {
                let invalid = || CronError::InvalidStep {
                    field,
                    token: token.to_owned(),
                    max,
                };
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(malformed());
                }
                let step = digits.parse::<u32>().map_err(|_| invalid())?;
                if !(1..=max).contains(&step) {
                    return Err(invalid());
                }
                step
            }
        };
        for value in (low..=high).step_by(step as usize) {
            let value = if field == CronField::DayOfWeek {
                value % 7
            } else {
                value
            };
            bits |= 1 << value;
        }
    }
    Ok(bits)
}

/// Days in `month` in a leap year.
const fn longest_month(month: u64) -> u64 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A proleptic Gregorian date, UTC.
struct Date {
    year: u64,
    month: u64,
    day: u64,
}

impl Date {
    /// The date `days` after 1970-01-01, by the era decomposition: 400
    /// years hold exactly 146097 days, counted from a 1 March year start
    /// so the leap day falls last.
    fn from_days(days: u64) -> Self {
        // Days from 0000-03-01 to 1970-01-01.
        let shifted = days + 719_468;
        let era = shifted / 146_097;
        let day_of_era = shifted % 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        let year = year_of_era + era * 400 + u64::from(month <= 2);
        Self { year, month, day }
    }

    /// Days from 1970-01-01 to this date; the inverse of [`Self::from_days`].
    fn to_days(&self) -> u64 {
        let year = self.year - u64::from(self.month <= 2);
        let era = year / 400;
        let year_of_era = year % 400;
        let shifted_month = if self.month > 2 {
            self.month - 3
        } else {
            self.month + 9
        };
        let day_of_year = (153 * shifted_month + 2) / 5 + self.day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    fn first_of_next_month(&self) -> u64 {
        let (year, month) = if self.month == 12 {
            (self.year + 1, 1)
        } else {
            (self.year, self.month + 1)
        };
        Self {
            year,
            month,
            day: 1,
        }
        .to_days()
    }
}

/// 0 is Sunday; 1970-01-01 was a Thursday.
const fn weekday(days: u64) -> u64 {
    (days + 4) % 7
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cron(expression: &str) -> CronSchedule {
        expression
            .parse()
            .unwrap_or_else(|err| panic!("{expression:?} must parse: {err}"))
    }

    fn refusal(expression: &str) -> CronError {
        match expression.parse::<CronSchedule>() {
            Ok(schedule) => panic!("{expression:?} must be refused, parsed as {schedule:?}"),
            Err(err) => err,
        }
    }

    /// Seconds since the epoch for a UTC date and time.
    fn at(year: u64, month: u64, day: u64, hour: u64, minute: u64) -> Duration {
        let days = Date { year, month, day }.to_days();
        Duration::from_secs((days * MINUTES_PER_DAY + hour * 60 + minute) * 60)
    }

    /// The next `count` fire times after `after`.
    fn fires(expression: &str, after: Duration, count: usize) -> Vec<Duration> {
        let schedule = cron(expression);
        let mut out = Vec::new();
        let mut cursor = after;
        for _ in 0..count {
            cursor = schedule.next_after(cursor).expect("a next fire time");
            out.push(cursor);
        }
        out
    }

    #[test]
    fn the_date_arithmetic_matches_known_instants() {
        assert_eq!(at(1970, 1, 1, 0, 0), Duration::ZERO);
        assert_eq!(at(2000, 3, 1, 0, 0), Duration::from_secs(951_868_800));
        assert_eq!(at(2023, 11, 14, 22, 13), Duration::from_secs(1_699_999_980));
        assert_eq!(at(2024, 2, 29, 0, 0), Duration::from_secs(1_709_164_800));
        assert_eq!(at(2100, 3, 1, 0, 0), Duration::from_secs(4_107_542_400));
        // 2023-11-14 was a Tuesday, 2024-02-29 a Thursday.
        assert_eq!(weekday(at(2023, 11, 14, 0, 0).as_secs() / 86_400), 2);
        assert_eq!(weekday(at(2024, 2, 29, 0, 0).as_secs() / 86_400), 4);
    }

    /// Every day across four centuries round-trips, which covers each
    /// leap rule: divisible by 4, by 100, and by 400.
    #[test]
    fn dates_round_trip_across_a_full_gregorian_cycle() {
        let mut previous = Date::from_days(0);
        assert_eq!((previous.year, previous.month, previous.day), (1970, 1, 1));
        for days in 1..=SEARCH_DAYS + 366 {
            let date = Date::from_days(days);
            assert_eq!(date.to_days(), days);
            let rolled = (date.year, date.month, date.day);
            let expected = if previous.day < days_in(previous.year, previous.month) {
                (previous.year, previous.month, previous.day + 1)
            } else if previous.month < 12 {
                (previous.year, previous.month + 1, 1)
            } else {
                (previous.year + 1, 1, 1)
            };
            assert_eq!(rolled, expected, "day {days}");
            previous = date;
        }
    }

    fn days_in(year: u64, month: u64) -> u64 {
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        if month == 2 && !leap {
            28
        } else {
            longest_month(month)
        }
    }

    #[test]
    fn every_minute_fires_on_the_next_minute_boundary() {
        let schedule = cron("* * * * *");
        let noon = at(2024, 5, 17, 12, 0);
        assert_eq!(schedule.next_after(noon), Some(at(2024, 5, 17, 12, 1)));
        // Strictly after: a boundary instant does not fire for itself.
        assert_eq!(
            schedule.next_after(noon + Duration::from_millis(59_999)),
            Some(at(2024, 5, 17, 12, 1)),
        );
        assert_eq!(
            schedule.next_after(noon - Duration::from_millis(1)),
            Some(noon),
        );
    }

    #[test]
    fn steps_lists_and_ranges_select_the_expected_minutes() {
        let start = at(2024, 5, 17, 12, 0);
        assert_eq!(
            fires("*/15 * * * *", start, 4),
            [
                at(2024, 5, 17, 12, 15),
                at(2024, 5, 17, 12, 30),
                at(2024, 5, 17, 12, 45),
                at(2024, 5, 17, 13, 0),
            ],
        );
        assert_eq!(
            fires("5,50,20 * * * *", start, 4),
            [
                at(2024, 5, 17, 12, 5),
                at(2024, 5, 17, 12, 20),
                at(2024, 5, 17, 12, 50),
                at(2024, 5, 17, 13, 5),
            ],
        );
        assert_eq!(
            fires("10-12 * * * *", start, 4),
            [
                at(2024, 5, 17, 12, 10),
                at(2024, 5, 17, 12, 11),
                at(2024, 5, 17, 12, 12),
                at(2024, 5, 17, 13, 10),
            ],
        );
        // A stepped range starts at the range's low end, not at zero.
        assert_eq!(
            fires("10-40/15,58 * * * *", start, 4),
            [
                at(2024, 5, 17, 12, 10),
                at(2024, 5, 17, 12, 25),
                at(2024, 5, 17, 12, 40),
                at(2024, 5, 17, 12, 58),
            ],
        );
    }

    #[test]
    fn the_hour_field_rolls_over_the_day() {
        assert_eq!(
            fires("30 9,17 * * *", at(2024, 5, 17, 12, 0), 3),
            [
                at(2024, 5, 17, 17, 30),
                at(2024, 5, 18, 9, 30),
                at(2024, 5, 18, 17, 30),
            ],
        );
        assert_eq!(
            fires("0 */8 * * *", at(2024, 5, 17, 16, 0), 2),
            [at(2024, 5, 18, 0, 0), at(2024, 5, 18, 8, 0)],
        );
    }

    #[test]
    fn the_month_and_the_year_roll_over() {
        assert_eq!(
            fires("0 0 1 * *", at(2024, 11, 15, 0, 0), 3),
            [
                at(2024, 12, 1, 0, 0),
                at(2025, 1, 1, 0, 0),
                at(2025, 2, 1, 0, 0),
            ],
        );
        assert_eq!(
            fires("59 23 31 12 *", at(2024, 12, 31, 23, 59), 2),
            [at(2025, 12, 31, 23, 59), at(2026, 12, 31, 23, 59)],
        );
        // The 31st skips every month without one.
        assert_eq!(
            fires("0 0 31 * *", at(2024, 1, 31, 0, 0), 3),
            [
                at(2024, 3, 31, 0, 0),
                at(2024, 5, 31, 0, 0),
                at(2024, 7, 31, 0, 0),
            ],
        );
        assert_eq!(
            fires("0 12 * 3,9 *", at(2024, 3, 31, 12, 0), 2),
            [at(2024, 9, 1, 12, 0), at(2024, 9, 2, 12, 0)],
        );
    }

    #[test]
    fn a_leap_day_schedule_waits_for_the_next_leap_year() {
        assert_eq!(
            fires("0 0 29 2 *", at(2023, 6, 1, 0, 0), 3),
            [
                at(2024, 2, 29, 0, 0),
                at(2028, 2, 29, 0, 0),
                at(2032, 2, 29, 0, 0),
            ],
        );
        // 2100 is not a leap year, so the gap there is eight years.
        assert_eq!(
            fires("0 0 29 2 *", at(2096, 2, 29, 0, 0), 1),
            [at(2104, 2, 29, 0, 0)],
        );
        // The 29th in a common year falls through to March.
        assert_eq!(
            fires("0 0 29 * *", at(2023, 1, 29, 0, 0), 1),
            [at(2023, 3, 29, 0, 0)],
        );
    }

    #[test]
    fn the_day_of_week_field_selects_weekdays() {
        // 2024-05-17 is a Friday.
        assert_eq!(
            fires("0 9 * * 1-5", at(2024, 5, 17, 9, 0), 3),
            [
                at(2024, 5, 20, 9, 0),
                at(2024, 5, 21, 9, 0),
                at(2024, 5, 22, 9, 0),
            ],
        );
        // 0 and 7 are both Sunday.
        let sunday = [at(2024, 5, 19, 0, 0), at(2024, 5, 26, 0, 0)];
        assert_eq!(fires("0 0 * * 0", at(2024, 5, 17, 0, 0), 2), sunday);
        assert_eq!(fires("0 0 * * 7", at(2024, 5, 17, 0, 0), 2), sunday);
        // A range through 7 wraps onto Sunday.
        assert_eq!(
            fires("0 0 * * 5-7", at(2024, 5, 17, 0, 0), 3),
            [
                at(2024, 5, 18, 0, 0),
                at(2024, 5, 19, 0, 0),
                at(2024, 5, 24, 0, 0),
            ],
        );
    }

    /// The classic rule: both day fields restricted means either matches.
    #[test]
    fn restricted_day_of_month_and_day_of_week_match_either() {
        // The 13th, or any Friday. 2024-09-13 is itself a Friday.
        assert_eq!(
            fires("0 0 13 * 5", at(2024, 9, 1, 0, 0), 6),
            [
                at(2024, 9, 6, 0, 0),
                at(2024, 9, 13, 0, 0),
                at(2024, 9, 20, 0, 0),
                at(2024, 9, 27, 0, 0),
                at(2024, 10, 4, 0, 0),
                at(2024, 10, 11, 0, 0),
            ],
        );
        assert_eq!(
            fires("0 0 13 * 5", at(2024, 10, 11, 0, 0), 2),
            [at(2024, 10, 13, 0, 0), at(2024, 10, 18, 0, 0)],
            "the 13th fires on a Sunday too",
        );
        // An impossible date still fires on its weekday.
        assert_eq!(
            fires("0 0 30 2 1", at(2024, 1, 1, 0, 0), 2),
            [at(2024, 2, 5, 0, 0), at(2024, 2, 12, 0, 0)],
        );
    }

    /// One restricted day field leaves the other out of the decision, and
    /// a field that starts with `*` is unrestricted even with a step.
    #[test]
    fn an_unrestricted_day_field_requires_the_other_to_match() {
        assert_eq!(
            fires("0 0 13 * *", at(2024, 9, 1, 0, 0), 2),
            [at(2024, 9, 13, 0, 0), at(2024, 10, 13, 0, 0)],
        );
        assert_eq!(
            fires("0 0 * * 5", at(2024, 9, 1, 0, 0), 2),
            [at(2024, 9, 6, 0, 0), at(2024, 9, 13, 0, 0)],
        );
        // Odd days that are also Mondays: 2024-09-09, 09-23, then 10-07.
        assert_eq!(
            fires("0 0 */2 * 1", at(2024, 9, 1, 0, 0), 3),
            [
                at(2024, 9, 9, 0, 0),
                at(2024, 9, 23, 0, 0),
                at(2024, 10, 7, 0, 0),
            ],
        );
        // A stepped range does not start with `*`, so it stays restricted
        // and the either rule applies: the first Friday, not the 13th.
        assert_eq!(
            fires("0 0 13 * 5-6/7", at(2024, 1, 1, 0, 0), 1),
            [at(2024, 1, 5, 0, 0)],
        );
    }

    /// A date and weekday pair that needs most of the Gregorian cycle to
    /// line up still resolves inside the search bound.
    #[test]
    fn a_rare_leap_day_weekday_pair_is_found() {
        // 29 February on a Sunday: 2032, then 2060.
        assert_eq!(
            fires("0 0 29 2 */7", at(2024, 3, 1, 0, 0), 2),
            [at(2032, 2, 29, 0, 0), at(2060, 2, 29, 0, 0)],
        );
    }

    #[test]
    fn each_field_accepts_its_bounds() {
        for expression in [
            "0 0 1 1 0",
            "59 23 31 12 7",
            "0-59 0-23 1-31 1-12 0-7",
            "*/59 */23 */31 */12 */7",
            "  0\t0  1 1 *  ",
        ] {
            cron(expression);
        }
        assert_eq!(cron("  */5 * * * *  ").expression(), "*/5 * * * *");
        assert_eq!(cron("*/5 * * * *").to_string(), "*/5 * * * *");
    }

    #[test]
    fn each_field_refuses_a_value_outside_its_bounds() {
        for (expression, field, value, min, max) in [
            ("60 * * * *", CronField::Minute, 60, 0, 59),
            ("* 24 * * *", CronField::Hour, 24, 0, 23),
            ("* * 0 * *", CronField::DayOfMonth, 0, 1, 31),
            ("* * 32 * *", CronField::DayOfMonth, 32, 1, 31),
            ("* * * 0 *", CronField::Month, 0, 1, 12),
            ("* * * 13 *", CronField::Month, 13, 1, 12),
            ("* * * * 8", CronField::DayOfWeek, 8, 0, 7),
            ("0-60 * * * *", CronField::Minute, 60, 0, 59),
            ("99999999999 * * * *", CronField::Minute, u32::MAX, 0, 59),
        ] {
            let err = refusal(expression);
            assert!(
                matches!(
                    err,
                    CronError::OutOfRange { field: f, value: v, min: lo, max: hi, .. }
                        if f == field && v == value && lo == min && hi == max
                ),
                "{expression:?}: {err:?}",
            );
        }
        // Operator wording pin.
        assert_eq!(
            refusal("* 24 * * *").to_string(),
            "hour field value 24 in \"24\" is outside 0-23",
        );
    }

    #[test]
    fn a_wrong_field_count_is_refused_with_what_is_unsupported() {
        for (expression, found) in [
            ("", 0),
            ("* * * *", 4),
            ("0 */5 * * * *", 6),
            ("0 0 12 * * * 2026", 7),
            ("CRON_TZ=UTC 0 12 * * *", 6),
        ] {
            assert_eq!(
                refusal(expression),
                CronError::FieldCount { found },
                "{expression:?}",
            );
        }
        // Operator wording pin.
        assert_eq!(
            refusal("0 */5 * * * *").to_string(),
            "expected 5 fields (minute hour day-of-month month day-of-week), found 6; \
             a seconds field, a year field and a timezone are not supported",
        );
    }

    #[test]
    fn macros_names_and_extensions_are_refused_not_misread() {
        for expression in ["@daily", "@hourly", "@reboot", "@every 5m"] {
            assert_eq!(refusal(expression), CronError::Macro, "{expression:?}");
        }
        for (expression, field) in [
            ("0 0 L * *", CronField::DayOfMonth),
            ("0 0 15W * *", CronField::DayOfMonth),
            ("0 0 * * 5L", CronField::DayOfWeek),
            ("0 0 * * 1#2", CronField::DayOfWeek),
            ("0 0 ? * 1", CronField::DayOfMonth),
            ("0 0 * JAN *", CronField::Month),
            ("0 0 * * MON-FRI", CronField::DayOfWeek),
            ("0 0 * * sun", CronField::DayOfWeek),
        ] {
            let err = refusal(expression);
            assert!(
                matches!(err, CronError::Unsupported { field: f, .. } if f == field),
                "{expression:?}: {err:?}",
            );
        }
        // Operator wording pin.
        assert_eq!(
            refusal("0 0 * * MON-FRI").to_string(),
            "day-of-week field \"MON-FRI\": names and the `L`, `W`, `#` and `?` \
             extensions are not supported; use numbers",
        );
    }

    #[test]
    fn malformed_tokens_are_refused() {
        for expression in [
            "1, * * * *",
            ",1 * * * *",
            "1-  * * * *",
            "-5 * * * *",
            "1-2-3 * * * *",
            "*/ * * * *",
            "5/15 * * * *",
            "*/1/2 * * * *",
            "** * * * *",
            "1.5 * * * *",
            "+5 * * * *",
            "*/-5 * * * *",
        ] {
            let err = refusal(expression);
            assert!(
                matches!(
                    err,
                    CronError::Malformed {
                        field: CronField::Minute,
                        ..
                    }
                ),
                "{expression:?}: {err:?}",
            );
        }
    }

    #[test]
    fn a_backwards_range_and_a_bad_step_are_refused() {
        assert!(matches!(
            refusal("* 22-2 * * *"),
            CronError::ReversedRange {
                field: CronField::Hour,
                ..
            }
        ));
        for expression in ["*/0 * * * *", "*/60 * * * *", "0-30/0 * * * *"] {
            let err = refusal(expression);
            assert!(
                matches!(
                    err,
                    CronError::InvalidStep {
                        field: CronField::Minute,
                        max: 59,
                        ..
                    }
                ),
                "{expression:?}: {err:?}",
            );
        }
        // Operator wording pin.
        assert_eq!(
            refusal("*/90 * * * *").to_string(),
            "minute field step in \"*/90\" must be between 1 and 59",
        );
    }

    #[test]
    fn a_date_that_never_occurs_is_refused() {
        for expression in ["0 0 30 2 *", "0 0 31 4,6,9,11 *", "0 0 30-31 2 */2"] {
            assert_eq!(refusal(expression), CronError::NeverFires, "{expression:?}");
        }
        // 29 February is real, and a restricted weekday rescues the rest.
        cron("0 0 29 2 *");
        cron("0 0 31 4 1");
    }
}
