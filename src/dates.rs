//! When a photo or video was taken, from its metadata or its file name.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use time::{Date, Month, OffsetDateTime, Time};

/// Seconds from QuickTime's epoch (1904-01-01) to the Unix epoch (1970-01-01).
pub const QUICKTIME_EPOCH_OFFSET: i64 = 2_082_844_800;

/// The Unix time a QuickTime header's creation time stands for, read as ffmpeg reads it, from
/// `unix`: the header's value taken as seconds since 1904 and converted to Unix time, as
/// `nom-exif` does. Some encoders write seconds since 1970 there, which reads as a date before
/// 1970; ffmpeg takes a value below the offset as Unix time, so such a date moves forward by the
/// offset. A value of 0 (1904-01-01) means the time is not set, and gives `None`.
pub fn quicktime_creation(unix: i64) -> Option<i64> {
    match unix {
        u if u == -QUICKTIME_EPOCH_OFFSET => None,
        u if (-QUICKTIME_EPOCH_OFFSET..0).contains(&u) => Some(u + QUICKTIME_EPOCH_OFFSET),
        u => Some(u),
    }
}

/// A date, with the time of day if it is known. Wall-clock time as recorded, no timezone.
/// Ordered by date, then time, with a date without a time first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Taken {
    pub date: Date,
    pub time: Option<Time>,
}

impl Taken {
    /// Whether the date could be when a digital photo was taken: not before 1980 and not more
    /// than a day in the future. Earlier or later dates usually come from broken metadata, like
    /// QuickTime's 1904 epoch or an unset camera clock.
    pub fn is_plausible(&self, now: OffsetDateTime) -> bool {
        let tomorrow = now.date().next_day().unwrap_or_else(|| now.date());
        self.date.year() >= 1980 && self.date <= tomorrow
    }
}

/// The date in a cataloged `taken_at`: `YYYY-MM-DDTHH:MM:SS`, possibly with an offset after it.
/// The two bytes between hour, minute and second are not checked.
pub fn from_taken_at(taken_at: &str) -> Option<Taken> {
    let (date, b) = dashed_date_start(taken_at.as_bytes())?;
    let b = b.strip_prefix(b"T")?;
    let (hour, b) = digits::<2>(b)?;
    let (_, b) = b.split_first()?;
    let (minute, b) = digits::<2>(b)?;
    let (_, b) = b.split_first()?;
    let (second, _) = digits::<2>(b)?;
    Some(Taken {
        date,
        time: Some(clock(hour, minute, second)?),
    })
}

/// A date in a file name, as phones and apps write them. The first match wins:
///
/// - `2017-10-08 18.15.36` (Dropbox), `2023-01-01-12-34-56` (screenshots), `2019-05-01 at
///   14.32.01` (WhatsApp, macOS screenshots): date and time.
/// - `20190501_143201` (Android `IMG_`, Pixel `PXL_`), `20190501-143201`: date and time.
/// - `2019-05-01`, and `20190501` between non-digits (WhatsApp `IMG-20190501-WA0001`): date
///   only.
pub fn from_name(name: &str) -> Option<Taken> {
    let starts = || number_starts(name.as_bytes());
    // Date and time first, so a name with both is not read as a date only.
    starts()
        .find_map(|b| {
            dashed_date_time(b)
                .or_else(|| dashed_date_at_time(b))
                .or_else(|| compact_date_time(b))
        })
        .or_else(|| starts().find_map(|b| dashed_date(b).or_else(|| compact_date(b))))
}

/// Every non-empty tail of `b` that does not start in the middle of a number.
fn number_starts(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..b.len()).filter_map(move |i| {
        let (before, tail) = b.split_at_checked(i)?;
        before
            .last()
            .is_none_or(|c| !c.is_ascii_digit())
            .then_some(tail)
    })
}

/// The value of the `N` ASCII digits `b` starts with, and the rest of `b`.
fn digits<const N: usize>(b: &[u8]) -> Option<(u16, &[u8])> {
    let (head, rest) = b.split_first_chunk::<N>()?;
    let value = head.iter().try_fold(0_u16, |n, &d| {
        d.is_ascii_digit().then(|| n * 10 + u16::from(d - b'0'))
    })?;
    Some((value, rest))
}

/// The byte `b` starts with if it is one of `set`, and the rest of `b`.
fn one_of<'a>(b: &'a [u8], set: &[u8]) -> Option<(u8, &'a [u8])> {
    let (&c, rest) = b.split_first()?;
    set.contains(&c).then_some((c, rest))
}

