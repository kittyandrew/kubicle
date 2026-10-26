//! What can be done to messages. The reader's toolbar, its menus and the list's context menu all draw these, so
//! an action's label, shortcut and rules live in one place.

use kubicle_api::{ComposeMode, Flag, FolderRole, ZipMode};
use kubicle_browser::api::enc;
use kubicle_ui::{icons, menu::MenuItem};
use leptos::prelude::*;

use crate::{folders, state::App, state::Key, state::Modal, state::narrow};

#[derive(Clone, Copy)]
pub struct Action {
    pub glyph: &'static str,
    pub label: fn(App) -> &'static str,
    /// Whether the toolbar shows it here: Edit only in Drafts.
    pub shown: fn(App) -> bool,
    /// Takes exactly one message; the others take every target.
    pub single: bool,
    /// Whether it would do anything for these targets, beyond their count: Reply list needs a List-Post address,
    /// a mark a message it changes, a reply or forward a message outside Drafts, as in Roundcube.
    pub useful: fn(App, &[Key]) -> bool,
    pub key: Option<&'static str>,
    /// The bool is whether Shift was held.
    pub run: fn(App, &[Key], bool),
}

const BASE: Action =
    Action { glyph: icons::MAIL, label: |_| "", shown: |_| true, single: false, useful: |_, _| true, key: None, run: |_, _, _| {} };

fn compose(app: App, keys: &[Key], mode: ComposeMode) { app.compose(mode, keys.first()); }

/// What the Forward button and `f` do: Roundcube's "Forwarding mode", inline unless the user chose attachment.
fn forward_mode(app: App) -> ComposeMode {
    if app.settings.with_untracked(|s| s.forward_attachment) { ComposeMode::ForwardAttachment } else { ComposeMode::Forward }
}

/// Several selected messages, or Select all, always go as attachments, as Roundcube's forward of a selection; one
/// selected row is that message alone, a collapsed thread's replies left out, as Roundcube's `env.uid` is.
fn forward(app: App, keys: &[Key], mode: ComposeMode) {
    if app.selection.with_untracked(|s| s.len() > 1) || app.everything.with_untracked(Option::is_some) {
        app.forward_attachments(keys);
    } else {
        compose(app, keys, mode);
    }
}

fn in_role(app: App, role: FolderRole) -> bool {
    app.folders.track(); // a label drawn before the session must change once the roles arrive
    app.folder.with(|f| app.has_role(f, role))
}

fn outside_drafts(app: App, keys: &[Key]) -> bool { keys.iter().all(|(folder, _)| !app.has_role(folder, FolderRole::Drafts)) }

/// Whether some target would change. Under Select all the flags of the other pages are not here, so every mark
/// applies.
fn changes(app: App, keys: &[Key], flag: Flag, set: bool) -> bool {
    app.everything.with_untracked(Option::is_some) || keys.iter().any(|k| app.row_flags(k).is_some_and(|f| f.has(flag) != set))
}

/// Roundcube's `mark_message`: only the messages the mark changes are sent.
fn mark(app: App, keys: &[Key], flag: Flag, set: bool) {
    let changing: Vec<Key> = keys.iter().filter(|k| app.row_flags(k).is_none_or(|f| f.has(flag) != set)).cloned().collect();
    app.set_flag(&changing, flag, set);
}

pub const EDIT: Action = Action {
    glyph: icons::COMPOSE,
    label: |_| "Edit",
    shown: |app| in_role(app, FolderRole::Drafts),
    single: true,
    key: Some("Enter"),
    run: |app, keys, _| compose(app, keys, ComposeMode::EditDraft),
    ..BASE
};
pub const REPLY: Action = Action {
    glyph: icons::REPLY,
    label: |_| "Reply",
    single: true,
    useful: outside_drafts,
    key: Some("r"),
    run: |app, keys, _| compose(app, keys, ComposeMode::Reply),
    ..BASE
};
/// The menu's Reply all, always to everyone.
pub const REPLY_ALL: Action = Action {
    glyph: icons::REPLY_ALL,
    label: |_| "Reply all",
    key: None,
    run: |app, keys, _| compose(app, keys, ComposeMode::ReplyAll),
    ..REPLY
};
/// The Reply all button and `R`: Reply list on list mail under "Default action of [Reply all] button", whose
/// default is Mailu's `reply_all_mode = 1`.
pub const REPLY_ALL_MAIN: Action = Action {
    key: Some("R"),
    run: |app, keys, _| {
        let list = app.settings.with_untracked(|s| s.reply_all_list) && (REPLY_LIST.useful)(app, keys);
        compose(app, keys, if list { ComposeMode::ReplyList } else { ComposeMode::ReplyAll });
    },
    ..REPLY_ALL
};
pub const REPLY_LIST: Action = Action {
    glyph: icons::REPLY_ALL,
    label: |_| "Reply list",
    // The row says so once the server read List-Post, the body as soon as it is here, as Roundcube's list does.
    useful: |app, keys| {
        outside_drafts(app, keys)
            && keys.first().is_some_and(|k| app.row(k).is_some_and(|r| r.list_post) || app.body_says(k, |m| m.list_post))
    },
    key: None,
    run: |app, keys, _| compose(app, keys, ComposeMode::ReplyList),
    ..REPLY
};
/// Forward and Forward as attachment take a selection, as Roundcube's; Forward inline only one message.
pub const FORWARD: Action = Action {
    glyph: icons::FORWARD,
    label: |_| "Forward",
    single: false,
    key: Some("f"),
    run: |app, keys, _| forward(app, keys, forward_mode(app)),
    ..REPLY
};
pub const FORWARD_INLINE: Action =
    Action { label: |_| "Forward inline", single: true, run: |app, keys, _| compose(app, keys, ComposeMode::Forward), ..FORWARD };
