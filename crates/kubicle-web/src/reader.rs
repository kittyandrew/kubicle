//! The reader. Every mail renders only inside a sandboxed frame (docs/design.md, Security), plain text in the app's
//! theme (`theme::paper`).
//!
//! The frame fills the space below the headers and scrolls by itself. Fitting its height to the mail would
//! need a measurement from inside the frame, which takes `allow-scripts`, or from outside, which its opaque
//! origin forbids; a frame that fills the pane needs neither and keeps the headers in view.

use kubicle_api::{Address, Attachment, Chosen, Flag, FolderRole, Header, MAX_UPLOAD_MIB, Message, Redirect, RemoteImages};
use kubicle_api::{SentCopy, Spoofed};
use kubicle_browser::{api, api::enc, text, time};
use kubicle_ui::{dialog::Dialog, dialog::DialogForm, icons, icons::icon, menu::Dropdown, menu::Menu, menu::MenuItem};
use leptos::{html, prelude::*, task::spawn_local};
use wasm_bindgen::{JsCast, closure::Closure};

use crate::{actions::ARCHIVE, actions::Action, actions::DELETE, actions::EDIT, actions::FORWARD, actions::FORWARD_MENU, actions::JUNK};
use crate::{actions::MARK_MENU, actions::MARK_READ, actions::NOT_JUNK, actions::PRINT, actions::REPLY, actions::REPLY_ALL_MAIN};
use crate::{actions::REPLY_ALL_MENU, actions::more_items, actions::new_tab, compose::address_field, compose::sent_copy_select};
use crate::{settings::select, state::App, state::Ask, state::Key, state::Modal, theme};

#[component]
pub fn ReaderPane() -> impl IntoView {
    let app = expect_context::<App>();
    // A memo, so a selection change that keeps what shows, as 0 to 1 on the open message, keeps the message's nodes
    // and the focus inside them.
    let shown = Memo::new(move |_| match (selected(app), app.open.get(), app.message.get()) {
        (n, _, _) if n > 1 => Shown::Several(n),
        (_, None, _) => Shown::Nothing,
        (_, open, None) if open == app.unloaded.get() => Shown::Unloaded,
        (_, Some(_), None) => Shown::Loading,
        (_, Some(_), Some(m)) => Shown::Message(m),
    });
    view! {
        <section class="reader" aria-label="Message">
            <Toolbar />
            {move || match shown.get() {
                // Roundcube clears the preview while several messages are selected; the toolbar acts on them all.
                Shown::Several(n) => view! { <div class="reader-empty">{icon(icons::MAIL)} <p>{format!("{n} messages selected")}</p></div> }.into_any(),
                Shown::Nothing => view! { <div class="reader-empty">{icon(icons::MAIL)} <p>"Select a message to read it"</p></div> }.into_any(),
                Shown::Loading => view! { <div class="reader-empty"><p>"Loading..."</p></div> }.into_any(),
                Shown::Unloaded => view! { <div class="reader-empty"><p>"Could not load message from server."</p></div> }.into_any(),
                Shown::Message(m) => message_view(app, m).into_any(),
            }}
            <PhoneNav />
            <RedirectDialog />
            <ImportDialog />
        </section>
    }
}

/// What the reader pane shows.
#[derive(Clone, PartialEq)]
enum Shown {
    Several(usize),
    Nothing,
    Loading,
    Unloaded,
    Message(Message),
}

/// Elastic's footer under a message on a phone: Previous, the Reply button moved down from the toolbar, and Next,
/// across pages, as Roundcube's `ui.js` builds it at 768 px and below.
#[component]
fn PhoneNav() -> impl IntoView {
    let app = expect_context::<App>();
    let shown = move || app.message.with(Option::is_some) && selected(app) <= 1;
    view! {
        <Show when=shown>
            <footer class="reader-nav phone-only">
                {nav_button(app, -1)} {action_button(app, REPLY, None)} {nav_button(app, 1)}
            </footer>
        </Show>
    }
}