/// Whether `b` does not start with a digit, so a number before it has ended.
fn number_ends(b: &[u8]) -> bool {
    b.first().is_none_or(|c| !c.is_ascii_digit())
}

/// A valid calendar date, or `None`.
fn calendar(year: u16, month: u16, day: u16) -> Option<Date> {
    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    Date::from_calendar_date(i32::from(year), month, u8::try_from(day).ok()?).ok()
}

/// A valid time of day, or `None`.
fn clock(hour: u16, minute: u16, second: u16) -> Option<Time> {
    let [hour, minute, second] = [hour, minute, second].map(|n| u8::try_from(n).ok());
    Time::from_hms(hour?, minute?, second?).ok()
}

/// `YYYY-MM-DD` at the start of `b`, and the rest of `b`.
fn dashed_date_start(b: &[u8]) -> Option<(Date, &[u8])> {
    let (year, b) = digits::<4>(b)?;
    let b = b.strip_prefix(b"-")?;
    let (month, b) = digits::<2>(b)?;
    let b = b.strip_prefix(b"-")?;
    let (day, b) = digits::<2>(b)?;
    Some((calendar(year, month, day)?, b))
}

/// `YYYYMMDD` at the start of `b`, and the rest of `b`.
fn compact_date_start(b: &[u8]) -> Option<(Date, &[u8])> {
    let (year, b) = digits::<4>(b)?;
    let (month, b) = digits::<2>(b)?;
    let (day, b) = digits::<2>(b)?;
    Some((calendar(year, month, day)?, b))
}

/// `HH`, `MM` and `SS` at the start of `b`, separated twice by the same one of `.:-`, and the
/// rest of `b`.
fn separated_time_start(b: &[u8]) -> Option<(Time, &[u8])> {
    let (hour, b) = digits::<2>(b)?;
    let (separator, b) = one_of(b, b".:-")?;
    let (minute, b) = digits::<2>(b)?;
    let b = b.strip_prefix(&[separator])?;
    let (second, b) = digits::<2>(b)?;
    Some((clock(hour, minute, second)?, b))
}

/// `YYYY-MM-DD` at the start of `b`, with no digit after it.
fn dashed_date(b: &[u8]) -> Option<Taken> {
    let (date, b) = dashed_date_start(b)?;
    number_ends(b).then_some(Taken { date, time: None })
}

/// `YYYY-MM-DD`, one of ` _T-`, then `HH`, `MM` and `SS` separated by one of `.:-`.
fn dashed_date_time(b: &[u8]) -> Option<Taken> {
    let (date, b) = dashed_date_start(b)?;
    let (_, b) = one_of(b, b" _T-")?;
    let (time, b) = separated_time_start(b)?;
    number_ends(b).then_some(Taken {
        date,
        time: Some(time),
    })
}

/// `YYYY-MM-DD at HH.MM.SS`, with `.`, `:` or `-` between the parts of the time.
fn dashed_date_at_time(b: &[u8]) -> Option<Taken> {
    let (date, b) = dashed_date_start(b)?;
    let b = b.strip_prefix(b" at ")?;
    let (time, b) = separated_time_start(b)?;
    number_ends(b).then_some(Taken {
        date,
        time: Some(time),
    })
}

/// `YYYYMMDD` with no digit after it.
fn compact_date(b: &[u8]) -> Option<Taken> {
    let (date, b) = compact_date_start(b)?;
    number_ends(b).then_some(Taken { date, time: None })
}