pub const FORWARD_ATTACHMENT: Action = Action {
    glyph: icons::ATTACH,
    label: |_| "Forward as attachment",
    single: false,
    key: None,
    run: |app, keys, _| forward(app, keys, ComposeMode::ForwardAttachment),
    ..REPLY
};
pub const REDIRECT: Action = Action {
    glyph: icons::SENT,
    label: |_| "Resend (bounce)",
    key: None,
    run: |app, keys, _| keys.first().iter().for_each(|k| app.modal.set(Some(Modal::Redirect((*k).clone())))),
    ..REPLY
};
pub const DELETE: Action =
    Action { glyph: icons::TRASH, label: |_| "Delete", key: Some("Del"), run: |app, keys, shift| app.delete(keys, shift), ..BASE };
/// Off for messages already in Archive or below it, as Roundcube's plugin (`rcmail_is_archive`).
pub const ARCHIVE: Action = Action {
    glyph: icons::ARCHIVE,
    label: |_| "Archive",
    useful: |app, keys| {
        app.role_folder(FolderRole::Archive).is_some_and(|archive| keys.iter().any(|(folder, _)| !app.in_tree(folder, &archive)))
    },
    key: Some("a"),
    run: |app, keys, _| app.archive(keys),
    ..BASE
};
/// markasjunk's two buttons: Junk outside Junk, Not junk in it, and both over results from several folders, each
/// for its own side of them (`markasjunk_toggle_button`).
pub const JUNK: Action = Action {
    glyph: icons::JUNK,
    label: |_| "Junk",
    shown: |app| !in_role(app, FolderRole::Junk) || crate::search::multi_folder(app),
    useful: |app, keys| in_junk(app, keys, false),
    key: Some("!"),
    run: |app, keys, _| app.report(keys, true),
    ..BASE
};
pub const NOT_JUNK: Action = Action {
    glyph: icons::JUNK,
    label: |_| "Not junk",
    shown: |app| in_role(app, FolderRole::Junk) || crate::search::multi_folder(app),
    useful: |app, keys| in_junk(app, keys, true),
    key: Some("!"),
    run: |app, keys, _| app.report(keys, false),
    ..BASE
};

