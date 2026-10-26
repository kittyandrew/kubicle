//! The one modal dialog: the browser's own `<dialog>` shown modal, a titled box with a Close button, and Esc or a
//! click outside to close it. The browser makes the page behind it inert and moves the focus to its first control.

use leptos::{ev::MouseEvent, ev::PointerEvent, html, prelude::*};
use wasm_bindgen::{JsCast, closure::Closure};

use crate::{icons, icons::icon};

/// Gives the message list the focus when it fell to the page body because its holder went away, so a keyboard user
/// goes on from the list, not the top of the page. Checked at once, for a view this change removed (Compose closed
/// by Discard), and after this frame: the browser drops the focus of a control turned disabled or hidden on a later
/// task, which may come after the frame's callbacks, so `lost` counts such a control as gone already.
pub fn keep_focus() {
    let repair = || {
        let lost =
            |e: web_sys::Element| e.tag_name() == "BODY" || e.matches(":disabled").unwrap_or(false) || e.get_client_rects().length() == 0;
        if document().active_element().is_none_or(lost) {
            give_focus();
        }
    };
    repair();
    request_animation_frame(repair);
}

/// Hands the focus on as soon as a redraw removes or disables the element holding it, whatever signal drew it, as
/// the message redrawn with its images removes "Always for" and a page turn's late answer disables Next on the last
/// page: `keep_focus` waits a frame, and a key pressed meanwhile reached the page body. The observer's callback runs
/// once the DOM changed, before any frame.
pub fn keep_focus_on_removal() {
    let held = StoredValue::new_local(None::<web_sys::Element>);
    let _focus = window_event_listener(leptos::ev::focusin, move |e| held.set_value(e.target().and_then(|t| t.dyn_into().ok())));
    let changed = Closure::<dyn Fn()>::new(move || {
        let Some(el) = held.get_value() else { return };
        let lost = !el.is_connected() || el.matches(":disabled").unwrap_or(false);
        // A control just disabled still holds the focus here: the browser drops it to the body on a later task.
        if lost && document().active_element().is_none_or(|active| active.tag_name() == "BODY" || active == el) {
            give_focus();
        }
    });
    let options = web_sys::MutationObserverInit::new();
    options.set_child_list(true);
    options.set_subtree(true);
    options.set_attributes(true);
    options.set_attribute_filter(&js_sys::Array::of1(&"disabled".into())); // `prop:disabled` sets the attribute too
    if let (Ok(observer), Some(body)) = (web_sys::MutationObserver::new(changed.as_ref().unchecked_ref()), document().body()) {
        let _ = observer.observe_with_options(&body, &options);
    }
    changed.forget(); // for the page's lifetime
}

/// The list, else what shows in its place: the folders at 769 to 1200 px, else the toolbar.
fn give_focus() {
    for place in [".list", ".folder[tabindex='0']", ".toolbar button.tool:enabled"] {
        let shown = document().query_selector(place).ok().flatten().map(JsCast::unchecked_into::<web_sys::HtmlElement>);
        if let Some(el) = shown.filter(|el| el.offset_parent().is_some()) {
            let _ = el.focus();
            return;
        }
    }
}

/// Within this many px of its border, on either side, a press resizes the dialog from that edge or corner, as
/// Roundcube's jQuery UI handles do; the title bar moves it. The outer side lies past the dialog's scrollbar, whose
/// presses Chromium keeps from the page.
const GRIP: f64 = 6.0;
/// The smallest box a resize leaves, in px: base.css's `min-width` and `min-height`.
const MIN: (f64, f64) = (256.0, 128.0);

/// The edges a point lies near: left, top, right, bottom.
fn edges(r: &web_sys::DomRect, x: f64, y: f64) -> [bool; 4] {
    let near = |at: f64, edge: f64| (at - edge).abs() < GRIP;
    let (across, down) = (x > r.left() - GRIP && x < r.right() + GRIP, y > r.top() - GRIP && y < r.bottom() + GRIP);
    [near(x, r.left()) && down, near(y, r.top()) && across, near(x, r.right()) && down, near(y, r.bottom()) && across]
}

