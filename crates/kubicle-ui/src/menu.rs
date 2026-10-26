//! Every menu in the app. Its content is data, `MenuItem`s, and `Menu` alone renders them: each entry is a button
//! that acts in its own click handler and only then closes the menu, as in Roundcube. Nothing in a menu can depend
//! on a default action (a label's click reaching its input) that would run after the menu closed and disposed of
//! its elements.

use std::sync::Arc;

use leptos::{ev::KeyboardEvent, html, prelude::*};
use wasm_bindgen::JsCast;

use crate::{icons, icons::icon};

type Run = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
enum Kind {
    Action,
    Check(bool),
    Separator,
    /// Shows these entries in the menu's place, as Move to... does in More.
    Submenu(Vec<MenuItem>),
}

#[derive(Clone)]
pub struct MenuItem {
    kind: Kind,
    label: String,
    glyph: Option<&'static str>,
    depth: usize,
    enabled: bool,
    /// The keyboard shortcut for the same action, shown beside it.
    key: Option<&'static str>,
    run: Option<Run>,
}

impl MenuItem {
    fn new(kind: Kind, label: impl Into<String>, run: Option<Run>) -> Self {
        Self { kind, label: label.into(), glyph: None, depth: 0, enabled: true, key: None, run }
    }

    pub fn action(label: impl Into<String>, run: impl Fn() + Send + Sync + 'static) -> Self {
        Self::new(Kind::Action, label, Some(Arc::new(run)))
    }

    pub fn check(label: &str, checked: bool, run: impl Fn() + Send + Sync + 'static) -> Self {
        Self::new(Kind::Check(checked), label, Some(Arc::new(run)))
    }

    #[must_use]
    pub fn separator() -> Self { Self::new(Kind::Separator, "", None) }

    pub fn submenu(label: impl Into<String>, items: Vec<MenuItem>) -> Self { Self::new(Kind::Submenu(items), label, None) }