/// Roundcube's Previous (`delta` -1) or Next (1) message button.
fn nav_button(app: App, delta: isize) -> impl IntoView {
    let (label, title) = if delta < 0 { ("Previous", "Show previous message") } else { ("Next", "Show next message") };
    let glyph =
        if delta < 0 { view! { <span class="flip">{icon(icons::CHEVRON)}</span> }.into_any() } else { icon(icons::CHEVRON).into_any() };
    view! {
        <button class="tool" title=title aria-label=title prop:disabled=move || !app.has_neighbour(delta) on:click=move |_| app.neighbour(delta)>
            {glyph} <span class="tool-label">{label}</span>
        </button>
    }
}

/// How many messages are selected, tracked; Select all counts the whole folder.
pub fn selected(app: App) -> usize {
    match app.everything.get() {
        Some(folder) => app.folders.with(|f| f.iter().find(|f| f.name == folder).map_or(0, |f| f.total as usize)),
        None => app.selection.with(Vec::len),
    }
}

/// One place in the toolbar.
#[derive(Clone, Copy)]
enum Tool {
    Button(Action),
    /// Roundcube's split button: the action, and beside it a menu named for screen readers.
    Split(Action, &'static str, &'static [Action]),
    /// A button that only opens its menu, as Mark.
    Menu(&'static str, &'static str, &'static [Action]),
}

/// Roundcube's toolbar in its order, More aside; Elastic's spacer stands before Delete.
const TOOLS: [Tool; 9] = [
    Tool::Button(EDIT),
    Tool::Button(REPLY),
    Tool::Split(REPLY_ALL_MAIN, "Reply-all options", &REPLY_ALL_MENU),
    Tool::Split(FORWARD, "Forwarding options", &FORWARD_MENU),
    Tool::Button(DELETE),
    Tool::Button(ARCHIVE),
    Tool::Button(JUNK),
    Tool::Button(NOT_JUNK),
    Tool::Menu("Mark", icons::TAG, &MARK_MENU),
];

const SPACER_BEFORE: usize = 4;

/// Tools that do not fit wrap to a second line of the toolbar, which stays hidden, and show in More instead.
/// A `ResizeObserver` on the toolbar and on each tool notices a change of width or label.
#[component]
fn Toolbar() -> impl IntoView {
    let app = expect_context::<App>();
    let entries = move |actions: &[Action]| actions.iter().map(|a| a.item(app, &a.targets(app))).collect::<Vec<_>>();
    let tools = NodeRef::<html::Div>::new();
    let wrapped = RwSignal::new(0u32); // a bit per slot
    let measure = move || {
        let Some(bar) = tools.try_get_untracked().flatten() else { return }; // an observer callback may outlive the bar
        let mut mask = 0;
        let children = bar.children();
        for child in (0..children.length()).filter_map(|i| children.item(i)) {
            // `offset_top` counts from `.tools` only as base.css positions it: a wrapped tool starts below it.
            let out = child.dyn_ref::<web_sys::HtmlElement>().is_some_and(|c| c.offset_top() >= bar.client_height());
            let _ = child.class_list().toggle_with_force("wrapped", out);
            if out && let Some(slot) = child.get_attribute("data-slot").and_then(|s| s.parse::<u32>().ok()) {
                mask |= 1 << slot;
            }
        }
        wrapped.set(mask);
    };
    let observer = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(bar) = tools.get() else { return };
        let callback = Closure::<dyn Fn()>::new(measure);
        let Ok(watch) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) else { return };
        watch.observe(&bar);
        let children = bar.children();
        (0..children.length()).filter_map(|i| children.item(i)).for_each(|child| watch.observe(&child));
        observer.set_value(Some((watch, callback)));
    });
    on_cleanup(move || observer.with_value(|o| o.iter().for_each(|(watch, _)| watch.disconnect())));
    let is_wrapped = move |slot: usize| wrapped.get() & (1 << slot) != 0;

    let tool = move |(slot, tool): (usize, Tool)| {
        let spacer = (slot == SPACER_BEFORE).then(|| view! { <span class="tool-sep"></span> });
        let tool = match tool {
            Tool::Button(a) => action_button(app, a, Some(slot)).into_any(),
            Tool::Split(a, label, menu) => view! {
                <span class="split" data-slot=slot>
                    {action_button(app, a, None)}
                    <Menu class="tool split-menu" label=label trigger=icon(icons::CARET) items=move || entries(menu) />
                </span>
            }
            .into_any(),
            Tool::Menu(label, glyph, menu) => view! {
                <span class="dropdown-slot" data-slot=slot>
                    <Dropdown label=label glyph=glyph text=label disabled=Signal::derive(move || MARK_READ.targets(app).is_empty())
                        items=move || entries(menu) />
                </span>
            }
            .into_any(),
        };
        view! { {spacer} {tool} }
    };
    let more = move || {
        let mut items = Vec::new();
        for (_, tool) in TOOLS.into_iter().enumerate().filter(|(slot, _)| is_wrapped(*slot)) {
            match tool {
                Tool::Button(a) => items.extend(a.shown_item(app, &a.targets(app))),
                Tool::Split(_, _, menu) => items.extend(entries(menu)),
                Tool::Menu(label, glyph, menu) => items.push(MenuItem::submenu(label, entries(menu)).glyph(glyph)),
            }
        }
        if !items.is_empty() {
            items.push(MenuItem::separator());
        }
        items.extend(more_items(app, &PRINT.targets(app), &app.targets()));
        items
    };
    view! {
        <header class="bar toolbar" role="toolbar" aria-label="Message actions">
            <button class="icon-button phone-only" aria-label="Back" on:click=move |_| app.back()>{icon(icons::BACK)}</button>
            <div class="tools" node_ref=tools>
                {TOOLS.into_iter().enumerate().map(tool).collect_view()}
            </div>
            <Dropdown label="More" glyph=icons::MORE text="More" align_right=true items=more />
            // Roundcube's message view adds Previous and Next after More (base.css shows them only there), hidden on
            // small screens, where the footer has them (`PhoneNav`).
            <span class="tool-sep nav-tools"></span>
            <span class="nav-tools">{nav_button(app, -1)} {nav_button(app, 1)}</span>
        </header>
    }
}

