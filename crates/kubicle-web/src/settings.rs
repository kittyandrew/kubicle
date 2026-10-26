//! Settings: kubicle's own (name, time zone, appearance, themes, folders, trusted senders) and
//! Roundcube's preferences for what kubicle does, in its section names. Each change saves at once.

use kubicle_api::{ArchiveType, Change, ExpandThreads, FolderRole, FolderSizes, IdentityPrefs, LogoutPurge, PAGE_SIZES, RemoteImages};
use kubicle_api::{ReplyMode, Settings, ThemeInfo, ThemeMode, ThemeSource, clamp_page_size, current_zone};
use kubicle_browser::{api, text};
use kubicle_ui::{icons, icons::icon};
use leptos::{html, prelude::*, task::spawn_local};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::{folders, state::App, state::Ask, state::View};

/// Adds a sender whose mail loads remote images. The server compares addresses ignoring case, so this does too.
pub fn trust(app: App, email: &str) {
    let email = email.to_owned();
    app.update_settings(move |s| {
        if !s.trusted_senders.iter().any(|t| t.eq_ignore_ascii_case(&email)) {
            s.trusted_senders.push(email.clone());
        }
    });
}

#[component]
pub fn SettingsView() -> impl IntoView {
    let app = expect_context::<App>();
    let (main, body) = (NodeRef::<html::Div>::new(), NodeRef::<html::Div>::new());
    let sections = sections(app);
    let names = sections.each_ref().map(|(name, _)| *name);
    let active = RwSignal::new(0usize);
    let card =
        move |i: usize| body.try_get_untracked().flatten()?.children().item(u32::try_from(i).ok()?)?.dyn_into::<web_sys::HtmlElement>().ok();
    // A clicked section stays marked while its scroll runs, and even when the area cannot scroll it to the top
    // (the last sections); the user's own wheel, touch or key scrolling hands the mark back to `follow`.
    let pinned = StoredValue::new(false);
    // The section a link went to washes light and fades, so the eye finds it, also where it cannot reach the top.
    let flashed = RwSignal::new(None::<usize>);
    let show = move |i: usize| {
        active.set(i);
        pinned.set_value(true);
        flashed.set(None); // and back next frame: a second click on the same link plays it again
        request_animation_frame(move || flashed.set(Some(i)));
        if let Some(card) = card(i) {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_behavior(web_sys::ScrollBehavior::Smooth);
            card.scroll_into_view_with_scroll_into_view_options(&options);
        }
    };
    if let Some(i) = app.take_settings_section().and_then(|name| names.iter().position(|n| *n == name)) {
        request_animation_frame(move || show(i)); // once the cards are drawn
    }
    // The section in view: the last whose top has reached the top of the area.
    let follow = move || {
        let Some(area) = main.get_untracked() else { return };
        let top = area.scroll_top();
        active.set((0..names.len()).rev().find(|&i| card(i).is_some_and(|c| c.offset_top() <= top + 24)).unwrap_or(0));
    };
    view! {
        <section class="settings" aria-label="Settings">
            <header class="bar">
                <button class="icon-button phone-only" aria-label="Back" on:click=move |_| app.back()>{icon(icons::BACK)}</button>
                <h1 class="bar-title">"Settings"</h1>
                <span class="grow"></span>
                <button class="icon-button" aria-label="Close settings" on:click=move |_| app.view.set(View::Mail)>{icon(icons::CLOSE)}</button>
            </header>
            // The nav and the cards scroll as one, the nav held in place: a wheel over the nav scrolls the cards too.
            <div class="settings-main" node_ref=main on:scroll=move |_| if !pinned.get_value() { follow() }
                on:wheel=move |_| pinned.set_value(false) on:touchstart=move |_| pinned.set_value(false)
                on:keydown=move |_| pinned.set_value(false)>
                <nav class="settings-nav" aria-label="Sections">
                    {names.into_iter().enumerate().map(|(i, name)| view! {
                        <button class="settings-link" class:active=move || active.get() == i on:click=move |_| show(i)>{name}</button>
                    }).collect_view()}
                </nav>
                <div class="settings-body" node_ref=body>
                    {sections.into_iter().enumerate().map(|(i, (name, body))| view! {
                        <section class="card" class:flash=move || flashed.get() == Some(i)><h2>{name}</h2>{body}</section>
                    }).collect_view()}
                </div>
            </div>
        </section>
    }
}

