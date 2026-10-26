//! Live updates: `api/events` is a server-sent event stream of `kubicle_api::Event`. Every stream starts with
//! `Event::Resync`, so opening it again catches up on whatever was missed. A stream can die unnoticed in sleep or
//! offline and still look open, so it opens again after `SILENCE` without a message, when the page shows again
//! after `QUIET` without one, and when the browser comes back online.

use std::{cell::Cell, cell::RefCell, time::Duration};

use kubicle_api::{Event, KEEPALIVE_SECONDS};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{EventSource, MessageEvent};

use crate::state::App;

/// Three of the server's keepalives missed.
const SILENCE: Duration = Duration::from_secs(3 * KEEPALIVE_SECONDS);
/// Longer than a live stream goes without a message, a keepalive's interval and a margin for the network.
const QUIET: Duration = Duration::from_secs(KEEPALIVE_SECONDS + 5);

/// The open stream and its handlers, which must live as long as it does.
type Open = (EventSource, Closure<dyn Fn(MessageEvent)>, Closure<dyn Fn()>);

thread_local! {
    static OPEN: RefCell<Option<Open>> = const { RefCell::new(None) };
    static WATCHDOG: Cell<Option<TimeoutHandle>> = const { Cell::new(None) };
    /// When the stream last opened or brought a message, in ms since the epoch.
    static HEARD: Cell<f64> = const { Cell::new(0.0) };
}

pub fn start(app: App) {
    open(app);
    let _ = window_event_listener(leptos::ev::online, move |_| open(app));
    // Fired at the document, from where it bubbles up to the window. Both listeners stay for the page's lifetime.
    let _ = window_event_listener_untyped("visibilitychange", move |_| {
        if !document().hidden() {
            shown(app);
        }
    });
}

/// Opens a stream in place of the current one; none while signed out, as Roundcube stops its refresh then.
fn open(app: App) {
    if let Some((old, _, _)) = OPEN.take() {
        old.close();
    }
    if kubicle_browser::api::is_signed_out() {
        return;
    }
    let Ok(source) = EventSource::new("api/events") else { return };
    let on_message = Closure::<dyn Fn(MessageEvent)>::new(move |e: MessageEvent| {
        watch(app);
        if let Some(Ok(event)) = e.data().as_string().map(|data| kubicle_browser::api::parse::<Event>(&data)) {
            app.event(event);
        }
    });
    let closed = source.clone();
    let on_error = Closure::<dyn Fn()>::new(move || {
        // CLOSED: the browser gave up, as it does on a 401 or a non-stream answer. Asking for the session
        // sends a signed-out user through sso.php; otherwise listen again shortly.
        if closed.ready_state() == EventSource::CLOSED {
            leptos::task::spawn_local(async move {
                let _ = kubicle_browser::api::get::<kubicle_api::Session>("api/session").await;
                set_timeout(move || open(app), Duration::from_secs(5));
            });
        }
    });
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    OPEN.set(Some((source, on_message, on_error)));
    watch(app);
}

/// The page shows again. A stream quiet past `QUIET` may have died while it was hidden or asleep, and opens again
/// now; one heard from since is alive, and opens again only if it stays quiet that long. Reopening a live one would
/// resync for nothing, reloading every folder's first page ahead of the user's next click.
fn shown(app: App) {
    let quiet = Duration::from_secs_f64((js_sys::Date::now() - HEARD.get()).max(0.0) / 1000.0);
    match QUIET.checked_sub(quiet) {
        Some(left) => arm(app, left),
        None => open(app),
    }
}

/// Opens the stream again unless a message arrives within `SILENCE`.
fn watch(app: App) {
    HEARD.set(js_sys::Date::now());
    arm(app, SILENCE);
}

/// Opens the stream again `after` from now, unless `watch` hears a message first.
fn arm(app: App, after: Duration) {
    if let Some(timer) = WATCHDOG.take() {
        timer.clear();
    }
    WATCHDOG.set(set_timeout_with_handle(move || open(app), after).ok());
}
