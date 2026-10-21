//! Schedule windows and the effective min.
//!
//! A window opens at `start` local time on each listed day and closes at the
//! next `end`. Wall times in a DST gap resolve forward and ambiguous ones take
//! the earlier instant, which is jiff's `compatible` disambiguation.

use buildkit_controller_api::v1alpha1::{Day, Scaling, Window};
use k8s_openapi::jiff::Timestamp;
use k8s_openapi::jiff::civil::{Date, Weekday};
use k8s_openapi::jiff::tz::TimeZone;

/// Days scanned ahead for the next boundary. Eight days cover every weekday.
const LOOKAHEAD_DAYS: i64 = 8;

/// `max(scaling.min, min of every window open at now)`.
#[must_use]
pub fn effective_min(scaling: &Scaling, tz: &TimeZone, now: Timestamp) -> i32 {
    let today = now.to_zoned(tz.clone()).date();
    let open = scaling.schedule.iter().filter(|w| {
        [today.yesterday().ok(), Some(today)]
            .into_iter()
            .flatten()
            .filter_map(|d| interval(w, tz, d))
            .any(|(o, c)| o <= now && now < c)
    });
    open.map(|w| w.min).fold(scaling.min, i32::max)
}

/// Next window open or close strictly after `now`.
#[must_use]
pub fn next_boundary(schedule: &[Window], tz: &TimeZone, now: Timestamp) -> Option<Timestamp> {
    let today = now.to_zoned(tz.clone()).date();
    (-1..=LOOKAHEAD_DAYS)
        .filter_map(|i| {
            today
                .checked_add(k8s_openapi::jiff::Span::new().days(i))
                .ok()
        })
        .flat_map(|d| schedule.iter().filter_map(move |w| interval(w, tz, d)))
        .flat_map(|(o, c)| [o, c])
        .filter(|t| *t > now)
        .min()
}

/// The `[open, close)` interval of `w` opening on local date `d`, if `d` is listed.
fn interval(w: &Window, tz: &TimeZone, d: Date) -> Option<(Timestamp, Timestamp)> {
    if !w.days.iter().any(|day| weekday(*day) == d.weekday()) {
        return None;
    }
    let (sh, sm) = hhmm(&w.start)?;
    let (eh, em) = hhmm(&w.end)?;
    let close_day = if (eh, em) > (sh, sm) {
        d
    } else {
        d.tomorrow().ok()?
    };
    let at = |day: Date, h: i8, m: i8| {
        tz.to_ambiguous_timestamp(day.at(h, m, 0, 0))
            .compatible()
            .ok()
    };
    let (open, close) = (at(d, sh, sm)?, at(close_day, eh, em)?);
    (open < close).then_some((open, close))
}

fn hhmm(s: &str) -> Option<(i8, i8)> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (i8, i8) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some((h, m))
}

fn weekday(d: Day) -> Weekday {
    match d {
        Day::Mon => Weekday::Monday,
        Day::Tue => Weekday::Tuesday,
        Day::Wed => Weekday::Wednesday,
        Day::Thu => Weekday::Thursday,
        Day::Fri => Weekday::Friday,
        Day::Sat => Weekday::Saturday,
        Day::Sun => Weekday::Sunday,
    }
}