fn theme_select(app: App, dark: bool) -> impl IntoView {
    let themes = move || app.themes.get().into_iter().filter(move |t| t.dark == dark).map(|t| (t.name, t.id));
    let current = move || app.settings.with(|s| if dark { s.dark_theme.clone() } else { s.light_theme.clone() });
    let pick =
        move |id: String| app.update_settings(move |s| if dark { s.dark_theme.clone_from(&id) } else { s.light_theme.clone_from(&id) });
    choice(if dark { "Dark theme" } else { "Light theme" }, themes, current, pick)
}

fn general(app: App) -> impl IntoView {
    let mode = |label: &'static str, value: ThemeMode| {
        view! {
            <label class="segment">
                <input type="radio" name="theme-mode" prop:checked=move || app.settings.with(|s| s.theme_mode == value)
                    on:change=move |_| app.update_settings(move |s| s.theme_mode = value) />
                <span>{label}</span>
            </label>
        }
    };
    view! {
        <label class="setting"><span>"Your name on sent mail"</span>
            <input class="input" type="text" placeholder="Address alone" prop:value=move || app.settings.with(|s| s.display_name.clone())
                on:change=move |e| {
                    let name = event_target_value(&e).trim().to_owned();
                    app.update_settings(move |s| s.display_name.clone_from(&name));
                } />
        </label>
        {choice("Time zone", zones, move || app.settings.with(|s| s.time_zone.clone()), move |zone: Option<String>| app.update_settings(move |s| s.time_zone.clone_from(&zone)))}
        <div class="setting"><span>"Appearance"</span>
            <div class="segments" role="radiogroup" aria-label="Appearance">
                {mode("Auto", ThemeMode::Auto)} {mode("Light", ThemeMode::Light)} {mode("Dark", ThemeMode::Dark)}
            </div>
        </div>
        {theme_select(app, false)} {theme_select(app, true)}
        // Roundcube's Browser Options: a link that asks the browser to open mailto: links here.
        <div class="setting">
            <button class="link" on:click=move |_| register_mailto(app)>"Register protocol handler for mailto: links"</button>
        </div>
    }
}

/// Roundcube's "Rows per page": any text saves, clamped into `PAGE_SIZES` as Roundcube's `prefs_save` does, text that
/// is no number as 0 and so as 2. The field shows the clamped number at once, also when the setting already held it.
fn rows_per_page(app: App) -> impl IntoView {
    let save = move |e: leptos::ev::Event| {
        let size = clamp_page_size(event_target_value(&e).trim().parse::<u32>().unwrap_or(0));
        if let Some(field) = e.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) {
            field.set_value(&size.to_string());
        }
        app.set_page_size(size);
    };
    view! {
        <label class="setting"><span>"Rows per page"</span>
            <input class="input narrow" type="number" min=*PAGE_SIZES.start() max=*PAGE_SIZES.end()
                prop:value=move || app.settings.with(|s| s.page_size.to_string()) on:change=save />
        </label>
    }
}

/// Roundcube's `register_protocol_handler`: the browser asks the user, and opens a `mailto:` link as this page with
/// `#compose=` and the link, which Compose fills from.
fn register_mailto(app: App) {
    let location = window().location();
    let page = location.origin().unwrap_or_default() + &location.pathname().unwrap_or_default();
    let navigator = window().navigator();
    let supported = js_sys::Reflect::get(&navigator, &"registerProtocolHandler".into()).is_ok_and(|f| f.is_function());
    if !supported {
        return app.notify("This feature is not supported by your web browser.".into());
    }
    if let Err(e) = navigator.register_protocol_handler("mailto", &format!("{page}#compose=%s"), "kubicle") {
        app.notify(e.as_string().unwrap_or_else(|| String::from(js_sys::JSON::stringify(&e).unwrap_or_default())));
    }
}

