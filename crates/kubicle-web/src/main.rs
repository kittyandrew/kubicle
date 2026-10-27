//! The kubicle client: Roundcube Elastic's widescreen layout (folders, list, reader), one pane at a time on a
//! phone. `state` holds the data and actions, the other modules draw it.
// The pedantic size limit is for values costly to copy; `App` is arena handles of a word or two each.
#![allow(clippy::large_types_passed_by_value)]

mod actions;
mod compose;
mod events;
mod folders;
mod list;
mod reader;
mod search;
mod settings;
mod state;
mod theme;

use kubicle_api::{ComposeMode, Folder, FolderRole};
use kubicle_browser::api;
use kubicle_ui::{dialog, dialog::Dialog, icons, icons::icon, menu};
use leptos::{ev::KeyboardEvent, prelude::*};
use wasm_bindgen::JsCast;

use crate::{actions::ARCHIVE, actions::Action, actions::DELETE, actions::FORWARD, actions::JUNK, actions::MARK_FLAGGED};
use crate::{actions::MARK_READ, actions::MARK_UNFLAGGED, actions::MARK_UNREAD, actions::NOT_JUNK, actions::REPLY, actions::REPLY_ALL_MAIN};
use crate::{state::App, state::Context, state::Modal, state::Pane, state::Reply, state::View};

fn main() {
    // A panic's message and place in the console, where wasm alone shows only "RuntimeError: unreachable".
    std::panic::set_hook(Box::new(|info| leptos::logging::error!("{info}")));
    theme::apply_cached();
    mount_to_body(Root);
}

#[component]
fn Root() -> impl IntoView {
    let app = App::new();
    provide_context(app);
    // Before its first session the page has nothing open to lose (Compose starts only once the session came), so a
    // refusal signs in at once, as Mailu's Roundcube sends a page request without a session to sso.php.
    api::on_signed_out(move || {
        if app.ready.get_untracked() || !api::sign_in_at_once() {
            app.signed_out.set(true);
        }
    });
    // After it, Roundcube's `session_error`: Compose stays, with the notice and its timers stopped, so nothing typed
    // is lost; anywhere else, and once Compose is left, the sign-in follows 2 s after the notice.
    Effect::new(move |_| {
        if app.signed_out.get() && app.view.get() != View::Compose {
            set_timeout(api::reauthenticate, std::time::Duration::from_secs(2));
        }
    });
    app.load_session();
    events::start(app); // alongside the session: events from here on reach the pages it loads
    theme::unpin_on_system_change(app);
    dialog::keep_focus_on_removal();

    // The tab names the view and the folder, as Roundcube's "Mailu Webmail :: Inbox » github", or "Search result" while
    // search results show (`list_pagetitle`); in mail the Inbox's unread count leads it.
    Effect::new(move |_| {
        let title = match app.view.get() {
            // A message window is named after its message, as Roundcube's `show` titles the page with the subject.
            View::Mail if app.single().is_some() => {
                let subject = app.message.with(|m| m.as_ref().map(|m| m.subject.trim().chars().take(128).collect::<String>()));
                let subject = subject.filter(|s| !s.is_empty()).unwrap_or_else(|| "(no subject)".into());
                format!("kubicle :: {subject}")
            }
            View::Mail => app.folders.with(|folders| {
                let unseen = folders.iter().find(|f| f.roles.contains(&FolderRole::Inbox)).map_or(0, |f| f.unseen);
                let unread = if unseen > 0 { format!("({unseen}) ") } else { String::new() };
                if app.search.with(|s| s.as_ref().is_some_and(|s| s.results.is_some())) {
                    return format!("{unread}kubicle :: Search result");
                }
                let open = app.folder.get();
                // Each ancestor's label then the folder's own: INBOX.github reads "Inbox » github".
                let mut path: Vec<&Folder> = folders.iter().filter(|f| kubicle_api::within(&open, &f.name, f.delimiter)).collect();
                path.sort_by_key(|f| f.name.len());
                format!("{unread}kubicle :: {}", path.into_iter().map(folders::label).collect::<Vec<_>>().join(" » "))
            }),
            View::Compose => "kubicle :: Compose".into(),
            View::Settings => "kubicle :: Settings".into(),
        };
        document().set_title(&title);
    });
    // Changes that may take away what holds the focus: leaving Compose or Settings; another message or pane
    // (a folder chosen where the folder column stood in the list's place); the open message drawn again (its
    // notice's Show, HTML or Plain text); a selection change that disables a tool (Select all disables Reply);
    // a page turn that disables a footer button at either end of the list, or while a search's page loads.
    let paging = Memo::new(move |_| app.listed.with(|l| l.paging));
    Effect::new(move |_| {
        paging.track();
        app.view.track();
        app.pane.track();
        app.open.track();
        app.message.track();
        app.selection.track();
        app.everything.track();
        dialog::keep_focus();
    });
    // A dialog belongs to the view it opened over, as Roundcube's to its page: a view replaced under it (Compose by a
    // Forward answered late, which sets the view again) takes it along unanswered.
    Effect::new(move |seen: Option<()>| {
        app.view.track();
        if seen.is_some() {
            app.drop_modal();
        }
    });
    let _keys = window_event_listener(leptos::ev::keydown, move |e| on_key(app, &e));
    let _back = window_event_listener(leptos::ev::popstate, move |_| app.step_back());

    let pane = move || match app.pane.get() {
        Pane::Folders => "folders",
        Pane::List => "list",
        Pane::Reader => "reader",
        Pane::Message => "message",
    };
    // A window opened by "Open in new window" shows the reader alone, and Compose in its place for a reply,
    // forward or edit made there; the dialogs are the main window's.
    let main = move || match app.view.get() {
        View::Compose => view! { <compose::Compose /> }.into_any(),
        _ if app.single().is_some() => view! { <reader::ReaderPane /> }.into_any(),
        View::Mail => view! { <list::ListPane /> <reader::ReaderPane /> }.into_any(),
        View::Settings => view! { <settings::SettingsView /> }.into_any(),
    };
    let folders = app.single().is_none().then(|| view! { <folders::FolderPane /> });
    view! {
        <div class="app" class:single=app.single().is_some() data-pane=pane class:loading=move || !app.ready.get()>
            {folders}
            {main}
        </div>
        {help(app)} {about(app)} {question(app)} {folders::picker(app)} {folders::folder_form(app)}
        {toast(app)} {signed_out(app)}
        {menu::context_menu(app.context_at, move || match app.context() {
            Some(Context::Messages(keys)) => actions::message_menu(app, &keys),
            Some(Context::Folder(name)) => folders::folder_menu(app, &name),
            Some(Context::Drop(keys, to)) => folders::drop_menu(app, keys, to),
            None => Vec::new(),
        })}
    }
}

