//! The message list: search, list options, one page of rows (a thread tree where threading is on), paging.

use kubicle_api::{Address, Attached, ComposeMode, ExpandThreads, Flag, FolderPrefs, FolderRole, Row, SortColumn, SortOrder};
use kubicle_browser::{text, time};
use kubicle_ui::{dialog::Dialog, dialog::DialogForm, icons, icons::icon, menu::Dropdown, menu::MenuItem};
use leptos::{ev::DragEvent, ev::KeyboardEvent, ev::MouseEvent, prelude::*};
use wasm_bindgen::JsCast;

use crate::{settings::choice, state::App, state::Context, state::Item, state::Key, state::Pane};

/// Whether the list draws threads, tracked: the folder's list mode, or threaded search results.
fn threaded(app: App) -> bool { app.listed.with(|l| l.threads) }

#[component]
pub fn ListPane() -> impl IntoView {
    let app = expect_context::<App>();
    // The folder's name, during a search too, as Elastic's `list_handler` sets it after `afterlistsearch`.
    let title = move || {
        let folder = app.folder.get();
        app.folders.with(|f| f.iter().find(|f| f.name == folder).map_or(folder.clone(), |f| crate::folders::label(f).to_owned()))
    };
    view! {
        <section class="list-pane" aria-label="Messages">
            <header class="bar">
                <button class="icon-button folders-button" aria-label="Folders" on:click=move |_| app.show_folders()>
                    {icon(icons::MENU)}
                </button>
                <h1 class="bar-title grow">{title}</h1>
                <ListMenus />
            </header>
            <crate::search::SearchBar />
            <Rows />
            <Paging />
            <button class="fab" aria-label="Compose" on:click=move |_| app.compose(ComposeMode::New, None)>{icon(icons::COMPOSE)}</button>
        </section>
    }
}

/// Roundcube's sort columns, in its order and with its names.
const SORTS: [(&str, SortColumn); 9] = [
    ("None", SortColumn::Unsorted),
    ("Arrival date", SortColumn::Arrival),
    ("Sent date", SortColumn::Date),
    ("Subject", SortColumn::Subject),
    ("From/To", SortColumn::FromTo),
    ("From", SortColumn::From),
    ("To", SortColumn::To),
    ("Cc", SortColumn::Cc),
    ("Size", SortColumn::Size),
];

/// Roundcube's empty selection closes the preview too.
fn select_none(app: App) {
    app.everything.set(None);
    app.close_message();
}

/// Roundcube's list header, less Refresh: the list is live.
#[component]
fn ListMenus() -> impl IntoView {
    let app = expect_context::<App>();
    let no_threads = move || !threaded(app);
    let select = move || {
        let pick = |label: &str, glyph, run: fn(App), can: bool| MenuItem::action(label, move || run(app)).glyph(glyph).enabled(can);
        // An entry that would change nothing is disabled, so no click on this menu is a dead one. Unread and
        // Flagged need a matching row not already selected, else selecting the matches changes nothing.
        let (rows, unread, flagged, whole_page) = app.rows.with(|rows| {
            let unselected = |pred: fn(&Row) -> bool| app.selection.with(|s| rows.iter().any(|i| pred(&i.row) && !s.contains(&i.key())));
            let whole_page = !rows.is_empty() && app.selection.with(|s| s.len() == rows.len());
            let (unread, flagged) = (unselected(|r| !r.flags.has(Flag::Seen)), unselected(|r| r.flags.has(Flag::Flagged)));
            (!rows.is_empty(), unread, flagged, whole_page)
        });
        let selected = app.selection.with(|s| !s.is_empty()) || app.everything.with(Option::is_some);
        // Not over search results: All names the folder, and an action on it would reach every message there.
        let all = rows && app.everything.with(Option::is_none) && app.search.with(Option::is_none);
        vec![
            // Off without rows, as Elastic's on its `listupdate`: no row would show or lose a checkbox.
            MenuItem::check("Selection", app.checkboxes.get(), move || app.toggle_checkboxes()).enabled(rows),
            pick("All", icons::ASTERISK, App::select_everything, all),
            pick("Current page", icons::MENU, App::select_page, rows && !whole_page),
            pick("Unread", icons::MAIL, |app| app.select(|i| !i.row.flags.has(Flag::Seen)), unread),
            pick("Flagged", icons::FLAG, |app| app.select(|i| i.row.flags.has(Flag::Flagged)), flagged),
            pick("Invert", icons::CHECK, App::invert_selection, rows),
            pick("None", icons::CLOSE, select_none, selected),
        ]
    };
    let threads = move || {
        let modes = [("Expand unread", ExpandThreads::Unread), ("Expand all", ExpandThreads::All), ("Collapse all", ExpandThreads::Never)];
        Vec::from(modes.map(|(label, mode)| MenuItem::action(label, move || app.set_thread_mode(mode))))
    };
    let options = RwSignal::new(false);
    // Between 769 and 1200 px the three fold into one menu beside the folder's name, as in Elastic's normal mode.
    let folded = move || {
        vec![
            MenuItem::submenu("Select", select()).glyph(icons::SELECT),
            MenuItem::submenu("Threads", threads()).glyph(icons::THREADS).enabled(!no_threads()),
            MenuItem::action("Options", move || options.set(true)).glyph(icons::OPTIONS),
        ]
    };
    view! {
        <div class="list-tools">
            <Dropdown label="Select" glyph=icons::SELECT text="Select" align_right=true items=select />
            <Dropdown label="Threads" glyph=icons::THREADS text="Threads" align_right=true disabled=Signal::derive(no_threads) items=threads />
            <button class="tool" aria-label="List options" title="List options" on:click=move |_| options.set(true)>
                {icon(icons::OPTIONS)} <span class="tool-label">"Options"</span>
            </button>
        </div>
        <span class="list-tools-folded"><Dropdown label="List actions" glyph=icons::MORE_VERTICAL align_right=true items=folded /></span>
        <ListOptions open=options />
    }
}

