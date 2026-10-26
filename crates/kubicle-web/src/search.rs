//! Search as the user types: an approved exception to Roundcube, which searches on Enter (docs/design.md,
//! Search). Each change of the box or the options is a new query; only the latest one's answers are drawn, and
//! the requests of older ones are aborted. The header phase, Dovecot's header search, answers first; the full
//! phase (the body, the entire message, Reply-To, Followup-To, Bcc, priority) follows once typing pauses.

use std::{cell::RefCell, time::Duration};

use kubicle_api::{ListPage, ListQuery, Period, Priority, Search, SearchFields, SearchFilter, SearchInterval, SearchScope};
use kubicle_browser::api;
use kubicle_ui::{icons, icons::icon};
use leptos::{ev::KeyboardEvent, prelude::*, task::spawn_local};
use wasm_bindgen::JsCast;

use crate::{settings::choice, state::App};

/// How long typing must pause before the full phase starts, so it never queues IMAP searches behind each other.
const PAUSE: Duration = Duration::from_millis(250);

#[derive(Clone, PartialEq)]
pub struct SearchView {
    /// A query with a `search`.
    pub query: ListQuery,
    /// What is drawn: the server's answer for `query`, or the one before it until that comes; `None` shows the folder.
    pub results: Option<ListPage>,
    /// The full phase has yet to answer: "Searching message text..." shows.
    pub searching_text: bool,
}

/// Roundcube's search type, date and scope: kept across searches in a folder; Esc, the reset button or another
/// folder resets them, as Roundcube's `reset-search` and `list_mailbox` do, but the scope to Subfolders, where
/// Roundcube goes back to the folder alone.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Options {
    pub filter: SearchFilter,
    pub interval: SearchInterval,
    pub scope: SearchScope,
}

/// The latest query's number, the aborts of its requests, its pending full phase and the options.
#[derive(Default)]
struct Latest {
    number: u64,
    options: Options,
    aborts: Vec<web_sys::AbortController>,
    pause: Option<TimeoutHandle>,
}

thread_local! {
    static LATEST: RefCell<Latest> = RefCell::new(Latest::default());
}

/// A new query from the box: back to the folder when the box is empty and nothing filters it. The results follow
/// the folder's list view: its sort, and its threads. It closes the open message, as Roundcube's `qsearch` clears
/// the list.
pub fn typed(app: App, text: String) {
    let folder = app.folder.get_untracked();
    let Options { filter, interval, scope } = LATEST.with_borrow(|l| l.options);
    let (prefs, size) = (app.prefs(&folder), app.settings.with_untracked(|s| s.page_size));
    let search = Search { text, fields: prefs.search, filter, interval, scope, full: false };
    if search.is_blank() {
        return end(app);
    }
    app.close_message();
    run(app, ListQuery { folder, page: 0, size, sort: prefs.sort, order: prefs.order, threads: prefs.threads, search: Some(search) });
}

/// The search on screen again in `folder`'s changed list view: on the same page after a sort change, as
/// Roundcube's list request carries `_search`; from the first after a list mode change.
pub fn follow_view(app: App, folder: &str, keep_page: bool) {
    let query = app.search.with_untracked(|s| s.as_ref().map(|s| s.query.clone()));
    let Some(query) = query.filter(|q| q.folder == folder) else { return };
    let prefs = app.prefs(folder);
    let page = if keep_page { query.page } else { 0 };
    run(app, ListQuery { sort: prefs.sort, order: prefs.order, threads: prefs.threads, page, ..query });
}

/// Whether the results on screen can come from more than one folder, as Roundcube's multi-folder listing; tracked.
pub fn multi_folder(app: App) -> bool {
    let scope = |s: &SearchView| Some((s.query.search.as_ref()?.scope, s.query.folder.clone()));
    let Some((scope, folder)) = app.search.with(|s| s.as_ref().and_then(scope)) else { return false };
    match scope {
        SearchScope::Folder => false,
        SearchScope::All => true,
        SearchScope::Subfolders =>
            app.folders.with(|f| f.iter().any(|f| f.subscribed && f.name != folder && kubicle_api::within(&f.name, &folder, f.delimiter))),
    }
}

