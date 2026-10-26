//! Dates in the user's time zone (Settings, else the browser's own, as Roundcube's "Auto"), through the browser's
//! `Intl`, which carries the zone database the wasm would otherwise have to ship.

use std::cell::RefCell;

use js_sys::{Array, Date, Intl, Object, Reflect};
use kubicle_api::current_zone;
use wasm_bindgen::JsValue;

thread_local! {
    static ZONE: RefCell<String> = const { RefCell::new(String::new()) };
    /// The zone's formats: `14:32`, `Tue 14:32`, `Oct 21`, `Oct 21, 2026`, `Tue, Oct 21, 2026, 14:32` and the
    /// `2026-10-21` (en-CA) that days compare by.
    static FORMATS: RefCell<Option<[Intl::DateTimeFormat; 6]>> = const { RefCell::new(None) };
}

/// The zone this browser is set to, by its current name: Chromium says `Europe/Kiev`.
#[must_use]
pub fn browser_zone() -> String {
    let resolved = Intl::DateTimeFormat::new(&Array::new(), &Object::new()).resolved_options();
    let zone = Reflect::get(&resolved, &"timeZone".into()).ok().and_then(|z| z.as_string());
    zone.map_or_else(|| "UTC".into(), |zone| current_zone(&zone).to_owned())
}

/// The zone dates show in now.
pub fn zone() -> String { ZONE.with_borrow(Clone::clone) }

/// Called with the user's zone, before any date is drawn and whenever the setting changes.
pub fn set_time_zone(zone: &str) {
    ZONE.with_borrow_mut(|z| zone.clone_into(z));
    let format = |locale: &str, fields: &[(&str, &str)]| {
        let options = Object::new();
        for (key, value) in [("timeZone", zone), ("hourCycle", "h23")].iter().chain(fields) {
            let _ = Reflect::set(&options, &(*key).into(), &(*value).into());
        }
        Intl::DateTimeFormat::new(&Array::of1(&locale.into()), &options)
    };
    let [hour, minute] = [("hour", "2-digit"), ("minute", "2-digit")];
    let [weekday, month, day, year] = [("weekday", "short"), ("month", "short"), ("day", "numeric"), ("year", "numeric")];
    FORMATS.set(Some([
        format("en-US", &[hour, minute]),
        format("en-US", &[weekday, hour, minute]),
        format("en-US", &[month, day]),
        format("en-US", &[month, day, year]),
        format("en-US", &[weekday, month, day, year, hour, minute]),
        format("en-CA", &[]),
    ]));
}

#[allow(clippy::cast_precision_loss)] // mail dates are far inside f64's exact integer range
fn text(format: &Intl::DateTimeFormat, unix: Option<i64>) -> String {
    let date = unix.map_or_else(Date::new_0, |unix| Date::new(&JsValue::from_f64(unix as f64 * 1000.0)));
    format.format().call1(&JsValue::NULL, &date).ok().and_then(|t| t.as_string()).unwrap_or_default()
}

/// The list column: the time today, weekday and time this week, `Mar 30` this year, else `Jan 17, 2012`.
#[allow(clippy::cast_possible_truncation)] // a difference of whole days
#[must_use]
pub fn short(unix: i64) -> String {
    FORMATS.with_borrow(|formats| {
        let [time, week, month_day, date, _, day] = formats.as_ref().expect("the session sets the time zone before the list draws a date");
        let (that, today) = (text(day, Some(unix)), text(day, None));
        let age = ((Date::parse(&today) - Date::parse(&that)) / 86_400_000.0) as i64; // both UTC midnights
        let format = match age {
            0 => time,
            1..=6 => week,
            _ if that.get(..4) == today.get(..4) => month_day,
            _ => date,
        };
        text(format, Some(unix))
    })
}

/// The reader's header: `Tue, Oct 21, 2026, 14:32`.
#[must_use]
pub fn long(unix: i64) -> String {
    FORMATS.with_borrow(|formats| text(&formats.as_ref().expect("the session sets the time zone first")[4], Some(unix)))
}