    #[must_use]
    pub fn glyph(self, glyph: &'static str) -> Self { Self { glyph: Some(glyph), ..self } }

    /// A disabled entry still shows, as in Roundcube, but does nothing and the arrow keys pass it by.
    #[must_use]
    pub fn enabled(self, enabled: bool) -> Self { Self { enabled, ..self } }

    /// Indents the entry, as a subfolder in a folder list.
    #[must_use]
    pub fn depth(self, depth: usize) -> Self { Self { depth, ..self } }

    #[must_use]
    pub fn key(self, key: &'static str) -> Self { Self { key: Some(key), ..self } }
}

/// A toolbar button, an icon over an optional label, that opens a menu.
#[component]
pub fn Dropdown(
    label: &'static str, glyph: &'static str, #[prop(optional)] text: Option<&'static str>, #[prop(optional)] align_right: bool,
    #[prop(optional, into)] disabled: Signal<bool>, items: impl Fn() -> Vec<MenuItem> + Send + Sync + 'static,
) -> impl IntoView {
    let trigger = view! { {icon(glyph)} {text.map(|t| view! { <span class="tool-label">{t}</span> })} };
    view! { <Menu class="tool" label=label align_right=align_right disabled=disabled trigger=trigger items=items /> }
}

/// A button showing `trigger` that opens a menu of `items` below it.
#[component]
pub fn Menu(
    class: &'static str, #[prop(into)] label: String, trigger: impl IntoView + 'static, #[prop(optional)] align_right: bool,
    #[prop(optional, into)] disabled: Signal<bool>, items: impl Fn() -> Vec<MenuItem> + Send + Sync + 'static,
) -> impl IntoView {
    let items: Items = Arc::new(items);
    let open = RwSignal::new(false);
    let button = NodeRef::<html::Button>::new();
    view! {
        <span class="dropdown">
            // `type="button"`: inside a form, a plain button would submit it.
            <button type="button" class=class title=label.clone() aria-label=label aria-haspopup="menu" node_ref=button
                aria-expanded=move || if open.get() { "true" } else { "false" } prop:disabled=move || disabled.get()
                on:click=move |_| open.update(|o| *o = !*o)>{trigger}</button>
            <Show when=move || open.get()>
                {popup(Arc::clone(&items), if align_right { "right" } else { "" }, button, move || open.set(false))}
            </Show>
        </span>
    }
}

/// A context menu at a point of the page, while `at` holds one.
pub fn context_menu(at: RwSignal<Option<(i32, i32)>>, items: impl Fn() -> Vec<MenuItem> + Send + Sync + 'static) -> impl IntoView {
    let items: Items = Arc::new(items);
    view! {
        <Show when=move || at.with(Option::is_some)>
            {at.get_untracked().map(|(x, y)| view! { <span class="menu-point" style=format!("left: {x}px; top: {y}px")></span> })}
            {popup(Arc::clone(&items), "context", NodeRef::new(), move || at.set(None))}
        </Show>
    }
}

type Items = Arc<dyn Fn() -> Vec<MenuItem> + Send + Sync>;

/// The open menu, the one renderer of entries: a popover the browser places below `source`, its implicit anchor, or
/// at the context menu's point, flipped where it would cross the viewport's edge. A submenu takes the menu's place.
/// Up and Down move between entries, Enter acts, Esc closes.
#[allow(clippy::too_many_lines)] // the menu's state, its keys and the entries it draws belong together
fn popup(
    items: Items, place: &'static str, source: NodeRef<html::Button>, close: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let sub = RwSignal::new(None::<Vec<MenuItem>>);
    let list = NodeRef::<html::Div>::new();
    // `try_`: a focus queued for the next frame may find the menu closed and gone.
    let entries = move || {
        let menu = list.try_get_untracked().flatten()?;
        let found = menu.query_selector_all("button.menu-item:not([aria-disabled])").ok()?;
        Some((0..found.length()).filter_map(|i| found.item(i)?.dyn_into::<web_sys::HtmlElement>().ok()).collect::<Vec<_>>())
    };
    let focus = move |step: isize| {
        let Some(entries) = entries().filter(|e| !e.is_empty()) else {
            // No entry takes the focus, as when every one is disabled: the menu holds it, so Esc still closes it.
            if let Some(menu) = list.try_get_untracked().flatten() {
                let _ = menu.focus();
            }
            return;
        };
        let active = document().active_element();
        let at = entries.iter().position(|e| e.is_same_node(active.as_ref().map(AsRef::as_ref)));
        let next = at.map_or(0, |i| (i.cast_signed() + step).rem_euclid(entries.len().cast_signed()).cast_unsigned());
        let _ = entries[next].focus();
    };
    // Hiding gives the focus back to what held it before the menu opened, so the keyboard never falls to the page
    // body; `toggle` then ends the menu.
    let hide = move || drop(list.try_get_untracked().flatten().map(|menu| menu.hide_popover()));
    // A context menu opens on the press itself, whose release the browser would take for a click outside, so it
    // closes on the next press outside instead.
    let context = place == "context";
    let outside = context.then(|| {
        window_event_listener(leptos::ev::pointerdown, move |e| {
            if !list.get_untracked().is_some_and(|m| m.contains(e.target().as_ref().and_then(|t| t.dyn_ref()))) {
                hide();
            }
        })
    });
    on_cleanup(move || outside.into_iter().for_each(WindowListenerHandle::remove));
    Effect::new(move |_| {
        if let Some(menu) = list.get() {
            let options = web_sys::ShowPopoverOptions::new();
            if let Some(button) = source.get_untracked() {
                options.set_source(&button); // its clicks then toggle the menu, and it anchors the menu
            }
            let _ = menu.show_popover_with_options(&options);
            request_animation_frame(move || focus(0));
        }
    });
    let keys = move |e: KeyboardEvent| {
        e.stop_propagation(); // every key: the window's would act on the list and the reader behind the menu
        match e.key().as_str() {
            "ArrowDown" => focus(1),
            "ArrowUp" => focus(-1),
            "Escape" | "Tab" => hide(),
            _ => return, // Enter and Space keep their default, the focused entry's click
        }
        e.prevent_default();
    };
    // `lead`: some entry has a check or an icon, so every entry keeps a slot for one and the labels line up.
    let entry = move |item: MenuItem, lead: bool| match item.kind {
        Kind::Separator => view! { <div class="menu-sep" role="separator"></div> }.into_any(),
        Kind::Action | Kind::Check(_) | Kind::Submenu(_) => {
            let (role, checked) = if let Kind::Check(on) = item.kind { ("menuitemcheckbox", Some(on)) } else { ("menuitem", None) };
            let (run, name, enabled) = (item.run.clone(), item.label.clone(), item.enabled);
            let inner = if let Kind::Submenu(items) = item.kind { Some(items) } else { None };
            let glyph = if checked == Some(true) { Some(icons::CHECK) } else { item.glyph };
            view! {
                // `type="button"`: a menu inside a form, as Redirect's, must not submit it.
                <button type="button" class="menu-item" role=role name=name
                    aria-checked=checked.map(|on| if on { "true" } else { "false" }) style=format!("--depth: {}", item.depth)
                    aria-disabled=(!enabled).then_some("true") aria-haspopup=inner.is_some().then_some("menu")
                    aria-keyshortcuts=item.key // the <kbd> is hidden, so the name stays the label alone
                    on:click=move |_| {
                        if !enabled { return; }
                        if let Some(items) = &inner {
                            sub.set(Some(items.clone()));
                            // The menu holds the focus, and its keys, until the new entries are drawn.
                            if let Some(menu) = list.get_untracked() { let _ = menu.focus(); }
                            request_animation_frame(move || focus(0));
                            return;
                        }
                        if let Some(run) = &run { run(); }
                        hide();
                    }>
                    {lead.then(|| view! { <span class="menu-lead">{glyph.map(icon)}</span> })}
                    <span class="grow">{item.label}</span>
                    {item.key.map(|k| view! { <kbd aria-hidden="true">{k}</kbd> })}
                </button>
            }
            .into_any()
        }
    };
    view! {
        <div class=format!("menu {place}") role="menu" tabindex="-1" popover=if context { "manual" } else { "auto" }
            node_ref=list on:keydown=keys
            on:toggle=move |_| if list.get_untracked().is_some_and(|m| !m.matches(":popover-open").unwrap_or(false)) { close() }>
            {move || {
                let entries = sub.get().unwrap_or_else(|| items());
                let lead = entries.iter().any(|i| i.glyph.is_some() || matches!(i.kind, Kind::Check(_)));
                entries.into_iter().map(|i| entry(i, lead)).collect_view()
            }}
        </div>
    }
}