/// Roundcube's list options: a small dialog with the sort column, the order and the list mode, applied on Save.
#[component]
fn ListOptions(open: RwSignal<bool>) -> impl IntoView {
    let app = expect_context::<App>();
    let orders = [("Ascending", SortOrder::Ascending), ("Descending", SortOrder::Descending)];
    view! {
        <Dialog open=open close=move || open.set(false) title="List options" class="narrow">
            // Drawn anew each time the dialog opens, from the folder's preferences as they are then.
            {
                let chosen = RwSignal::new(app.prefs(&app.folder.get_untracked()));
                let save = move || {
                    open.set(false);
                    let FolderPrefs { sort, order, threads, .. } = chosen.get_untracked();
                    app.edit_prefs(&app.folder.get_untracked(), move |p| (p.sort, p.order, p.threads) = (sort, order, threads));
                };
                view! {
                    <DialogForm label="Save" submit=save cancel=move || open.set(false)>
                        {choice("Sorting column", || SORTS, move || chosen.with(|p| p.sort), move |v| chosen.update(|p| p.sort = v))}
                        {choice("Sorting order", move || orders, move || chosen.with(|p| p.order), move |v| chosen.update(|p| p.order = v))}
                        {choice("List mode", || [("List", false), ("Threads", true)], move || chosen.with(|p| p.threads), move |v| chosen.update(|p| p.threads = v))}
                    </DialogForm>
                }
            }
        </Dialog>
    }
}

/// The context menu from the keyboard (the `ContextMenu` key, Shift+F10): for the targets, at the open row.
fn keyboard_menu(app: App) {
    let keys = app.targets();
    let row = document().query_selector(".list .row.open").ok().flatten().or_else(|| document().query_selector(".list").ok().flatten());
    if let (false, Some(row)) = (keys.is_empty(), row) {
        let rect = row.get_bounding_client_rect();
        #[allow(clippy::cast_possible_truncation)] // page pixels
        app.open_context(((rect.left() + 24.0) as i32, rect.bottom() as i32), Context::Messages(keys));
    }
}

