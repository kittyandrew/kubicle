//! The client's state and every action on it. Loaded list pages and message bodies stay in memory, so moving
//! through them never waits for the network (docs/design.md, Speed budgets); the server's events say what to
//! drop.

use std::{cell::RefCell, collections::HashMap, collections::HashSet, collections::VecDeque, fmt::Write as _, rc::Rc, time::Duration};

use kubicle_api::{Address, ArchiveType, Change, Chosen, ComposeMode, ComposeStart, Draft, DraftCopy, Event, ExpandThreads, Flag, Flags};
use kubicle_api::{Folder, FolderPrefs, FolderRole, ListPage, ListQuery, Message, Messages, Prefill, Quota, Row, Session, Settings};
use kubicle_api::{SortColumn, ThemeInfo, Viewing, ZipMode};
use kubicle_browser::{api, api::enc, text, time};
use leptos::{prelude::*, task::spawn_local};
use wasm_bindgen_futures::JsFuture;

use crate::search::SearchView;

/// A message: its folder and UID.
pub type Key = (String, u32);

/// One line of the visible list.
#[derive(Clone, PartialEq)]
pub struct Item {
    pub folder: String,
    pub row: Row,
    /// A thread root's replies, when it has any and threading is on.
    pub thread: Option<Thread>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Thread {
    pub expanded: bool,
    /// A collapsed thread hides unread replies: Roundcube's `unroot`.
    pub unread_below: bool,
    /// A collapsed thread hides flagged replies: Roundcube's `flaggedroot`.
    pub flagged_below: bool,
}

impl Item {
    pub fn key(&self) -> Key { (self.folder.clone(), self.row.uid) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Mail,
    Compose,
    Settings,
}

/// The one pane a phone shows; wider screens show all three, or the reader alone in `Message`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pane {
    Folders,
    List,
    Reader,
    /// Roundcube's message view in this tab on a wide screen, from a double-click: the reader alone, with Previous
    /// and Next.
    Message,
}

/// What the open context menu is for.
#[derive(Clone)]
pub enum Context {
    Messages(Vec<Key>),
    Folder(String),
    /// Messages dropped on a folder with Shift held: Roundcube's Move or Copy menu.
    Drop(Vec<Key>, String),
}

/// Whether this tab is a compose window, as Roundcube's `extwin`: opened for one Compose (a `#compose=`,
/// `#compose-start=` or `#draft=ID&window=1` address), it closes once that Compose is sent, closed or discarded. A
/// message window (`#open=`) is one too, as Roundcube's keeps `_extwin` for a Compose started there.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ComposeWindow {
    #[default]
    No,
    Open,
    /// Closes once its Compose was left (`crate::compose::close`).
    Closing,
}

/// State nothing on screen reads.
#[derive(Default)]
struct Cache {
    user: String,
    /// The user and `Session::account`, which the browser's draft copies are kept under.
    account: String,
    logout_url: String,
    /// `Session::admin_url` and `Session::source_url`, which About links to.
    admin_url: String,
    source_url: Option<String>,
    max_message_size: Option<u64>,
    anchor: Option<Key>,
    toast_seq: u32,
    drag: Vec<Key>,
    /// Each loaded page with the `page_clock` tick it was last shown or loaded at, for `keep_page`.
    pages: HashMap<(String, u32), (u64, ListPage)>,
    page_clock: u64,
    pending_pages: HashSet<(String, u32)>,
    /// Per folder, the changes sent and not yet answered, and how many were sent or reported by the server in all
    /// (`settled`).
    changes: HashMap<String, (u32, u32)>,
    /// Folders whose page, message window or search on screen loads again once their changes are answered.
    reload_after_changes: HashSet<String>,
    /// The Compose start on its way, which a change asked after it waits for (`post_changes`).
    composing: Option<js_sys::Promise>,
    /// The settings save on its way, which the next one waits for (`save_settings`).
    saving_settings: Option<js_sys::Promise>,
    /// Each Compose's last draft save, by its id, which resolves to its copy as JSON (`save_draft`).
    saving_draft: HashMap<String, js_sys::Promise>,
    bodies: HashMap<Key, Message>,
    body_order: VecDeque<Key>,
    pending_bodies: HashSet<Key>,
    /// A `Resync` came on this page: the first stream opened before anything but the session, with its page, loaded.
    resynced: bool,
    /// The compose id, the draft Compose opens on and whether it forwards Bcc lines, from `open_compose_as`.
    seed: Option<(String, Draft, bool)>,
    /// The one message a window opened by "Open in new window" shows, from its `#open=UID/FOLDER` address.
    single: Option<Key>,
    /// The compose a reload left, from its `#draft=ID` address, or one handed to this tab by "Open in new window":
    /// Compose opens on it again.
    reopen: Option<String>,
    /// The compose start a "Compose in a new window" tab starts from, its `#compose-start=` address, or new mail
    /// with a `mailto:` link from a mail, opened as `#compose=` in a new tab: Compose starts filled from it.
    compose_start: Option<(ComposeStart, Option<String>)>,
    compose_window: ComposeWindow,
    /// What runs when the open question closes, told how.
    answer: Option<Box<dyn FnOnce(Reply) + Send + Sync>>,
    /// The Threads menu's choice per folder for this page's life, as Roundcube's `autoexpand_threads`, over the
    /// setting "Expand message threads".
    thread_modes: HashMap<String, ExpandThreads>,
    /// Thread roots whose expansion that choice or the user already set: a refresh keeps them as they are.
    decided: HashSet<Key>,
    context: Option<Context>,
    /// The section Settings opens at, by its heading.
    settings_section: Option<&'static str>,
    /// When a message was last shown, and until when a quick run of them holds back marking read, as
    /// Roundcube's 400 ms `preview_delay_select`; in ms since the epoch.
    shown_at: f64,
    settle_until: f64,
    /// The message shown's pending "Mark previewed messages as read": opening Compose or Settings drops it, as
    /// leaving Roundcube's preview does.
    read_timer: Option<TimeoutHandle>,
    /// Compose is sending: Esc and Back stay in it until the answer.
    sending: bool,
    /// A page turn on its way for Previous or Next, or the message window's next after a delete: when the page
    /// shows, its first row opens, or with `true` its last.
    open_edge: Option<bool>,
    /// The folder a message drag hovers and the timer that expands it after a second (`hover_folder`).
    hover: Option<(String, TimeoutHandle)>,
}

/// How long a message shown less than this after the one before waits to count as read.
const SETTLE_MS: f64 = 400.0;

const BODY_CACHE: usize = 200;
/// Pages kept past those one step away (`keep_page`): a 50-row page holds about 11 KB of wasm memory.
const CACHED_PAGES: usize = 32;
const PREFETCH_AHEAD: usize = 3;
const PREFETCH_FOLDERS: usize = 2;

#[derive(Clone, Copy)]
pub struct App {
    pub ready: RwSignal<bool>,
    pub settings: RwSignal<Settings>,
    pub folders: RwSignal<Vec<Folder>>,
    pub themes: RwSignal<Vec<ThemeInfo>>,
    pub folder: RwSignal<String>,
    pub page: RwSignal<u32>,
    /// The page on screen; `None` while the first copy loads.
    pub list: RwSignal<Option<ListPage>>,
    pub search: RwSignal<Option<SearchView>>,
    pub listed: Memo<Listed>,
    pub rows: Memo<Vec<Item>>,
    pub selection: RwSignal<Vec<Key>>,
    /// Roundcube's Select all: every message of this folder, on every page. Actions on it reach the server as
    /// `Messages::All`; any other change of the selection ends it.
    pub everything: RwSignal<Option<String>>,
    pub open: RwSignal<Option<Key>>,
    /// Roundcube's focused row (`last_selected`), where the list's keys act: the open row, or one that Ctrl+arrows,
    /// Ctrl+Home or End or a modifier click moved to without opening it.
    pub cursor: RwSignal<Option<Key>>,
    /// The open message; `None` while it loads.
    pub message: RwSignal<Option<Message>>,
    /// The open message the server could not load: the reader says so, as Roundcube's preview shows its error page.
    pub unloaded: RwSignal<Option<Key>>,
    expanded: RwSignal<HashSet<Key>>,
    pub view: RwSignal<View>,
    pub pane: RwSignal<Pane>,
    /// The one modal on screen, if any; a question comes from `ask`.
    pub modal: RwSignal<Option<Modal>>,
    pub toast: RwSignal<Option<(u32, String)>>,
    /// Where the context menu is open, in page pixels.
    pub context_at: RwSignal<Option<(i32, i32)>>,
    /// The list's checkbox column, Roundcube's Selection.
    pub checkboxes: RwSignal<bool>,
    /// Roundcube's "recent" folders: new unread mail arrived since the user last opened them.
    pub recent: RwSignal<HashSet<String>>,
    /// Collapsed folders a message drag opened by hovering them; not saved, as Roundcube's `folder_collapsed` skips
    /// the save during a drag.
    pub drag_open: RwSignal<HashSet<String>>,
    pub quota: RwSignal<Option<Quota>>,
    /// The session ended (`api::signed_out`): Roundcube's notice shows until the page signs in again.
    pub signed_out: RwSignal<bool>,
    cache: StoredValue<Cache>,
}

/// What the list draws from: the search's results once it has some, else the folder's page, which stays while a
/// query has nothing to draw yet. `paging` (page, pages, page size) is `None` while that search waits.
#[derive(Clone, Default, PartialEq)]
pub struct Listed {
    /// Each row with its folder, a collapsed thread's replies included.
    pub rows: Vec<(String, Row)>,
    pub threads: bool,
    /// The sort the rows came in: the search's for its results, else the page's.
    pub sort: SortColumn,
    pub paging: Option<(u32, u32, u32)>,
    pub total: u32,
}

fn listed(list: Option<&ListPage>, search: Option<&SearchView>) -> Listed {
    let waiting = search.is_some_and(|s| s.results.is_none());
    let Some(l) = search.and_then(|s| s.results.as_ref()).or(list) else { return Listed::default() };
    let q = &l.query;
    let paging = (!waiting).then(|| (q.page, l.total.div_ceil(q.size), q.size));
    let rows = l.rows.iter().map(|h| (h.folder.clone(), h.row.clone())).collect();
    Listed { rows, threads: q.threads, sort: q.sort, paging, total: l.total }
}

/// The rows as the list draws them: each thread root with its replies when expanded.
fn thread_items(rows: &[(String, Row)], expanded: &HashSet<Key>) -> Vec<Item> {
    let mut out = Vec::with_capacity(rows.len());
    for run in threads(rows) {
        let item = |(folder, row): &(String, Row)| Item { folder: folder.clone(), row: row.clone(), thread: None };
        let ((folder, root), replies) = (&run[0], &run[1..]);
        // A page may open in the middle of a thread: those replies show as they are.
        if root.depth > 0 || replies.is_empty() {
            out.extend(run.iter().map(item));
            continue;
        }
        let expanded = expanded.contains(&(folder.clone(), root.uid));
        let below = |test: fn(&Row) -> bool| !expanded && replies.iter().any(|(_, r)| test(r));
        let (unread_below, flagged_below) = (below(|r| !r.flags.has(Flag::Seen)), below(|r| r.flags.has(Flag::Flagged)));
        out.push(Item { thread: Some(Thread { expanded, unread_below, flagged_below }), ..item(&run[0]) });
        if expanded {
            out.extend(replies.iter().map(item));
        }
    }
    out
}

/// Rows in runs of a thread root and its replies; one row per run when threading is off.
fn threads(rows: &[(String, Row)]) -> impl Iterator<Item = &[(String, Row)]> { rows.chunk_by(|_, (_, row)| row.depth > 0) }

pub fn narrow() -> bool { window().match_media("(max-width: 768px)").ok().flatten().is_some_and(|q| q.matches()) }

/// After a deploy the service worker starts the page on its stored, older build; reloads once onto the server's
/// `build`, which the worker fetched meanwhile. True when it reloads. Once per build, so a reload that still
/// finds the old build (a slow fetch) cannot loop.
fn reload_onto(build: &str) -> bool {
    const KEY: &str = "kubicle.reloaded";
    let own = js_sys::Reflect::get(&js_sys::global(), &"kubicleBuild".into()).ok().and_then(|b| b.as_string());
    let storage = window().session_storage().ok().flatten();
    if own.as_deref() == Some(build) || storage.as_ref().is_some_and(|s| s.get_item(KEY).ok().flatten().as_deref() == Some(build)) {
        return false;
    }
    let _ = storage.map(|s| s.set_item(KEY, build));
    window().location().reload().is_ok()
}

/// An entry in the browser's history, marked as ours by its state, so its Back returns from the pane or view
/// about to open.
fn push_history() { let _ = window().history().map(|h| h.push_state(&true.into(), "")); }

/// Keeps the entry's state, `push_history`'s mark.
pub fn set_address(hash: &str) {
    let _ = window().history().map(|h| h.replace_state_with_url(&h.state().unwrap_or_default(), "", Some(hash)));
}