/// An action's toolbar button, which the footer's Reply is too; `slot` is its place in the toolbar.
fn action_button(app: App, a: Action, slot: Option<usize>) -> impl IntoView {
    view! {
        <button class="tool" data-slot=slot style:display=move || if (a.shown)(app) { "" } else { "none" }
            prop:disabled=move || !a.applies(app, &a.targets(app))
            on:click=move |e| (a.run)(app, &a.targets(app), e.shift_key())>
            {icon(a.glyph)} <span class="tool-label">{move || (a.label)(app)}</span>
        </button>
    }
}

/// Roundcube's bounce dialog: From, To, Cc, Bcc and where the copy goes. The message itself goes out unchanged.
#[component]
fn RedirectDialog() -> impl IntoView {
    let app = expect_context::<App>();
    let target = Memo::new(move |_| if let Some(Modal::Redirect(key)) = app.modal.get() { Some(key) } else { None });
    view! {
        <Dialog open=Signal::derive(move || target.with(Option::is_some)) close=move || app.dismiss() title="Resend (bounce)">
            {move || target.get().map(|key| redirect_form(app, key))}
        </Dialog>
    }
}

/// The dialog's form for one message, drawn anew, and so empty, each time the dialog opens.
fn redirect_form(app: App, (folder, uid): Key) -> impl IntoView {
    let choices = App::addresses();
    let choices = move || choices.get().unwrap_or_default();
    let picked = RwSignal::new(None::<Address>); // else the account's own, the first choice once they arrive
    let from = move || picked.get().or_else(|| choices().first().cloned()).unwrap_or_default();
    let (to, cc, bcc) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let (cc_shown, bcc_shown) = (RwSignal::new(false), RwSignal::new(false));
    let sent_copy = RwSignal::new(SentCopy::NotSaved); // Roundcube's bounce files no copy unless asked
    let sending = RwSignal::new(false);
    // Read from the message, which the list may not have loaded yet.
    let warn_bcc = RwSignal::new(false);
    let key = (folder.clone(), uid);
    spawn_local(async move {
        let Some(message) = app.body(&key).await else { return };
        warn_bcc.try_set(message.bcc);
    });
    let send = move || {
        let (folder, from) = (folder.clone(), from());
        let parse = |field: RwSignal<String>| text::parse_addresses(&field.get_untracked());
        let redirect = Redirect { folder, uid, from, to: parse(to), cc: parse(cc), bcc: parse(bcc), sent_copy: sent_copy.get_untracked() };
        if redirect.to.is_empty() && redirect.cc.is_empty() && redirect.bcc.is_empty() {
            return app.notify("Please enter at least one recipient.".into());
        }
        sending.set(true);
        spawn_local(async move {
            let result = api::post("api/redirect", &redirect).await;
            sending.try_set(false); // `try_`: the dialog may have closed with its reader meanwhile
            if let Err(e) = result {
                return app.notify(format!("Not sent: {}", e.0));
            }
            app.dismiss();
            app.notify("Message sent successfully.".into());
        });
    };
    // Roundcube's "+" beside To adds Cc and Bcc, each with an "x" in the same place that takes it away again.
    let adds = move || {
        let add = |label: &str, shown: RwSignal<bool>| MenuItem::action(label, move || shown.set(true)).enabled(!shown.get());
        vec![add("Cc", cc_shown), add("Bcc", bcc_shown)]
    };
    let added = move |label: &'static str, value: RwSignal<String>, shown: RwSignal<bool>| {
        view! {
            <Show when=move || shown.get()>
                <div class="field-line">
                    {address_field(label, value, || {}, NodeRef::new())}
                    <button type="button" class="icon-button danger" aria-label=format!("Remove {label}")
                        on:click=move |_| { value.set(String::new()); shown.set(false); }>{icon(icons::CLOSE)}</button>
                </div>
            </Show>
        }
    };
    view! {
        <DialogForm label="Resend" glyph=icons::SENT busy=sending submit=send cancel=move || app.dismiss()>
            <p class="muted small">
                "The message will be sent without any modifications, i.e. using the original body, headers and recipients."
            </p>
            <Show when=move || warn_bcc.get()>{bcc_warning()}</Show>
            <label class="field"><span class="field-label">"From"</span>
                {select("From", move || choices().into_iter().map(|a| (a.to_string(), a)), from, move |a| picked.set(Some(a)))}
            </label>
            <div class="field-line">
                {address_field("To", to, || {}, NodeRef::new())}
                <Menu class="icon-button" label="Add Cc or Bcc" align_right=true trigger=icon(icons::PLUS) items=adds />
            </div>
            {added("Cc", cc, cc_shown)} {added("Bcc", bcc, bcc_shown)}
            {sent_copy_select(app, sent_copy, || {})}
        </DialogForm>
    }
}