/// Roundcube's identities, here the account and the addresses it sent from (`api/addresses`): for each,
/// Organization, Reply-To and Bcc, which Compose applies when that address is the From.
fn identities(app: App) -> impl IntoView {
    let addresses = App::addresses();
    let row = move |address: kubicle_api::Address| {
        let email = address.email.to_lowercase();
        let field = move |label: &'static str, get: fn(&IdentityPrefs) -> String, set: fn(&mut IdentityPrefs, &str)| {
            let (key, shown) = (email.clone(), email.clone());
            view! {
                <label class="setting"><span>{label}</span>
                    <input class="input" type="text" spellcheck=if label == "Organization" { "true" } else { "false" }
                        prop:value=move || app.settings.with(|s| s.identities.get(&shown).map(get).unwrap_or_default())
                        on:change=move |e| {
                            let (key, text) = (key.clone(), event_target_value(&e));
                            app.update_settings(move |s| {
                                set(s.identities.entry(key.clone()).or_default(), &text);
                                s.identities.retain(|_, i| *i != IdentityPrefs::default());
                            });
                        } />
                </label>
            }
        };
        view! {
            <div class="identity">
                <h3 class="identity-address">{address.email.clone()}</h3>
                {field("Organization", |i| i.organization.clone(), |i, t| t.trim().clone_into(&mut i.organization))}
                {field("Reply-To", |i| text::address_list(&i.reply_to), |i, t| i.reply_to = text::parse_addresses(t))}
                {field("Bcc", |i| text::address_list(&i.bcc), |i, t| i.bcc = text::parse_addresses(t))}
            </div>
        }
    };
    view! {
        <p class="muted small">"Your account and the addresses you sent from. Reply-To and Bcc go into a new message when it is from that address; Organization goes out with it."</p>
        {move || addresses.get().unwrap_or_default().into_iter().map(row).collect_view()}
    }
}

/// Saves the settings with a user theme added (`Some` CSS) or removed. The server checks a theme, so the settings
/// here change only once it took them; the session then brings the new theme list.
async fn save_theme(app: App, id: String, css: Option<String>) -> bool {
    app.checked_settings(move |s| match &css {
        Some(css) => drop(s.themes.insert(id.clone(), css.clone())),
        None => drop(s.themes.remove(&id)),
    })
    .await
}