fn toast(app: App) -> impl IntoView {
    let toast = NodeRef::<leptos::html::Div>::new();
    // A popover, shown again with each notice, so it stands above a modal dialog opened since.
    Effect::new(move || {
        app.toast.track();
        if let Some(t) = toast.get() {
            let _ = t.hide_popover();
            let _ = t.show_popover();
        }
    });
    view! {
        // Not while the session-end notice shows: it alone speaks, as Roundcube's, over the failed request's own toast.
        <Show when=move || app.toast.with(Option::is_some) && !app.signed_out.get()>
            // The same notice given again is a new one with the same words, as Roundcube restarts its timer: its
            // number changes, so the page shows it was given.
            <div class="toast" popover="manual" node_ref=toast role="status" data-seq=move || app.toast.with(|t| t.as_ref().map(|(seq, _)| *seq))>
                {move || app.toast.with(|t| t.as_ref().map(|(_, text)| text.clone()).unwrap_or_default())}
            </div>
        </Show>
    }
}

/// Roundcube's persistent "Your session is invalid or expired.", with a button to sign in again, which comes back
/// to the same Compose, folder and page.
fn signed_out(app: App) -> impl IntoView {
    view! {
        <Show when=move || app.signed_out.get()>
            <div class="toast signed-out" role="alert">
                "Your session is invalid or expired."
                <button class="button primary" on:click=move |_| api::reauthenticate()>"Sign in again"</button>
            </div>
        </Show>
    }
}

fn typing(e: &KeyboardEvent) -> bool {
    let Some(el) = e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return false };
    matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") || el.closest("[contenteditable]").ok().flatten().is_some()
}