/// This browser's local storage, `None` where storage is off: per-browser preferences, as Elastic keeps its own,
/// and Compose's copies of drafts.
pub fn storage() -> Option<web_sys::Storage> { window().local_storage().ok().flatten() }

pub fn local(key: &str) -> Option<String> { storage()?.get_item(key).ok().flatten() }

pub fn set_local(key: &str, value: &str) { let _ = storage().map(|s| s.set_item(key, value)); }

const CHECKBOXES_KEY: &str = "kubicle.checkboxes";

/// What a compose window's `#draft=ID` address ends in, so a reload keeps it one.
pub const WINDOW: &str = "&window=1";

#[derive(Clone, PartialEq)]
pub enum Modal {
    Help,
    /// Roundcube's About: kubicle, its licence and source, and Mailu's own pages.
    About,
    Ask(Ask),
    /// Redirect's dialog for this message: Roundcube's bounce.
    Redirect(Key),
    /// Import's dialog, into the open folder.
    Import,
    /// The folder chooser: "Go to folder" (`g`), or with messages "Move to folder" for them (`v`).
    Folder(Option<Vec<Key>>),
    /// Roundcube's folder form: a new folder, or with a name that folder's rename; the parent folder it starts on.
    FolderForm {
        renaming: Option<String>,
        parent: Option<String>,
    },
}

/// A question in kubicle's own dialog, in place of the browser's `confirm` and `prompt`: `field` holds the text a
/// prompt starts with, `None` for a plain yes-or-no.
#[derive(Clone, PartialEq)]
pub struct Ask {
    pub title: &'static str,
    pub question: String,
    pub field: Option<String>,
    /// Radio choices in place of a field, as (value, label, enabled); the answer is the chosen value, the first
    /// enabled one ticked at first, as Roundcube's Mark all as read dialog.
    pub choices: Vec<(&'static str, &'static str, bool)>,
    pub yes: &'static str,
    /// A second button beside yes, which replies `Reply::Alt`.
    pub alt: Option<&'static str>,
    /// The button that answers nothing; an alert has none.
    pub no: Option<&'static str>,
    pub danger: bool,
}

/// How a question closed: its yes button with the field's text (empty for a plain question), its second button
/// (`Ask::alt`), or unanswered: Cancel, Esc or a click outside.
pub enum Reply {
    Yes(String),
    Alt,
    Cancel,
}

impl Ask {
    pub fn confirm(title: &'static str, question: String, yes: &'static str) -> Self {
        Self { title, question, field: None, choices: Vec::new(), yes, alt: None, no: Some("Cancel"), danger: true }
    }

    /// Roundcube's `alert_dialog`: a notice that waits for OK.
    pub fn alert(question: &str) -> Self { Self { no: None, danger: false, ..Self::confirm("Attention", question.into(), "OK") } }

    pub fn text(title: &'static str, question: &str, start: &str, yes: &'static str) -> Self {
        Self { field: Some(start.into()), danger: false, ..Self::confirm(title, question.into(), yes) }
    }
}

impl App {
    pub fn new() -> Self {
        let hash = window().location().hash().unwrap_or_default();
        let decode = |text: &str| js_sys::decode_uri_component(text).ok().map(String::from);
        let single = hash.strip_prefix("#open=").and_then(|rest| {
            let (uid, folder) = rest.split_once('/')?;
            Some((decode(folder)?, uid.parse().ok()?))
        });
        // The open folder lives in the address, as Roundcube's `_mbox`, so a reload stays in it; checked against
        // the folder list once the session arrives. A standalone window's is its message's: the toolbar reads it.
        let at = hash.strip_prefix("#folder=").map(|rest| rest.split_once("&page=").unwrap_or((rest, "1")));
        let page = at.map_or(0, |(_, page)| page.parse::<u32>().map_or(0, |p| p.saturating_sub(1)));
        let folder = single.as_ref().map(|k| k.0.clone()).or_else(|| at.and_then(|(folder, _)| decode(folder)));
        let decoded = |prefix: &str| hash.strip_prefix(prefix).and_then(decode);
        let mailto =
            decoded("#compose=").map(|uri| (ComposeStart { mode: ComposeMode::New, messages: Vec::new(), zone: time::zone() }, Some(uri)));
        let start = || Some((api::parse(&decoded("#compose-start=")?).ok()?, None));
        let compose_start = mailto.or_else(start);
        let draft = hash.strip_prefix("#draft=").map(|rest| rest.strip_suffix(WINDOW).map_or((rest, false), |id| (id, true)));
        let window = compose_start.is_some() || draft.is_some_and(|(_, window)| window) || single.is_some();
        let compose_window = if window { ComposeWindow::Open } else { ComposeWindow::No };
        let reopen = draft.map(|(id, _)| id.to_owned());
        let (list, search) = (RwSignal::new(None::<ListPage>), RwSignal::new(None::<SearchView>));
        let (expanded, settings) = (RwSignal::new(HashSet::new()), RwSignal::new(Settings::default()));
        let listed = Memo::new(move |_| list.with(|l| search.with(|s| listed(l.as_ref(), s.as_ref()))));
        let rows = Memo::new(move |_| listed.with(|l| expanded.with(|e| thread_items(&l.rows, e))));
        Self {
            ready: RwSignal::new(false),
            settings,
            folders: RwSignal::new(Vec::new()),
            themes: RwSignal::new(Vec::new()),
            folder: RwSignal::new(folder.unwrap_or_else(|| "INBOX".into())),
            page: RwSignal::new(page),
            list,
            search,
            listed,
            rows,
            selection: RwSignal::new(Vec::new()),
            everything: RwSignal::new(None),
            open: RwSignal::new(None),
            cursor: RwSignal::new(None),
            message: RwSignal::new(None),
            unloaded: RwSignal::new(None),
            expanded,
            view: RwSignal::new(View::Mail),
            pane: RwSignal::new(if single.is_some() { Pane::Reader } else { Pane::List }),
            modal: RwSignal::new(None),
            toast: RwSignal::new(None),
            context_at: RwSignal::new(None),
            checkboxes: RwSignal::new(local(CHECKBOXES_KEY).as_deref() == Some("true")),
            recent: RwSignal::new(HashSet::new()),
            drag_open: RwSignal::new(HashSet::new()),
            quota: RwSignal::new(None),
            signed_out: RwSignal::new(false),
            cache: StoredValue::new(Cache { single, reopen, compose_start, compose_window, ..Cache::default() }),
        }
    }

    fn cache<R: Default>(self, f: impl FnOnce(&mut Cache) -> R) -> R { self.cache.try_update_value(f).unwrap_or_default() }

    pub fn user(self) -> String { self.cache(|c| c.user.clone()) }

    pub fn account(self) -> String { self.cache(|c| c.account.clone()) }

    pub fn logout_url(self) -> String { self.cache(|c| c.logout_url.clone()) }

    pub fn admin_url(self) -> String { self.cache(|c| c.admin_url.clone()) }

    pub fn source_url(self) -> Option<String> { self.cache(|c| c.source_url.clone()) }

    pub fn max_message_size(self) -> Option<u64> { self.cache(|c| c.max_message_size) }

    /// The From choices, the account's own first, loaded by each form that offers them as it opens and not with
    /// the session: the server finds them in the headers of all of Sent, which app start would wait for.
    pub fn addresses() -> LocalResource<Vec<Address>> {
        LocalResource::new(|| async { api::get::<Vec<Address>>("api/addresses").await.unwrap_or_default() })
    }

    pub fn single(self) -> Option<Key> { self.cache(|c| c.single.clone()) }

    pub fn open_settings(self, section: Option<&'static str>) {
        if !crate::compose::may_leave(self, move || self.open_settings(section)) {
            return;
        }
        self.cache(|c| c.settings_section = section);
        self.cache(|c| c.read_timer.take()).inspect(TimeoutHandle::clear);
        self.view.set(View::Settings);
        self.go(Pane::Reader);
    }

    pub fn take_settings_section(self) -> Option<&'static str> { self.cache(|c| c.settings_section.take()) }

    pub fn open_context(self, at: (i32, i32), context: Context) {
        self.cache(|c| c.context = Some(context));
        self.context_at.set(Some(at));
    }

    /// The context menu of a list row. A row outside the selection becomes the selection first, as in Thunderbird
    /// and Outlook; the open message stays open.
    pub fn row_menu(self, key: &Key, at: (i32, i32)) {
        if !self.selection.with_untracked(|s| s.contains(key)) {
            self.set_selection(vec![key.clone()]);
        }
        self.open_context(at, Context::Messages(self.with_replies(self.selection.get_untracked())));
    }

    /// Roundcube's Selection: the checkbox column, remembered in this browser as Elastic remembers it.
    pub fn toggle_checkboxes(self) {
        self.checkboxes.update(|c| *c ^= true);
        set_local(CHECKBOXES_KEY, &self.checkboxes.get_untracked().to_string());
    }

    pub fn context(self) -> Option<Context> { self.cache(|c| c.context.clone()) }

    pub fn set_drag(self, keys: Vec<Key>) { self.cache(|c| c.drag = keys); }

    pub fn take_drag(self) -> Vec<Key> { self.cache(|c| std::mem::take(&mut c.drag)) }

    /// A drag of messages is on its way.
    pub fn dragging(self) -> bool { self.cache(|c| !c.drag.is_empty()) }

    /// Roundcube's tree `autoexpand` (`treelist.js`): a collapsed folder a message drag rests on for a second opens,
    /// for this page's life and unsaved, as Roundcube's `folder_collapsed` skips the save during a drag. `None` ends
    /// the wait: the pointer left that folder, or the drag ended.
    pub fn hover_folder(self, name: Option<String>) {
        if self.cache(|c| c.hover.as_ref().map(|(h, _)| h.clone())) == name {
            return; // dragover repeats every few hundred ms: the timer runs on
        }
        self.cache(|c| c.hover.take()).inspect(|(_, timer)| timer.clear());
        let Some(name) = name else { return };
        let open = name.clone();
        let timer = set_timeout_with_handle(move || self.drag_open.update(|d| _ = d.insert(open)), Duration::from_secs(1)).ok();
        self.cache(|c| c.hover = timer.map(|timer| (name, timer)));
    }

    /// Fetches the session: on start, and again after a theme change for the server's theme list.
    pub fn load_session(self) {
        // The first page comes in the session, in the view the stored settings give it: the keys act only once both
        // are here. boot.js asked before the wasm started, if it could.
        let (opening, page) = (self.folder.get_untracked(), self.page.get_untracked());
        let first = !self.ready.get_untracked() && self.single().is_none();
        let url = if first { format!("api/session?folder={}&page={page}", enc(&opening)) } else { "api/session".into() };
        let session = api::started("session").unwrap_or_else(|| api::request(&url));
        spawn_local(async move {
            let session = match api::read::<Session>(session).await {
                Ok(session) => session,
                Err(e) => return self.notify(format!("Could not load the mailbox: {}", e.0)),
            };
            if reload_onto(&session.build) {
                return;
            }
            api::set_csrf(&session.csrf);
            // A reload after a theme change can cross a later save, whose answer holds newer settings than this.
            let settings = match self.settings.get_untracked() {
                here if self.ready.get_untracked() && here.revision >= session.settings.revision => here,
                _ => session.settings,
            };
            time::set_time_zone(&settings.time_zone.clone().unwrap_or_else(time::browser_zone));
            crate::theme::apply(&settings, &session.themes);
            self.cache(|c| {
                c.account = format!("{}.{}", session.user, session.account);
                (c.user, c.logout_url, c.max_message_size) = (session.user, session.logout_url, session.max_message_size);
                (c.admin_url, c.source_url) = (session.admin_url, session.source_url);
            });
            self.settings.set(settings);
            self.themes.set(session.themes);
            self.folders.set(session.folders);
            self.quota.set(session.quota);
            if !self.ready.get_untracked() {
                self.ready.set(true);
                let folder = self.folder.get_untracked();
                if !self.folders.with_untracked(|f| f.iter().any(|f| f.selectable && f.name == folder)) {
                    self.folder.set("INBOX".into()); // the address named a folder that is gone or never existed
                    self.page.set(0);
                    self.set_folder_address("INBOX", 0);
                }
                if let Some(key) = self.single() {
                    self.load_window_page(key.clone(), true);
                    return self.open_message(&key);
                }
                if let Some((start, mailto)) = self.cache(|c| c.compose_start.take()) {
                    self.show_address(); // a reload must not open Compose again
                    self.start_compose(&start, None, mailto);
                }
                self.watch(self.folder.get_untracked());
                // The session's page, unless the address named a folder gone, whose place INBOX took.
                let (folder, page) = (self.folder.get_untracked(), self.page.get_untracked());
                if let Some(first) = session.page.filter(|p| p.query == self.list_query(&folder, page)) {
                    self.keep_page((folder.clone(), page), first);
                }
                self.show_page(folder, page); // loads it, where the session brought none
                if self.cache(|c| c.resynced) {
                    self.recheck_first_page(); // the stream's first Resync came before the session
                }
                self.prefetch_folders(false);
                crate::compose::restore(self);
            }
        });
    }