fn themes(app: App) -> impl IntoView {
    let (id, css) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let file_ref = NodeRef::<html::Input>::new();
    let load_file = move |_| {
        let Some(file) = file_ref.get_untracked().and_then(|i| i.files()).and_then(|f| f.get(0)) else { return };
        let name = file.name().trim_end_matches(".css").to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        spawn_local(async move {
            if let Ok(text) = JsFuture::from(file.text()).await {
                css.try_set(text.as_string().unwrap_or_default());
                id.try_update(|id| {
                    if id.is_empty() {
                        *id = name;
                    }
                });
            }
        });
    };
    let save = move |_| {
        spawn_local(async move {
            if save_theme(app, id.get_untracked().trim().to_owned(), Some(css.get_untracked())).await {
                id.try_set(String::new()); // `try_`: Settings may have closed meanwhile
                css.try_set(String::new());
            }
        });
    };
    // A theme's own CSS in the editor, every token in place to change a few; " copy" and an empty Id keep a save
    // from overwriting it. What the editor holds goes only after a yes.
    let start_from = move |theme: &ThemeInfo| {
        let (url, name) = (crate::theme::url(theme), theme.name.clone());
        let question = format!("Replace the CSS in the editor with {name}'s?");
        let fill = move || {
            let (url, name) = (url.clone(), name.clone());
            spawn_local(async move {
                let text = match api::get_text(&url).await {
                    Ok(text) => text,
                    Err(e) => return app.notify(format!("Could not load the theme: {}", e.0)),
                };
                css.try_set(text.replacen(&format!("name=\"{name}\""), &format!("name=\"{name} copy\""), 1));
                id.try_set(String::new());
            });
        };
        if css.with_untracked(|c| c.trim().is_empty()) {
            fill();
        } else {
            app.ask(Ask::confirm("Start from a theme", question, "Replace"), move |_| fill());
        }
    };
    let row = move |t: ThemeInfo| {
        let source = match t.source {
            ThemeSource::BuiltIn => "built in",
            ThemeSource::System => "system",
            ThemeSource::User => "yours",
        };
        let (pick, theme) = (t.clone(), t.id.clone());
        view! {
            <li class="plain-row">
                <button class="theme-pick grow" title="Start a new theme from this one" on:click=move |_| start_from(&pick)>{t.name}</button>
                <span class="muted small">{if t.dark { "dark, " } else { "light, " }} {source}</span>
                {(t.source == ThemeSource::User).then(|| view! {
                    <button class="icon-button" aria-label="Remove" on:click=move |_| {
                        let theme = theme.clone();
                        spawn_local(async move { save_theme(app, theme, None).await; });
                    }>{icon(icons::TRASH)}</button>
                })}
            </li>
        }
    };
    view! {
        <ul class="plain-list">
            <For each=move || app.themes.get() key=|t| (t.id.clone(), t.name.clone()) children=row />
        </ul>
        <p class="muted small">"A theme is a CSS file that sets the tokens on :root and nothing else. Click a theme to start from its CSS."</p>
        <label class="setting"><span>"Id"</span>
            <input class="input" type="text" placeholder="my-theme" bind:value=id />
        </label>
        <textarea class="input code" rows="8" aria-label="Theme CSS" placeholder="/* kubicle-theme name=\"My theme\" dark=false */"
            bind:value=css></textarea>
        <div class="row-actions">
            <label class="button">
                {icon(icons::DOWNLOAD)} "Load a .css file"
                <input class="visually-hidden" type="file" accept=".css,text/css" node_ref=file_ref on:change=load_file />
            </label>
            <span class="grow"></span>
            <button class="button primary" on:click=save>"Save theme"</button>
        </div>
    }
}

/// Roundcube's folder manager: a filter box over the tree, each folder's subscription, count and size, and the folder
/// form for Create and Rename. A filter keeps the folders whose name holds it and the folders above them.
fn folder_list(app: App) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    // Asked once as the section draws: the server sums each folder's message sizes.
    let sizes = LocalResource::new(|| async { api::get::<FolderSizes>("api/sizes").await.unwrap_or_default() });
    let nodes = Memo::new(move |_| {
        let needle = filter.get().to_lowercase();
        let nodes = app.folders.with(|f| folders::tree(f, false));
        let hit = |n: &folders::Node| n.folder.name.to_lowercase().contains(&needle);
        let kept: Vec<String> = nodes.iter().filter(|n| hit(n)).map(|n| n.folder.name.clone()).collect();
        let above_hit = |n: &folders::Node| kept.iter().any(|k| kubicle_api::within(k, &n.folder.name, n.folder.delimiter));
        nodes.into_iter().filter(|n| needle.is_empty() || above_hit(n)).collect::<Vec<_>>()
    });
    let row = move |n: folders::Node| {
        let special = n.folder.role() != FolderRole::Other;
        let (sub, ren, del, folder_name) = (n.folder.name.clone(), n.folder.name.clone(), n.folder.name.clone(), n.folder.name.clone());
        let rename = move |_| folders::rename(app, &ren);
        let delete = move |_| folders::delete(app, &del);
        let size = move || sizes.get().and_then(|s| s.get(&folder_name).map(|&b| kubicle_api::size(b))).unwrap_or_default();
        view! {
            <li class="plain-row" style=format!("--depth: {}", n.depth)>
                <input type="checkbox" title="Show in the folder list" aria-label=format!("Subscribe to {}", n.label) prop:checked=n.folder.subscribed
                    // Roundcube's `folders.php`: a special folder is locked only while subscribed, so one unsubscribed
                    // elsewhere can come back; a `\Noselect` one always, as the pane shows it above its subfolders.
                    prop:disabled=(special && n.folder.subscribed) || !n.folder.selectable
                    on:change=move |_| app.send(Change::Subscribe { name: sub.clone(), subscribed: !n.folder.subscribed }) />
                {icon(icons::folder(&n.folder))} <span class="grow">{n.label}</span>
                <span class="muted small" title="Messages">{text::count(n.folder.total)}</span>
                <span class="muted small folder-size" title="Size">{size}</span>
                <button class="icon-button" aria-label="Rename" title="Rename" prop:disabled=special on:click=rename>{icon(icons::COMPOSE)}</button>
                <button class="icon-button danger" aria-label="Delete" title="Delete" prop:disabled=special on:click=delete>{icon(icons::TRASH)}</button>
            </li>
        }
    };
    view! {
        <div class="row-actions">
            <input class="input grow" type="search" placeholder="Search" aria-label="Filter folders" bind:value=filter />
            <button class="button" on:click=move |_| folders::create_in(app, None)>{icon(icons::PLUS)} "Create"</button>
        </div>
        <ul class="plain-list">
            <For each=move || nodes.get() key=|n| (n.folder.name.clone(), n.folder.subscribed, n.folder.total, n.folder.roles.clone()) children=row />
        </ul>
    }
}