/// The same text again as a new query from the first page, for changed options.
/// A type or a date filters the folder with the box empty too, as Roundcube's `is_search_pending` says.
pub fn rerun(app: App) { typed(app, text(app)); }

/// The search box's text.
fn text(app: App) -> String {
    app.search.with_untracked(|s| s.as_ref().and_then(|s| Some(s.query.search.as_ref()?.text.clone()))).unwrap_or_default()
}

/// The search on screen asked again on its page, for mail that changed: Roundcube's list after a move keeps the
/// session's `_page` and refreshes the search set.
pub fn refresh(app: App) {
    if let Some(query) = app.search.with_untracked(|s| s.as_ref().map(|s| s.query.clone())) {
        run(app, query);
    }
}

pub fn page(app: App, page: u32) {
    if let Some(query) = app.search.with_untracked(|s| s.as_ref().map(|s| s.query.clone())) {
        run(app, ListQuery { page, ..query });
    }
}

pub fn end(app: App) {
    stop();
    if app.search.with_untracked(Option::is_some) {
        app.search.set(None); // else every folder change would redraw a list no search covers
    }
}

/// A folder switch: Roundcube's `list_mailbox`, which puts the type, date and scope back with the search.
pub fn reset(app: App) {
    LATEST.with_borrow_mut(|l| l.options = Options::default());
    end(app);
}

/// The reset button and Esc: Roundcube's `reset-search`, the search and its options gone and page 1 of the folder.
pub fn reset_to_list(app: App) {
    reset(app);
    app.show_page(app.folder.get_untracked(), 0);
}

/// Aborts the requests and the pending full phase of the query before.
fn stop() -> u64 {
    LATEST.with_borrow_mut(|l| {
        l.aborts.drain(..).for_each(|a| a.abort());
        if let Some(pause) = l.pause.take() {
            pause.clear();
        }
        l.number += 1;
        l.number
    })
}

fn run(app: App, query: ListQuery) {
    let number = stop();
    let search = query.search.clone().unwrap_or_default();
    let full = search.needs_full_phase();
    // The rows drawn stay until the server's answer replaces them; the same query again, for mail that changed,
    // keeps the selection too, as a folder refresh does. `App::remove` and `App::patch_rows` applied this tab's
    // changes to them.
    if app.search.with_untracked(|s| s.as_ref().is_none_or(|s| s.query != query)) {
        app.set_selection(Vec::new());
    }
    let results = app.search.with_untracked(|s| s.as_ref().and_then(|s| s.results.clone()));
    app.search.set(Some(SearchView { query: query.clone(), results, searching_text: full }));
    // The server's header phase would search a priority too (sync.rs, `order`), which waits for the pause
    // with the rest (docs/design.md, Search): only the full phase asks.
    if !matches!(search.filter, SearchFilter::Priority(_)) {
        ask(app, number, query.clone());
    }
    if full {
        let query = ListQuery { search: Some(Search { full: true, ..search }), ..query };
        let pause = set_timeout_with_handle(move || ask(app, number, query), PAUSE).ok();
        LATEST.with_borrow_mut(|l| l.pause = pause);
    }
}

/// Sends one phase of query `number`, and draws its answer only while that is still the latest query.
fn ask(app: App, number: u64, query: ListQuery) {
    let Ok(abort) = web_sys::AbortController::new() else { return };
    let signal = abort.signal();
    LATEST.with_borrow_mut(|l| l.aborts.push(abort));
    spawn_local(async move {
        let asked = app.settled(None);
        let result = api::get_aborting::<ListPage>(&crate::state::list_url(&query, &format!("page={}", query.page)), &signal).await;
        if LATEST.with_borrow(|l| l.number) != number {
            return; // a newer query took its place, and its answers will draw
        }
        match result {
            // It may hold what a change took off the list, as a page may (`App::stale`): asked again once applied.
            Ok(_) if app.stale(None, asked) => app.after_changes(None),
            // A page past the end, as after another tab moved the last page's hits: the last page instead, as the
            // folder's list steps back. Only the final phase knows the total; the header phase may count fewer.
            Ok(r) if r.rows.is_empty() && query.page > 0 && r.total > 0 && (full(&query) || !needs_full_phase(&query)) =>
                page(app, (r.total.div_ceil(query.size) - 1).min(query.page - 1)),
            Ok(results) => {
                let threaded = if results.query.threads { results.rows.first().map(|h| h.folder.clone()) } else { None };
                app.search.update(|s| {
                    // The header phase never replaces the full phase's answer, which holds it.
                    if let Some(view) = s.as_mut().filter(|v| full(&query) || !needs_full_phase(&v.query) || v.searching_text) {
                        view.results = Some(results);
                        view.searching_text &= !full(&query);
                    }
                });
                if let Some(folder) = threaded {
                    app.expand_new_threads(&folder);
                }
                app.open_edge();
            }
            Err(e) => app.notify(format!("Search failed: {}", e.0)),
        }
    });
}

