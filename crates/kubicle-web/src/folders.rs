//! The folder pane: subscribed folders as a tree, special folders first, unread counts, and drop targets for
//! dragged messages.

use std::{collections::HashMap, collections::HashSet};

use kubicle_api::{Change, ComposeMode, Folder, FolderRole, Settings, ThemeMode};
use kubicle_browser::text;
use kubicle_ui::{dialog::Dialog, dialog::DialogForm, dialog::keep_focus, icons, icons::icon, menu::Dropdown, menu::MenuItem};
use leptos::{ev::DragEvent, ev::KeyboardEvent, ev::MouseEvent, prelude::*};
use wasm_bindgen::JsCast;

use crate::{state::App, state::Ask, state::Context, state::Key, state::Modal, state::View};

#[derive(Clone, PartialEq)]
pub struct Node {
    pub folder: Folder,
    pub label: String,
    pub depth: usize,
}

/// What the folder pane, the title and the list header call a folder: a special folder by its role, as Roundcube
/// names them while `show_real_foldernames` is off, else the last part of its name.
pub fn label(f: &Folder) -> &str {
    match f.role() {
        FolderRole::Inbox => "Inbox",
        FolderRole::Drafts => "Drafts",
        FolderRole::Sent => "Sent",
        FolderRole::Junk => "Junk",
        FolderRole::Trash => "Trash",
        FolderRole::Archive => "Archive",
        FolderRole::Other => f.delimiter.and_then(|d| f.name.rsplit_once(d)).map_or(&f.name, |(_, last)| last),
    }
}

/// Folders in display order: each parent before its children, siblings by role and then name. With `subscribed_only`,
/// the folder pane's: subscribed folders, and above them each parent that is not, as Roundcube's virtual folders.
pub fn tree(folders: &[Folder], subscribed_only: bool) -> Vec<Node> {
    let shown = if subscribed_only { with_parents(folders) } else { folders.to_vec() };
    // A folder with several roles sorts by its first, as Roundcube's `sort_folder_list` places it.
    let roles: HashMap<&str, FolderRole> = shown.iter().filter_map(|f| Some((f.name.as_str(), *f.roles.first()?))).collect();
    let mut nodes: Vec<(Vec<(FolderRole, String)>, Node)> = Vec::with_capacity(shown.len());
    for f in &shown {
        let parts: Vec<&str> = f.delimiter.map_or_else(|| vec![f.name.as_str()], |d| f.name.split(d).collect());
        let mut sort = Vec::with_capacity(parts.len());
        let mut prefix = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                prefix.push(f.delimiter.unwrap_or('/'));
            }
            prefix.push_str(part);
            sort.push((roles.get(prefix.as_str()).copied().unwrap_or(FolderRole::Other), part.to_lowercase()));
        }
        nodes.push((sort, Node { folder: f.clone(), label: label(f).to_owned(), depth: parts.len() - 1 }));
    }
    nodes.sort_by(|a, b| a.0.cmp(&b.0)); // level by level: a folder's children follow it, before its next sibling
    nodes.into_iter().map(|(_, n)| n).collect()
}

/// The subscribed folders, and each parent of one that is unsubscribed or missing on the server, as a folder that
/// cannot be opened and counts nothing: Roundcube's virtual folder (`rcmail_action::folder_tree_html`), which keeps a
/// subfolder under its parent and makes a `\Noselect` parent need no subscription.
fn with_parents(folders: &[Folder]) -> Vec<Folder> {
    let mut shown: Vec<Folder> = folders.iter().filter(|f| f.subscribed).cloned().collect();
    let mut names: HashSet<String> = shown.iter().map(|f| f.name.clone()).collect();
    for f in folders.iter().filter(|f| f.subscribed) {
        let Some(d) = f.delimiter else { continue };
        let mut at = f.name.as_str();
        while let Some((parent, _)) = at.rsplit_once(d) {
            if names.insert(parent.to_owned()) {
                let roles = folders.iter().find(|p| p.name == parent).map(|p| p.roles.clone()).unwrap_or_default();
                shown.push(Folder { name: parent.to_owned(), delimiter: Some(d), roles, uid_next: 1, ..Folder::default() });
            }
            at = parent;
        }
    }
    shown
}