fn trusted(app: App) -> impl IntoView {
    let row = move |email: String| {
        let remove = email.clone();
        view! {
            <li class="plain-row">
                <span class="grow">{email}</span>
                <button class="icon-button" aria-label="Remove" on:click=move |_| {
                    let remove = remove.clone();
                    app.update_settings(move |s| s.trusted_senders.retain(|t| *t != remove));
                }>{icon(icons::CLOSE)}</button>
            </li>
        }
    };
    // The field hands its text to `trust` on Enter or the button, then empties itself.
    let value = RwSignal::new(String::new());
    let submit = move || {
        let email = value.get_untracked().trim().to_owned();
        if !email.is_empty() {
            value.set(String::new());
            trust(app, &email);
        }
    };
    view! {
        <p class="muted small">"Mail from these addresses shows remote images without asking."</p>
        <ul class="plain-list">
            <For each=move || app.settings.with(|s| s.trusted_senders.clone()) key=String::clone children=row />
        </ul>
        <div class="row-actions">
            <input class="input grow" type="text" placeholder="name@example.com" aria-label="Add" bind:value=value
                on:keydown=move |e| if e.key() == "Enter" { submit() } />
            <button class="button" on:click=move |_| submit()>{icon(icons::PLUS)} "Add"</button>
        </div>
    }
}

/// The Time zone setting's choices: Auto, then every zone the browser knows (`Intl.supportedValuesOf`), each by its
/// current name, which moves a renamed one in the order (`Asia/Saigon` is `Asia/Ho_Chi_Minh`).
fn zones() -> Vec<(String, Option<String>)> {
    let zones = js_sys::Intl::supported_values_of(js_sys::Intl::SupportedValuesKey::TimeZone);
    let mut zones: Vec<String> = zones.iter().map(|z| current_zone(&String::from(z)).to_owned()).collect();
    zones.sort();
    let auto = (format!("Auto ({})", kubicle_browser::time::browser_zone()), None);
    std::iter::once(auto).chain(zones.into_iter().map(|z| (z.clone(), Some(z)))).collect()
}

/// A setting's labelled `select`.
pub fn choice<T: Clone + PartialEq + Send + Sync + 'static, L: IntoView + 'static, I: IntoIterator<Item = (L, T)>>(
    label: &'static str, options: impl Fn() -> I + Copy + Send + Sync + 'static, current: impl Fn() -> T + Copy + Send + Sync + 'static,
    pick: impl Fn(T) + Send + Sync + 'static,
) -> impl IntoView {
    view! { <label class="setting"><span>{label}</span>{select(label, options, current, pick)}</label> }
}