    /// Asks in a dialog; `then` runs only on the yes button, with the field's text.
    pub fn ask(self, ask: Ask, then: impl FnOnce(String) + Send + Sync + 'static) {
        self.ask_reply(ask, |reply| {
            let Reply::Yes(text) = reply else { return };
            then(text);
        });
    }

    /// `ask`, and `then` hears every way it closes. The dialog gives the focus back to its opener after a cancel, so
    /// a `then` that moves the focus then does so a frame later.
    pub fn ask_reply(self, ask: Ask, then: impl FnOnce(Reply) + Send + Sync + 'static) {
        self.cache(|c| c.answer = Some(Box::new(then)));
        self.modal.set(Some(Modal::Ask(ask)));
    }

    /// Closes the open dialog, telling a question how. A yes or the second button runs once the dialog has closed and
    /// given the focus back, so a next question it asks opens as a dialog of its own and takes the focus, where the
    /// field that held it is gone.
    pub fn reply(self, reply: Reply) {
        self.modal.set(None);
        let Some(then) = self.cache(|c| c.answer.take()) else { return };
        if matches!(reply, Reply::Cancel) { then(reply) } else { set_timeout(move || then(reply), Duration::ZERO) }
    }

    pub fn dismiss(self) { self.reply(Reply::Cancel); }

    /// Closes the open dialog with no answer at all: the view it opened over is gone, and a question's `then` would
    /// act on that view's disposed state.
    pub fn drop_modal(self) {
        if self.modal.with_untracked(Option::is_some) {
            self.cache(|c| c.answer = None);
            self.modal.set(None);
        }
    }

    pub fn notify(self, text: String) {
        let seq = self.show_until_next(text);
        let hide = move || {
            if self.toast.with_untracked(|t| t.as_ref().is_some_and(|(shown, _)| *shown == seq)) {
                self.toast.set(None);
            }
        };
        set_timeout(hide, Duration::from_secs(4));
    }

    /// Shows `text` until the next toast takes its place, as Roundcube's loading notices stay up until the answer.
    fn show_until_next(self, text: String) -> u32 {
        let seq = self.cache(|c| {
            c.toast_seq += 1;
            c.toast_seq
        });
        self.toast.set(Some((seq, text)));
        seq
    }

    /// Sends a change and says so when it fails. The page shows the server's result through the
    /// `Event::FolderChanged` the change publishes, which also reaches every other tab.
    pub fn send(self, change: Change) { self.post_changes(vec![change], None); }

    /// Sends the changes of one action in turn. Roundcube's `busy` notice for it ("Copying message(s)...") shows
    /// at once, since the changes may wait seconds behind a Compose start; once the server took them all, `notice`
    /// (the confirmation after a move, delete, copy or mark), else the error.
    pub fn send_all(self, changes: Vec<Change>, busy: &'static str, notice: &'static str) {
        self.post_changes(changes, Some((busy, notice)));
    }

    /// Posts `changes` in turn, after the Compose start asked before them: the server then reads the message being
    /// answered or forwarded before a later change moves it, in the order the user asked. Pages of a change's folder
    /// asked until its answer are stale (`stale`).
    fn post_changes(self, changes: Vec<Change>, notices: Option<(&'static str, &'static str)>) {
        if changes.is_empty() {
            return;
        }
        if let Some((busy, _)) = notices {
            self.show_until_next(busy.into());
        }
        changes.iter().filter_map(changed_folder).for_each(|folder| self.changing(folder));
        let composing = self.cache(|c| c.composing.clone());
        spawn_local(async move {
            if let Some(start) = composing {
                let _ = JsFuture::from(start).await; // only its order matters: Compose reports its own failure
            }
            let mut failed = None;
            for change in &changes {
                if failed.is_none() {
                    failed = api::post("api/change", change).await.err();
                }
                changed_folder(change).inspect(|folder| self.answered(folder));
            }
            match (failed, notices) {
                (Some(e), _) => self.notify(format!("Not saved: {}", e.0)),
                (None, Some((_, notice))) => self.notify(notice.into()),
                (None, None) => {}
            }
        });
    }

    /// A change of `folder` is on its way to the server, until `answered`: pages of the folder asked meanwhile are
    /// stale (`stale`).
    fn changing(self, folder: &str) {
        self.cache(|c| {
            let (open, sent) = c.changes.entry(folder.to_owned()).or_default();
            (*open, *sent) = (*open + 1, *sent + 1);
        });
    }

    /// A change of `folder` got its answer, sent or refused: once none of its folder's is on its way, what had to wait
    /// for them loads, as for the server's word that the folder changed.
    fn answered(self, folder: &str) {
        let reload = self.cache(|c| {
            let open = &mut c.changes.entry(folder.to_owned()).or_default().0;
            *open -= 1;
            *open == 0 && c.reload_after_changes.remove(folder)
        });
        if reload {
            self.folder_changed(folder);
        }
    }

    /// Where the changes of `folder`, or of every folder for a search (`None`), which may cover any, stand for a page
    /// asked now: how many were sent or reported, or `None` while one is on its way, which the server may not have
    /// applied when it answers the page.
    pub fn settled(self, folder: Option<&str>) -> Option<u32> {
        let mine = |f: &String| folder.is_none_or(|w| w == f);
        self.cache(|c| c.changes.iter().filter(|(f, _)| mine(f)).try_fold(0, |sum, (_, (open, count))| (*open == 0).then_some(sum + count)))
    }

    /// Whether an answer asked when `settled` said `asked` is stale. A page asked before a change was sent, or while
    /// one was on its way, may show what the change took off the list here, and is not shown; one asked before the
    /// server reported a change may have been read before it, after the reload the report made.
    pub fn stale(self, folder: Option<&str>, asked: Option<u32>) -> bool { asked.is_none() || asked != self.settled(folder) }

    /// After an answer `stale` turned away, for `folder`, or for a search with `None`: the view of a folder with
    /// changes on their way loads again once they are answered, at once when none is left.
    pub fn after_changes(self, folder: Option<&str>) {
        let deferred = self.cache(|c| {
            let waiting = c.changes.iter().filter(|(f, (open, _))| *open > 0 && folder.is_none_or(|w| w == *f));
            let open: Vec<String> = waiting.map(|(f, _)| f.clone()).collect();
            c.reload_after_changes.extend(open.iter().cloned());
            !open.is_empty()
        });
        match (deferred, folder) {
            (true, _) => {}
            (false, Some(folder)) => self.folder_changed(folder),
            (false, None) => crate::search::refresh(self),
        }
    }

    /// Changes the settings here at once and saves them. When another tab or device saved since this copy was
    /// read, the server refuses it, and `f` runs again on the server's copy. A save that fails shows the stored
    /// settings again, as Roundcube's form after a failed save; this copy as it was when they cannot be read.
    pub fn update_settings(self, f: impl Fn(&mut Settings) + 'static) {
        let before = self.settings.get_untracked();
        let mut next = before.clone();
        f(&mut next);
        self.show_settings(next.clone());
        spawn_local(async move {
            match self.save_settings(next, f).await {
                Some(saved) => self.adopt_settings(saved),
                None => self.show_settings(api::get::<Settings>("api/settings").await.unwrap_or(before)),
            }
        });
    }

    /// As `update_settings`, but shows the change only once the server took it, for what it checks first (a page
    /// size, a theme); returns whether it did.
    pub async fn checked_settings(self, f: impl Fn(&mut Settings)) -> bool {
        let mut next = self.settings.get_untracked();
        f(&mut next);
        self.save_settings(next, f).await.map(|saved| self.adopt_settings(saved)).is_some()
    }

    /// Posts `settings`, which `f` made from the copy here, and returns them as stored. This tab's saves go one at
    /// a time, a later one with `f` run again on the copy the one before it stored, so a 409 means another tab or
    /// device came first: `f` then runs once more, on the server's copy. `None` after telling the user why.
    async fn save_settings(self, mut settings: Settings, f: impl Fn(&mut Settings)) -> Option<Settings> {
        let mut done = None;
        let mine = js_sys::Promise::new(&mut |resolve, _| done = Some(resolve));
        if let Some(before) = self.cache(|c| c.saving_settings.replace(mine.clone())) {
            let _ = JsFuture::from(before).await;
            settings = self.settings.get_untracked();
            f(&mut settings);
        }
        let saved = self.post_settings(settings, f).await;
        // Once answered it is no save to wait for: a later one would run `f` again on settings that hold it, and a
        // toggle (Details) would undo itself.
        self.cache(|c| _ = c.saving_settings.take_if(|p| js_sys::Object::is(p, &mine)));
        let _ = done.map(|resolve| resolve.call0(&wasm_bindgen::JsValue::NULL));
        saved
    }

    async fn post_settings(self, mut settings: Settings, f: impl Fn(&mut Settings)) -> Option<Settings> {
        let mut retried = false;
        let error = loop {
            match api::post_json::<u64>("api/settings", &settings).await {
                Ok(revision) => return Some(Settings { revision, ..settings }),
                Err(e) if e.1 == 409 && !retried => match api::get::<Settings>("api/settings").await {
                    Ok(fresh) => (settings, retried) = (fresh, true),
                    Err(e) => break e,
                },
                Err(e) => break e,
            }
            f(&mut settings);
        };
        self.notify(format!("Settings not saved: {}", error.0));
        None
    }

    /// Takes the settings as the server stored them, from this tab's save or any other's (`Event::Settings`),
    /// unless a newer copy is already here: an event can arrive after the answer to a later save.
    pub fn adopt_settings(self, settings: Settings) {
        if settings.revision > self.settings.with_untracked(|s| s.revision) {
            self.show_settings(settings);
        }
    }

    /// Puts `new` in place of the settings here and redoes what depends on them: the theme, the time zone, the
    /// pages of folders whose view changed, and the bodies kept of senders who were trusted or no longer are.
    fn show_settings(self, new: Settings) {
        let Some(old) = self.settings.try_update(|s| std::mem::replace(s, new)) else { return };
        let new = self.settings.get_untracked();
        crate::theme::apply(&new, &self.themes.get_untracked());
        let zone = new.time_zone.clone().unwrap_or_else(time::browser_zone);
        if zone != time::zone() {
            time::set_time_zone(&zone);
            self.list.update(|_| {}); // redraws the dates
        }
        if old.themes != new.themes {
            self.load_session(); // for the theme list, which the server builds
        }
        if old.page_size == new.page_size {
            let view = |s: &Settings, f: &str| s.folders.get(f).copied().unwrap_or_default();
            let names: HashSet<&String> = old.folders.keys().chain(new.folders.keys()).collect();
            for folder in names {
                let (before, after) = (view(&old, folder), view(&new, folder));
                // A sort change keeps the page, as Roundcube's list keeps `_page`; a list mode change goes to page 1.
                if before.threads != after.threads {
                    self.reload_list(Some(folder));
                    crate::search::follow_view(self, folder, false);
                } else if (before.sort, before.order) != (after.sort, after.order) {
                    self.refresh(folder);
                    crate::search::follow_view(self, folder, true);
                }
            }
        } else {
            self.reload_list(None);
        }
        let trusted = |s: &Settings, email: &str| s.trusted_senders.iter().any(|t| t.eq_ignore_ascii_case(email));
        self.cache(|c| {
            // A body fetched before a trust change has its remote images as that sender or the setting was then.
            let stale = |m: &Message| {
                old.remote_images != new.remote_images || m.from.first().is_some_and(|f| trusted(&old, &f.email) != trusted(&new, &f.email))
            };
            c.bodies.retain(|_, m| !stale(m));
            c.body_order.retain(|k| c.bodies.contains_key(k));
        });
    }

    /// Loads the pages of `folder`, or of every folder for `None`, again after a change of how they show.
    fn reload_list(self, folder: Option<&str>) {
        match folder {
            Some(folder) => self.drop_pages(folder),
            None => self.cache(|c| c.pages.clear()),
        }
        if folder.is_some_and(|f| *f != self.folder.get_untracked()) {
            return; // its pages load again when it is opened
        }
        let every_folder = folder.is_none();
        self.page.set(0); // the rows on screen stay until the new first page replaces them
        self.show_address();
        spawn_local(async move {
            self.fetch_page(self.folder.get_untracked(), 0, true).await;
            if every_folder {
                self.prefetch_folders(false);
            }
        });
    }

    /// The server checks a page size, so the settings and the list change only once it took the new one.
    pub fn set_page_size(self, size: u32) { spawn_local(async move { _ = self.checked_settings(move |s| s.page_size = size).await }); }