/// The pane's rows: the subscribed tree less the subfolders of collapsed folders, but for those a drag opened. A
/// folder with subfolders carries whether it is expanded.
fn pane_rows(folders: &[Folder], settings: &Settings, drag_open: &HashSet<String>) -> Vec<(Node, Option<bool>)> {
    let nodes = tree(folders, true);
    let below = |n: &Node| n.folder.delimiter.map(|d| format!("{}{d}", n.folder.name));
    let mut hidden: Option<String> = None;
    let mut rows = Vec::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        if hidden.as_ref().is_some_and(|h| node.folder.name.starts_with(h)) {
            continue;
        }
        let prefix = below(node);
        let parent = prefix.as_ref().is_some_and(|p| nodes.get(i + 1).is_some_and(|next| next.folder.name.starts_with(p)));
        let collapsed =
            parent && settings.folders.get(&node.folder.name).is_some_and(|p| p.collapsed) && !drag_open.contains(&node.folder.name);
        hidden = prefix.filter(|_| collapsed);
        rows.push((node.clone(), parent.then_some(!collapsed)));
    }
    rows
}

#[component]
pub fn FolderPane() -> impl IntoView {
    let app = expect_context::<App>();
    let signing_out = RwSignal::new(false);
    let rows = Memo::new(move |_| app.folders.with(|f| app.settings.with(|s| app.drag_open.with(|d| pane_rows(f, s, d)))));
    // The tree is one Tab stop, as Roundcube's: the open folder's row, else the first.
    let stop = Memo::new(move |_| {
        let open = app.folder.get();
        rows.with(|r| if r.iter().any(|(n, _)| n.folder.name == open) { Some(open) } else { r.first().map(|(n, _)| n.folder.name.clone()) })
    });
    // The scheme on screen: the pinned one, or the system's under Auto. The switch pins the other one.
    let dark = move || match app.settings.with(|s| s.theme_mode) {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::Auto => window().match_media("(prefers-color-scheme: dark)").ok().flatten().is_some_and(|q| q.matches()),
    };
    let mode = move || if dark() { ("Light", "Light mode") } else { ("Dark", "Dark mode") };
    let user = move || {
        app.ready.track();
        app.user()
    };

    view! {
        <nav class="folders" aria-label="Folders">
            <header class="bar">
                <span class="account" title=user>{user}</span>
                <span class="bar-end">
                    <Dropdown label="Folder actions" glyph=icons::MORE_VERTICAL align_right=true items=move || folder_actions(app) />
                </span>
            </header>
            <div class="folders-top">
                <button class="button primary compose-button" on:click=move |_| app.compose(ComposeMode::New, None)>
                    {icon(icons::COMPOSE)} "Compose"
                </button>
            </div>
            <ul class="folder-list" on:keydown=move |e| tree_keys(app, &e)>
                <For each=move || rows.get()
                    key=|(n, expanded)| (n.folder.name.clone(), n.folder.selectable, n.label.clone(), n.depth, *expanded)
                    children=move |(n, expanded)| folder_row(app, n, expanded, stop) />
            </ul>
            {quota_bar(app)}
            <div class="folders-bottom">
                <button class="tool" class:on=move || app.view.get() == View::Settings
                    aria-pressed=move || if app.view.get() == View::Settings { "true" } else { "false" }
                    title=move || if app.view.get() == View::Settings { "Close settings" } else { "Settings" }
                    on:click=move |_| if app.view.get_untracked() == View::Settings { app.view.set(View::Mail) } else { app.open_settings(None) }
                >
                    {icon(icons::SETTINGS)} <span class="tool-label">"Settings"</span>
                </button>
                <button class="tool" title="Keyboard shortcuts" aria-label="Keyboard shortcuts" on:click=move |_| app.modal.set(Some(Modal::Help))>
                    {icon(icons::HELP)} <span class="tool-label">"Keys"</span>
                </button>
                <button class="tool" title=move || mode().1 aria-label=move || mode().1 on:click=move |_| {
                        // Read before the update: reading the settings while they are being written panics.
                        let next = if dark() { ThemeMode::Light } else { ThemeMode::Dark };
                        app.update_settings(move |s| s.theme_mode = next);
                    }>
                    {move || icon(if dark() { icons::SUN } else { icons::MOON })} <span class="tool-label">{move || mode().0}</span>
                </button>
                <button class="tool logout" title="Sign out" prop:disabled=signing_out on:click=move |_| logout(app, signing_out)>
                    {icon(icons::LOGOUT)} <span class="tool-label">"Sign out"</span>
                </button>
            </div>
        </nav>
    }
}

