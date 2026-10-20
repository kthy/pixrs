//! When a photo or video was taken, from its metadata or its file name.

/*************************************************************************************************
 * Copyright © 2022-2026 Kristian Thy (<thy@42.dk>)                                              *
 * This file is part of PixRS: <https://github.com/kthy/pixrs>                                   *
 *************************************************************************************************/

use time::{Date, Month, OffsetDateTime};

/// A date, with the time of day if it is known. Wall-clock time as recorded, no timezone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Taken {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    /// Hour, minute and second.
    pub time: Option<(u8, u8, u8)>,
}

impl Taken {
    /// A valid calendar date and time, or `None`.
    fn new(year: i32, month: u8, day: u8, time: Option<(u8, u8, u8)>) -> Option<Self> {
        Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
        if let Some((h, m, s)) = time {
            if h > 23 || m > 59 || s > 59 {
                return None;
            }
        }
        Some(Taken {
            year,
            month,
            day,
            time,
        })
    }

    /// Whether the date could be when a digital photo was taken: not before 1980 and not more
    /// than a day in the future. Earlier or later dates usually come from broken metadata, like
    /// QuickTime's 1904 epoch or an unset camera clock.
    pub fn is_plausible(&self, now: OffsetDateTime) -> bool {
        let tomorrow = now.date().next_day().unwrap_or(now.date());
        let (y, m, d) = (tomorrow.year(), tomorrow.month() as u8, tomorrow.day());
        self.year >= 1980 && (self.year, self.month, self.day) <= (y, m, d)
    }
}

/// The date in a cataloged `taken_at` (`YYYY-MM-DDTHH:MM:SS`, possibly with an offset after it).
pub fn from_taken_at(taken_at: &str) -> Option<Taken> {
    let b = taken_at.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    Taken::new(
        number(&b[0..4])? as i32,
        number(&b[5..7])? as u8,
        number(&b[8..10])? as u8,
        Some((
            number(&b[11..13])? as u8,
            number(&b[14..16])? as u8,
            number(&b[17..19])? as u8,
        )),
    )
}

/// A date in a file name, as phones and apps write them. The first match wins:
///
/// - `2017-10-08 18.15.36` (Dropbox), `2023-01-01-12-34-56` (screenshots), `2019-05-01 at
///   14.32.01` (WhatsApp, macOS screenshots): date and time.
/// - `20190501_143201` (Android `IMG_`, Pixel `PXL_`), `20190501-143201`: date and time.
/// - `2019-05-01`, and `20190501` between non-digits (WhatsApp `IMG-20190501-WA0001`): date
///   only.
pub fn from_name(name: &str) -> Option<Taken> {
    let b = name.as_bytes();
    let starts = |i: usize| i == 0 || !b[i - 1].is_ascii_digit();
    // Date and time first, so a name with both is not read as a date only.
    for i in 0..b.len() {
        if !starts(i) {
            continue;
        }
        if let Some(t) = dashed_date_time(&b[i..])
            .or_else(|| dashed_date_at_time(&b[i..]))
            .or_else(|| compact_date_time(&b[i..]))
        {
            return Some(t);
        }
    }
    for i in 0..b.len() {
        if !starts(i) {
            continue;
        }
        if let Some(t) = dashed_date(&b[i..]).or_else(|| compact_date(&b[i..])) {
            return Some(t);
        }
    }
    None
}

/// The value of ASCII digits, or `None` if any is not a digit.
fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0, |n, &d| n * 10 + u32::from(d - b'0')))
}

/// Whether `b` has no digit right after `len` bytes.
fn ends(b: &[u8], len: usize) -> bool {
    b.get(len).is_none_or(|c| !c.is_ascii_digit())
}

/// `YYYY-MM-DD` at the start of `b`.
fn dashed_date(b: &[u8]) -> Option<Taken> {
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' || !ends(b, 10) {
        return None;
    }
    Taken::new(
        number(&b[0..4])? as i32,
        number(&b[5..7])? as u8,
        number(&b[8..10])? as u8,
        None,
    )
}

/// `YYYY-MM-DD`, one of ` _T-`, then `HH`, `MM` and `SS` separated by one of `.:-`.
fn dashed_date_time(b: &[u8]) -> Option<Taken> {
    if b.len() < 19 || !b" _T-".contains(&b[10]) || !ends(b, 19) {
        return None;
    }
    let separator = |c: u8| b".:-".contains(&c);
    if !separator(b[13]) || b[13] != b[16] {
        return None;
    }
    let date = Taken::new(
        number(&b[0..4])? as i32,
        number(&b[5..7])? as u8,
        number(&b[8..10])? as u8,
        None,
    )
    .filter(|_| b[4] == b'-' && b[7] == b'-')?;
    Taken::new(
        date.year,
        date.month,
        date.day,
        Some((
            number(&b[11..13])? as u8,
            number(&b[14..16])? as u8,
            number(&b[17..19])? as u8,
        )),
    )
}

