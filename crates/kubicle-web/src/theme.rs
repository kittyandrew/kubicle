//! Themes are plain stylesheets that set tokens (docs/design.md, Themes). index.html holds two `<link>`s,
//! one per scheme; `auto` lets their `media` follow the system, a pinned mode switches one off.

use kubicle_api::{Settings, ThemeInfo, ThemeMode, ThemeSource};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::{state::local, state::set_local};

const CACHE_KEY: &str = "kubicle.theme";

/// The tokens render.rs's paper reads, so text mail in its frame reads as the app does, each with the token that
/// stands in for it in a theme without it.
const PAPER_TOKENS: [(&str, &str); 9] = [
    ("--bg", ""),
    ("--fg", ""),
    ("--fg-muted", ""),
    ("--link", ""),
    ("--hover", ""),
    ("--quote-1", "--link"),
    ("--quote-2", "--warning"),
    ("--quote-3", "--danger"),
    ("--font-mono", ""),
];

thread_local! {
    /// The theme's `PAPER_TOKENS` and colour scheme as a style for a frame's head, set again when the theme changes.
    static PAPER: RwSignal<String> = RwSignal::new(String::new());
}

/// `document` for the reader's frame; text mail takes the theme's colours, tracked.
pub fn paper(document: &str, text: bool) -> String {
    let style = PAPER.with(|p| p.get());
    if text { document.replacen("</head>", &format!("{style}</head>"), 1) } else { document.to_owned() } // the first is render.rs's own
}

/// With the content's hash, so the browser keeps the file for good; index.html's defaults carry it the same way.
pub fn url(theme: &ThemeInfo) -> String {
    let dir = match theme.source {
        ThemeSource::BuiltIn => "themes",
        ThemeSource::System => "system-themes",
        ThemeSource::User => "api/theme",
    };
    format!("{dir}/{}.css?v={}", theme.id, theme.version)
}

fn link(id: &str) -> Option<web_sys::HtmlLinkElement> { document().get_element_by_id(id)?.dyn_into().ok() }

fn set_links(light: &str, dark: &str, mode: ThemeMode) {
    let (light_media, dark_media) = match mode {
        ThemeMode::Light => ("all", "not all"),
        ThemeMode::Dark => ("not all", "all"),
        ThemeMode::Auto => ("(prefers-color-scheme: light)", "(prefers-color-scheme: dark)"),
    };
    for (id, href, media) in [("theme-light", light, light_media), ("theme-dark", dark, dark_media)] {
        let Some(link) = link(id) else { continue };
        if link.get_attribute("href").as_deref() != Some(href) {
            link.set_href(href);
        }
        link.set_media(media);
    }
}

/// Applies the choice the last session made, so a returning user does not see the default theme first.
pub fn apply_cached() {
    if let Some((light, dark, mode)) = local(CACHE_KEY).and_then(|c| kubicle_browser::api::parse::<(String, String, ThemeMode)>(&c).ok()) {
        set_links(&light, &dark, mode);
    }
    watch_theme_color();
}

pub fn apply(settings: &Settings, themes: &[ThemeInfo]) {
    let find = |id: &str, dark: bool| {
        themes.iter().find(|t| t.id == id).or_else(|| themes.iter().find(|t| t.dark == dark && t.source == ThemeSource::BuiltIn))
    };
    let light = find(&settings.light_theme, false).map_or_else(|| "themes/latte.css".into(), url);
    let dark = find(&settings.dark_theme, true).map_or_else(|| "themes/frappe.css".into(), url);
    set_links(&light, &dark, settings.theme_mode);
    update_theme_color(); // a pinned scheme only switches a stylesheet already loaded, which fires no `load`
    set_local(CACHE_KEY, &kubicle_browser::api::stringify(&(light, dark, settings.theme_mode)));
}

/// A change of the system's light or dark scheme drops a pinned Light or Dark, so the app follows the system again,
/// as Elastic deletes its `colorMode` cookie then.
pub fn unpin_on_system_change(app: crate::state::App) {
    let Ok(Some(query)) = window().match_media("(prefers-color-scheme: dark)") else { return };
    let unpin = Closure::<dyn Fn()>::new(move || {
        if app.settings.with_untracked(|s| s.theme_mode != ThemeMode::Auto) {
            app.update_settings(|s| s.theme_mode = ThemeMode::Auto);
        }
    });
    let _ = query.add_event_listener_with_callback("change", unpin.as_ref().unchecked_ref());
    unpin.forget(); // for the page's lifetime
}

/// Keeps the installed app's title bar (`theme-color`) on the theme's header colour, and `PAPER` on its tokens:
/// after either stylesheet loads and when the system scheme flips.
fn watch_theme_color() {
    let update = Closure::<dyn Fn()>::new(update_theme_color);
    for link in ["theme-light", "theme-dark"].into_iter().filter_map(link) {
        let _ = link.add_event_listener_with_callback("load", update.as_ref().unchecked_ref());
    }
    if let Ok(Some(query)) = window().match_media("(prefers-color-scheme: dark)") {
        let _ = query.add_event_listener_with_callback("change", update.as_ref().unchecked_ref());
    }
    update.forget(); // listeners for the page's lifetime
    update_theme_color();
}

fn update_theme_color() {
    let Some(root) = document().document_element() else { return };
    let Ok(Some(style)) = window().get_computed_style(&root) else { return };
    let value = |name: &str| style.get_property_value(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    // A value that could end the rule or the style element is left out, so a user theme cannot write into the frame.
    let safe = |v: &String| !v.contains(['<', '{', '}', ';']);
    let tokens = PAPER_TOKENS.iter().filter_map(|(t, or)| value(t).or_else(|| value(or)).filter(safe).map(|v| format!("{t}:{v}")));
    let tokens: Vec<String> = tokens.collect();
    let scheme = value("color-scheme").filter(safe).unwrap_or_else(|| "light".into());
    PAPER.with(|p| p.set(format!("<style>:root{{{}}}html{{color-scheme:{scheme}}}</style>", tokens.join(";"))));
    let color = style.get_property_value("--bg-header").unwrap_or_default();
    let Some(meta) = document().query_selector("meta[name=theme-color]").ok().flatten() else { return };
    if !color.trim().is_empty() {
        meta.unchecked_into::<web_sys::HtmlMetaElement>().set_content(color.trim());
    }
}