/// The query is a full phase (`Search::full`), whose search reads the message text.
fn full(query: &ListQuery) -> bool { query.search.as_ref().is_some_and(|s| s.full) }

fn needs_full_phase(query: &ListQuery) -> bool { query.search.as_ref().is_some_and(Search::needs_full_phase) }

/// The search box and its options, Roundcube's search menu.
#[component]
pub fn SearchBar() -> impl IntoView {
    let app = expect_context::<App>();
    let open = RwSignal::new(false);
    let text = move || {
        app.search.track();
        text(app)
    };
    let on_key = move |e: KeyboardEvent| match e.key().as_str() {
        "Escape" => {
            e.stop_propagation(); // the window's Esc would close the open message as well
            reset_to_list(app);
            let _ = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()).map(|el| el.blur());
        }
        "ArrowDown" => {
            e.prevent_default();
            _ = document().query_selector(".list").ok().flatten().and_then(|l| l.dyn_into::<web_sys::HtmlElement>().ok()).map(|l| l.focus());
            app.jump(false, false, false);
        }
        _ => {}
    };
    // Elastic's "Show unread messages": switches the type between Unread and All and searches at once.
    let unread =
        move || app.search.with(|s| s.as_ref().and_then(|s| s.query.search.as_ref()).is_some_and(|s| s.filter == SearchFilter::Unread));
    let toggle_unread = move |_| {
        let filter = if untrack(unread) { SearchFilter::All } else { SearchFilter::Unread };
        LATEST.with_borrow_mut(|l| l.options.filter = filter);
        rerun(app);
    };
    let options = NodeRef::<leptos::html::Button>::new();
    view! {
        // Esc closes the options from the button that opened them too, and goes no further while it does. The focus
        // goes back to that button: a field inside the options goes with them.
        <div class="list-search" on:keydown=move |e: KeyboardEvent| if e.key() == "Escape" && open.get_untracked() {
            open.set(false);
            e.stop_propagation();
            let _ = options.get_untracked().map(|b| b.focus());
        }>
            <div class="searchbar" role="search">
                {icon(icons::SEARCH)}
                // Chromium's own clear button is hidden (base.css): it emptied the text but left the type and date.
                <input id="search" class="search-input" type="search" placeholder="Search" aria-label="Search mail" prop:value=text
                    on:input=move |e| typed(app, event_target_value(&e)) on:keydown=on_key />
                // Elastic's reset button, while a search is pending: the text, a type or a date.
                <Show when=move || app.search.with(Option::is_some)>
                    <button class="tool reset" aria-label="Reset search" title="Reset search"
                        on:click=move |_| { open.set(false); reset_to_list(app); }>{icon(icons::CLOSE)}</button>
                </Show>
                <button class="tool unread" class:selected=unread aria-pressed=move || if unread() { "true" } else { "false" }
                    aria-label="Show unread messages" title="Show unread messages" on:click=toggle_unread>{icon(icons::MAIL)}</button>
                <button class="tool" aria-label="Search options" title="Search options" node_ref=options
                    on:click=move |_| open.update(|o| *o ^= true)>{icon(icons::FILTER)}</button>
            </div>
            <Show when=move || open.get()>{options_panel(app, open)}</Show>
        </div>
    }
}