    pub fn prefs(self, folder: &str) -> FolderPrefs { self.settings.with_untracked(|s| s.folders.get(folder).copied().unwrap_or_default()) }

    /// Changes a folder's preferences. A changed view reloads the list alongside the save (`show_settings`): the
    /// list query names the view itself.
    pub fn edit_prefs(self, folder: &str, f: impl Fn(&mut FolderPrefs) + 'static) {
        let folder = folder.to_owned();
        self.update_settings(move |s| f(s.folders.entry(folder.clone()).or_default()));
    }

    /// The folder with `role`, the one place the client looks a role up: a folder may hold several.
    pub fn role_folder(self, role: FolderRole) -> Option<String> {
        self.folders.with_untracked(|f| f.iter().find(|f| f.roles.contains(&role)).map(|f| f.name.clone()))
    }

    pub fn has_role(self, name: &str, role: FolderRole) -> bool { self.role_folder(role).is_some_and(|f| f == name) }

    pub fn go(self, pane: Pane) {
        if pane == Pane::Reader && self.pane.get_untracked() == Pane::Message {
            return; // the message view already shows the reader, alone
        }
        if pane > self.pane.get_untracked() && narrow() {
            push_history();
        }
        self.pane.set(pane);
    }

    /// The list header's Folders button, where the folder column is hidden: on a phone the pane before the list,
    /// between 769 and 1200 px the folder column in the list's place, as in Elastic's normal mode.
    pub fn show_folders(self) { if narrow() { self.back() } else { self.pane.set(Pane::Folders) } }

    /// The in-app back button and Esc on a phone: through the browser history when this entry is one we added.
    pub fn back(self) {
        let ours = window().history().and_then(|h| h.state()).is_ok_and(|s| s.is_truthy());
        if ours && window().history().map(|h| h.back()).is_ok() {
            return;
        }
        self.step_back();
    }

    /// The browser's back, and ours with no entry of ours to go back over: one level up.
    pub fn step_back(self) {
        match (self.view.get_untracked(), self.pane.get_untracked()) {
            (View::Compose, _) if self.cache(|c| c.sending) => {}
            (View::Compose, _) => crate::compose::close(self),
            (View::Settings, _) => self.view.set(View::Mail),
            // A message window has no list to go back to: Back closes it, as the tab was opened for that message.
            (View::Mail, _) if self.single().is_some() => drop(window().close()),
            (View::Mail, Pane::Reader | Pane::Message) => self.pane.set(Pane::List),
            (View::Mail, _) => self.pane.set(Pane::Folders),
        }
    }

    pub fn select_folder(self, name: String) {
        let again = name.clone();
        if !crate::compose::may_leave(self, move || self.select_folder(again)) {
            return;
        }
        self.view.set(View::Mail);
        self.recent.maybe_update(|r| r.remove(&name));
        crate::search::reset(self); // a type, date or scope goes with the search, as Roundcube's list_mailbox does
        self.close_message();
        self.watch(name.clone());
        self.folder.set(name.clone());
        self.show_page(name, 0);
        self.go(Pane::List);
    }

    /// Puts the open folder and page in the address without a history entry, as Roundcube's `_mbox` and `_page`: a
    /// reload stays there, and switching folders is not a step back. The page counts from 1, as Roundcube's. A
    /// message window's address names the message it shows instead, so a reload shows the same one.
    fn set_folder_address(self, folder: &str, page: u32) {
        if let Some((folder, uid)) = self.single() {
            return set_address(&format!("#open={uid}/{}", enc(&folder)));
        }
        let page = if page > 0 { format!("&page={}", page + 1) } else { String::new() };
        set_address(&format!("#folder={}{page}", enc(folder)));
    }

    /// Puts the open folder and page back in the address, where Compose had put its own.
    pub fn show_address(self) { self.set_folder_address(&self.folder.get_untracked(), self.page.get_untracked()); }

    /// Tells the server which folder is on screen, the one it watches for new mail (`kubicle_api::Viewing`).
    fn watch(self, folder: String) {
        spawn_local(async move {
            if let Err(e) = api::post("api/view", &Viewing { folder }).await {
                self.notify(format!("New mail may show late: {}", e.0));
            }
        });
    }

    /// Select all holds across pages of its folder, as Roundcube's does.
    pub fn show_page(self, folder: String, page: u32) {
        self.page.set(page);
        self.set_folder_address(&folder, page);
        let everything = self.everything.get_untracked().filter(|f| *f == folder);
        self.set_selection(Vec::new());
        self.everything.set(everything);
        let cached = self.cache(|c| {
            c.page_clock += 1;
            let (used, list) = c.pages.get_mut(&(folder.clone(), page))?;
            *used = c.page_clock;
            Some(list.clone())
        });
        let hit = cached.is_some();
        self.list.set(cached);
        if hit { after_paint(move || self.after_page()) } else { spawn_local(self.fetch_page(folder, page, false)) }
    }

    /// The page of `folder` as the settings show it: their page size and the folder's sort and mode.
    fn list_query(self, folder: &str, page: u32) -> ListQuery {
        let (prefs, size) = (self.prefs(folder), self.settings.with_untracked(|s| s.page_size));
        ListQuery { folder: folder.to_owned(), page, size, sort: prefs.sort, order: prefs.order, threads: prefs.threads, search: None }
    }

    /// Loads a page into memory, and onto the screen when it is the page wanted there, which alone reports a
    /// failure. `force` loads it even while a copy is on its way, since that copy may predate a change.
    async fn fetch_page(self, folder: String, page: u32, force: bool) {
        let key = (folder.clone(), page);
        if !self.cache(|c| c.pending_pages.insert(key.clone())) && !force {
            return;
        }
        let url = list_url(&self.list_query(&folder, page), &format!("page={page}"));
        self.take_page(folder, page, api::request(&url)).await;
    }

    /// The answer to a page request `sent`, which `pending_pages` holds until it comes.
    async fn take_page(self, folder: String, page: u32, sent: JsFuture) {
        let key = (folder.clone(), page);
        let asked = self.settled(Some(&folder));
        let result = api::read::<ListPage>(sent).await;
        self.cache(|c| c.pending_pages.remove(&key));
        let wanted = self.folder.get_untracked() == folder && self.page.get_untracked() == page;
        let stale = self.stale(Some(&folder), asked);
        match result {
            // Asked for before the view changed: the request for the new view is on its way.
            Ok(list) if list.query != self.list_query(&folder, page) => {}
            // It may bring back what a change took off the list here: the page loads again once the server applied it.
            Ok(_) if stale && wanted => self.refresh(&folder),
            Ok(_) if stale => {}
            // A page past the end, as after the last page's messages went elsewhere: the last page instead, as
            // Roundcube steps back when the user empties it.
            Ok(list) if wanted && list.rows.is_empty() && page > 0 && list.total > 0 =>
                self.show_page(folder, (list.total.div_ceil(list.query.size) - 1).min(page - 1)),
            Ok(list) => {
                self.keep_page(key, list.clone());
                if wanted {
                    self.list.set(Some(list));
                    after_paint(move || self.after_page());
                }
            }
            Err(e) if wanted => self.notify(format!("Could not load {folder}: {}", e.0)),
            Err(_) => {}
        }
    }

    /// Keeps a loaded page. Past `CACHED_PAGES`, the least recently used go, but never one a single step reaches:
    /// a folder's first page (a folder switch), or the open folder's page on screen and the pages beside it (a page
    /// turn, or an arrow key past the page's edge).
    fn keep_page(self, key: (String, u32), list: ListPage) {
        let (folder, page) = (self.folder.get_untracked(), self.page.get_untracked());
        let one_step = |(f, p): &(String, u32)| *p == 0 || (*f == folder && p.abs_diff(page) <= 1);
        self.cache(|c| {
            c.page_clock += 1;
            c.pages.insert(key, (c.page_clock, list));
            while c.pages.len() > CACHED_PAGES {
                let evictable = c.pages.iter().filter(|(kept, _)| !one_step(kept));
                let Some((oldest, _)) = evictable.min_by_key(|(_, (used, _))| *used) else { break };
                let oldest = oldest.clone();
                c.pages.remove(&oldest);
            }
        });
    }

    /// Next and previous pages and the first messages, so the likely next step is already here.
    fn after_page(self) {
        let place = |l: &ListPage| (l.query.folder.clone(), l.query.page, l.total.div_ceil(l.query.size), l.query.threads);
        let Some((folder, page, count, threads)) = self.list.with_untracked(|l| l.as_ref().map(place)) else { return };
        if threads {
            self.expand_new_threads(&folder);
        }
        self.open_edge();
        let missing = |p: &u32| *p < count && !self.cache(|c| c.pages.contains_key(&(folder.clone(), *p)));
        [page + 1, page.wrapping_sub(1)].into_iter().filter(missing).for_each(|p| spawn_local(self.fetch_page(folder.clone(), p, false)));
        if self.open.get_untracked().is_none() {
            self.rows.with_untracked(|r| r.iter().take(PREFETCH_AHEAD).for_each(|i| self.fetch_message(i.key(), None)));
        }
    }

    /// The first page of every other folder, once the current page has painted, so a folder switch shows at once;
    /// `again` refetches those already here, each kept until its fresh copy replaces it. Two at a time: the server
    /// answers one user's requests in turn, so a second keeps it busy over the round trip, and more would only
    /// queue a click's request behind them.
    fn prefetch_folders(self, again: bool) {
        after_paint(move || {
            let current = self.folder.get_untracked();
            let wanted = |name: &String| *name != current && (again || !self.cache(|c| c.pages.contains_key(&(name.clone(), 0))));
            let names: Vec<String> =
                self.folders.with_untracked(|f| f.iter().filter(|f| f.selectable && f.subscribed).map(|f| f.name.clone()).collect());
            let queue = Rc::new(RefCell::new(names.into_iter().filter(wanted).collect::<VecDeque<_>>()));
            for _ in 0..PREFETCH_FOLDERS {
                let queue = queue.clone();
                spawn_local(async move {
                    loop {
                        let Some(folder) = queue.borrow_mut().pop_front() else { break }; // let-else drops the borrow before the await
                        self.fetch_page(folder, 0, again).await;
                    }
                });
            }
        });
    }

    fn drop_pages(self, folder: &str) { self.cache(|c| c.pages.retain(|(f, _), _| f != folder)); }

    /// Reloads the visible page in place, keeping the old rows on screen until the new ones arrive; while a change
    /// of the folder is on its way, once it is answered (`settled`).
    pub fn refresh(self, folder: &str) {
        self.drop_pages(folder);
        if self.settled(Some(folder)).is_none() {
            return self.cache(|c| _ = c.reload_after_changes.insert(folder.to_owned()));
        }
        if self.folder.get_untracked() == folder {
            spawn_local(self.fetch_page(folder.to_owned(), self.page.get_untracked(), true));
        }
    }

    /// Fetches a message body as the server shows it unless it is cached or on its way, or in `variant` always.
    fn fetch_message(self, key: Key, variant: Option<Chosen>) {
        if variant.is_none() && self.cache(|c| c.bodies.contains_key(&key) || !c.pending_bodies.insert(key.clone())) {
            return;
        }
        spawn_local(async move {
            let result = self.load_message(&key, variant).await;
            self.cache(|c| c.pending_bodies.remove(&key));
            let open = self.open.get_untracked().as_ref() == Some(&key);
            match result {
                Ok(message) if open => {
                    self.message.set(Some(message));
                    if variant.is_none() {
                        self.read_when_shown(&key);
                    }
                }
                Err(_) if open => {
                    self.message.set(None);
                    self.unloaded.set(Some(key));
                }
                Ok(_) | Err(_) => {}
            }
        });
    }

    /// A message's body, fetched and kept. Without a `variant` the server shows it as chosen earlier in the session,
    /// else as "Prefer HTML" says.
    async fn load_message(self, key: &Key, variant: Option<Chosen>) -> Result<Message, api::Error> {
        let mut url = format!("api/message?folder={}&uid={}", enc(&key.0), key.1);
        if let Some(Chosen { images, text }) = variant {
            let _ = write!(url, "&images={images}{}", text.map(|t| format!("&text={t}")).unwrap_or_default());
        }
        let message = api::get::<Message>(&url).await?;
        self.cache(|c| {
            if c.bodies.insert(key.clone(), message.clone()).is_none() {
                c.body_order.push_back(key.clone());
            }
            let excess = c.body_order.len().saturating_sub(BODY_CACHE);
            c.body_order.drain(..excess).for_each(|old| _ = c.bodies.remove(&old));
        });
        Ok(message)
    }

    /// A message's body from memory, else fetched now: for what needs the body of a message not opened, as the
    /// Bcc warning of Resend and Forward as attachment.
    pub async fn body(self, key: &Key) -> Option<Message> {
        let cached = self.cache(|c| c.bodies.get(key).cloned());
        if cached.is_some() { cached } else { self.load_message(key, None).await.ok() }
    }