/// Roundcube's quota widget in the folder column's footer (Elastic's `quota-widget`): a disk, a bar and its percent,
/// the use in the tooltip, red from 90%. Shown only for an account with a limit: an unlimited one's empty bar says
/// nothing.
fn quota_bar(app: App) -> impl IntoView {
    move || {
        let q = app.quota.get()?; // the server sends none for an account without a limit (`kubicle_api::Quota`)
        let percent = (q.used.saturating_mul(200).saturating_add(q.limit) / q.limit.saturating_mul(2)).min(100); // rounded
        let width = if (1..10).contains(&percent) { 10 } else { percent }; // as Elastic: a sliver would not show
        let title = format!("Disk usage: {} / {} ({percent}%)", kubicle_api::size(q.used), kubicle_api::size(q.limit));
        Some(view! {
            <div class="quota" class:warning=(percent >= 90) title=title.clone() aria-label=title>
                {icon(icons::DISK)}
                <span class="quota-bar"><span class="quota-value" style=format!("width: {width}%")></span></span>
                <span class="quota-count">{format!("{percent}%")}</span>
            </div>
        })
    }
}

fn folder_row(app: App, n: Node, expanded: Option<bool>, stop: Memo<Option<String>>) -> impl IntoView {
    let name = n.folder.name.clone();
    let (click_name, drop_name, over_name, toggle_name, menu_name) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    let (recent_name, hover_name, stop_name, count_name) = (name.clone(), name.clone(), name.clone(), name.clone());
    // Read here, not keyed: a changed count must not replace the button, which may hold the keyboard focus.
    let counts = Memo::new(move |_| {
        let own = |f: &&Folder| f.name == count_name && f.subscribed; // a virtual parent counts nothing
        app.folders.with(|f| f.iter().find(own).map_or((0, 0), |f| (f.unseen, f.total)))
    });
    let unseen = Memo::new(move |_| counts.get().0);
    // Roundcube's `set_unread_count_display`: a collapsed folder is bold while a subscribed folder below it has
    // unread mail; its badge stays its own count.
    let (below_name, delimiter) = (name.clone(), n.folder.delimiter);
    let unread_below = Memo::new(move |_| {
        let hidden =
            |f: &Folder| f.subscribed && f.unseen > 0 && f.name != below_name && kubicle_api::within(&f.name, &below_name, delimiter);
        expanded == Some(false) && app.folders.with(|f| f.iter().any(hidden))
    });
    // Elastic hollows Trash's icon when Trash itself holds nothing (`.trash.empty`), even where it is Archive too.
    let (trash, glyph) = (n.folder.role() == FolderRole::Trash, icons::folder(&n.folder));
    let glyph = move || if trash && counts.get().1 == 0 { icons::TRASH_EMPTY } else { glyph };
    let active = Signal::derive(move || app.folder.with(|f| *f == name) && app.view.with(|v| *v == View::Mail));
    let over = RwSignal::new(false);
    let accepts = move || n.folder.selectable && app.folder.with_untracked(|f| *f != over_name);
    let collapsed = expanded == Some(false);
    // Roundcube's tree `autoexpand`: a collapsed folder a message drag rests on for a second opens.
    let hover = move |_: DragEvent| {
        if app.dragging() {
            app.hover_folder(collapsed.then(|| hover_name.clone()));
        }
    };
    let leave = move |e: DragEvent| {
        let node = |t: Option<web_sys::EventTarget>| t.and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        let (li, to) = (node(e.current_target()), node(e.related_target()));
        if !li.is_some_and(|li| li.contains(to.as_ref())) {
            app.hover_folder(None);
        }
    };
    view! {
        <li style=format!("--depth: {}", n.depth) data-folder=n.folder.name on:dragenter=hover.clone() on:dragover=hover on:dragleave=leave
            on:drop=move |_| app.hover_folder(None)>
            <button class="folder" class:active=active class:drop=move || over.get() class:noselect=!n.folder.selectable
                class:unread=move || { unseen.get() > 0 || unread_below.get() }
                class:recent=move || app.recent.with(|r| r.contains(&recent_name))
                tabindex=move || if stop.with(|s| s.as_ref() == Some(&stop_name)) { "0" } else { "-1" }
                aria-current=move || if active.get() { "page" } else { "false" }
                on:click=move |_| if n.folder.selectable { app.select_folder(click_name.clone()) }
                on:dragover=move |e: DragEvent| if accepts() { e.prevent_default(); over.set(true); }
                on:dragleave=move |_| over.set(false)
                on:contextmenu=move |e: MouseEvent| {
                    e.prevent_default();
                    app.open_context((e.client_x(), e.client_y()), Context::Folder(menu_name.clone()));
                }
                on:drop=move |e: DragEvent| {
                    e.prevent_default();
                    over.set(false);
                    let keys = app.take_drag();
                    if e.shift_key() && !e.ctrl_key() && !e.meta_key() { // Roundcube's `drag_menu`: Shift alone
                        app.open_context((e.client_x(), e.client_y()), Context::Drop(keys, drop_name.clone()));
                    } else {
                        app.move_to(&keys, &drop_name);
                    }
                }
            >
                {move || icon(glyph())} <span class="folder-name">{n.label}</span>
                {move || (unseen.get() > 0).then(|| view! { <span class="badge">{text::count(unseen.get())}</span> })}
            </button>
            {expanded.map(|expanded| view! {
                <button class="twisty folder-twisty" class:expanded=expanded
                    tabindex="-1" // Left and Right do this from the folder
                    aria-label=if expanded { "Collapse subfolders" } else { "Expand subfolders" }
                    on:click=move |_| {
                        app.drag_open.update(|d| _ = d.remove(&toggle_name)); // a drag's opening ends with the user's own
                        app.edit_prefs(&toggle_name, move |p| p.collapsed = expanded);
                        if expanded { open_collapsed(app, &toggle_name); }
                    }>{icon(icons::CHEVRON)}</button>
            })}
        </li>
    }
}