/// Roundcube's Import: `.eml` files or mbox mailboxes into the open folder, as they are.
#[component]
fn ImportDialog() -> impl IntoView {
    let app = expect_context::<App>();
    let open = Signal::derive(move || matches!(app.modal.get(), Some(Modal::Import)));
    let input = NodeRef::<html::Input>::new();
    let busy = RwSignal::new(false);
    let import = move || {
        let list = input.get_untracked().and_then(|i| i.files());
        let files: Vec<web_sys::File> = list.map_or_else(Vec::new, |l| (0..l.length()).filter_map(|i| l.get(i)).collect());
        if files.is_empty() {
            return app.notify("Please select a file to upload.".into());
        }
        #[allow(clippy::cast_precision_loss)] // 36 MiB is exact in an f64
        let limit = (MAX_UPLOAD_MIB << 20) as f64;
        if files.iter().map(|f| f.size()).sum::<f64>() > limit {
            return app.notify(format!("The files are larger than {MAX_UPLOAD_MIB} MB together."));
        }
        let folder = app.folder.get_untracked();
        busy.set(true);
        spawn_local(async move {
            let result = api::post_files::<u32>(&format!("api/import?folder={}", enc(&folder)), &files).await;
            busy.try_set(false);
            let count = match result {
                Ok(count) => count,
                Err(e) => return app.notify(format!("Import failed: {}", e.0)),
            };
            app.dismiss();
            app.notify(format!("Successfully imported {count} messages"));
        });
    };
    view! {
        <Dialog open=open close=move || app.dismiss() title="Import messages" class="narrow">
            <DialogForm label="Import" glyph=icons::UPLOAD busy=busy submit=import cancel=move || app.dismiss()>
                <p class="muted small">{format!("Mail files (.eml) or mailboxes (mbox), up to {MAX_UPLOAD_MIB} MB together, into the open folder.")}</p>
                <input type="file" multiple accept=".eml,.mbox,message/rfc822,application/mbox,text/plain" aria-label="Files to import" node_ref=input />
            </DialogForm>
        </Dialog>
    }
}