/// Whether some target is in the Junk folder (`inside`), or outside it.
fn in_junk(app: App, keys: &[Key], inside: bool) -> bool {
    app.role_folder(FolderRole::Junk).is_some_and(|junk| keys.iter().any(|(folder, _)| (*folder == junk) == inside))
}
pub const MARK_READ: Action = Action {
    glyph: icons::MAIL_OPEN,
    label: |_| "As read",
    key: Some("Shift+U"),
    useful: |app, keys| changes(app, keys, Flag::Seen, true),
    run: |app, keys, _| mark(app, keys, Flag::Seen, true),
    ..BASE
};
pub const MARK_UNREAD: Action = Action {
    glyph: icons::MAIL,
    label: |_| "As unread",
    key: Some("u"),
    useful: |app, keys| changes(app, keys, Flag::Seen, false),
    run: |app, keys, _| mark(app, keys, Flag::Seen, false),
    ..BASE
};
pub const MARK_FLAGGED: Action = Action {
    glyph: icons::FLAG,
    label: |_| "As flagged",
    key: Some("s"),
    useful: |app, keys| changes(app, keys, Flag::Flagged, true),
    run: |app, keys, _| mark(app, keys, Flag::Flagged, true),
    ..BASE
};
pub const MARK_UNFLAGGED: Action = Action {
    glyph: icons::UNFLAG,
    label: |_| "As unflagged",
    key: Some("s"),
    useful: |app, keys| changes(app, keys, Flag::Flagged, false),
    run: |app, keys, _| mark(app, keys, Flag::Flagged, false),
    ..BASE
};
pub const PRINT: Action = Action {
    glyph: icons::PRINT,
    label: |_| "Print this message",
    single: true,
    run: |app, keys, _| keys.first().iter().for_each(|k| print(app, k)),
    ..BASE
};
pub const DOWNLOAD: Action = Action {
    glyph: icons::DOWNLOAD,
    label: |_| "Source (.eml)",
    single: true,
    run: |_, keys, _| keys.first().into_iter().for_each(download),
    ..BASE
};
pub const EDIT_AS_NEW: Action = Action {
    glyph: icons::COMPOSE,
    label: |_| "Edit as new",
    single: true,
    run: |app, keys, _| compose(app, keys, ComposeMode::EditAsNew),
    ..BASE
};
pub const SHOW_SOURCE: Action = Action {
    glyph: icons::SOURCE,
    label: |_| "Show source",
    single: true,
    run: |_, keys, _| keys.first().iter().for_each(|k| open_tab(k, "source")),
    ..BASE
};
/// Not inside a message window, nor on a phone, as Elastic hides and disables it there.
pub const OPEN_WINDOW: Action = Action {
    glyph: icons::OPEN,
    label: |_| "Open in new window",
    shown: |app| app.single().is_none() && !narrow(),
    single: true,
    run: |_, keys, _| keys.first().into_iter().for_each(open_window),
    ..BASE
};

/// The menus beside Reply all and Forward, and Mark's, in Roundcube's order.
pub const REPLY_ALL_MENU: [Action; 2] = [REPLY_ALL, REPLY_LIST];
pub const FORWARD_MENU: [Action; 3] = [FORWARD_INLINE, FORWARD_ATTACHMENT, REDIRECT];
pub const MARK_MENU: [Action; 4] = [MARK_READ, MARK_UNREAD, MARK_FLAGGED, MARK_UNFLAGGED];

impl Action {
    /// What it takes, tracked: the targets, or for a single one the open message unless several are selected. The
    /// toolbar and the keys both ask here, so a key does what its button would.
    pub fn targets(self, app: App) -> Vec<Key> {
        app.list.track();
        app.search.track();
        let open = app.open.get();
        match (self.single, crate::reader::selected(app)) {
            (false, _) => app.targets(),
            (true, 0 | 1) => open.into_iter().collect(),
            (true, _) => Vec::new(),
        }
    }

    pub fn applies(self, app: App, keys: &[Key]) -> bool {
        (if self.single { keys.len() == 1 } else { !keys.is_empty() }) && (self.useful)(app, keys)
    }

    /// This action as a menu entry for `keys`, or none where the toolbar hides it.
    pub fn shown_item(self, app: App, keys: &[Key]) -> Option<MenuItem> { if (self.shown)(app) { Some(self.item(app, keys)) } else { None } }

    /// This action as a menu entry for `keys`.
    pub fn item(self, app: App, keys: &[Key]) -> MenuItem {
        let (enabled, targets) = (self.applies(app, keys), keys.to_vec());
        let item = MenuItem::action((self.label)(app), move || (self.run)(app, &targets, false)).glyph(self.glyph).enabled(enabled);
        if let Some(key) = self.key { item.key(key) } else { item }
    }
}

pub fn items(app: App, keys: &[Key], actions: &[Action]) -> Vec<MenuItem> { actions.iter().map(|a| a.item(app, keys)).collect() }

/// The folders `keys` can go to, as menu entries that hand the chosen one to `to`.
fn folder_items(app: App, keys: &[Key], to: fn(App, &[Key], &str)) -> Vec<MenuItem> {
    let nodes = app.folders.with_untracked(|f| folders::tree(f, false));
    let entry = |n: folders::Node| {
        let (folder, keys) = (n.folder.name.clone(), keys.to_vec());
        let elsewhere = keys.iter().any(|(f, _)| *f != folder); // the messages' own folder would take none of them
        MenuItem::action(n.label, move || to(app, &keys, &folder)).enabled(elsewhere).glyph(icons::folder(&n.folder)).depth(n.depth)
    };
    nodes.into_iter().filter(|n| n.folder.selectable).map(entry).collect()
}

/// Move to... and Copy to...: the folder chooser in the menu's place.
pub fn move_item(app: App, keys: &[Key]) -> MenuItem {
    MenuItem::submenu("Move to...", folder_items(app, keys, App::move_to)).glyph(icons::MOVE).key("v").enabled(!keys.is_empty())
}