/// Roundcube's tree keys on a focused folder: Up and Down move the focus, Left and Right collapse and expand, with
/// Shift the folder and every folder below it (`treelist.js`, `toggle(id, recursive)`), when the folder's own state
/// changes.
fn tree_keys(app: App, e: &KeyboardEvent) {
    let key = e.key();
    let arrow = matches!(key.as_str(), "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight");
    let row = document().active_element().and_then(|el| el.closest(".folder-list > li").ok().flatten());
    let Some(row) = row.filter(|_| arrow && !e.alt_key() && !e.ctrl_key() && !e.meta_key()) else { return };
    let folder_of = |li: Option<web_sys::Element>| {
        li.and_then(|li| li.query_selector(".folder").ok().flatten()).map(JsCast::unchecked_into::<web_sys::HtmlElement>)
    };
    match key.as_str() {
        "ArrowUp" => _ = folder_of(row.previous_element_sibling()).map(|f| f.focus()),
        "ArrowDown" => _ = folder_of(row.next_element_sibling()).map(|f| f.focus()),
        _ => {
            let twisty = row.query_selector(".folder-twisty").ok().flatten();
            let changes = |t: &web_sys::Element| t.class_name().split(' ').any(|c| c == "expanded") == (key == "ArrowLeft");
            if let Some(twisty) = twisty.filter(changes) {
                // The row is drawn again with its new state, so the focus goes to the one now in its place.
                let at = std::iter::successors(row.previous_element_sibling(), web_sys::Element::previous_element_sibling).count();
                match row.get_attribute("data-folder").filter(|_| e.shift_key()) {
                    Some(name) => fold_subtree(app, &name, key == "ArrowLeft"),
                    None => twisty.unchecked_into::<web_sys::HtmlElement>().click(),
                }
                request_animation_frame(move || {
                    let li = document().query_selector(&format!(".folder-list > li:nth-child({})", at + 1)).ok().flatten();
                    _ = folder_of(li).map(|f| f.focus());
                });
            }
        }
    }
    e.prevent_default();
    e.stop_propagation(); // else the window's keys move through the messages as well
}

/// Collapses or expands `name` and every folder below it that has subfolders, in one save.
fn fold_subtree(app: App, name: &str, collapse: bool) {
    let parents: Vec<String> = app.folders.with_untracked(|folders| {
        let nodes = tree(folders, true);
        let has_children = |n: &Node| {
            let d = n.folder.delimiter;
            nodes.iter().any(|m| m.folder.name != n.folder.name && kubicle_api::within(&m.folder.name, &n.folder.name, d))
        };
        let in_subtree = |n: &&Node| kubicle_api::within(&n.folder.name, name, n.folder.delimiter);
        nodes.iter().filter(in_subtree).filter(|n| has_children(n)).map(|n| n.folder.name.clone()).collect()
    });
    app.drag_open.update(|d| d.retain(|n| !parents.contains(n)));
    app.update_settings(move |s| parents.iter().for_each(|p| s.folders.entry(p.clone()).or_default().collapsed = collapse));
    if collapse {
        open_collapsed(app, name);
    }
}