#[component]
fn Rows() -> impl IntoView {
    let app = expect_context::<App>();
    let loading = move || app.list.with(Option::is_none) && app.search.with(|s| s.as_ref().is_none_or(|s| s.results.is_none()));
    // A folder column where the results can come from more than one folder, as Roundcube's multi-folder listing.
    let all_folders = move || crate::search::multi_folder(app);
    // Roundcube's date column shows the arrival under Arrival sort. A sort change can keep a row's node, which
    // its key does not name, so the cell reads this.
    let arrival = Memo::new(move |_| app.listed.with(|l| l.sort == SortColumn::Arrival));
    view! {
        <div class="list" class:sort-size=move || app.list.with(|l| l.as_ref().is_some_and(|l| l.query.sort == SortColumn::Size))
            class:threads=move || threaded(app) role="listbox" aria-label="Message list" aria-multiselectable="true" tabindex="0"
            aria-activedescendant=move || app.cursor.with(|o| o.as_ref().map(|(_, uid)| format!("row-{uid}")))
            // The rows' handlers live here, once, not on each of 50 rows a page turn would rebuild.
            on:click=move |e: MouseEvent| {
                let Some((target, key)) = row_at(app, &e) else { return };
                let within = |selector| target.closest(selector).ok().flatten().is_some();
                let has = |flag| app.row_flags(&key).is_some_and(|f| f.has(flag));
                if within(".row-flag") {
                    app.set_flag(std::slice::from_ref(&key), Flag::Flagged, !has(Flag::Flagged));
                } else if within(".row-status") {
                    app.set_flag(std::slice::from_ref(&key), Flag::Seen, !has(Flag::Seen));
                } else if within(".twisty") {
                    app.toggle_thread(key, None);
                } else if within(".row-check") {
                    app.toggle_select(key);
                } else if e.shift_key() {
                    app.extend_to(&key, e.ctrl_key() || e.meta_key()); // Roundcube's Ctrl+Shift+click adds the range
                } else if e.ctrl_key() || e.meta_key() {
                    app.toggle_select(key);
                } else {
                    app.open_message(&key);
                    app.go(Pane::Reader);
                }
            }
            // Roundcube's `msglist_dbl_click`: a draft opens in Compose, any other message in the message view.
            on:dblclick=move |e: MouseEvent| {
                let Some((_, key)) = row_at(app, &e) else { return };
                if app.has_role(&key.0, FolderRole::Drafts) {
                    app.compose(ComposeMode::EditDraft, Some(&key));
                } else if !within_control(&e) {
                    app.open_full(&key);
                }
            }
            on:dragstart=move |e: DragEvent| {
                let Some((_, key)) = row_at(app, &e) else { return };
                let selection = app.selection.get_untracked();
                let keys = app.with_replies(if selection.contains(&key) { selection } else { vec![key] });
                if let Some(dt) = e.data_transfer() {
                    dt.set_effect_allowed("move");
                    let _ = dt.set_data("text/plain", &format!("{} message(s)", keys.len()));
                }
                app.set_drag(keys);
            }
            on:dragend=move |_| { app.set_drag(Vec::new()); app.hover_folder(None); }
            on:contextmenu=move |e: MouseEvent| {
                e.prevent_default();
                match row_at(app, &e) {
                    Some((_, key)) => app.row_menu(&key, (e.client_x(), e.client_y())),
                    None => keyboard_menu(app),
                }
            }
            on:keydown=move |e: KeyboardEvent| if e.key() == "ContextMenu" || (e.shift_key() && e.key() == "F10") {
                e.prevent_default();
                keyboard_menu(app);
            }
        >
            // `row` draws the flags, depth, reply count and thread state once, so the key holds them: a change
            // redraws the row.
            <For each=move || app.rows.get() key=|item: &Item| (item.key(), item.row.flags, item.row.depth, item.row.replies, item.thread)
                children=move |item: Item| row(app, &item, all_folders(), arrival) />
            <Show when=move || app.rows.with(Vec::is_empty)>
                <p class="empty">{move || if loading() { "Loading..." } else { "No messages" }}</p>
            </Show>
        </div>
    }
}

/// Whether a click landed on one of a row's own controls (status, flag, twisty, checkbox), which a double-click
/// toggles twice rather than opening the message.
fn within_control(e: &MouseEvent) -> bool {
    let target = e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok());
    target.is_some_and(|t| t.closest(".row-status, .row-flag, .twisty, .row-check").ok().flatten().is_some())
}

/// The row an event on the list happened in, by its place among the rows, and the element it hit.
fn row_at(app: App, e: &web_sys::Event) -> Option<(web_sys::Element, Key)> {
    let target = e.target()?.dyn_into::<web_sys::Element>().ok()?;
    let row = target.closest(".row").ok()??;
    let rows = row.parent_element()?.children(); // the rows, in `app.rows` order, then the "No messages" line
    let at = (0..rows.length()).position(|i| rows.item(i).as_ref() == Some(&row))?;
    Some((target, app.rows.with_untracked(|r| r.get(at).map(Item::key))?))
}