#[component]
pub fn Dialog(
    #[prop(into)] open: Signal<bool>, close: impl Fn() + Copy + Send + Sync + 'static, #[prop(into)] title: TextProp,
    #[prop(optional)] class: &'static str, children: ChildrenFn,
) -> impl IntoView {
    let dialog = NodeRef::<html::Dialog>::new();
    // Each open draws the dialog anew, as a closed one is not in the page.
    Effect::new(move || _ = dialog.get().filter(|d| !d.open()).map(|d| d.show_modal()));
    Effect::new(move |was: Option<bool>| {
        let now = open.get();
        if !now && was == Some(true) {
            keep_focus(); // the opener may be gone, as Discard's is once its answer closed Compose
        }
        now
    });
    // The press a pointer began, while it moves or resizes the dialog: where, the box then, and the edges it drags
    // (left, top, right, bottom; none moves the whole box).
    let grab = StoredValue::new_local(None::<(f64, f64, web_sys::DomRect, [bool; 4])>);
    // A click on the backdrop lands on the dialog itself, so it closes the dialog only when the press began there
    // too: a drag that ends outside the box, or a click from the keys at (0, 0), is no backdrop click.
    let pressed_outside = StoredValue::new(false);
    let on_backdrop = |e: &MouseEvent| e.target().is_some_and(|t| t.is_instance_of::<web_sys::HtmlDialogElement>());
    let press = move |e: PointerEvent| {
        let Some(d) = dialog.get_untracked() else { return };
        let (r, x, y) = (d.get_bounding_client_rect(), f64::from(e.client_x()), f64::from(e.client_y()));
        let on = |selector| e.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()?.closest(selector).ok()?).is_some();
        let edges = edges(&r, x, y);
        let outside = x < r.left() || x > r.right() || y < r.top() || y > r.bottom();
        pressed_outside.set_value(outside && !edges.contains(&true));
        if edges.contains(&true) || (!outside && on(".dialog-head") && !on("button")) {
            grab.set_value(Some((x, y, r, edges)));
            let _ = d.set_pointer_capture(e.pointer_id());
            e.prevent_default(); // no text selection while it drags
        }
    };
    let drag = move |e: PointerEvent| {
        let Some(d) = dialog.get_untracked() else { return };
        let (px, py) = (f64::from(e.client_x()), f64::from(e.client_y()));
        let Some((x0, y0, start, [left, top, right, bottom])) = grab.get_value() else {
            let cursor = match edges(&d.get_bounding_client_rect(), px, py) {
                [true, true, ..] | [.., true, true] => "nwse-resize",
                [true, _, _, true] | [_, true, true, _] => "nesw-resize",
                [true, ..] | [_, _, true, _] => "ew-resize",
                [_, true, ..] | [.., true] => "ns-resize",
                _ => "",
            };
            let _ = web_sys::HtmlElement::style(&d).set_property("cursor", cursor);
            return;
        };
        let moving = !(left || top || right || bottom);
        let mut sides = [start.left(), start.top(), start.right(), start.bottom()];
        let dy = if moving { (py - y0).max(-start.top()) } else { py - y0 }; // the title bar stays on screen
        for (side, dragged, delta) in [(0, left, px - x0), (1, top, dy), (2, right, px - x0), (3, bottom, dy)] {
            if moving || dragged {
                sides[side] += delta;
            }
        }
        let [x1, y1, x2, y2] = sides;
        let (x1, y1) = (if left { x1.min(x2 - MIN.0) } else { x1 }, if top { y1.min(y2 - MIN.1) } else { y1 });
        let (x2, y2) = (if right { x2.max(x1 + MIN.0) } else { x2 }, if bottom { y2.max(y1 + MIN.1) } else { y2 });
        let place = format!("margin: 0; inset: auto; left: {x1}px; top: {y1}px; width: {}px; height: {}px", x2 - x1, y2 - y1);
        let _ = d.set_attribute("style", &format!("{place}; max-width: none; max-height: none"));
    };
    view! {
        <Show when=move || open.get()>
            // Closed before it leaves the page, the browser gives the focus back to what held it when it opened.
            {on_cleanup(move || _ = dialog.try_get_untracked().flatten().map(|d| d.close()))}
            <dialog class=format!("dialog {class}") aria-label=title.clone() node_ref=dialog on:pointerdown=press on:pointermove=drag
                on:cancel=move |e: web_sys::Event| { e.prevent_default(); close(); }
                on:click=move |e: MouseEvent| if pressed_outside.get_value() && on_backdrop(&e) { close() }
                on:pointerup=move |_| grab.set_value(None) on:pointercancel=move |_| grab.set_value(None)>
                {children()}
                // After the content, so the browser's first focus is the content's first control; base.css draws it
                // on top.
                <header class="dialog-head">
                    <h2>{ let title = title.clone(); move || title.get() }</h2>
                    <button class="icon-button" aria-label="Close" on:click=move |_| close()>{icon(icons::CLOSE)}</button>
                </header>
            </dialog>
        </Show>
    }
}

/// A dialog's form: its fields, then the button that submits it, named `label`, and Cancel. `busy` turns the submit
/// button off while the answer is on its way.
#[component]
pub fn DialogForm(
    label: &'static str, #[prop(optional)] glyph: Option<&'static str>, #[prop(optional, into)] busy: Signal<bool>,
    submit: impl Fn() + 'static, cancel: impl Fn() + 'static, children: Children,
) -> impl IntoView {
    view! {
        <form on:submit=move |e| { e.prevent_default(); submit(); }>
            <div class="dialog-body">{children()}</div>
            <footer class="dialog-buttons">
                <button type="submit" class="button primary" prop:disabled=move || busy.get()>{glyph.map(icon)} {label}</button>
                <button type="button" class="button" on:click=move |_| cancel()>"Cancel"</button>
            </footer>
        </form>
    }
}