    /// Roundcube's "Mark previewed messages as read", counted from when the body shows: at once, after a delay
    /// while it stays open, or never. Sent only then, so its STORE never queues ahead of the body's FETCH on the
    /// one IMAP connection (a cold open took ~200 ms, not ~35). Never with several selected, as Roundcube; at once
    /// waits out a quick run of arrow keys, so the rows passed stay unread.
    fn read_when_shown(self, key: &Key) {
        let unseen = move |key: &Key| self.row_flags(key).is_some_and(|f| !f.has(Flag::Seen));
        if self.selection.with_untracked(|s| s.len() > 1) || self.everything.with_untracked(Option::is_some) {
            return;
        }
        let settle = self.cache(|c| c.settle_until) - js_sys::Date::now();
        let wait = match self.settings.with_untracked(|s| s.mark_read_after) {
            _ if !unseen(key) => return,
            Some(0) if settle > 0.0 => Duration::from_secs_f64(settle / 1000.0),
            Some(0) => return self.mark_seen(key.clone()),
            Some(seconds) => Duration::from_secs(seconds.into()),
            None => return,
        };
        let key = key.clone();
        let mark = move || {
            if self.open.get_untracked().as_ref() == Some(&key) && unseen(&key) {
                self.mark_seen(key);
            }
        };
        let timer = set_timeout_with_handle(mark, wait).ok();
        self.cache(|c| std::mem::replace(&mut c.read_timer, timer)).inspect(TimeoutHandle::clear);
    }

    /// Shows the open message again in another variant.
    pub fn show_variant(self, variant: Chosen) {
        let Some(key) = self.open.get_untracked() else { return };
        self.fetch_message(key, Some(variant));
    }

    /// Shows a message: from memory when prefetched, else once it arrives. With `select`, it also becomes the
    /// selection, as a click or an arrow key makes it; Shift+arrow keeps the range.
    fn show(self, key: &Key, select: bool) {
        let now = js_sys::Date::now();
        self.cache(|c| {
            if now - c.shown_at < SETTLE_MS {
                c.settle_until = now + SETTLE_MS;
            }
            c.shown_at = now;
        });
        self.cursor.set(Some(key.clone()));
        if select {
            self.cache(|c| c.anchor = Some(key.clone()));
            self.set_selection(vec![key.clone()]);
        }
        if self.open.get_untracked().as_ref() != Some(key) {
            if self.single().is_some() {
                self.cache(|c| c.single = Some(key.clone()));
                self.set_folder_address(&key.0, 0);
            }
            self.open.set(Some(key.clone()));
            self.unloaded.set(None);
            let cached = self.cache(|c| c.bodies.get(key).cloned());
            if cached.is_none() {
                self.fetch_message(key.clone(), None);
            }
            self.message.set(cached);
        }
        if self.message.with_untracked(Option::is_some) {
            self.read_when_shown(key);
        }
        let key = key.clone();
        after_paint(move || {
            self.rows.with_untracked(|rows| {
                let Some(i) = rows.iter().position(|r| r.key() == key) else { return };
                let ahead = rows.iter().skip(i + 1).take(PREFETCH_AHEAD);
                ahead.chain(i.checked_sub(1).and_then(|p| rows.get(p))).for_each(|r| self.fetch_message(r.key(), None));
            });
        });
    }

    pub fn open_message(self, key: &Key) { self.show(key, true); }

    /// Closes the message and clears the selection; Select all stays, as it does across a page turn.
    pub fn close_message(self) {
        self.open.set(None);
        self.cursor.set(None);
        self.message.set(None);
        self.selection.set(Vec::new());
    }

    /// A draft's `copy` is going: a save replaces it (`api/draft` appends, then expunges it), a send drops it or
    /// Discard moves it to Trash. It leaves the list, the reader and the body cache now, as the reader's actions on it
    /// would find no message, and the list comes back with no message open, as Roundcube's after its Compose. A
    /// failed save lists its folder again.
    pub fn draft_copy_gone(self, copy: &DraftCopy) {
        let key = (copy.folder.clone(), copy.uid);
        self.cache(|c| {
            c.bodies.remove(&key);
            c.body_order.retain(|k| *k != key);
        });
        self.drop_pages(&copy.folder);
        self.shown_pages(|p| p.rows.retain(|h| (&h.folder, h.row.uid) != (&copy.folder, copy.uid)));
        if self.open.get_untracked().as_ref() == Some(&key) {
            self.close_message();
        } else if self.selection.with_untracked(|s| s.contains(&key)) {
            self.selection.update(|s| s.retain(|k| *k != key));
        }
    }

    /// Saves Compose `compose`'s `draft` in place of its copy (`api/draft` appends, then expunges the old one), in
    /// that copy's folder, else in Drafts, and answers the new copy. It waits for that Compose's save still on its way
    /// and replaces the copy that one left: two at once would replace the same copy and leave one of theirs behind.
    /// Pages of the folder asked meanwhile may still list the old copy, and are not shown (`stale`).
    pub async fn save_draft(self, compose: &str, mut draft: Draft) -> Result<DraftCopy, api::Error> {
        let mut done = None;
        let mine = js_sys::Promise::new(&mut |resolve, _| done = Some(resolve));
        let before = self.cache(|c| c.saving_draft.insert(compose.to_owned(), mine));
        // A save on its way puts its copy where this one's goes: a Compose's copies stay in one folder.
        let folder = draft.copy.as_ref().map(|c| c.folder.clone()).or_else(|| self.role_folder(FolderRole::Drafts));
        folder.as_ref().inspect(|folder| self.changing(folder));
        if let Some(before) = before {
            draft.copy = saved_copy(JsFuture::from(before).await);
        }
        draft.copy.as_ref().inspect(|old| self.draft_copy_gone(old));
        let result = api::post_json::<DraftCopy>("api/draft", &draft).await;
        let copy = result.as_ref().ok().cloned().or(draft.copy); // a refused save leaves the old copy
        let resolved = api::stringify(&copy);
        let _ = done.map(|resolve| resolve.call1(&wasm_bindgen::JsValue::NULL, &resolved.into()));
        if let Some(folder) = folder {
            self.answered(&folder);
            if result.is_err() {
                self.refresh(&folder); // the old copy stays: list it again
            }
        }
        result
    }

    /// Compose `compose`'s copy once its save on its way, if any, is answered; else `copy`, the one it knows. Send
    /// and Discard name the copy they drop by it.
    pub async fn draft_copy(self, compose: &str, copy: Option<DraftCopy>) -> Option<DraftCopy> {
        match self.cache(|c| c.saving_draft.get(compose).cloned()) {
            Some(saving) => saved_copy(JsFuture::from(saving).await),
            None => copy,
        }
    }

    /// Posts one change and answers how it went, for a caller that waits on it; pages of its folder asked meanwhile
    /// are stale, as for `send`.
    pub async fn post_change(self, change: &Change) -> Result<(), api::Error> {
        let folder = changed_folder(change);
        folder.inspect(|folder| self.changing(folder));
        let result = api::post("api/change", change).await;
        folder.inspect(|folder| self.answered(folder));
        result
    }

    /// Up and Down, as Roundcube's list: the row `delta` away from the cursor's, taken as `key_to` takes it. With
    /// nothing selected an arrow moves the focus alone, as Roundcube's `use_arrow_key` adds Ctrl then.
    pub fn step(self, delta: isize, ctrl: bool, shift: bool) {
        let target = self.rows.with_untracked(|rows| {
            let last = rows.len().checked_sub(1)?;
            let next = match self.place(rows) {
                Some(Ok(i)) => i.saturating_add_signed(delta).min(last),
                // From a row the list does not show, the row after its place, or the one before it.
                Some(Err(i)) if delta > 0 => i.min(last),
                Some(Err(i)) => i.saturating_sub(1),
                None => 0,
            };
            rows.get(next).map(Item::key)
        });
        let Some(target) = target else { return };
        self.key_to(&target, ctrl || (!shift && self.nothing_selected()), shift);
    }

    pub fn nothing_selected(self) -> bool { self.selection.with_untracked(Vec::is_empty) && self.everything.with_untracked(Option::is_none) }

    /// Where the list's keys act from among `rows`: the cursor's row, else the open one's (a refresh may have taken
    /// the cursor's away). `Err(i)` for a row the list does not show, as an open reply in a thread collapsed since,
    /// with `i` the rows shown before its place, as Roundcube walks on from a hidden row.
    fn place(self, rows: &[Item]) -> Option<Result<usize, usize>> {
        let listed = self.listed.with_untracked(|l| l.rows.iter().map(|(f, r)| (f.clone(), r.uid)).collect::<Vec<Key>>());
        let found = [self.cursor.get_untracked(), self.open.get_untracked()].into_iter().flatten().find_map(|key| {
            match rows.iter().position(|r| r.key() == key) {
                Some(i) => Some(Ok(i)),
                None => listed.iter().position(|k| *k == key).map(Err),
            }
        })?;
        Some(found.map_err(|at| rows.iter().filter(|r| listed[..at].contains(&r.key())).count()))
    }

    /// Roundcube's `select_row` from the keys, the cursor moving to `key`: alone it opens `key` as the only selection;
    /// with Shift it selects the range from the anchor to it, with Ctrl and Shift adds that range; with Ctrl alone the
    /// selection stays as it is.
    pub fn key_to(self, key: &Key, ctrl: bool, shift: bool) {
        match (ctrl, shift) {
            (false, false) => self.show(key, true),
            (true, false) => self.cursor.set(Some(key.clone())),
            (_, true) => self.extend_to(key, ctrl),
        }
        scroll_into_view(".list .row.focused");
    }

    /// Roundcube's Previous and Next in the message view and the phone's reader: the message `delta` away in the
    /// folder's order (or the search's), on the page before or after when the open one is at an edge of its page.
    /// Next on a collapsed thread opens it first and goes into it, as Elastic's footer does.
    pub fn neighbour(self, delta: isize) {
        let Some(open) = self.open.get_untracked() else { return };
        let collapsed = self.rows.with_untracked(|r| r.iter().any(|i| i.key() == open && i.thread.is_some_and(|t| !t.expanded)));
        if delta > 0 && collapsed {
            self.toggle_thread(open.clone(), Some(true));
        }
        let target = self.rows.with_untracked(|rows| {
            let i = rows.iter().position(|r| r.key() == open)?;
            rows.get(i.checked_add_signed(delta)?).map(Item::key)
        });
        if let Some(target) = target {
            return self.key_to(&target, false, false);
        }
        let Some((page, pages)) = self.page_position() else { return };
        let to = if delta > 0 { Some(page + 1).filter(|p| *p < pages) } else { page.checked_sub(1) };
        let Some(to) = to else { return };
        self.cache(|c| c.open_edge = Some(delta < 0));
        self.turn(to);
    }

    /// Whether `neighbour(delta)` has a message to go to, tracked: for the buttons' disabled state.
    pub fn has_neighbour(self, delta: isize) -> bool {
        let Some(open) = self.open.get() else { return false };
        let place = self.rows.with(|rows| rows.iter().position(|r| r.key() == open).map(|i| (i, rows.len())));
        let Some((i, len)) = place else { return false };
        let on_page = if delta > 0 { i + 1 < len } else { i > 0 };
        on_page || self.page_position().is_some_and(|(page, pages)| if delta > 0 { page + 1 < pages } else { page > 0 })
    }

    /// The page shown and the number of pages, of the search results or else the list; tracked.
    fn page_position(self) -> Option<(u32, u32)> { self.listed.with(|l| l.paging.map(|(page, pages, _)| (page, pages))) }

    /// Page `page` of the search's results, or else of the folder.
    pub fn turn(self, page: u32) {
        if self.search.with_untracked(Option::is_some) {
            crate::search::page(self, page);
        } else {
            self.show_page(self.folder.get_untracked(), page);
        }
    }

    /// Whether the list has a page after the one shown.
    fn has_later_page(self) -> bool { untrack(|| self.page_position()).is_some_and(|(page, pages)| page + 1 < pages) }

    /// Opens the first or last row of a page that just showed, when Previous, Next or a window's delete asked.
    pub fn open_edge(self) {
        let Some(last) = self.cache(|c| c.open_edge.take()) else { return };
        self.jump(last, false, false);
    }

    /// A double-click: Roundcube's message view in this tab, the reader alone with Previous and Next; Esc or Back
    /// returns to the list. On a phone the reader already fills the screen.
    pub fn open_full(self, key: &Key) {
        self.open_message(key);
        if narrow() || self.single().is_some() {
            return self.go(Pane::Reader);
        }
        if self.pane.get_untracked() != Pane::Message {
            push_history();
        }
        self.pane.set(Pane::Message);
    }

    /// Home and End, as Roundcube's `select_first` and `select_last`: the first or last row, taken as `key_to` takes it.
    pub fn jump(self, last: bool, ctrl: bool, shift: bool) {
        let Some(target) = self.rows.with_untracked(|r| if last { r.last() } else { r.first() }.map(Item::key)) else { return };
        self.key_to(&target, ctrl, shift);
    }