/// Roundcube's `folder_collapsed`: a folder collapsed while a folder below it is open becomes the open one, unless it
/// cannot be opened (a virtual or `\Noselect` parent). Only in the mail view, which a folder change would replace
/// Compose or Settings with.
fn open_collapsed(app: App, name: &str) {
    let open = app.folder.get_untracked();
    let folder = app.folders.with_untracked(|f| f.iter().find(|f| f.name == name).cloned());
    let opens = folder.is_some_and(|f| f.selectable && f.subscribed && open != f.name && kubicle_api::within(&open, name, f.delimiter));
    if opens && app.view.get_untracked() == View::Mail {
        app.select_folder(name.to_owned());
    }
}

/// Fastmail's "Go to folder" (`g`): a few letters narrow the folders, Up and Down choose, Enter opens. With
/// messages it is Gmail's Move to (`v`), and Enter moves them there.
pub fn picker(app: App) -> impl IntoView {
    let moving = Memo::new(move |_| if let Some(Modal::Folder(keys)) = app.modal.get() { Some(keys) } else { None });
    let (query, active) = (RwSignal::new(String::new()), RwSignal::new(0usize));
    let input = NodeRef::<leptos::html::Input>::new();
    // At once, not on the dialog's next frame: letters typed right after `g` would land on the list.
    Effect::new(move |_| _ = input.get().map(|i| i.focus()));
    // Cleared on close, not on open: an effect on open can land after the first letter typed.
    Effect::new(move |_| {
        if moving.with(Option::is_none) {
            query.set(String::new());
            active.set(0);
        }
    });
    // Folders whose label starts with the letters, then those with the letters anywhere in their name.
    let found = Memo::new(move |_| {
        let q = query.get().to_lowercase();
        let nodes = app.folders.with(|f| tree(f, moving.with(|m| matches!(m, Some(None)))));
        let nodes = nodes.into_iter().filter(|n| n.folder.selectable && n.folder.name.to_lowercase().contains(&q));
        let (mut first, rest): (Vec<Node>, Vec<Node>) = nodes.partition(|n| n.label.to_lowercase().starts_with(&q));
        first.extend(rest);
        first
    });
    let choose = move |i: usize| {
        let Some(name) = found.with_untracked(|f| f.get(i).map(|n| n.folder.name.clone())) else { return };
        let keys = moving.get_untracked().flatten();
        app.dismiss();
        if let Some(keys) = keys { app.move_to(&keys, &name) } else { app.select_folder(name) }
    };
    let keys = move |e: KeyboardEvent| {
        let n = found.with_untracked(Vec::len).max(1);
        match e.key().as_str() {
            "ArrowDown" => active.set((active.get_untracked() + 1) % n),
            "ArrowUp" => active.set((active.get_untracked() + n - 1) % n),
            "Enter" => choose(active.get_untracked()),
            _ => return,
        }
        e.prevent_default();
        crate::state::scroll_into_view(".picker-list .active");
    };
    let title = move || if moving.with(|m| matches!(m, Some(Some(_)))) { "Move to folder" } else { "Go to folder" }.to_owned();
    view! {
        <Dialog open=Signal::derive(move || moving.with(Option::is_some)) close=move || app.dismiss() title=title class="narrow">
            <div class="picker">
                <input class="input" type="text" autocomplete="off" role="combobox" aria-label="Folder name"
                    aria-controls="picker-list" aria-expanded="true" node_ref=input prop:value=move || query.get()
                    on:input=move |e| { query.set(event_target_value(&e)); active.set(0); } on:keydown=keys />
                <ul class="menu picker-list" role="listbox" id="picker-list">
                    {move || found.get().into_iter().enumerate().map(|(i, n)| view! {
                        <li role="option" class="menu-item" class:active=move || active.get() == i
                            aria-selected=move || if active.get() == i { "true" } else { "false" }
                            on:mousedown=move |e| { e.prevent_default(); choose(i); }>
                            {icon(icons::folder(&n.folder))} <span>{n.label}</span>
                            {(n.depth > 0).then(|| view! { <span class="muted small">{n.folder.name}</span> })}
                        </li>
                    }).collect_view()}
                </ul>
            </div>
        </Dialog>
    }
}