/// Enter or Space on a focused button, link or menu entry belongs to that control, not to the list's keys.
fn on_control(e: &KeyboardEvent) -> bool {
    let control = "button, a, summary, [role=button], [role=link], [role=menuitem], [role=menuitemcheckbox], [role=menuitemradio]";
    e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).is_some_and(|el| el.matches(control).unwrap_or(false))
}

/// Where the folder column stands in the list's place (769 to 1200 px, a phone's folder pane), the list's keys would
/// act on messages nobody sees, as Roundcube's list takes keys only while it holds the focus. True for such a key,
/// one that moves in the list or acts on its selection; Up and Down go to the open folder instead, and the folder
/// tree's own keys go on from there (Enter on a folder is its button's, `on_control`).
fn hidden_list_key(app: App, key: &str) -> bool {
    let list_key = [
        "ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "PageUp", "PageDown", "Home", "End", " ", "Enter", "Delete", "Backspace", "a",
        "r", "R", "f", "u", "U", "s", "!", "v",
    ]
    .contains(&key);
    let list = document().query_selector(".list").ok().flatten().map(JsCast::unchecked_into::<web_sys::HtmlElement>);
    if !list_key || app.pane.get_untracked() != Pane::Folders || list.is_some_and(|l| l.offset_parent().is_some()) {
        return false;
    }
    if matches!(key, "ArrowDown" | "ArrowUp") {
        _ = document().query_selector(".folder[tabindex='0']").ok().flatten().map(|f| f.unchecked_into::<web_sys::HtmlElement>().focus());
    }
    true
}

/// Roundcube's message view: a message opened full (a double-click), in a window of its own, or the reader on a phone.
fn message_view(app: App) -> bool {
    let pane = app.pane.get_untracked();
    pane == Pane::Message || app.single().is_some() || (pane == Pane::Reader && state::narrow())
}