    /// Space on the cursor's row, as Roundcube's list: with Ctrl alone it goes in or out of the selection, else it is
    /// taken as `key_to` takes it, so Space alone opens it as the only selection.
    pub fn space(self, ctrl: bool, shift: bool) {
        // The cursor's row, else the open one's, as `step` finds them: a refresh may have taken the cursor's away.
        let shown = self.rows.with_untracked(|rows| match self.place(rows) {
            Some(Ok(i)) => rows.get(i).map(Item::key),
            _ => None,
        });
        let Some(key) = shown.or_else(|| self.open.get_untracked()) else { return };
        if ctrl && !shift { self.toggle_select(key) } else { self.key_to(&key, ctrl, shift) }
    }

    /// A row in or out of the selection, as Ctrl+click and Ctrl+Space: it becomes the cursor and the anchor.
    pub fn toggle_select(self, key: Key) {
        self.hide_message();
        self.cache(|c| c.anchor = Some(key.clone()));
        self.cursor.set(Some(key.clone()));
        self.everything.set(None);
        self.selection.update(|s| match s.iter().position(|k| *k == key) {
            Some(i) => drop(s.remove(i)),
            None => s.push(key),
        });
    }

    /// Selects the rows from the anchor to `key`, or with `add` adds them to the selection, as Roundcube's
    /// `shift_select`; `key` becomes the cursor.
    pub fn extend_to(self, key: &Key, add: bool) {
        let anchor = self.cache(|c| c.anchor.clone()).or_else(|| self.open.get_untracked());
        let range: Option<Vec<Key>> = self.rows.with_untracked(|rows| {
            let to = rows.iter().position(|r| &r.key() == key)?;
            let from = anchor.and_then(|a| rows.iter().position(|r| r.key() == a)).unwrap_or(to);
            Some(rows[from.min(to)..=from.max(to)].iter().map(Item::key).collect())
        });
        let Some(range) = range else { return };
        self.hide_message();
        self.cursor.set(Some(key.clone()));
        if add {
            self.everything.set(None);
            self.selection.update(|s| s.extend(range.into_iter().filter(|k| !s.contains(k)).collect::<Vec<_>>()));
        } else {
            self.set_selection(range);
        }
    }

    /// Roundcube's `multi_selecting`: a selection Ctrl or Shift makes shows no message, even one of a single row.
    fn hide_message(self) {
        self.open.set(None);
        self.message.set(None);
    }

    /// Selects the rows on screen that `pick` chooses.
    pub fn select(self, pick: impl Fn(&Item) -> bool) {
        self.set_selection(self.rows.with_untracked(|r| r.iter().filter(|i| pick(i)).map(Item::key).collect()));
    }

    pub fn select_page(self) { self.select(|_| true); }

    pub fn invert_selection(self) {
        let selected = self.selection.get_untracked();
        self.select(|i| !selected.contains(&i.key()));
    }

    /// The Threads menu: expands the threads of the folder on screen that `mode` picks, on this page and on those
    /// it shows later, for as long as this page lives.
    pub fn set_thread_mode(self, mode: ExpandThreads) {
        let folder = self.folder.get_untracked();
        self.cache(|c| {
            c.thread_modes.insert(folder.clone(), mode);
            c.decided.retain(|(f, _)| *f != folder);
        });
        self.expanded.update(|e| e.retain(|(f, _)| *f != folder));
        self.expand_new_threads(&folder);
    }

    /// Roundcube's "Expand message threads", or the Threads menu's choice, for the threads on the page or in the
    /// threaded search results not yet expanded or collapsed by either or by the user: a refresh leaves those as they
    /// are.
    pub fn expand_new_threads(self, folder: &str) {
        let mode = self.cache(|c| c.thread_modes.get(folder).copied()).unwrap_or_else(|| self.settings.with_untracked(|s| s.expand_threads));
        let pick = |t: &[(String, Row)]| match mode {
            ExpandThreads::All => true,
            ExpandThreads::Unread => t.iter().any(|(_, r)| !r.flags.has(Flag::Seen)),
            ExpandThreads::Never => false,
        };
        let rows = self.thread_rows().unwrap_or_default();
        let roots = threads(&rows).filter(|t| t[0].0 == folder && t[0].1.depth == 0 && t.len() > 1);
        let fresh: Vec<(Key, bool)> = roots.map(|t| ((t[0].0.clone(), t[0].1.uid), pick(t))).collect();
        let fresh: Vec<(Key, bool)> = self.cache(|c| fresh.into_iter().filter(|(k, _)| c.decided.insert(k.clone())).collect());
        if fresh.iter().any(|(_, expand)| *expand) {
            self.expanded.update(|e| e.extend(fresh.into_iter().filter(|(_, expand)| *expand).map(|(k, _)| k)));
        }
    }

    pub fn toggle_thread(self, key: Key, expand: Option<bool>) {
        self.cache(|c| c.decided.insert(key.clone()));
        self.expanded.update(|e| if expand.unwrap_or(!e.contains(&key)) { _ = e.insert(key) } else { _ = e.remove(&key) });
    }

    /// Left and Right, as Roundcube's list (`use_arrow_key`) on the cursor's row: Right expands a collapsed thread,
    /// else opens the reply below a row with replies; Left collapses an expanded thread, else opens the message a reply
    /// answers, with `to_root` (Ctrl) its thread's first. Over the rows as drawn, so search results and a page that
    /// opens mid-thread go by what shows; a flat list has nothing to do.
    pub fn thread_step(self, right: bool, to_root: bool) {
        enum Step {
            Fold(Key),
            Open(Key),
        }
        let step = self.rows.with_untracked(|rows| {
            let i = self.place(rows)?.ok()?;
            let item = &rows[i];
            let parent = |r: &&Item| if to_root { r.row.depth == 0 } else { r.row.depth < item.row.depth };
            match (right, item.thread) {
                (_, Some(t)) if t.expanded != right => Some(Step::Fold(item.key())),
                (true, _) => rows.get(i + 1).filter(|next| next.row.depth > item.row.depth).map(|next| Step::Open(next.key())),
                // A reply's parent is the nearest shallower row above it, its root the nearest at depth 0.
                (false, _) if item.row.depth > 0 => rows[..i].iter().rev().find(parent).map(|r| Step::Open(r.key())),
                (false, _) => None,
            }
        });
        match step {
            Some(Step::Open(key)) => self.key_to(&key, false, false),
            Some(Step::Fold(root)) => self.toggle_thread(root, Some(right)),
            None => {}
        }
    }

    /// The thread root of the open row, for a message window's thread to show.
    pub fn open_root(self) -> Option<Key> {
        let open = self.open.get_untracked()?;
        let rows = self.thread_rows()?;
        let i = rows.iter().position(|(f, r)| *f == open.0 && r.uid == open.1)?;
        let (folder, root) = rows[..=i].iter().rev().find(|(_, r)| r.depth == 0)?;
        Some((folder.clone(), root.uid))
    }

    /// The rows the list draws threads from, when it draws threads.
    fn thread_rows(self) -> Option<Vec<(String, Row)>> { self.listed.with_untracked(|l| l.threads.then(|| l.rows.clone())) }

    /// What actions apply to: the selection, else the open message, a collapsed thread with its replies. Under
    /// Select all, the rows on screen stand for the folder, which `group` sends as `Messages::All`.
    pub fn targets(self) -> Vec<Key> {
        if let Some(folder) = self.everything.get_untracked() {
            return self.rows.with_untracked(|r| r.iter().map(Item::key).filter(|k| k.0 == folder).collect());
        }
        let selection = self.selection.get_untracked();
        self.with_replies(if selection.is_empty() { self.open.get_untracked().into_iter().collect() } else { selection })
    }

    /// `keys` and the hidden replies of each collapsed thread root among them, which a delete, move, mark or drag
    /// of the root takes too, as Roundcube's `get_selection` does.
    pub fn with_replies(self, mut keys: Vec<Key>) -> Vec<Key> {
        let Some(rows) = self.thread_rows() else { return keys };
        let key = |(f, r): &(String, Row)| (f.clone(), r.uid);
        let replies: Vec<Key> = self.expanded.with_untracked(|expanded| {
            let collapsed = |t: &&[(String, Row)]| t[0].1.depth == 0 && !expanded.contains(&key(&t[0])) && keys.contains(&key(&t[0]));
            threads(&rows).filter(collapsed).flat_map(|t| t[1..].iter().map(key)).collect()
        });
        keys.extend(replies.into_iter().filter(|k| !keys.contains(k)).collect::<Vec<_>>());
        keys
    }

    pub fn set_selection(self, keys: Vec<Key>) {
        self.everything.set(None);
        self.selection.set(keys);
    }

    pub fn select_everything(self) {
        self.set_selection(Vec::new());
        self.everything.set(Some(self.folder.get_untracked()));
    }

    pub fn selected(self, key: &Key) -> bool {
        self.selection.with(|s| s.contains(key)) || self.everything.with(|e| e.as_ref() == Some(&key.0))
    }

    /// The changes for `keys`, a folder at a time; Select all's folder as `Messages::All`.
    fn group(self, keys: &[Key]) -> Vec<(String, Messages)> {
        let everything = self.everything.get_untracked();
        let mut out: Vec<(String, Vec<u32>)> = Vec::new();
        for (folder, uid) in keys {
            match out.iter_mut().find(|(f, _)| f == folder) {
                Some((_, uids)) => uids.push(*uid),
                None => out.push((folder.clone(), vec![*uid])),
            }
        }
        if let Some(folder) = everything.clone().filter(|f| !out.iter().any(|(o, _)| o == f)) {
            out.push((folder, Vec::new())); // a folder whose page on screen is empty
        }
        let all = |folder: &String| everything.as_ref() == Some(folder);
        out.into_iter().map(|(f, uids)| if all(&f) { (f, Messages::All) } else { (f, Messages::Uids(uids)) }).collect()
    }

    /// Changes rows everywhere the client holds them: the visible page, the cached pages and search results.
    fn patch_rows(self, keys: &[Key], f: impl Fn(&mut Row)) {
        let hit = |h: &&mut kubicle_api::Hit| keys.iter().any(|(folder, uid)| *folder == h.folder && *uid == h.row.uid);
        let patch = |page: &mut ListPage| page.rows.iter_mut().filter(hit).for_each(|h| f(&mut h.row));
        self.shown_pages(patch);
        self.cache(|c| c.pages.values_mut().for_each(|(_, page)| patch(page)));
    }

    /// Runs `f` on the page on screen and the search's results.
    fn shown_pages(self, f: impl Fn(&mut ListPage)) {
        self.list.update(|l| l.iter_mut().for_each(&f));
        self.search.update(|s| s.iter_mut().flat_map(|s| s.results.iter_mut()).for_each(&f));
    }

    /// Changes `folder`'s counts at once; the server's next folder event brings the true ones.
    fn counts(self, folder: &str, change: impl FnOnce(&mut Folder)) {
        self.folders.update(|folders| {
            let Some(f) = folders.iter_mut().find(|f| f.name == folder) else { return };
            change(f);
        });
    }

    /// Loads, as a message window's list, the page of its folder that holds the message it shows, so Previous, Next
    /// and the message after a delete follow the folder's order. The `first` load applies "Mark previewed messages
    /// as read", which needs the flags, as Roundcube's `show` does; later reloads do not, so an "As unread" made in
    /// the window stays.
    fn load_window_page(self, key: Key, first: bool) {
        spawn_local(async move {
            let (folder, uid) = key.clone();
            let url = list_url(&self.list_query(&folder, 0), &format!("around={uid}"));
            let asked = self.settled(Some(&folder));
            match api::get::<ListPage>(&url).await {
                // It may bring back what a change took off; the window loads again once the server applied it.
                Ok(_) if self.stale(Some(&folder), asked) => self.after_changes(Some(&folder)),
                Ok(list) => {
                    self.folder.set(folder.clone());
                    self.page.set(list.query.page);
                    self.keep_page((folder, list.query.page), list.clone());
                    self.list.set(Some(list));
                    if first {
                        self.show(&key, false);
                    }
                    // A reply in a collapsed thread shows its thread, so Previous and Next can walk it.
                    if let Some(root) = self.open_root().filter(|root| self.open.get_untracked().as_ref() != Some(root)) {
                        self.toggle_thread(root, Some(true));
                    }
                    after_paint(move || self.after_page());
                }
                Err(e) => self.notify(format!("Could not load {folder}: {}", e.0)),
            }
        });
    }

    /// What `test` says of a message's body here, the open one or one fetched ahead; false for one not loaded.
    pub fn body_says(self, key: &Key, test: fn(&Message) -> bool) -> bool {
        let open = self.message.with(|m| m.as_ref().filter(|m| m.folder == key.0 && m.uid == key.1).map(test));
        open.unwrap_or_else(|| self.cache(|c| c.bodies.get(key).is_some_and(test)))
    }