/// An address as a link to its menu, as Roundcube links each address in the header (`address_string`): the name,
/// or with "Show email address with display name" both name and address. kubicle keeps no address book, so the
/// menu offers new mail and a copy. An address that is not one shows as text.
fn address_menu(app: App, address: &Address) -> AnyView {
    let shown = address.clone();
    let label = move || app.settings.with(|s| text::reader_label(&shown, s.show_email));
    if !kubicle_api::valid_email(&address.email) {
        return view! { <span>{label}</span> }.into_any();
    }
    let (to, email) = (address.clone(), address.email.clone());
    let items = move || {
        let (to, email) = (to.clone(), email.clone());
        vec![
            MenuItem::action("Compose to", move || app.compose_mailto(format!("mailto:{}", enc(&to.to_string())))).glyph(icons::COMPOSE),
            MenuItem::action("Copy address", move || {
                let _ = window().navigator().clipboard().write_text(&email);
                app.notify(format!("Copied {email}"));
            })
            .glyph(icons::COPY),
        ]
    };
    // The menu's label is its tooltip: the address, as Roundcube's title on each address.
    view! { <span class="address"><Menu class="link" label=address.email.clone() trigger=label items=items /></span> }.into_any()
}

/// A header's addresses, the first `max` of them and "N more..." for the rest, which opens them all in a dialog
/// titled after the row, as Roundcube's `address_string` does.
fn addresses(app: App, list: &[Address], max: Option<usize>, title: &'static str, more: RwSignal<Option<More>>) -> impl IntoView + use<> {
    let shown = max.map_or(list.len(), |m| m.min(list.len()));
    let mut out = Vec::new();
    for (n, address) in list.iter().take(shown).enumerate() {
        if n > 0 {
            out.push(", ".into_any());
        }
        out.push(address_menu(app, address));
    }
    if list.len() > shown {
        let (all, rest) = (list.to_vec(), list.len() - shown);
        let open = move |_| more.set(Some(More(title, all.clone())));
        out.push(view! { ", " <button class="link more" on:click=open>{format!("{rest} more...")}</button> }.into_any());
    }
    out
}