/// Roundcube's message view is a page of its own, which these keys scroll. Here the frame scrolls itself with the
/// focus inside it, as its opaque origin keeps it from the app, so the keys only stay off the list.
fn message_key(key: &str) -> bool {
    matches!(key, "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "PageUp" | "PageDown" | " " | "Home" | "End")
}

/// Roundcube's list keys, plus single letters for the common actions (docs/design.md, Keys).
fn on_key(app: App, e: &KeyboardEvent) {
    if !app.ready.get_untracked() {
        return; // nothing to act on before the session
    }
    let key = e.key();
    if app.modal.with_untracked(|m| matches!(m, Some(Modal::Help))) {
        if key != "?" {
            app.dismiss();
        }
        return;
    }
    // Any other open dialog keeps the keys to itself: the list's would act on the mail behind it.
    if document().query_selector("dialog[open]").ok().flatten().is_some() {
        return;
    }
    // Esc closes the innermost thing open, from inside a field too (a focused menu, the search and suggestions take
    // theirs first): a menu, Compose or Settings as their Close does, a selection, then the open message.
    if key == "Escape" {
        let backdrop = document().query_selector(".backdrop").ok().flatten(); // the search options', whose click closes them
        if let Some(backdrop) = backdrop {
            backdrop.unchecked_into::<web_sys::HtmlElement>().click(); // their own Esc needs the focus inside
        } else if let Some(menu) = document().query_selector(".menu:popover-open").ok().flatten() {
            let _ = menu.unchecked_into::<web_sys::HtmlElement>().hide_popover();
        } else if app.single().is_some() && app.view.get_untracked() == View::Mail {
            // A message window has nothing behind its message: Esc neither closes it nor empties the window.
        } else if app.view.get_untracked() != View::Mail || state::narrow() || app.pane.get_untracked() == Pane::Message {
            app.back();
        } else if app.search.with_untracked(Option::is_some) {
            search::reset_to_list(app);
        } else if app.selection.with_untracked(|s| !s.is_empty()) || app.everything.with_untracked(Option::is_some) {
            app.set_selection(Vec::new());
        } else {
            app.close_message();
        }
        e.prevent_default();
        return;
    }
    let ctrl = e.ctrl_key() || e.meta_key();
    // Roundcube's list takes Ctrl with these (Cmd on a Mac); any other Ctrl key is the browser's.
    let with_ctrl = matches!(key.as_str(), "a" | "ArrowDown" | "ArrowUp" | "ArrowLeft" | "ArrowRight" | "Home" | "End" | " ");
    if typing(e) || e.alt_key() || (ctrl && !with_ctrl) || app.view.get_untracked() != View::Mail {
        return;
    }
    if matches!(key.as_str(), "Enter" | " ") && on_control(e) {
        return;
    }
    let shift = e.shift_key();
    if hidden_list_key(app, &key) || (message_view(app) && message_key(&key)) {
        e.prevent_default();
        return;
    }
    // A key runs the toolbar's action under the toolbar's rules: no reply or forward in Drafts.
    let act = |a: Action| {
        let keys = a.targets(app);
        if a.applies(app, &keys) {
            (a.run)(app, &keys, shift);
        }
    };
    match key.as_str() {
        "?" => app.modal.set(Some(Modal::Help)),
        "ArrowDown" => app.step(1, ctrl, shift),
        "ArrowUp" => app.step(-1, ctrl, shift),
        "PageUp" | "PageDown" => list::turn_page(app, key == "PageDown"),
        " " => app.space(ctrl, shift),
        "Home" => app.jump(false, ctrl, shift),
        "End" => app.jump(true, ctrl, shift),
        "ArrowLeft" | "ArrowRight" => app.thread_step(key == "ArrowRight", ctrl),
        // With nothing selected, Enter opens the focused row, as Roundcube's list selects it first.
        "Enter" if app.nothing_selected() => {
            app.space(false, false);
            app.go(Pane::Reader);
        }
        "Enter" => match app.open.get_untracked() {
            Some(open) if app.has_role(&open.0, FolderRole::Drafts) => app.compose(ComposeMode::EditDraft, Some(&open)),
            _ => app.go(Pane::Reader),
        },
        "Delete" | "Backspace" => act(DELETE),
        "a" if ctrl => app.select_page(),
        "a" => act(ARCHIVE),
        "c" => app.compose(ComposeMode::New, None),
        "r" => act(REPLY),
        "R" => act(REPLY_ALL_MAIN),
        "f" => act(FORWARD),
        "u" => act(MARK_UNREAD),
        "U" => act(MARK_READ), // `u`'s undo: moving through the list already marks read
        "s" => act(if MARK_FLAGGED.applies(app, &MARK_FLAGGED.targets(app)) { MARK_FLAGGED } else { MARK_UNFLAGGED }),
        "!" => act(if (JUNK.shown)(app) && JUNK.applies(app, &JUNK.targets(app)) { JUNK } else { NOT_JUNK }),
        "g" if app.single().is_none() => app.modal.set(Some(Modal::Folder(None))), // a reader window has no list to go to
        "v" => {
            let keys = app.targets();
            if !keys.is_empty() {
                app.modal.set(Some(Modal::Folder(Some(keys))));
            }
        }
        "/" => _ = document().get_element_by_id("search").and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok()).map(|i| i.focus()),
        _ => return,
    }
    e.prevent_default();
}

const KEYS: [(&str, &str); 32] = [
    ("Up / Down", "Previous or next message"),
    ("Shift + Up / Down", "Extend the selection"),
    ("Ctrl + Up / Down", "Move the focus without opening"),
    ("Ctrl + Shift + Up / Down", "Add to the selection"),
    ("Home / End", "First or last message; with Shift, select to it"),
    ("Page Up / Page Down", "Previous or next page"),
    ("Space", "Open the focused message"),
    ("Ctrl + Space", "Add or remove the focused message from the selection"),
    ("Right", "Expand the thread, then go to the first reply"),
    ("Left", "Collapse the thread, or go to the message replied to"),
    ("Ctrl + Left", "Go to the thread's first message"),
    ("Enter", "Open the message"),
    ("Delete", "Move to Trash"),
    ("Shift + Delete", "Delete for good"),
    ("Ctrl + A", "Select every message on the page"),
    ("Ctrl + click", "Add to the selection"),
    ("Shift + click", "Select a range"),
    ("Ctrl + Shift + click", "Add a range to the selection"),
    ("Esc", "Close Compose, Settings or the message; clear search or selection"),
    ("c", "Compose"),
    ("r / R", "Reply / Reply all"),
    ("f", "Forward"),
    ("a", "Archive"),
    ("u / Shift + U", "Mark as unread / read"),
    ("s", "Flag or unflag"),
    ("!", "Junk"),
    ("v", "Move to folder"),
    ("g", "Go to folder"),
    ("Up / Down", "In the folders: previous or next folder"),
    ("Left / Right", "In the folders: collapse or expand subfolders"),
    ("/", "Search"),
    ("?", "This help"),
];