    /// A message's flags, from the page on screen or the search results: the one copy the client keeps.
    pub fn row_flags(self, key: &Key) -> Option<Flags> { self.row(key).map(|r| r.flags) }

    /// A message's row, from the page on screen or the search results.
    pub fn row(self, (folder, uid): &Key) -> Option<Row> {
        let find = |page: &ListPage| page.rows.iter().find(|h| h.folder == *folder && h.row.uid == *uid).map(|h| h.row.clone());
        self.list
            .with_untracked(|l| l.as_ref().and_then(find))
            .or_else(|| self.search.with_untracked(|s| s.as_ref()?.results.as_ref().and_then(find)))
    }

    /// One message read, never the folder: `set_flag` stands for all of it while Select all is on.
    pub fn mark_seen(self, key: Key) {
        if self.row_flags(&key).is_some_and(|f| !f.has(Flag::Seen)) {
            self.counts(&key.0, |f| f.unseen = f.unseen.saturating_sub(1));
        }
        self.patch_rows(std::slice::from_ref(&key), |row| row.flags.set(Flag::Seen, true));
        self.send(Change::Flag { folder: key.0, messages: Messages::Uids(vec![key.1]), flag: Flag::Seen, set: true });
    }

    /// A mark the user chose (the Mark menu, a row's status or flag): Roundcube confirms it.
    pub fn set_flag(self, keys: &[Key], flag: Flag, set: bool) {
        let flips = keys.iter().filter(|key| flag == Flag::Seen && self.row_flags(key).is_some_and(|f| f.has(Flag::Seen) != set));
        flips.for_each(|key| self.counts(&key.0, |f| f.unseen = if set { f.unseen.saturating_sub(1) } else { f.unseen + 1 }));
        self.patch_rows(keys, |row| row.flags.set(flag, set));
        let mut changes = Vec::new();
        for (folder, messages) in self.group(keys) {
            if flag == Flag::Seen && messages == Messages::All {
                self.counts(&folder, |f| f.unseen = if set { 0 } else { f.total });
            }
            changes.push(Change::Flag { folder, messages, flag, set });
        }
        self.send_all(changes, "Marking message(s)...", "Message(s) marked successfully.");
    }

    /// Takes `keys` off the list at once and opens the next message, as Roundcube does after a delete or move. `to`
    /// counts them, as read when `read` (the server marks them on the way).
    fn remove(self, keys: &[Key], to: Option<&str>, read: bool) {
        let next = self.rows.with_untracked(|rows| {
            let first = rows.iter().position(|r| keys.contains(&r.key()))?;
            let last = rows.iter().rposition(|r| keys.contains(&r.key()))?;
            let below = rows[last + 1..].iter().find(|r| !keys.contains(&r.key()));
            // A message window goes on in the folder's order only: there is no next after the last one.
            let above = || rows[..first].iter().rev().find(|r| !keys.contains(&r.key()));
            below.or_else(|| if self.single().is_some() { None } else { above() }).map(Item::key)
        });
        let was_open = self.open.get_untracked().is_some_and(|o| keys.contains(&o));
        if !was_open && self.cursor.get_untracked().is_some_and(|c| keys.contains(&c)) {
            self.cursor.set(next.clone()); // else Enter and the keys act from a message that is gone
        }
        for key in keys {
            let unseen = u32::from(self.row_flags(key).is_some_and(|f| !f.has(Flag::Seen)));
            self.counts(&key.0, |f| (f.total, f.unseen) = (f.total.saturating_sub(1), f.unseen.saturating_sub(unseen)));
            let arrives = if read { 0 } else { unseen };
            to.inspect(|to| self.counts(to, |f| (f.total, f.unseen) = (f.total + 1, f.unseen + arrives)));
            self.drop_pages(&key.0);
        }
        to.inspect(|to| self.drop_pages(to));
        let gone = |folder: &str, uid: u32| keys.iter().any(|(f, u)| f == folder && *u == uid);
        self.shown_pages(|page| page.rows.retain(|h| !gone(&h.folder, h.row.uid)));
        self.everything.set(None); // the messages it stood for are gone; callers took its `Messages::All` first
        let display_next = self.settings.with_untracked(|s| s.display_next);
        match next {
            Some(next) if was_open && display_next => self.open_message(&next),
            // In a message window the next message may be the next page's first, which the page's reload after the
            // move brings to this page's end; past the folder's last message the window closes.
            None if was_open && display_next && self.single().is_some() && self.has_later_page() => {
                self.cache(|c| c.open_edge = Some(true));
                self.close_message();
            }
            _ if was_open && self.single().is_some() => drop(window().close()),
            _ if was_open => self.close_message(),
            _ => self.set_selection(Vec::new()),
        }
        // The last page emptied: the one before, as Roundcube steps back.
        let Some((page @ 1.., ..)) = self.listed.with_untracked(|l| l.paging.filter(|_| l.rows.is_empty())) else { return };
        self.turn(page - 1);
    }

    /// Copy to...: the messages stay, and `to` counts their copies at once.
    pub fn copy_to(self, keys: &[Key], to: &str) {
        let keys: Vec<Key> = keys.iter().filter(|(f, _)| f != to).cloned().collect();
        let changes = self.group(&keys).into_iter().map(|(folder, messages)| Change::Copy { folder, messages, to: to.to_owned() });
        for key in &keys {
            let unseen = u32::from(self.row_flags(key).is_some_and(|f| !f.has(Flag::Seen)));
            self.counts(to, |f| (f.total, f.unseen) = (f.total + 1, f.unseen + unseen));
        }
        self.drop_pages(to);
        self.send_all(changes.collect(), "Copying message(s)...", "Message(s) copied successfully.");
    }

    /// A move, a drag or Delete to Trash. Into Trash the server marks them read on the way under "Mark the message
    /// as read on delete", whatever moved them, as Roundcube's IMAP layer does.
    pub fn move_to(self, keys: &[Key], to: &str) {
        let keys: Vec<Key> = keys.iter().filter(|(f, _)| f != to).cloned().collect();
        let changes = self.group(&keys).into_iter().map(|(folder, messages)| Change::Move { folder, messages, to: to.to_owned() });
        let trash = self.role_folder(FolderRole::Trash).is_some_and(|t| t == to);
        self.remove(&keys, Some(to), trash && self.settings.with_untracked(|s| s.read_when_deleted));
        let notice = if trash { "Message(s) moved to Trash successfully." } else { "Message(s) moved successfully." };
        self.send_all(changes.collect(), "Moving message(s)...", notice);
    }

    /// Whether `name` is the folder `root` or below it.
    pub fn in_tree(self, name: &str, root: &str) -> bool {
        let delimiter = self.folders.with_untracked(|f| f.iter().find(|f| f.name == name).and_then(|f| f.delimiter));
        kubicle_api::within(name, root, delimiter)
    }

    /// Roundcube's archive plugin: to the Archive folder, or the folders below it the server divides it into by the
    /// dates in the user's zone, leaving what is already in it or below it.
    pub fn archive(self, keys: &[Key]) {
        let Some(archive) = self.role_folder(FolderRole::Archive) else {
            return self.notify("There is no Archive folder.".into());
        };
        let keys: Vec<Key> = keys.iter().filter(|(f, _)| !self.in_tree(f, &archive)).cloned().collect();
        let zone = time::zone();
        let changes = self.group(&keys).into_iter().map(|(folder, messages)| Change::Archive { folder, messages, zone: zone.clone() });
        let (read, divided) = self.settings.with_untracked(|s| (s.read_on_archive, s.archive_type != ArchiveType::None));
        self.remove(&keys, (!divided).then_some(archive.as_str()), read); // a divided archive counts once its folders report
        self.send_all(changes.collect(), "Moving message(s)...", "Successfully archived"); // the archive plugin moves as Move does
    }

    /// Roundcube's markasjunk: Junk (`spam`) reports and moves the targets outside Junk to it, Not junk those in Junk
    /// to INBOX. A search over several folders can hold both, so each takes only its own side.
    pub fn report(self, keys: &[Key], spam: bool) {
        let Some(junk) = self.role_folder(FolderRole::Junk) else { return self.notify("There is no Junk folder.".into()) };
        let to = if spam { Some(junk.clone()) } else { self.role_folder(FolderRole::Inbox) };
        let Some(to) = to else { return self.notify("There is no Inbox folder.".into()) };
        let keys: Vec<Key> = keys.iter().filter(|(f, _)| (*f == junk) != spam).cloned().collect();
        let changes = self.group(&keys).into_iter().map(|(folder, messages)| Change::Report { folder, messages, spam }).collect();
        self.remove(&keys, Some(&to), false);
        let notice = if spam { "Successfully reported as junk" } else { "Successfully reported as not junk" };
        self.send_all(changes, "Loading...", notice);
    }

    /// Roundcube's zipdownload of a selection: the browser saves the archive as the server streams it.
    pub fn download(self, keys: &[Key], mode: ZipMode) {
        let sets: Vec<(String, String)> = self.group(keys).into_iter().map(|(f, m)| (f, kubicle_api::uid_set(&m))).collect();
        let _ = window().location().set_href(&format!("api/zip?mode={mode:?}&sets={}", enc(&api::stringify(&sets))));
    }

    /// Delete, as Roundcube's `delete_messages`: for good at once in Trash or with no Trash folder (Special Folders'
    /// None for Trash), else to Trash, or with Shift for good after a confirmation.
    pub fn delete(self, keys: &[Key], shift: bool) {
        // Roundcube's "Directly delete messages in Junk" makes Junk delete for good as Trash does.
        let delete_junk = self.settings.with_untracked(|s| s.delete_junk);
        let deletes = |f: &str| self.has_role(f, FolderRole::Trash) || (delete_junk && self.has_role(f, FolderRole::Junk));
        let trash = self.role_folder(FolderRole::Trash);
        let at_once = trash.is_none() || keys.iter().all(|(f, _)| deletes(f));
        if let Some(trash) = trash.filter(|_| !at_once && !shift) {
            return self.move_to(keys, &trash);
        }
        let count = match self.everything.get_untracked() {
            Some(folder) => self.folders.with_untracked(|f| f.iter().find(|f| f.name == folder).map_or(0, |f| f.total as usize)),
            None => keys.len(),
        };
        let keys = keys.to_vec();
        let delete = move || {
            let changes = self.group(&keys).into_iter().map(|(folder, messages)| Change::Delete { folder, messages }).collect();
            self.remove(&keys, None, false);
            self.send_all(changes, "Deleting message(s)...", "Message(s) deleted successfully.");
        };
        if at_once {
            return delete();
        }
        let question = format!("Delete {count} message(s) for good?");
        self.ask(Ask::confirm("Delete for good", question, "Delete"), move |_| delete());
    }

    /// Roundcube's Empty (`folder_purge.php`): in Trash or below it, every message goes for good; anywhere else,
    /// Junk included, every message moves to Trash.
    pub fn empty(self, folder: String) {
        let trash = self.role_folder(FolderRole::Trash);
        let in_trash = trash.as_ref().is_some_and(|t| self.in_tree(&folder, t));
        let question = if in_trash || trash.is_none() {
            format!("Delete every message in {folder} for good?")
        } else {
            format!("Move every message in {folder} to Trash?")
        };
        self.ask(Ask::confirm("Empty folder", question, "Empty"), move |_| self.empty_now(folder, trash.filter(|_| !in_trash)));
    }

    fn empty_now(self, folder: String, to_trash: Option<String>) {
        let mut moved = (0, 0);
        self.counts(&folder, |f| moved = (std::mem::take(&mut f.total), std::mem::take(&mut f.unseen)));
        if self.folder.get_untracked() == folder {
            self.close_message();
            self.set_selection(Vec::new());
            self.list.update(|l| l.iter_mut().for_each(|l| l.rows.clear()));
        }
        let change = match to_trash {
            Some(to) => {
                let unseen = if self.settings.with_untracked(|s| s.read_when_deleted) { 0 } else { moved.1 };
                self.counts(&to, |f| (f.total, f.unseen) = (f.total + moved.0, f.unseen + unseen));
                self.drop_pages(&to);
                Change::Move { folder, messages: Messages::All, to }
            }
            None => Change::Delete { folder, messages: Messages::All },
        };
        self.send_all(vec![change], "Loading...", "Folder successfully emptied.");
    }

    /// Roundcube's `mark_all_read_state` for `folder`: 1 when it has unread mail, 2 when a subscribed folder below
    /// it has, 4 when any other subscribed folder has. Its own count decides 1, not the collapsed subtree's as in
    /// Roundcube, so "Selected folder only" never marks a folder with nothing unread.
    pub fn mark_all_read_state(self, folder: &str) -> u8 {
        let unread = |f: &&Folder| f.subscribed && f.selectable && f.unseen > 0;
        let bit = |f: &Folder| match f.name == folder {
            true => 1,
            false if kubicle_api::within(&f.name, folder, f.delimiter) => 2,
            false => 4,
        };
        self.folders.with(|folders| folders.iter().filter(unread).fold(0, |state, f| state | bit(f)))
    }