/// What the reader's small dialog shows: every address of a row, under the row's title.
#[derive(Clone)]
struct More(&'static str, Vec<Address>);

fn more_dialog(app: App, more: RwSignal<Option<More>>) -> impl IntoView {
    let title = move || more.with(|m| m.as_ref().map_or("", |m| m.0)).to_owned();
    view! {
        <Dialog open=Signal::derive(move || more.with(Option::is_some)) close=move || more.set(None) title=title class="narrow">
            <div class="dialog-body more">
                {move || more.get().map(|More(_, all)| addresses(app, &all, None, "", more))}
            </div>
            <footer class="dialog-buttons">
                <button type="button" class="button" on:click=move |_| more.set(None)>"Close"</button>
            </footer>
        </Dialog>
    }
}

/// Roundcube's header table rows (`message_headers`): addresses linked, up to `max` per row, dates in the reader's
/// zone.
fn header_rows(app: App, rows: &[Header], max: Option<usize>, more: RwSignal<Option<More>>) -> Vec<AnyView> {
    let header_row = |row: &Header| {
        let value = match row {
            Header::Date(date) => time::long(*date).into_any(),
            Header::Priority(priority) => priority.label().into_any(),
            _ => addresses(app, row.addresses().unwrap_or_default(), max, row.title(), more).into_any(),
        };
        view! { <tr><th>{row.title()}</th><td>{value}</td></tr> }.into_any()
    };
    rows.iter().map(header_row).collect()
}

/// Roundcube's `emptyattachment`.
const EMPTY: &str = "This attachment appears to be empty. Please, check with the person who sent this.";

/// Roundcube's warning where a message goes on whole with its own Bcc lines: Resend and Forward as attachment.
pub fn bcc_warning() -> impl IntoView {
    view! { <p class="notice" role="status">{icon(icons::REPORT)} "The forwarded message(s) contain confidential recipients (Bcc)."</p> }
}

/// The raw header block as Roundcube's dialog shows it (`headers.php`): a line's leading run of name characters, the
/// name and its colon, in bold; a folded line indented by four spaces whatever it had; lines not wrapped, so the box
/// scrolls sideways.
fn headers_dialog(headers: RwSignal<Option<String>>) -> impl IntoView {
    let lines = move || {
        let text = headers.get().unwrap_or_default();
        let lines = text.trim().lines().map(|line| {
            let folded = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if folded.len() < line.len() {
                return view! { <div>{format!("\u{a0}\u{a0}\u{a0}\u{a0}{folded}")}</div> }.into_any();
            }
            let name = line.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))).unwrap_or(line.len());
            view! { <div><b>{line[..name].to_owned()}</b>{line[name..].to_owned()}</div> }.into_any()
        });
        lines.collect::<Vec<_>>()
    };
    view! {
        <Dialog open=Signal::derive(move || headers.with(Option::is_some)) close=move || headers.set(None) title="Message headers">
            <div class="raw-headers">{lines}</div>
        </Dialog>
    }
}

/// A chip, or a preview for an image, with Roundcube's menu: Open in a tab where the browser can show it (an image,
/// text, HTML or a forwarded message), and Download. An empty file gets no menu: a click says it is empty, in
/// Roundcube's alert.
fn attachment(app: App, a: Attachment) -> impl IntoView {
    let size = view! { <span class="muted small">{kubicle_api::size(a.size)}</span> };
    if a.size == 0 {
        return view! {
            <button type="button" class="chip" title=a.filename.clone() on:click=move |_| app.ask(Ask::alert(EMPTY), |_| {})>
                {icon(icons::ATTACH)} <span class="chip-name">{a.filename.clone()}</span> {size}
            </button>
        }
        .into_any();
    }
    let trigger = match &a.preview_url {
        Some(preview) => view! {
            <img src=preview.clone() alt="" loading="lazy" />
            <span class="chip-name">{a.filename.clone()} " " {size}</span>
        }
        .into_any(),
        None => view! { {icon(icons::ATTACH)} <span class="chip-name">{a.filename.clone()}</span> {size} }.into_any(),
    };
    let class = if a.preview_url.is_some() { "preview" } else { "chip" };
    let items = move || {
        let open = a.view_url.clone().or_else(|| a.preview_url.clone());
        let open = open.map(|url| MenuItem::action("Open", move || new_tab(&url)).glyph(icons::OPEN));
        // The download URL answers with `Content-Disposition: attachment`, so the page stays where it is.
        let url = a.url.clone();
        let download = MenuItem::action("Download", move || drop(window().location().set_href(&url))).glyph(icons::DOWNLOAD);
        open.into_iter().chain([download]).collect()
    };
    view! { <Menu class=class label=a.filename.clone() trigger=trigger items=items /> }.into_any()
}