/// `YYYYMMDD`, `_` or `-`, then `HHMMSS` (more digits may follow, like Pixel's milliseconds).
fn compact_date_time(b: &[u8]) -> Option<Taken> {
    let (date, b) = compact_date_start(b)?;
    let (_, b) = one_of(b, b"_-")?;
    let (hour, b) = digits::<2>(b)?;
    let (minute, b) = digits::<2>(b)?;
    let (second, _) = digits::<2>(b)?;
    Some(Taken {
        date,
        time: Some(clock(hour, minute, second)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use time::macros::datetime;

    /// A date with an optional `(hour, minute, second)`.
    fn at(year: i32, month: u8, day: u8, time: Option<(u8, u8, u8)>) -> Taken {
        let month = Month::try_from(month).unwrap();
        Taken {
            date: Date::from_calendar_date(year, month, day).unwrap(),
            time: time.map(|(h, m, s)| Time::from_hms(h, m, s).unwrap()),
        }
    }

    /// `YYYY-MM-DD`, with ` HH:MM:SS` if there is a time, or `-` for none.
    fn show(taken: Option<Taken>) -> String {
        taken.map_or_else(
            || String::from("-"),
            |t| {
                let time = t.time.map_or_else(String::new, |c| {
                    format!(" {:02}:{:02}:{:02}", c.hour(), c.minute(), c.second())
                });
                let d = t.date;
                format!(
                    "{:04}-{:02}-{:02}{time}",
                    d.year(),
                    u8::from(d.month()),
                    d.day()
                )
            },
        )
    }

    /// What `from_name` and `from_taken_at` make of names that match, almost match or do not.
    #[test]
    fn test_parsers_accept_exactly_these() {
        let cases = [
            ("2017-10-08 18.15.36.jpg", "2017-10-08 18:15:36", "-"),
            ("2017-10-10 11.52.31-11.jpg", "2017-10-10 11:52:31", "-"),
            ("2017-10-08_18:15:36", "2017-10-08 18:15:36", "-"),
            (
                "2017-10-08T18-15-36",
                "2017-10-08 18:15:36",
                "2017-10-08 18:15:36",
            ),
            ("2017-10-08-18-15-36", "2017-10-08 18:15:36", "-"),
            ("2017-10-08 18.15:36", "2017-10-08", "-"),
            ("2017-10-08 18.15.361", "2017-10-08", "-"),
            ("2017-10-08x18.15.36", "2017-10-08", "-"),
            (
                "Screenshot_2023-01-01-12-34-56.png",
                "2023-01-01 12:34:56",
                "-",
            ),
            (
                "WhatsApp Image 2019-05-01 at 14.32.01.jpeg",
                "2019-05-01 14:32:01",
                "-",
            ),
            ("2019-05-01 at 14:32:01", "2019-05-01 14:32:01", "-"),
            ("2019-05-01 at 14.32.011", "2019-05-01", "-"),
            ("2019-05-01 at 14.32", "2019-05-01", "-"),
            ("2019-05-01  at 14.32.01", "2019-05-01", "-"),
            ("IMG_20190501_143201.jpg", "2019-05-01 14:32:01", "-"),
            ("20190501-143201", "2019-05-01 14:32:01", "-"),
            ("20190501 143201", "2019-05-01", "-"),
            ("PXL_20230101_123456789.jpg", "2023-01-01 12:34:56", "-"),
            ("IMG-20190501-WA0001.jpg", "2019-05-01", "-"),
            ("VID-20190501-WA0001.mp4", "2019-05-01", "-"),
            ("120190501", "-", "-"),
            ("201905011", "-", "-"),
            ("x20190501x", "2019-05-01", "-"),
            ("2019-05-01", "2019-05-01", "-"),
            ("Ferie 2019-05-01.jpg", "2019-05-01", "-"),
            ("2019-05-011", "-", "-"),
            ("12019-05-01", "-", "-"),
            ("2019-13-01", "-", "-"),
            ("2019-00-01", "-", "-"),
            ("2019-02-29", "-", "-"),
            ("2024-02-29 00.00.00", "2024-02-29 00:00:00", "-"),
            ("2023-02-29 00.00.00", "-", "-"),
            ("2019-05-01 24.00.00", "2019-05-01", "-"),
            ("2019-05-01 23.60.00", "2019-05-01", "-"),
            ("2019-05-01 23.59.60", "2019-05-01", "-"),
            ("IMG_20191301_143201.jpg", "-", "-"),
            ("2019-05-01 25.00.00.jpg", "2019-05-01", "-"),
            (
                "2024-05-01T10:00:00+02:00",
                "2024-05-01 10:00:00",
                "2024-05-01 10:00:00",
            ),
            (
                "2023-01-01T00:00:59",
                "2023-01-01 00:00:59",
                "2023-01-01 00:00:59",
            ),
            ("2023-02-30T00:00:00", "-", "-"),
            ("2024-05-01T10x00y00", "2024-05-01", "2024-05-01 10:00:00"),
            ("2024-05-01 10:00:00", "2024-05-01 10:00:00", "-"),
            ("2024-05-01T10:00", "2024-05-01", "-"),
            ("2024-05-01T1é:00:00", "2024-05-01", "-"),
            ("0000-01-01 00.00.00", "0000-01-01 00:00:00", "-"),
            ("9999-12-31_235959", "9999-12-31", "-"),
            ("2O19-05-01", "-", "-"),
            ("２０１９-05-01", "-", "-"),
            ("é2019-05-01é", "2019-05-01", "-"),
            (
                "2019-05-01 at 14.32.01 2020-01-01 10.00.00",
                "2019-05-01 14:32:01",
                "-",
            ),
            ("1999-01-01 2020-01-01 10.00.00", "2020-01-01 10:00:00", "-"),
            ("DSC0047.jpg", "-", "-"),
            ("123456789.jpg", "-", "-"),
            ("garbage", "-", "-"),
            ("", "-", "-"),
            ("-", "-", "-"),
        ];
        for (input, name, taken_at) in cases {
            assert_eq!(show(from_name(input)), name, "from_name({input:?})");
            assert_eq!(
                show(from_taken_at(input)),
                taken_at,
                "from_taken_at({input:?})"
            );
        }
    }

    #[test]
    fn test_from_taken_at() {
        assert_eq!(
            from_taken_at("2024-05-01T10:00:00+02:00"),
            Some(at(2024, 5, 1, Some((10, 0, 0))))
        );
        assert_eq!(
            from_taken_at("2023-01-01T00:00:59"),
            Some(at(2023, 1, 1, Some((0, 0, 59))))
        );
        assert_eq!(from_taken_at("2023-02-30T00:00:00"), None);
        assert_eq!(from_taken_at("garbage"), None);
    }

    #[test]
    fn test_from_name() {
        let cases = [
            (
                "2017-10-08 18.15.36.jpg",
                Some(at(2017, 10, 8, Some((18, 15, 36)))),
            ),
            (
                "2017-10-10 11.52.31-11.jpg",
                Some(at(2017, 10, 10, Some((11, 52, 31)))),
            ),
            (
                "Screenshot_2023-01-01-12-34-56.png",
                Some(at(2023, 1, 1, Some((12, 34, 56)))),
            ),
            (
                "IMG_20190501_143201.jpg",
                Some(at(2019, 5, 1, Some((14, 32, 1)))),
            ),
            (
                "PXL_20230101_123456789.jpg",
                Some(at(2023, 1, 1, Some((12, 34, 56)))),
            ),
            ("VID-20190501-WA0001.mp4", Some(at(2019, 5, 1, None))),
            ("Ferie 2019-05-01.jpg", Some(at(2019, 5, 1, None))),
            (
                "WhatsApp Image 2019-05-01 at 14.32.01.jpeg",
                Some(at(2019, 5, 1, Some((14, 32, 1)))),
            ),
            (
                "Screenshot 2023-01-01 at 12.34.56.png",
                Some(at(2023, 1, 1, Some((12, 34, 56)))),
            ),
            ("DSC0047.jpg", None),
            ("123456789.jpg", None),
            ("IMG_20191301_143201.jpg", None),
            ("2019-05-01 25.00.00.jpg", Some(at(2019, 5, 1, None))),
        ];
        for (name, expected) in cases {
            assert_eq!(from_name(name), expected, "{}", name);
        }
    }

    #[test]
    fn test_quicktime_creation() {
        let unix = datetime!(2014-05-12 18:18:27 UTC).unix_timestamp();
        // Unix seconds in the header, read as seconds since 1904: 1948-05-11T18:18:27.
        assert_eq!(
            quicktime_creation(unix - QUICKTIME_EPOCH_OFFSET),
            Some(unix)
        );
        // Seconds since 1904, as the format defines them.
        assert_eq!(quicktime_creation(unix), Some(unix));
        assert_eq!(quicktime_creation(0), Some(0));
        assert_eq!(quicktime_creation(-1), Some(QUICKTIME_EPOCH_OFFSET - 1));
        assert_eq!(quicktime_creation(-QUICKTIME_EPOCH_OFFSET), None);
        assert_eq!(
            quicktime_creation(-QUICKTIME_EPOCH_OFFSET - 1),
            Some(-QUICKTIME_EPOCH_OFFSET - 1)
        );
    }

    #[test]
    fn test_plausible() {
        let now = datetime!(2026-10-20 12:00 UTC);
        let date = |y, m, d| at(y, m, d, None);
        assert!(date(1980, 1, 1).is_plausible(now));
        assert!(date(2026, 10, 21).is_plausible(now));
        assert!(!date(1979, 12, 31).is_plausible(now));
        assert!(!date(1904, 1, 1).is_plausible(now));
        assert!(!date(2026, 10, 22).is_plausible(now));
    }
}