    /// Roundcube's Mark all as read: at once when only this folder has unread mail, else its dialog of three scopes,
    /// each enabled where it has some.
    pub fn mark_all_read(self, folder: String) {
        let state = self.mark_all_read_state(&folder);
        if state <= 1 {
            return self.mark_folders_read(&folder, "cur");
        }
        let choices = vec![
            ("cur", "Selected folder only", state & 1 != 0),
            ("sub", "Selected folder and its subfolders", state & 2 != 0),
            ("all", "All folders", state & 4 != 0),
        ];
        let ask = Ask { choices, field: None, danger: false, yes: "Mark", ..Ask::confirm("Mark all as read", String::new(), "") };
        self.ask(ask, move |scope| self.mark_folders_read(&folder, &scope));
    }

    /// Marks every message read in `folder`, with `sub` the subscribed folders below it too, with `all` every
    /// subscribed folder: one STORE `1:*` for each that has unread mail.
    fn mark_folders_read(self, folder: &str, scope: &str) {
        let names: Vec<String> = self.folders.with_untracked(|folders| {
            let taken = |f: &Folder| match scope {
                "all" => f.subscribed,
                "sub" => f.subscribed && kubicle_api::within(&f.name, folder, f.delimiter),
                _ => f.name == folder,
            };
            folders.iter().filter(|f| f.selectable && f.unseen > 0 && taken(f)).map(|f| f.name.clone()).collect()
        });
        let keys: Vec<Key> = self.rows.with_untracked(|r| r.iter().map(Item::key).filter(|k| names.contains(&k.0)).collect());
        self.patch_rows(&keys, |r| r.flags.set(Flag::Seen, true));
        for name in &names {
            self.counts(name, |f| f.unseen = 0);
        }
        let changes = names.into_iter().map(|folder| Change::Flag { folder, messages: Messages::All, flag: Flag::Seen, set: true });
        self.send_all(changes.collect(), "Marking message(s)...", "Message(s) marked successfully.");
    }

    /// Compose on a new message, or one that answers, forwards or reopens `key`. As Roundcube's, the original turns
    /// read once Compose has it, unless it only travels as an attachment.
    pub fn compose(self, mode: ComposeMode, key: Option<&Key>) {
        if key.is_none() && mode != ComposeMode::New {
            return;
        }
        let messages = key.iter().map(|(folder, uid)| (folder.clone(), Messages::Uids(vec![*uid]))).collect();
        let read = key.filter(|_| mode != ComposeMode::ForwardAttachment).cloned();
        self.start_compose(&ComposeStart { mode, messages, zone: time::zone() }, read, None);
    }

    /// Forward as attachment of several messages, or under Select all of the whole folder, as Roundcube's forward of
    /// a selection: each run of one folder's messages, in the order given.
    pub fn forward_attachments(self, keys: &[Key]) {
        let run = |run: &[Key]| (run[0].0.clone(), Messages::Uids(run.iter().map(|(_, uid)| *uid).collect()));
        let messages = match self.everything.get_untracked() {
            Some(folder) => vec![(folder, Messages::All)],
            None => keys.chunk_by(|a, b| a.0 == b.0).map(run).collect(),
        };
        self.start_compose(&ComposeStart { mode: ComposeMode::ForwardAttachment, messages, zone: time::zone() }, None, None);
    }

    /// Roundcube's "Compose in a new window": not on a phone, and not from a window that is already one.
    fn compose_elsewhere(self) -> bool { self.settings.with_untracked(|s| s.compose_extwin) && !narrow() && !self.compose_window() }

    pub fn compose_window(self) -> bool { self.cache(|c| c.compose_window != ComposeWindow::No) }

    pub fn close_after_leaving(self) { self.cache(|c| c.compose_window = ComposeWindow::Closing); }

    pub fn take_closing(self) -> bool {
        self.cache(|c| {
            let closing = c.compose_window == ComposeWindow::Closing;
            if closing {
                c.compose_window = ComposeWindow::Open;
            }
            closing
        })
    }

    /// New mail from a `mailto:` link, here, as Roundcube's `compose` command opens it: one in a plain-text mail, or
    /// an address's Compose to.
    pub fn compose_mailto(self, uri: String) {
        self.start_compose(&ComposeStart { mode: ComposeMode::New, messages: Vec::new(), zone: time::zone() }, None, Some(uri));
    }

    /// Opens Compose on the server's prefilled draft for `start`, filled from `mailto`, a `mailto:` URI, when there is
    /// one; `read` turns read once Compose has it. With "Compose in a new window" the new tab does this instead,
    /// opened here within the click, as a browser allows a new tab only then.
    fn start_compose(self, start: &ComposeStart, read: Option<Key>, mailto: Option<String>) {
        let again = (start.clone(), read.clone(), mailto.clone());
        if !self.compose_elsewhere() && !crate::compose::may_leave(self, move || self.start_compose(&again.0, again.1, again.2)) {
            return; // a new tab ("Compose in a new window") leaves the Compose here as it is
        }
        let read = move || {
            let Some(key) = read.filter(|key| self.row_flags(key).is_none_or(|f| !f.has(Flag::Seen))) else { return };
            self.mark_seen(key);
        };
        if self.compose_elsewhere() {
            let hash = match &mailto {
                Some(uri) => format!("#compose={}", enc(uri)),
                None => format!("#compose-start={}", enc(&api::stringify(start))),
            };
            crate::actions::new_tab(&hash);
            return read();
        }
        let Ok(start) = api::post_shared("api/compose", start) else { return };
        self.cache(|c| c.composing = Some(start.clone()));
        spawn_local(async move {
            match api::read::<Prefill>(JsFuture::from(start)).await {
                Ok(mut prefill) => {
                    if let Some(uri) = &mailto {
                        text::fill_from_mailto(&mut prefill.draft, uri);
                    }
                    read();
                    self.open_compose_as(crate::compose::new_id(), prefill.draft, prefill.forwards_bcc);
                    if let Some(notice) = prefill.notice {
                        self.notify(notice);
                    }
                }
                Err(e) => self.notify(format!("Could not start the message: {}", e.0)),
            }
        });
    }

    /// Opens Compose on `draft` as the compose `id`, the one a reload or a restored browser copy continues;
    /// `forwards_bcc` when a message it forwards whole carries Bcc lines.
    pub fn open_compose_as(self, id: String, draft: Draft, forwards_bcc: bool) {
        self.cache(|c| c.seed = Some((id, draft, forwards_bcc)));
        self.cache(|c| c.read_timer.take()).inspect(TimeoutHandle::clear);
        self.view.set(View::Compose);
        self.go(Pane::Reader);
    }

    pub fn take_reopen(self) -> Option<String> { self.cache(|c| c.reopen.take()) }

    pub fn set_sending(self, sending: bool) { self.cache(|c| c.sending = sending); }

    pub fn sending(self) -> bool { self.cache(|c| c.sending) }

    pub fn take_seed(self) -> (String, Draft, bool) {
        self.cache(|c| c.seed.take()).expect("Compose opens only through open_compose_as, which leaves its draft")
    }

    /// boot.js asks for the first page, with the session, before the event stream opens, so a change in between goes
    /// unheard: the page is fetched again once the stream's first `Resync` has come.
    fn recheck_first_page(self) { spawn_local(self.fetch_page(self.folder.get_untracked(), self.page.get_untracked(), true)); }

    /// Takes the server's folder list, marking Roundcube's "recent" folders: any but the open one whose unread count
    /// and next UID both grew, so mail that arrived unread, not mail another app marked unread. The mark goes when
    /// the folder opens or has no unread mail left.
    fn set_folders(self, folders: Vec<Folder>) {
        let open = self.folder.get_untracked();
        let grew: Vec<String> = self.folders.with_untracked(|old| {
            let grew = |f: &&Folder| old.iter().any(|o| o.name == f.name && f.unseen > o.unseen && f.uid_next > o.uid_next);
            folders.iter().filter(|f| f.name != open).filter(grew).map(|f| f.name.clone()).collect()
        });
        let stays = |name: &String| folders.iter().any(|f| f.name == *name && f.unseen > 0);
        if !grew.is_empty() || self.recent.with_untracked(|r| !r.iter().all(stays)) {
            self.recent.update(|r| {
                r.extend(grew);
                r.retain(stays);
            });
        }
        self.folders.set(folders);
    }

    pub fn event(self, event: Event) {
        match event {
            Event::Resync(folders, quota) => {
                self.set_folders(folders);
                self.quota.set(quota);
                // The first stream opened before the session, so what that loads is current, whichever came first;
                // only the first page left earlier. A window showing one message has no list, only its row.
                let first = !self.cache(|c| std::mem::replace(&mut c.resynced, true));
                match self.single() {
                    // Before the session, load_session checks its page once it has come.
                    None if first && self.list.with_untracked(Option::is_some) => self.recheck_first_page(),
                    _ if first || !self.ready.get_untracked() => {}
                    Some(key) => self.load_window_page(key, false),
                    None => {
                        // Changes may have gone unheard: first pages stay until fresh copies replace them, so a
                        // folder switch meanwhile still shows at once; later pages go.
                        self.cache(|c| c.pages.retain(|(_, page), _| *page == 0));
                        self.reload(&self.folder.get_untracked());
                        self.prefetch_folders(true);
                        spawn_local(async move {
                            let Ok(settings) = api::get::<Settings>("api/settings").await else { return };
                            self.adopt_settings(settings);
                        });
                    }
                }
            }
            Event::Folders(folders) => self.set_folders(folders),
            Event::Quota(quota) => self.quota.set(quota),
            Event::Settings(settings) => self.adopt_settings(*settings),
            Event::FolderChanged { folder } => {
                // A page of it asked before this report may predate the change, so it is not kept (`stale`).
                self.cache(|c| c.changes.entry(folder.clone()).or_default().1 += 1);
                // Before the session nothing is drawn (dates need its time zone), and what it then loads is current.
                if self.ready.get_untracked() {
                    self.folder_changed(&folder);
                }
            }
            Event::SessionExpired => api::signed_out(),
            Event::Notice(text) => self.notify(text),
            Event::Keepalive => {}
        }
    }

    /// What shows `folder` loads again, for changes the server reported or this tab's once answered.
    fn folder_changed(self, folder: &str) { if self.single().is_some() { self.window_changed(folder) } else { self.reload(folder) } }

    /// A message window reloads the page around the message it shows, wherever new mail moved it.
    fn window_changed(self, folder: &str) {
        if let Some(key) = self.open.get_untracked().filter(|o| o.0 == folder) {
            self.load_window_page(key, false);
        } else if self.single().is_some_and(|k| k.0 == folder) {
            self.refresh(folder); // the window waits for a page with the message after a deleted one
        }
    }

    /// Reloads `folder`'s page on screen and any search, for changes the server reported.
    fn reload(self, folder: &str) {
        self.refresh(folder);
        crate::search::refresh(self);
    }
}

/// The folder whose messages `change` takes, the one whose rows the server's answer to it brought up to date.
fn changed_folder(change: &Change) -> Option<&str> {
    match change {
        Change::Flag { folder, .. }
        | Change::Move { folder, .. }
        | Change::Archive { folder, .. }
        | Change::Report { folder, .. }
        | Change::Copy { folder, .. }
        | Change::Delete { folder, .. } => Some(folder),
        Change::CreateFolder { .. } | Change::RenameFolder { .. } | Change::DeleteFolder { .. } | Change::Subscribe { .. } => None,
    }
}

/// The `api/list` request for `q` from `at`: `page=N` or `around=UID`.
pub fn list_url(q: &ListQuery, at: &str) -> String {
    let search = q.search.as_ref().map(|s| format!("&search={}", enc(&api::stringify(s)))).unwrap_or_default();
    format!("api/list?folder={}&{at}&size={}&sort={:?}&order={:?}&threads={}{search}", enc(&q.folder), q.size, q.sort, q.order, q.threads)
}

/// What a `save_draft` promise resolved to: the draft's copy, or none.
fn saved_copy(resolved: Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>) -> Option<DraftCopy> {
    api::parse(&resolved.ok()?.as_string()?).ok().flatten()
}

/// Scrolls the element `selector` finds into view once this frame has drawn it, no further than it takes.
pub fn scroll_into_view(selector: &'static str) {
    request_animation_frame(move || {
        if let Ok(Some(found)) = document().query_selector(selector) {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            found.scroll_into_view_with_scroll_into_view_options(&options);
        }
    });
}

/// Runs `f` once the browser has painted the current change, so prefetch requests never delay a frame.
fn after_paint(f: impl FnOnce() + 'static) { request_animation_frame(move || set_timeout(f, Duration::ZERO)); }