/// The notice over a mail whose remote images were left out. "Always for" shows only where trust loads images, as
/// Roundcube's "Always allow from" only in its trusted modes; a trusted sender's images stay out when the From
/// domain did not vouch for the mail (`Authenticity::from_verified`).
fn images_notice(app: App, email: &str) -> impl IntoView + use<> {
    let trusting = app.settings.with_untracked(|s| s.remote_images == RemoteImages::Trusted);
    let trusted = app.settings.with_untracked(|s| s.trusted_senders.iter().any(|t| t.eq_ignore_ascii_case(email)));
    let hidden = if trusting && trusted {
        "Remote images are hidden: this mail did not pass its sender's domain checks (DKIM or DMARC)."
    } else {
        "Remote images are hidden."
    };
    let email = email.to_owned();
    // A long address gives way in its local part, cut before the @, so the domain always shows; the title is whole.
    let (local, domain) = email.rsplit_once('@').map_or_else(|| (email.clone(), String::new()), |(l, d)| (l.to_owned(), format!("@{d}")));
    let always = format!("Always show remote images from {email}");
    let trust = move |_| {
        crate::settings::trust(app, &email);
        app.show_variant(Chosen { images: true, text: None });
    };
    view! {
        <p class="notice images" role="status">
            {icon(icons::IMAGE)} <span>{hidden}</span>
            <button class="link" on:click=move |_| app.show_variant(Chosen { images: true, text: None })>"Show"</button>
            {(trusting && !trusted).then(|| view! {
                <button class="link trust" title=always on:click=trust>"Always for "<span class="trust-local">{local}</span>{domain}</button>
            })}
        </p>
    }
}

/// Roundcube's summary line (`message_summary`): "From" the sender, or in Sent and Drafts "To" the recipients, else
/// Cc, else Bcc, the first with "N more..." for the rest; with nobody, the date alone.
fn summary(app: App, m: &Message, more: RwSignal<Option<More>>) -> AnyView {
    let bcc = m.headers.iter().find_map(|h| if let Header::Bcc(b) = h { Some(b.clone()) } else { None }).unwrap_or_default();
    let (role, people) = if app.has_role(&m.folder, FolderRole::Sent) || app.has_role(&m.folder, FolderRole::Drafts) {
        ("To", [&m.to, &m.cc, &bcc].into_iter().find(|l| !l.is_empty()).cloned().unwrap_or_default())
    } else {
        ("From", m.from.clone())
    };
    if people.is_empty() {
        view! { "On " <time>{time::long(m.date)}</time> }.into_any()
    } else {
        view! { {role} " " {addresses(app, &people, Some(1), "", more)} " on " <time>{time::long(m.date)}</time> }.into_any()
    }
}

/// The lines above the mail, in Roundcube's order (`message_objects`) with kubicle's sender check after the first:
/// a draft's Edit, a From that may be forged, remote images left out, look-alike addresses.
fn notices(app: App, m: &Message, sender: &str) -> impl IntoView + use<> {
    let key = (m.folder.clone(), m.uid);
    // Drafts or one of its subfolders, where Roundcube says a message is a draft.
    let draft = app.role_folder(FolderRole::Drafts).is_some_and(|d| app.in_tree(&m.folder, &d));
    // The warning that the From may be forged (docs/design.md, Sender authentication).
    let spoofed = m.authenticity.as_ref().and_then(|a| a.warning.clone()).map(|warning| match warning {
        Spoofed::Dmarc(domain) => format!("This message failed {domain}'s sender check, so it may not be from {domain}."),
        Spoofed::Own(domain) => format!("This message claims to come from your own domain {domain}, but not from a signed-in account."),
        Spoofed::Account(sender) => format!("This message was sent by the account {sender}, not by the address it shows as From."),
        Spoofed::Unclear => "The mail server's sender check of this message could not be read, so the sender is not verified.".to_owned(),
    });
    view! {
        {draft.then(|| view! {
            <p class="notice draft" role="status">
                <span>"This is a draft message."</span>
                <button class="link" on:click=move |_| (EDIT.run)(app, std::slice::from_ref(&key), false)>"Edit"</button>
            </p>
        })}
        {spoofed.map(|text| view! { <p class="notice danger" role="alert">{icon(icons::JUNK)}{text}</p> })}
        {m.body.remote_blocked.then(|| images_notice(app, sender))}
        {m.suspicious.then(|| view! {
            <p class="notice" role="status">{icon(icons::REPORT)} "This message contains suspicious email addresses that may be fraudulent."</p>
        })}
    }
}