pub fn copy_item(app: App, keys: &[Key]) -> MenuItem {
    MenuItem::submenu("Copy to...", folder_items(app, keys, App::copy_to)).glyph(icons::COPY).enabled(!keys.is_empty())
}

/// zipdownload's "Download..." menu: the source of one message, or an archive of several in Mbox or Maildir form.
pub fn download_item(app: App, open: &[Key], targets: &[Key]) -> MenuItem {
    let several = targets.len() > 1 || app.everything.with_untracked(Option::is_some);
    let zip = |label: &str, mode: ZipMode| {
        let keys = targets.to_vec();
        MenuItem::action(label, move || app.download(&keys, mode)).glyph(icons::DOWNLOAD).enabled(several)
    };
    let items = vec![DOWNLOAD.item(app, open), zip("Mbox format (.zip)", ZipMode::Mbox), zip("Maildir format (.zip)", ZipMode::Maildir)];
    MenuItem::submenu("Download...", items).glyph(icons::DOWNLOAD).enabled(!targets.is_empty())
}

pub fn mark_item(app: App, keys: &[Key]) -> MenuItem {
    MenuItem::submenu("Mark", items(app, keys, &MARK_MENU)).glyph(icons::TAG).enabled(!keys.is_empty())
}

/// Opens `url` in a tab of its own, which `noopener` keeps from reaching back to this page.
pub fn new_tab(url: &str) { let _ = window().open_with_url_and_target_and_features(url, "_blank", "noopener"); }

/// Opens one of the server's views of a message (`api/source`) in a tab of its own.
pub fn open_tab((folder, uid): &Key, route: &str) { new_tab(&format!("api/{route}?folder={}&uid={uid}", enc(folder))); }

/// The print page in a tab of its own, dated in the user's zone. The message turns read whatever the mark-read
/// setting, as Roundcube's Print marks it, except in the "Open in new window" window, as in Roundcube's full view.
fn print(app: App, key: &Key) {
    let (folder, uid) = key;
    new_tab(&format!("api/print?folder={}&uid={uid}&tz={}", enc(folder), enc(&kubicle_browser::time::zone())));
    if app.single().is_none() && app.row_flags(key).is_none_or(|f| !f.has(Flag::Seen)) {
        app.mark_seen(key.clone()); // never `mark`: under Select all that would stand for the whole folder
    }
}

/// Download... > Source (.eml). The answer carries `Content-Disposition: attachment`, so the page stays where it is.
fn download((folder, uid): &Key) { let _ = window().location().set_href(&format!("api/download?folder={}&uid={uid}", enc(folder))); }

/// The message alone in a new tab, from the `#open=UID/FOLDER` address the app reads at start.
pub fn open_window((folder, uid): &Key) { new_tab(&format!("#open={uid}/{}", enc(folder))); }

/// Roundcube's More menu below the buttons that moved into it: `open` is the open message, `targets` what
/// Move and Copy take.
pub fn more_items(app: App, open: &[Key], targets: &[Key]) -> Vec<MenuItem> {
    let import = MenuItem::action("Import", move || app.modal.set(Some(Modal::Import))).glyph(icons::UPLOAD);
    let mut items = vec![PRINT.item(app, open), import, download_item(app, open, targets), EDIT_AS_NEW.item(app, open)];
    items.extend([SHOW_SOURCE.item(app, open), move_item(app, targets), copy_item(app, targets)]);
    items.extend(OPEN_WINDOW.shown_item(app, open));
    items
}

/// The list's context menu for `keys`.
pub fn message_menu(app: App, keys: &[Key]) -> Vec<MenuItem> {
    let mut menu = Vec::new();
    if (OPEN_WINDOW.shown)(app) {
        menu.extend([OPEN_WINDOW.item(app, keys), MenuItem::separator()]);
    }
    menu.extend(EDIT.shown_item(app, keys));
    menu.extend(items(app, keys, &[REPLY, REPLY_ALL, REPLY_LIST, FORWARD, FORWARD_ATTACHMENT, REDIRECT]));
    menu.extend([MenuItem::separator(), mark_item(app, keys), MenuItem::separator(), ARCHIVE.item(app, keys)]);
    menu.extend([move_item(app, keys), copy_item(app, keys)]);
    menu.extend([JUNK, NOT_JUNK].into_iter().filter_map(|a| a.shown_item(app, keys)));
    menu.extend([DELETE.item(app, keys), MenuItem::separator(), PRINT.item(app, keys)]);
    let open = if keys.len() == 1 { keys.to_vec() } else { Vec::new() };
    menu.extend([download_item(app, &open, keys), EDIT_AS_NEW.item(app, keys), SHOW_SOURCE.item(app, keys)]);
    menu
}