/// A choice among `options`, the one `current` names selected, handed to `pick` when it changes; the options are
/// read again when the signals they read change. Every `<select>` in the client is one of these.
pub fn select<T: Clone + PartialEq + Send + Sync + 'static, L: IntoView + 'static, I: IntoIterator<Item = (L, T)>>(
    label: &'static str, options: impl Fn() -> I + Copy + Send + Sync + 'static, current: impl Fn() -> T + Copy + Send + Sync + 'static,
    pick: impl Fn(T) + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <select class="input" aria-label=label on:change=move |e| {
            if let Some((_, v)) = event_target_value(&e).parse().ok().and_then(|i| options().into_iter().nth(i)) { pick(v) }
        }>
            {move || options().into_iter().enumerate().map(|(i, (name, v))| {
                view! { <option value=i.to_string() prop:selected=move || current() == v>{name}</option> }
            }).collect_view()}
        </select>
    }
}

fn switch(app: App, label: &'static str, get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> impl IntoView {
    view! {
        <label class="setting"><span>{label}</span>
            <input type="checkbox" prop:checked=move || app.settings.with(get)
                on:change=move |e| { let on = event_target_checked(&e); app.update_settings(move |s| set(s, on)); } />
        </label>
    }
}

/// Roundcube's choices for "Clear Trash on logout".
const PURGES: [(&str, LogoutPurge); 5] = [
    ("never", LogoutPurge::Never),
    ("all messages", LogoutPurge::All),
    ("older than 30 days", LogoutPurge::OlderThan(30)),
    ("older than 60 days", LogoutPurge::OlderThan(60)),
    ("older than 90 days", LogoutPurge::OlderThan(90)),
];

/// The archive plugin's labels (`plugins/archive/localization/en_US.inc`).
const DIVISIONS: [(&str, ArchiveType); 8] = [
    ("None", ArchiveType::None),
    ("Year (e.g. Archive/2012)", ArchiveType::Year),
    ("Month (e.g. Archive/2012/06)", ArchiveType::Month),
    ("Month - Thunderbird compatible (e.g. Archive/2012/2012-06)", ArchiveType::ThunderbirdMonth),
    ("Sender email", ArchiveType::Sender),
    ("Original folder", ArchiveType::Folder),
    ("Year and the original folder (e.g. Archive/2012/...)", ArchiveType::FolderYear),
    ("Year, month and the original folder (e.g. Archive/2012/06/...)", ArchiveType::FolderMonth),
];