/// Roundcube's message header: subject, one summary line, then Details, Headers and Plain text.
fn message_view(app: App, m: Message) -> impl IntoView {
    let key = (m.folder.clone(), m.uid);
    let sender = m.from.first().cloned().unwrap_or_default();
    let subject = if m.subject.trim().is_empty() { "(no subject)".to_owned() } else { m.subject.clone() };
    let more = RwSignal::new(None::<More>);
    let summary = summary(app, &m, more);
    let plain = m.has_html && m.body.text;
    let details = move || app.settings.with(|s| s.message_details);
    let headers = RwSignal::new(None::<String>);
    let source = format!("api/source?folder={}&uid={}&headers=true", enc(&m.folder), m.uid);
    let show_headers = move |_| {
        let source = source.clone();
        spawn_local(async move {
            match api::get_text(&source).await {
                Ok(text) => drop(headers.try_set(Some(text))), // the reader may be gone: Compose opened meanwhile
                Err(e) => app.notify(format!("Could not load the headers: {}", e.0)),
            }
        });
    };
    let window_url = format!("#open={}/{}", m.uid, enc(&m.folder));
    // Roundcube's table, then kubicle's sender checks.
    let rows = m.headers.clone();
    let signed = m.authenticity.as_ref().filter(|a| !a.signed_by.is_empty()).map(|a| ("Signed by", a.signed_by.join(", ")));
    let mailed = m.authenticity.as_ref().and_then(|a| a.mailed_by.clone()).map(|d| ("Mailed by", d));
    let checks: Vec<(&str, String)> = signed.into_iter().chain(mailed).collect();
    let notices = notices(app, &m, &sender.email);
    // The flag before the subject, as Elastic draws it, following the list's rows as marks change.
    let flagged = move || {
        app.list.track();
        app.search.track();
        app.row_flags(&key).is_some_and(|f| f.has(Flag::Flagged))
    };
    view! {
        <article class="message">
            <header class="message-header">
                <div class="subject-line">
                    <Show when=flagged><span class="flagged" title="Flagged">{icon(icons::FLAG)}</span></Show>
                    <h2 class="message-subject">{subject}</h2>
                    // Not inside a message window, and hidden on a phone (base.css), as Elastic's.
                    {app.single().is_none().then(|| view! {
                        <a class="icon-button open-window" href=window_url target="_blank" rel="noopener" title="Open in new window" aria-label="Open in new window">
                            {icon(icons::OPEN)}
                        </a>
                    })}
                </div>
                <div class="message-meta">
                    <span class="avatar" aria-hidden="true">{text::initial(&sender)}</span>
                    <div class="header-lines">
                        <div class="summary">{summary}</div>
                        <div class="header-links">
                            <button class="link" on:click=move |_| app.update_settings(|s| s.message_details ^= true)>
                                {icon(icons::MAIL)} "Details"
                            </button>
                            <button class="link" on:click=show_headers>{icon(icons::INFO)} "Headers"</button>
                            {m.has_html.then(|| view! {
                                <button class="link" on:click=move |_| app.show_variant(Chosen { images: false, text: Some(!plain) })>
                                    {icon(if plain { icons::SOURCE } else { icons::MENU })} {if plain { "HTML" } else { "Plain text" }}
                                </button>
                            })}
                        </div>
                    </div>
                </div>
                <Show when=details>
                    <table class="details">
                        {header_rows(app, &rows, Some(10), more)}
                        {checks.clone().into_iter().map(|(l, v)| view! { <tr><th>{l}</th><td>{v}</td></tr> }).collect_view()}
                    </table>
                </Show>
            </header>
            {notices}
            <div class="attachments" aria-label="Attachments">{m.attachments.into_iter().map(|a| attachment(app, a)).collect_view()}</div>
            <div class="mail-frame">
                // Popups escape the sandbox: a mail's links, kubicle's own `#compose=` ones too, open as pages that
                // run their scripts. Never `allow-scripts` or `allow-same-origin` (render.rs).
                <iframe class:text=m.body.text sandbox="allow-popups allow-popups-to-escape-sandbox" referrerpolicy="no-referrer" title="Message body" srcdoc=move || theme::paper(&m.body.document, m.body.text)></iframe>
            </div>
            {headers_dialog(headers)} {more_dialog(app, more)}
        </article>
    }
}