const TYPES: [(&str, SearchFilter); 10] = [
    ("All", SearchFilter::All),
    ("Unread", SearchFilter::Unread),
    ("Flagged", SearchFilter::Flagged),
    ("Unanswered", SearchFilter::Unanswered),
    ("With attachment", SearchFilter::WithAttachment),
    ("Priority: Highest", SearchFilter::Priority(Priority::Highest)),
    ("Priority: High", SearchFilter::Priority(Priority::High)),
    ("Priority: Normal", SearchFilter::Priority(Priority::Normal)),
    ("Priority: Low", SearchFilter::Priority(Priority::Low)),
    ("Priority: Lowest", SearchFilter::Priority(Priority::Lowest)),
];

const DATES: [(&str, SearchInterval); 7] = [
    ("Any time", SearchInterval::Any),
    ("Younger than a week", SearchInterval::YoungerThan(Period::Week)),
    ("Younger than a month", SearchInterval::YoungerThan(Period::Month)),
    ("Younger than a year", SearchInterval::YoungerThan(Period::Year)),
    ("Older than a week", SearchInterval::OlderThan(Period::Week)),
    ("Older than a month", SearchInterval::OlderThan(Period::Month)),
    ("Older than a year", SearchInterval::OlderThan(Period::Year)),
];

const SCOPES: [(&str, SearchScope); 3] =
    [("Current folder", SearchScope::Folder), ("Subfolders", SearchScope::Subfolders), ("All folders", SearchScope::All)];

/// Roundcube's search menu: the fields, remembered per folder, then the type, the date and the scope. Each change
/// searches again at once.
fn options_panel(app: App, open: RwSignal<bool>) -> impl IntoView {
    let field = move |label: &'static str, depth: u8, get: fn(&SearchFields) -> bool, set: fn(&mut SearchFields, bool)| {
        let checked = move || app.settings.with(|_| get(&app.prefs(&app.folder.get_untracked()).search));
        // As Roundcube's: "Entire message" covers every other field, which then cannot be picked one by one.
        let covered = move || label != "Entire message" && app.settings.with(|_| app.prefs(&app.folder.get_untracked()).search.text);
        view! {
            <label class="search-field" style=format!("--depth: {depth}")>
                <input type="checkbox" prop:checked=checked prop:disabled=covered on:change=move |e| {
                    let on = event_target_checked(&e);
                    app.edit_prefs(&app.folder.get_untracked(), move |p| set(&mut p.search, on));
                    rerun(app);
                } />
                {label}
            </label>
        }
    };
    view! {
        <div class="backdrop" on:click=move |_| open.set(false)></div>
        <div class="menu search-options" role="dialog" aria-label="Search options">
            {field("Subject", 0, |f| f.subject, |f, on| f.subject = on)}
            {field("Sender", 0, |f| f.from && f.reply_to && f.followup_to, |f, on| (f.from, f.reply_to, f.followup_to) = (on, on, on))}
            {field("From", 1, |f| f.from, |f, on| f.from = on)}
            {field("Reply-To", 1, |f| f.reply_to, |f, on| f.reply_to = on)}
            {field("Followup-To", 1, |f| f.followup_to, |f, on| f.followup_to = on)}
            {field("Recipient", 0, |f| f.to && f.cc && f.bcc, |f, on| (f.to, f.cc, f.bcc) = (on, on, on))}
            {field("To", 1, |f| f.to, |f, on| f.to = on)}
            {field("Cc", 1, |f| f.cc, |f, on| f.cc = on)}
            {field("Bcc", 1, |f| f.bcc, |f, on| f.bcc = on)}
            {field("Body", 0, |f| f.body, |f, on| f.body = on)}
            {field("Entire message", 0, |f| f.text, |f, on| f.text = on)}
            <div class="menu-sep"></div>
            {choice("Type", || TYPES, || LATEST.with_borrow(|l| l.options.filter), move |v| { LATEST.with_borrow_mut(|l| l.options.filter = v); rerun(app) })}
            {choice("Date", || DATES, || LATEST.with_borrow(|l| l.options.interval), move |v| { LATEST.with_borrow_mut(|l| l.options.interval = v); rerun(app) })}
            {choice("Scope", || SCOPES, || LATEST.with_borrow(|l| l.options.scope), move |v| { LATEST.with_borrow_mut(|l| l.options.scope = v); rerun(app) })}
        </div>
    }
}