fn row(app: App, item: &Item, show_folder: bool, arrival: Memo<bool>) -> impl IntoView + use<> {
    let (key, r, thread) = (item.key(), item.row.clone(), item.thread);
    let recipients = app.folders.with_untracked(|f| kubicle_api::lists_recipients(f, &item.folder));
    let who = people(if recipients { &r.to } else { std::slice::from_ref(&r.from) });
    let subject = if r.subject.trim().is_empty() { "(no subject)".to_owned() } else { r.subject.clone() };
    let title = subject.clone(); // the whole subject on hover, as the row may cut it
    let (k1, k2) = (key.clone(), key.clone());
    let selected = Signal::derive(move || app.selected(&k1));
    let focused = move || app.cursor.with(|c| c.as_ref() == Some(&k2));
    let open = move || app.open.with(|o| o.as_ref() == Some(&key));
    let flagged = r.flags.has(Flag::Flagged);
    let below = thread.filter(|t| !t.expanded);
    view! {
        <div class="row" class:unread=!r.flags.has(Flag::Seen) class:flagged=flagged
            class:unroot=below.is_some_and(|t| t.unread_below) && r.flags.has(Flag::Seen) // an unread root is bold instead
            class:flaggedroot=below.is_some_and(|t| t.flagged_below) class:selected=selected class:open=open class:focused=focused
            role="option" id=format!("row-{}", r.uid) aria-selected=move || if selected.get() { "true" } else { "false" }
            draggable="true" style=format!("--depth: {}", r.depth.min(8))>
            // The checkbox Roundcube's Selection adds to each row; the list's click handler toggles the selection.
            {move || app.checkboxes.get().then(|| view! {
                <span class="row-check">
                    <input type="checkbox" tabindex="-1" aria-label="Select" prop:checked=move || selected.get() />
                </span>
            })}
            // The thread column: a root's expander.
            <span class="row-thread">
                {thread.map(|t| view! {
                    <button class="twisty" class:expanded=t.expanded
                        aria-label=if t.expanded { "Collapse thread" } else { "Expand thread" }>{icon(icons::CHEVRON)}</button>
                })}
            </span>
            <span class="row-main">
                <span class="row-line">
                    <span class="row-from">{who}</span>
                    {show_folder.then(|| view! { <span class="row-folder">{item.folder.clone()}</span> })}
                    <time class="row-date">{move || time::short(if arrival.get() { r.arrival } else { r.date })}</time>
                    <span class="row-size">{kubicle_api::size(u64::from(r.size))}</span>
                </span>
                <span class="row-line">
                    {status(&r, below.is_some_and(|t| t.unread_below))}
                    <span class="row-subject" title=title>{subject}</span>
                    {below.filter(|_| r.replies > 0).map(|_| view! { <span class="replies">{r.replies}</span> })}
                </span>
            </span>
            <span class="row-flags">
                <button class="row-flag" class:on=flagged aria-label=if flagged { "Unflag" } else { "Flag" }>{icon(icons::FLAG)}</button>
                {attached(r.attached)}
            </span>
        </div>
    }
}

/// The sender or recipient cell, as Roundcube's `address_string` with three at most: each name, or the address where
/// there is none or the name holds an `@` (`text::reader_name`), with the address as its tooltip; `...` after the
/// third. An address that is not valid shows as `name <address>`, without a tooltip.
fn people(list: &[Address]) -> impl IntoView + use<> {
    let shown = list.iter().take(3).enumerate().map(|(i, a)| {
        let valid = kubicle_api::valid_email(&a.email);
        view! { {(i > 0).then_some(", ")} <span title=valid.then(|| a.email.clone())>{text::reader_label(a, false)}</span> }
    });
    (shown.collect_view(), (list.len() > 3).then_some("..."))
}

/// Roundcube's status mark before the subject: replied, forwarded or both, else the unread dot, else a hollow dot for
/// a collapsed thread with unread replies, else a small dot. A click marks the message read or unread.
fn status(r: &Row, unread_below: bool) -> impl IntoView + use<> {
    let (class, title) = match (r.flags.has(Flag::Answered), r.flags.has(Flag::Forwarded)) {
        (true, true) => ("replied forwarded", "Replied and forwarded"),
        (true, false) => ("replied", "Replied"),
        (false, true) => ("forwarded", "Forwarded"),
        _ if !r.flags.has(Flag::Seen) => ("unread", "Unread"),
        _ if unread_below => ("unreadchildren", "Unread replies"),
        _ => ("read", ""),
    };
    let glyphs = [(r.flags.has(Flag::Answered), icons::REPLY), (r.flags.has(Flag::Forwarded), icons::FORWARD)];
    let seen = r.flags.has(Flag::Seen);
    view! {
        <button class=format!("row-status {class}") title=title aria-label=if seen { "Mark as unread" } else { "Mark as read" }>
            {glyphs.into_iter().filter(|(on, _)| *on).map(|(_, glyph)| icon(glyph)).collect_view()}
        </button>
    }
    .into_any()
}

/// The attachment cell under the flag: a paperclip, a report or a lock, as Roundcube picks by content type.
fn attached(kind: Attached) -> Option<impl IntoView + use<>> {
    let (glyph, title) = match kind {
        Attached::None => return None,
        Attached::Files => (icons::ATTACH, "With attachment"),
        Attached::Report => (icons::REPORT, "Report"),
        Attached::Encrypted => (icons::LOCK, "Encrypted"),
    };
    Some(view! { <span class="row-attached" title=title>{icon(glyph)}</span> })
}