/// Settings' cards, each under its heading, which the section list beside them names: kubicle's own, and between
/// them Roundcube's preference sections that apply to what kubicle does (program/actions/settings/index.php).
fn sections(app: App) -> [(&'static str, AnyView); 10] {
    let read_after = [
        ("never", None),
        ("immediately", Some(0)),
        ("after 5 seconds", Some(5)),
        ("after 10 seconds", Some(10)),
        ("after 20 seconds", Some(20)),
        ("after 30 seconds", Some(30)),
    ];
    // Roundcube's choices: none, or any folder but INBOX (`index.php`, its `onchange` guard), by real name, one
    // folder for several roles too; and kubicle's own first, the folder the server marks.
    let folder_for = move |(role, label): (FolderRole, &'static str)| {
        let names = move || {
            let choosable = |f: &&kubicle_api::Folder| f.selectable && !f.roles.contains(&FolderRole::Inbox);
            let names = app.folders.with(|f| f.iter().filter(choosable).map(|f| f.name.clone()).collect::<Vec<_>>());
            let first = [("As the server marks it".to_owned(), None), ("None".to_owned(), Some(None))];
            first.into_iter().chain(names.into_iter().map(|n| (n.clone(), Some(Some(n)))))
        };
        let current = move || app.settings.with(|s| s.special_folders.get(&role).cloned());
        let pick = move |name: Option<Option<String>>| {
            app.update_settings(move |s| match &name {
                Some(name) => drop(s.special_folders.insert(role, name.clone())),
                None => drop(s.special_folders.remove(&role)),
            });
        };
        choice(label, names, current, pick)
    };
    [
        ("General", general(app).into_any()),
        ("Mailbox View", view! {
            {choice("Mark previewed messages as read", move || read_after, move || app.settings.with(|s| s.mark_read_after), move |v| app.update_settings(move |s| s.mark_read_after = v))}
            {choice("Expand message threads", || [("never", ExpandThreads::Never), ("all", ExpandThreads::All), ("only with unread messages", ExpandThreads::Unread)], move || app.settings.with(|s| s.expand_threads), move |v| app.update_settings(move |s| s.expand_threads = v))}
            {rows_per_page(app)}
            {switch(app, "Display next message after delete or move", |s| s.display_next, |s, on| s.display_next = on)}
        }.into_any()),
        ("Displaying Messages", view! {
            {switch(app, "Show email address with display name", |s| s.show_email, |s, on| s.show_email = on)}
            {switch(app, "Display HTML", |s| s.prefer_html, |s, on| s.prefer_html = on)}
            {choice("Allow remote resources", || [("never", RemoteImages::Never), ("from trusted senders", RemoteImages::Trusted), ("always", RemoteImages::Always)], move || app.settings.with(|s| s.remote_images), move |v| app.update_settings(move |s| s.remote_images = v))}
        }.into_any()),
        ("Composing Messages", view! {
            {switch(app, "Compose in a new window", |s| s.compose_extwin, |s, on| s.compose_extwin = on)}
            {switch(app, "Always request a return receipt", |s| s.mdn_default, |s, on| s.mdn_default = on)}
            {switch(app, "Always request a delivery status notification", |s| s.dsn_default, |s, on| s.dsn_default = on)}
            {switch(app, "Place replies in the folder of the message being replied to", |s| s.reply_same_folder, |s, on| s.reply_same_folder = on)}
            {choice("When replying", || [("do not quote the original message", ReplyMode::Empty), ("start new message below the quote", ReplyMode::Below), ("start new message above the quote", ReplyMode::Above), ("start new message above the quote (no indentation)", ReplyMode::AboveNoIndent)], move || app.settings.with(|s| s.reply_mode), move |v| app.update_settings(move |s| s.reply_mode = v))}
            {choice("Forwarding mode", || [("inline", false), ("as attachment", true)], move || app.settings.with(|s| s.forward_attachment), move |v| app.update_settings(move |s| s.forward_attachment = v))}
            {choice("Default action of [Reply all] button", || [("reply to all", false), ("reply to mailing list only (if found)", true)], move || app.settings.with(|s| s.reply_all_list), move |v| app.update_settings(move |s| s.reply_all_list = v))}
        }.into_any()),
        ("Special Folders", view! {
            {[(FolderRole::Drafts, "Drafts"), (FolderRole::Sent, "Sent"), (FolderRole::Junk, "Junk"), (FolderRole::Trash, "Trash"), (FolderRole::Archive, "Archive")].map(folder_for).collect_view()}
            {choice("Divide archive by", || DIVISIONS, move || app.settings.with(|s| s.archive_type), move |v| app.update_settings(move |s| s.archive_type = v))}
        }.into_any()),
        ("Server Settings", view! {
            {switch(app, "Mark the message as read on delete", |s| s.read_when_deleted, |s, on| s.read_when_deleted = on)}
            {switch(app, "Directly delete messages in Junk", |s| s.delete_junk, |s, on| s.delete_junk = on)}
            {choice("Clear Trash on logout", || PURGES, move || app.settings.with(|s| s.logout_purge), move |v| app.update_settings(move |s| s.logout_purge = v))}
            {switch(app, "Mark the message as read on archive", |s| s.read_on_archive, |s, on| s.read_on_archive = on)}
        }.into_any()),
        ("Themes", themes(app).into_any()),
        ("Folders", folder_list(app).into_any()),
        ("Identities", identities(app).into_any()),
        ("Trusted senders", trusted(app).into_any()),
    ]
}