fn help(app: App) -> impl IntoView {
    let open = Signal::derive(move || matches!(app.modal.get(), Some(Modal::Help)));
    view! {
        <Dialog open=open close=move || app.dismiss() title="Keyboard shortcuts">
            <table class="keys">
                {KEYS.iter().map(|(k, what)| view! { <tr><td><kbd>{*k}</kbd></td><td>{*what}</td></tr> }).collect_view()}
            </table>
        </Dialog>
    }
}

/// Roundcube's About dialog: the name and version, the licence with where the source is, and its "Get support"
/// button, which under Mailu opens Mailu's own pages (password, auto-reply, forwarding).
fn about(app: App) -> impl IntoView {
    let open = Signal::derive(move || matches!(app.modal.get(), Some(Modal::About)));
    let support = move |_| actions::new_tab(&app.admin_url());
    view! {
        <Dialog open=open close=move || app.dismiss() title="About" class="narrow">
            <div class="dialog-body about">
                <h3>{concat!("kubicle ", env!("CARGO_PKG_VERSION"))}</h3>
                <p>"A webmail for Mailu."</p>
                <p>
                    "License: AGPL-3.0. "
                    {app.source_url().map(|url| view! { <a href=url target="_blank" rel="noopener noreferrer">"Source code"</a> })}
                </p>
            </div>
            <footer class="dialog-buttons">
                <button type="button" class="button primary" on:click=support>{icon(icons::HELP)} "Get support"</button>
                <button type="button" class="button" on:click=move |_| app.dismiss()>"Close"</button>
            </footer>
        </Dialog>
    }
}

/// The question `App::ask` puts up, kubicle's own `confirm` and `prompt`.
fn question(app: App) -> impl IntoView {
    let ask = Memo::new(move |_| if let Some(Modal::Ask(ask)) = app.modal.get() { Some(ask) } else { None });
    let text = RwSignal::new(String::new());
    // The field's text, or the first enabled choice.
    Effect::new(move |_| {
        let initial = ask.with(|a| {
            let a = a.as_ref()?;
            a.field.clone().or_else(|| a.choices.iter().find(|c| c.2).map(|c| c.0.to_owned()))
        });
        text.set(initial.unwrap_or_default());
    });
    let choices = move || ask.with(|a| a.as_ref().map(|a| a.choices.clone()).unwrap_or_default());
    let title = move || ask.with(|a| a.as_ref().map_or("", |a| a.title)).to_owned();
    let yes = move || app.reply(Reply::Yes(text.get_untracked()));
    view! {
        <Dialog open=Signal::derive(move || ask.with(Option::is_some)) close=move || app.dismiss() title=title class="narrow">
            <form class="ask" on:submit=move |e| { e.prevent_default(); yes(); }>
                <p>{move || ask.with(|a| a.as_ref().map(|a| a.question.clone()))}</p>
                <Show when=move || ask.with(|a| a.as_ref().is_some_and(|a| a.field.is_some()))>
                    <input class="input" type="text" bind:value=text />
                </Show>
                <ul class="choices" role="radiogroup">
                    {move || choices().into_iter().map(|(value, label, enabled)| view! {
                        <li><label class:muted=!enabled>
                            <input type="radio" name="ask-choice" value=value prop:checked=move || text.get() == value prop:disabled=!enabled
                                on:change=move |_| text.set(value.to_owned()) />
                            {label}
                        </label></li>
                    }).collect_view()}
                </ul>
                <footer class="dialog-buttons">
                    <button type="submit" class="button primary" class:danger=move || ask.with(|a| a.as_ref().is_some_and(|a| a.danger))>
                        {move || ask.with(|a| a.as_ref().map_or("OK", |a| a.yes))}
                    </button>
                    {move || ask.with(|a| a.as_ref().and_then(|a| a.alt)).map(|alt| view! {
                        <button type="button" class="button" on:click=move |_| app.reply(Reply::Alt)>{alt}</button>
                    })}
                    {move || ask.with(|a| a.as_ref().and_then(|a| a.no)).map(|no| view! {
                        <button type="button" class="button" on:click=move |_| app.dismiss()>{no}</button>
                    })}
                </footer>
            </form>
        </Dialog>
    }
}