/// Folder management, shared by the folder pane's context menu and the settings: Roundcube's folder form, a name
/// and a Parent folder. The server's folder event brings the new list, and its settings event the preferences moved
/// to the new name.
pub fn rename(app: App, name: &str) {
    let parent = app.folders.with_untracked(|f| {
        let delimiter = f.iter().find(|f| f.name == name).and_then(|f| f.delimiter)?;
        let (parent, _) = name.rsplit_once(delimiter)?;
        f.iter().any(|f| f.name == parent).then(|| parent.to_owned())
    });
    app.modal.set(Some(Modal::FolderForm { renaming: Some(name.to_owned()), parent }));
}

/// Roundcube's create and edit form (`folder_edit.php`): Folder name, and Parent folder ("---" for the top), which
/// moves the folder when it changes. The name is checked here as the server checks it, so its sentence shows at once.
pub fn folder_form(app: App) -> impl IntoView {
    let form =
        Memo::new(move |_| if let Some(Modal::FolderForm { renaming, parent }) = app.modal.get() { Some((renaming, parent)) } else { None });
    let title = move || if form.with(|f| f.as_ref().is_some_and(|(r, _)| r.is_some())) { "Edit folder" } else { "Create folder" };
    view! {
        <Dialog open=Signal::derive(move || form.with(Option::is_some)) close=move || app.dismiss() title=move || title().to_owned() class="narrow">
            {move || form.get().map(|(renaming, parent)| folder_fields(app, renaming, parent))}
        </Dialog>
    }
}

fn folder_fields(app: App, renaming: Option<String>, parent: Option<String>) -> impl IntoView {
    let delimiter = app.folders.with_untracked(|f| f.iter().find_map(|f| f.delimiter));
    let leaf = renaming.as_deref().map_or("", |n| delimiter.and_then(|d| n.rsplit_once(d)).map_or(n, |(_, leaf)| leaf));
    let (name, chosen) = (RwSignal::new(leaf.to_owned()), RwSignal::new(parent));
    let moving = StoredValue::new(renaming.clone());
    // Every folder but the one renamed and those below it, which it cannot move into.
    let parents = move || {
        let nodes = app.folders.with(|f| tree(f, false));
        let allowed =
            |n: &Node| moving.with_value(|m| m.as_deref().is_none_or(|m| !kubicle_api::within(&n.folder.name, m, n.folder.delimiter)));
        let indented = |n: Node| (format!("{}{}", "\u{a0}\u{a0}".repeat(n.depth), n.label), Some(n.folder.name));
        std::iter::once(("---".to_owned(), None)).chain(nodes.into_iter().filter(allowed).map(indented)).collect::<Vec<_>>()
    };
    let save = move || {
        let leaf = name.get_untracked().trim().to_owned();
        if let Err(e) = kubicle_api::check_folder_name(&leaf, delimiter) {
            return app.notify(e);
        }
        let (parent, renaming) = (chosen.get_untracked(), renaming.clone());
        leptos::task::spawn_local(async move {
            let change = match &renaming {
                Some(from) => Change::RenameFolder { from: from.clone(), name: leaf.clone(), parent: parent.clone() },
                None => Change::CreateFolder { name: leaf.clone(), parent: parent.clone() },
            };
            if let Err(e) = kubicle_browser::api::post("api/change", &change).await {
                return app.notify(format!("Not saved: {}", e.0));
            }
            app.dismiss();
            // The open folder, or one inside the renamed one, stays open under its new name.
            let (Some(from), open) = (renaming, app.folder.get_untracked()) else { return };
            let to = kubicle_api::folder_path(parent.as_deref(), &leaf, delimiter);
            if to != from && kubicle_api::within(&open, &from, delimiter) {
                app.select_folder(format!("{to}{}", &open[from.len()..]));
            }
        });
    };
    view! {
        <DialogForm label="Save" submit=save cancel=move || app.dismiss()>
            <label class="field"><span class="field-label">"Folder name"</span>
                <input class="input" type="text" bind:value=name />
            </label>
            <label class="field"><span class="field-label">"Parent folder"</span>
                {crate::settings::select("Parent folder", parents, move || chosen.get(), move |p| chosen.set(p))}
            </label>
        </DialogForm>
    }
}