/// Roundcube's list footer: first and previous page, "1-50 of 179" with an en dash (Roundcube's "Messages 1 to 50
/// of 179" in its title), the page box, next and last.
#[component]
fn Paging() -> impl IntoView {
    let app = expect_context::<App>();
    // The page shown, the page count, the first and last message on it and the total; None while nothing is.
    let at = move || {
        let (page, pages, size, shown, total) = position(app)?;
        let first = page * size + 1;
        (total > 0).then(|| (page, pages, first, first + u32::try_from(shown).unwrap_or(1).max(1) - 1, total))
    };
    // Short on screen, where it shares the footer with five controls; the title says it in full.
    let info = move |full: bool| {
        let searching = app.search.with(|s| s.as_ref().map(|s| (s.results.is_some(), s.searching_text)));
        match (at(), searching) {
            (Some((_, _, first, last, total)), searching) => {
                let (first, last, total) = (text::count(first), text::count(last), text::count(total));
                let range = if full {
                    format!("{} {first} to {last} of {total}", if threaded(app) { "Threads" } else { "Messages" })
                } else {
                    format!("{first}\u{2013}{last} of {total}")
                };
                let more = if searching == Some((true, true)) { ", searching message text..." } else { "" };
                format!("{range}{more}")
            }
            (None, Some((false, _))) => "Searching...".into(),
            (None, Some((true, true))) => "Searching message text...".into(),
            (None, Some((true, false))) => "No results".into(),
            (None, None) => String::new(),
        }
    };
    let page = move || at().map_or(0, |a| a.0);
    let last = move || at().map_or(0, |a| a.1.saturating_sub(1));
    let button = move |label: &'static str, glyph: &'static str, flip: bool, to: fn(u32, u32) -> u32, off: fn(u32, u32) -> bool| {
        view! {
            <button class="icon-button" aria-label=label title=label prop:disabled=move || at().is_none() || off(page(), last())
                on:click=move |_| go_to(app, to(page(), last()))>
                {if flip { view! { <span class="flip">{icon(glyph)}</span> }.into_any() } else { icon(glyph).into_any() }}
            </button>
        }
    };
    let jump = move |e| {
        let (Ok(n), Some((now, pages, ..))) = (event_target_value(&e).trim().parse::<u32>(), untrack(at)) else { return };
        let to = n.clamp(1, pages.max(1)) - 1;
        if to != now {
            go_to(app, to);
        }
    };
    view! {
        <footer class="paging">
            {button("First page", icons::CHEVRONS, true, |_, _| 0, |p, _| p == 0)}
            {button("Previous page", icons::CHEVRON, true, |p, _| p.saturating_sub(1), |p, _| p == 0)}
            <span class="muted small" title=move || info(true) aria-label=move || info(true)>{move || info(false)}</span>
            // As wide as the last page's number, so typing any page fits and the box never changes width.
            <input class="input page-box" inputmode="numeric" aria-label="Page" prop:disabled=move || at().is_none()
                style:width=move || format!("calc({}ch + 1.1rem)", (last() + 1).to_string().len().max(2))
                prop:value=move || at().map_or(String::new(), |a| (a.0 + 1).to_string()) on:change=jump />
            {button("Next page", icons::CHEVRON, false, |p, _| p + 1, |p, l| p >= l)}
            {button("Last page", icons::CHEVRONS, false, |_, l| l, |p, l| p >= l)}
        </footer>
    }
}

/// Where the list or the search results stand: page, pages, page size, rows shown, total.
fn position(app: App) -> Option<(u32, u32, u32, usize, u32)> {
    app.listed.with(|l| {
        let (page, pages, size) = l.paging?;
        // A threaded page counts its threads, its replies aside, as the server pages and totals them.
        Some((page, pages, size, l.rows.iter().filter(|(_, r)| !l.threads || r.depth == 0).count(), l.total))
    })
}

fn go_to(app: App, page: u32) {
    app.close_message();
    app.turn(page);
}

/// Page Up and Page Down, as Roundcube's `list_keypress`: the previous or the next page, where there is one.
pub fn turn_page(app: App, forward: bool) {
    let Some((page, pages, ..)) = untrack(|| position(app)) else { return };
    match (forward, page) {
        (true, _) if page + 1 < pages => go_to(app, page + 1),
        (false, 1..) => go_to(app, page - 1),
        _ => {}
    }
}