/// `YYYY-MM-DD at HH.MM.SS`.
fn dashed_date_at_time(b: &[u8]) -> Option<Taken> {
    if b.len() < 22 || &b[10..14] != b" at " {
        return None;
    }
    let mut joined = b[..10].to_vec();
    joined.push(b' ');
    joined.extend_from_slice(&b[14..]);
    dashed_date_time(&joined)
}

/// `YYYYMMDD` between non-digits.
fn compact_date(b: &[u8]) -> Option<Taken> {
    if b.len() < 8 || !ends(b, 8) {
        return None;
    }
    Taken::new(
        number(&b[0..4])? as i32,
        number(&b[4..6])? as u8,
        number(&b[6..8])? as u8,
        None,
    )
}

/// `YYYYMMDD`, `_` or `-`, then `HHMMSS` (more digits may follow, like Pixel's milliseconds).
fn compact_date_time(b: &[u8]) -> Option<Taken> {
    if b.len() < 15 || !b"_-".contains(&b[8]) {
        return None;
    }
    Taken::new(
        number(&b[0..4])? as i32,
        number(&b[4..6])? as u8,
        number(&b[6..8])? as u8,
        Some((
            number(&b[9..11])? as u8,
            number(&b[11..13])? as u8,
            number(&b[13..15])? as u8,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use time::macros::datetime;

    fn at(year: i32, month: u8, day: u8, time: Option<(u8, u8, u8)>) -> Option<Taken> {
        Some(Taken {
            year,
            month,
            day,
            time,
        })
    }

    #[test]
    fn test_from_taken_at() {
        assert_eq!(
            from_taken_at("2024-05-01T10:00:00+02:00"),
            at(2024, 5, 1, Some((10, 0, 0)))
        );
        assert_eq!(
            from_taken_at("2023-01-01T00:00:59"),
            at(2023, 1, 1, Some((0, 0, 59)))
        );
        assert_eq!(from_taken_at("2023-02-30T00:00:00"), None);
        assert_eq!(from_taken_at("garbage"), None);
    }

    #[test]
    fn test_from_name() {
        let cases = [
            (
                "2017-10-08 18.15.36.jpg",
                at(2017, 10, 8, Some((18, 15, 36))),
            ),
            (
                "2017-10-10 11.52.31-11.jpg",
                at(2017, 10, 10, Some((11, 52, 31))),
            ),
            (
                "Screenshot_2023-01-01-12-34-56.png",
                at(2023, 1, 1, Some((12, 34, 56))),
            ),
            ("IMG_20190501_143201.jpg", at(2019, 5, 1, Some((14, 32, 1)))),
            (
                "PXL_20230101_123456789.jpg",
                at(2023, 1, 1, Some((12, 34, 56))),
            ),
            ("VID-20190501-WA0001.mp4", at(2019, 5, 1, None)),
            ("Ferie 2019-05-01.jpg", at(2019, 5, 1, None)),
            (
                "WhatsApp Image 2019-05-01 at 14.32.01.jpeg",
                at(2019, 5, 1, Some((14, 32, 1))),
            ),
            (
                "Screenshot 2023-01-01 at 12.34.56.png",
                at(2023, 1, 1, Some((12, 34, 56))),
            ),
            ("DSC0047.jpg", None),
            ("123456789.jpg", None),
            ("IMG_20191301_143201.jpg", None),
            ("2019-05-01 25.00.00.jpg", at(2019, 5, 1, None)),
        ];
        for (name, expected) in cases {
            assert_eq!(from_name(name), expected, "{}", name);
        }
    }

    #[test]
    fn test_plausible() {
        let now = datetime!(2026-10-20 12:00 UTC);
        let date = |y, m, d| Taken::new(y, m, d, None).unwrap();
        assert!(date(1980, 1, 1).is_plausible(now));
        assert!(date(2026, 10, 21).is_plausible(now));
        assert!(!date(1979, 12, 31).is_plausible(now));
        assert!(!date(1904, 1, 1).is_plausible(now));
        assert!(!date(2026, 10, 22).is_plausible(now));
    }
}