pub fn delete(app: App, name: &str) {
    let name = name.to_owned();
    let question = format!("Delete the folder {name}, its subfolders and every message in them?");
    app.ask(Ask::confirm("Delete folder", question, "Delete"), move |_| app.send(Change::DeleteFolder { name }));
}

/// A new folder in the folder form, inside `parent`.
pub fn create_in(app: App, parent: Option<&str>) {
    app.modal.set(Some(Modal::FolderForm { renaming: None, parent: parent.map(str::to_owned) }));
}

/// Roundcube's folder actions, the menu in the folder column's header, in its order: for the folder on screen. Mark
/// all as read is on while any folder has unread mail. About ends it: Elastic's rail, which kubicle has not, holds
/// Roundcube's.
fn folder_actions(app: App) -> Vec<MenuItem> {
    let folder = app.folder.get_untracked();
    let Some(f) = app.folders.with_untracked(|f| f.iter().find(|f| f.name == folder).cloned()) else { return Vec::new() };
    let (empty, read) = (folder.clone(), folder.clone());
    let unread = app.mark_all_read_state(&folder) > 0;
    vec![
        MenuItem::action("Empty", move || app.empty(empty.clone())).glyph(icons::TRASH).enabled(f.total > 0), // any folder, as Roundcube
        MenuItem::action("Mark all as read", move || app.mark_all_read(read.clone())).glyph(icons::MAIL).enabled(unread),
        MenuItem::action("Manage folders", move || app.open_settings(Some("Folders"))).glyph(icons::SETTINGS),
        MenuItem::separator(),
        MenuItem::action("About", move || app.modal.set(Some(Modal::About))).glyph(icons::HELP),
    ]
}

/// The folder pane's context menu. Special folders cannot be renamed or deleted, as in the settings.
pub fn folder_menu(app: App, name: &str) -> Vec<MenuItem> {
    let Some(folder) = app.folders.with_untracked(|f| f.iter().find(|f| f.name == name).cloned()) else { return Vec::new() };
    let (special, unread) = (folder.role() != FolderRole::Other, app.mark_all_read_state(name) > 0);
    let (open, read, empty) = (name.to_owned(), name.to_owned(), name.to_owned());
    let (rename_name, delete_name, parent) = (name.to_owned(), name.to_owned(), name.to_owned());
    vec![
        MenuItem::action("Open", move || app.select_folder(open.clone())).glyph(icons::folder(&folder)).enabled(folder.selectable),
        MenuItem::action("Mark all as read", move || app.mark_all_read(read.clone())).glyph(icons::MAIL).enabled(unread),
        MenuItem::action("Empty", move || app.empty(empty.clone())).glyph(icons::TRASH).enabled(folder.total > 0),
        MenuItem::separator(),
        MenuItem::action("New subfolder", move || create_in(app, Some(&parent))).glyph(icons::PLUS),
        MenuItem::action("Rename", move || rename(app, &rename_name)).glyph(icons::COMPOSE).enabled(!special),
        MenuItem::action("Delete", move || delete(app, &delete_name)).glyph(icons::TRASH).enabled(!special),
    ]
}

/// Roundcube's drag menu, for a drop with Shift held.
pub fn drop_menu(app: App, keys: Vec<Key>, to: String) -> Vec<MenuItem> {
    let (copied, target) = (keys.clone(), to.clone());
    vec![
        MenuItem::action("Move here", move || app.move_to(&keys, &to)).glyph(icons::MOVE),
        MenuItem::action("Copy here", move || app.copy_to(&copied, &target)).glyph(icons::COPY),
    ]
}

/// Not while Compose sends, as Roundcube's `busy` lock refuses every command: sign-out frees the uploads it still reads.
/// Once started the button is disabled, as Roundcube's redirect sets `busy`: a second press would wait out the first.
fn logout(app: App, signing_out: RwSignal<bool>) {
    if app.sending() || !crate::compose::may_leave(app, move || logout(app, signing_out)) {
        return;
    }
    signing_out.set(true);
    keep_focus(); // the disabled button drops the focus
    leptos::task::spawn_local(async move {
        crate::compose::sign_out(app).await;
        let _ = kubicle_browser::api::post("api/logout", &()).await;
        let _ = window().location().set_href(&app.logout_url());
    });
}
