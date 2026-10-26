//! Compose: plain text only. Every change goes to the browser's storage at once, and to IMAP Drafts through the
//! server about ten seconds after typing pauses (docs/design.md, Drafts never lose text). Each Compose has an id
//! of its own, as Roundcube's `compose_id`: its browser copy is stored under it, and the address names it, so two
//! tabs never share a copy and a reload stays in Compose.

use std::{rc::Rc, time::Duration};

use kubicle_api::{Address, Change, ComposeMode, Draft, DraftAttachment, DraftCopy, FolderRole, MAX_UPLOAD_MIB, Messages, Priority};
use kubicle_api::{Priority::High, Priority::Highest, Priority::Low, Priority::Lowest, Priority::Normal, ReplyMode, SentCopy};
use kubicle_api::{TEXT_ALLOWANCE, encoded_size, largest_file};
use kubicle_browser::{api, api::enc, text};
use kubicle_ui::{icons, icons::icon, menu::Menu, menu::MenuItem};
use leptos::{ev::DragEvent, ev::KeyboardEvent, html, prelude::*, task::spawn_local};
use serde::{Deserialize, Serialize};

use crate::{settings::select, state::App, state::Ask, state::Pane, state::Reply, state::View, state::local};
use crate::{state::set_local, state::storage};

/// An address field's text: the addresses, then a separator to type the next one after.
fn list_text(list: &[Address]) -> String { if list.is_empty() { String::new() } else { text::address_list(list) + ", " } }

const SAVE_DELAY: Duration = Duration::from_secs(10);

/// Roundcube's `max_disclosed_recipients`, which Mailu leaves at its default: more To and Cc than this asks first.
const MAX_DISCLOSED: usize = 5;

const PRIORITIES: [(&str, Priority); 5] = [("Lowest", Lowest), ("Low", Low), ("Normal", Normal), ("High", High), ("Highest", Highest)];

/// How often an open Compose marks its browser copy as held, and how long a mark lasts: a copy no tab marked for
/// that long belongs to a tab that is gone. Long, since a browser runs a hidden tab's timers once a minute.
const ALIVE_EVERY: Duration = Duration::from_secs(30);
const ALIVE_FOR: f64 = 180_000.0;

/// A Compose's copy in this browser. Only an `edited` one is saved to Drafts or offered back after its tab is gone;
/// `alive` is when its tab last marked it, in ms since the epoch. `forwards_bcc` keeps the Bcc warning of what it
/// forwards whole (`kubicle_api::Prefill`) across a reload. A `pending` one was left while signed out, and goes to
/// Drafts at the next start, unasked.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Stored {
    draft: Draft,
    edited: bool,
    alive: f64,
    forwards_bcc: bool,
    pending: bool,
}

pub fn new_id() -> String {
    let digits = || js_sys::Math::random().to_string().chars().filter(char::is_ascii_digit).collect::<String>();
    digits() + &digits()
}

/// `account` is `App::account`, so an account made anew under the address never finds the last one's copies.
fn prefix(account: &str) -> String { format!("kubicle.draft.{account}.") }

fn stored(account: &str, id: &str) -> Option<Stored> { api::parse(&local(&(prefix(account) + id))?).ok() }

/// Every browser copy of this account's, by compose id. Ids are digits (`new_id`): another address that starts with
/// this one, `a@b.c.d` beside `a@b.c`, shares the prefix.
fn all_stored(account: &str) -> Vec<(String, Stored)> {
    let (Some(s), prefix) = (storage(), prefix(account)) else { return Vec::new() };
    let keys: Vec<String> = (0..s.length().unwrap_or(0)).filter_map(|i| s.key(i).ok().flatten()).collect();
    let ids =
        keys.into_iter().filter_map(|k| k.strip_prefix(&prefix).filter(|id| id.bytes().all(|b| b.is_ascii_digit())).map(str::to_owned));
    ids.filter_map(|id| Some((id.clone(), stored(account, &id)?))).collect()
}

fn store(account: &str, id: &str, draft: Draft, edited: bool, forwards_bcc: bool) {
    put(account, id, &Stored { draft, edited, alive: js_sys::Date::now(), forwards_bcc, pending: false });
}

fn put(account: &str, id: &str, copy: &Stored) { set_local(&(prefix(account) + id), &api::stringify(copy)); }

fn forget(account: &str, id: &str) { let _ = storage().map(|s| s.remove_item(&(prefix(account) + id))); }

/// No tab marked the copy for `ALIVE_FOR`: the tab that held it is gone.
fn gone(copy: &Stored) -> bool { js_sys::Date::now() - copy.alive > ALIVE_FOR }

/// Frees the server's copies of `files` once Compose `compose`'s save on its way, which reads them, is answered.
/// What a failure leaves goes a month on, at the server's next start.
async fn drop_uploads(app: App, compose: String, files: Vec<DraftAttachment>) {
    let ids: Vec<String> = files.into_iter().map(|f| f.id).collect();
    if ids.is_empty() {
        return;
    }
    let _ = app.draft_copy(&compose, None).await;
    let _ = api::post("api/upload/remove", &ids).await;
}

/// Leaving the draft, by any way out: a last save to the server if it was edited, after which the browser copy
/// and the uploads can go; signed out, the copy waits for the sign-in, 2 s on. Send and Discard forget the browser
/// copy first, so nothing is saved after them; a send on its way (`sending`) frees the uploads itself, or keeps them
/// for the restore question. A compose window that `close` is closing closes only then: sooner cuts the save off.
fn leave(app: App, id: &str, sending: bool) {
    let (account, id, closing) = (app.account(), id.to_owned(), app.take_closing());
    let shut = move || {
        if closing {
            let _ = window().close();
        }
    };
    let Some(local) = stored(&account, &id) else { return shut() };
    if local.edited && api::is_signed_out() {
        put(&account, &id, &Stored { pending: true, ..local });
        return shut();
    }
    spawn_local(async move {
        if local.edited {
            save_kept(app, id, local.draft).await;
        } else {
            forget(&account, &id);
            if !sending {
                drop_uploads(app, id, local.draft.attachments).await;
            }
        }
        shut();
    });
}

/// Saves the browser copy `id` to Drafts, then forgets it and frees its uploads, as the saved copy holds the files
/// and reopening it uploads them again; a refused save keeps both, for the restore question.
async fn save_kept(app: App, id: String, draft: Draft) {
    let files = draft.attachments.clone();
    match app.save_draft(&id, draft).await {
        Ok(_) => {
            forget(&app.account(), &id);
            app.notify("Draft saved.".into());
            drop_uploads(app, id, files).await;
        }
        Err(e) => app.notify(format!("Draft kept in this browser only: {}", e.0)),
    }
}

/// After sign-in: copies left while signed out go to Drafts, unasked. Then Compose again on the copy the address
/// names (`#draft=`), as a reload of Compose stays in it; else the newest edited copy whose tab is gone, offered as
/// Roundcube's restore dialog offers it.
pub fn restore(app: App) {
    let account = app.account();
    for (id, copy) in all_stored(&account).into_iter().filter(|(_, c)| c.pending) {
        store(&account, &id, copy.draft.clone(), true, copy.forwards_bcc); // taken, so another tab starting now leaves it
        spawn_local(save_kept(app, id, copy.draft));
    }
    if let Some((copy, id)) = app.take_reopen().and_then(|id| Some((stored(&account, &id)?, id))) {
        return app.open_compose_as(id, copy.draft, copy.forwards_bcc);
    }
    let newest = all_stored(&account).into_iter().filter(|(_, c)| c.edited && gone(c)).max_by(|a, b| a.1.alive.total_cmp(&b.1.alive));
    let Some((id, copy)) = newest else { return };
    let subject = if copy.draft.subject.trim().is_empty() { "(no subject)" } else { copy.draft.subject.trim() };
    #[allow(clippy::cast_possible_truncation)] // ms since the epoch, well inside an i64
    let saved = kubicle_browser::time::long((copy.alive / 1000.0) as i64);
    let question = format!("A message you did not send, \"{subject}\", was kept in this browser on {saved}. Restore it?");
    let ask = Ask { alt: Some("Delete"), no: Some("Ignore"), danger: false, ..Ask::confirm("Restore message", question, "Restore") };
    let (account, kept) = (account.clone(), id.clone());
    app.ask_reply(ask, move |reply| match reply {
        Reply::Yes(_) => app.open_compose_as(id, copy.draft, copy.forwards_bcc),
        Reply::Alt => {
            forget(&account, &kept);
            spawn_local(drop_uploads(app, kept, copy.draft.attachments));
            restore(app);
        }
        Reply::Cancel => {}
    });
}

/// Sign-out: an edited copy in this browser goes to Drafts first, in turn with a save of it on its way, then every
/// copy goes, as Roundcube's `clear_compose_data`, so no text stays behind on a shared computer. Their uploads go
/// too, as Roundcube's go with its session, but those of a Compose open in another tab, which carries on there
/// after the next sign-in.
pub async fn sign_out(app: App) {
    let (account, open) = (app.account(), OPEN.with_borrow(Clone::clone));
    let drafts = app.role_folder(FolderRole::Drafts).is_some();
    for (id, copy) in all_stored(&account) {
        if copy.edited && drafts {
            let _ = app.save_draft(&id, copy.draft.clone()).await; // best effort: the user is leaving
        }
        forget(&account, &id);
        if open.as_ref() == Some(&id) || gone(&copy) {
            drop_uploads(app, id, copy.draft.attachments).await;
        }
    }
}

/// Whether Compose may be left now, by any way out but Send and "Open in new window": Close, Esc, Back, a folder,
/// Settings, another Compose, Sign out. With no Drafts folder (Special Folders' None) an edited draft cannot be
/// saved, so this asks Roundcube's `notsentwarning` first; its Discard forgets the browser copy and `retry` leaves.
pub fn may_leave(app: App, retry: impl FnOnce() + Send + Sync + 'static) -> bool {
    let account = app.account();
    let Some(id) = OPEN.with_borrow(Clone::clone) else { return true };
    let Some(copy) = stored(&account, &id).filter(|c| c.edited && app.role_folder(FolderRole::Drafts).is_none()) else { return true };
    let question = "The message has not been sent and has unsaved changes. Do you want to discard your changes?";
    app.ask(Ask::confirm("Are you sure...", question.into(), "Discard"), move |_| {
        forget(&account, &id);
        spawn_local(drop_uploads(app, id, copy.draft.attachments));
        retry();
    });
    false
}

/// Drops the draft: its copy, `copy` or the one a save on its way makes, goes to Trash when the draft was `opened`
/// from Drafts, else for good, and its uploads `files` go. A compose window stays until the server has the changes,
/// which its closing would cut off.
fn discard(app: App, id: &str, copy: Option<DraftCopy>, opened: bool, files: Vec<DraftAttachment>) {
    forget(&app.account(), id);
    let (id, window) = (id.to_owned(), app.compose_window());
    if !window {
        close(app);
    }
    spawn_local(async move {
        if let Some(copy) = app.draft_copy(&id, copy).await {
            let (folder, messages) = (copy.folder.clone(), Messages::Uids(vec![copy.uid]));
            let change = match app.role_folder(FolderRole::Trash).filter(|_| opened) {
                Some(to) => Change::Move { folder, messages, to },
                None => Change::Delete { folder, messages },
            };
            app.draft_copy_gone(&copy); // moved to Trash or deleted, as a save would replace it
            if let Err(e) = app.post_change(&change).await {
                app.notify(format!("The draft stays on the server: {}", e.0));
            }
        }
        drop_uploads(app, id, files).await;
        if window {
            close(app);
        }
    });
}

/// Leaves Compose for Mail; a compose window then closes, as Roundcube's `extwin` does, once `leave` saved the
/// draft. A browser may refuse to close a tab no script opened, and that tab stays on Mail.
pub fn close(app: App) {
    if !may_leave(app, move || close(app)) {
        return;
    }
    if app.compose_window() {
        app.close_after_leaving();
    }
    app.view.set(View::Mail);
}

#[component]
#[allow(clippy::too_many_lines)] // the draft, its saving and its form belong together
pub fn Compose() -> impl IntoView {
    let app = expect_context::<App>();
    let from_choices = App::addresses();
    let (id, start, forwards_bcc) = app.take_seed();
    let id = StoredValue::new(id);
    let reply = matches!(start.sources.first().map(|s| s.mode), Some(ComposeMode::Reply | ComposeMode::ReplyAll | ComposeMode::ReplyList));
    let (copy, from, to) = (RwSignal::new(start.copy.clone()), RwSignal::new(start.from.to_string()), RwSignal::new(list_text(&start.to)));
    // Roundcube's add-header fields: each shows once it has text or its button was pressed.
    let extras = [("Cc", &start.cc), ("Bcc", &start.bcc), ("Reply-To", &start.reply_to), ("Followup-To", &start.followup_to)]
        .map(|(label, list)| (label, RwSignal::new(list_text(list)), RwSignal::new(!list.is_empty())));
    let [cc, bcc, reply_to, followup_to] = extras.map(|(_, text, _)| text);
    let [_, bcc_shown, reply_to_shown, _] = extras.map(|(_, _, shown)| shown);
    let (subject, body) = (RwSignal::new(start.subject.clone()), RwSignal::new(start.body.clone()));
    let attachments = RwSignal::new(start.attachments.clone());
    let (priority, sent_copy) = (RwSignal::new(start.priority), RwSignal::new(start.sent_copy.clone()));
    let [return_receipt, delivery_status, keep_formatting] =
        [start.return_receipt, start.delivery_status, start.keep_formatting].map(RwSignal::new);
    // The identity whose Reply-To and Bcc are in the fields, which a change of From swaps for the new one's.
    let identity = StoredValue::new(start.from.email.to_lowercase());
    let base = StoredValue::new(start);
    let (uploads, status, sending) = (RwSignal::new(Vec::<Uploading>::new()), RwSignal::new(""), RwSignal::new(false));
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let stop_timer = move || timer.get_value().inspect(TimeoutHandle::clear);
    let (to_ref, subject_ref) = (NodeRef::<html::Input>::new(), NodeRef::<html::Input>::new());
    // "Open in new window" gave this Compose to another tab, which carries on from its browser copy.
    let handed_off = StoredValue::new(false);
    let (body_ref, attach_ref) = (NodeRef::<html::Textarea>::new(), NodeRef::<html::Button>::new());

    // The address fields keep the text as typed; parsing it is lenient, so half an address survives.
    let parsed = |field: RwSignal<String>| text::parse_addresses(&field.get_untracked());
    let current = move || Draft {
        copy: copy.get_untracked(),
        from: parsed(from).into_iter().next().unwrap_or_else(|| base.with_value(|d| d.from.clone())),
        to: parsed(to),
        cc: parsed(cc),
        bcc: parsed(bcc),
        reply_to: parsed(reply_to),
        followup_to: parsed(followup_to),
        subject: subject.get_untracked(),
        body: body.get_untracked(),
        attachments: attachments.get_untracked(),
        priority: priority.get_untracked(),
        return_receipt: return_receipt.get_untracked(),
        delivery_status: delivery_status.get_untracked(),
        sent_copy: sent_copy.get_untracked(),
        keep_formatting: keep_formatting.get_untracked(),
        ..base.get_value()
    };
    // As Roundcube's `compose_field_hash`: a draft is saved only once it differs from how Compose opened, so
    // opening and closing an empty or untouched form leaves nothing behind.
    let opened = StoredValue::new(current());
    // A copy restored after a reload may hold text the server never got, so it counts as edited from the start.
    let restored = stored(&app.account(), &id.get_value()).is_some_and(|c| c.edited);
    // Compared without the copy, which each save to Drafts replaces with no edit made.
    let edited = move || restored || opened.with_value(|o| Draft { copy: o.copy.clone(), ..current() } != *o);
    let save_to_server = move || {
        // Signed out, the save after the sign-in takes it from the browser copy; with no Drafts folder nothing goes to
        // the server, as Roundcube offers no Save then.
        if api::is_signed_out() || app.role_folder(FolderRole::Drafts).is_none() {
            return status.set("Saved in this browser");
        }
        status.set("Saving...");
        let (draft, compose) = (current(), id.get_value());
        spawn_local(async move {
            let result = app.save_draft(&compose, draft).await;
            if copy.is_disposed() || stored(&app.account(), &compose).is_none() {
                return; // left meanwhile, and `leave` saved the last text, or Discard dropped the draft
            }
            let Ok(saved) = result else { return status.set("Saved in this browser only") };
            status.set("Draft saved");
            copy.set(Some(saved));
            store(&app.account(), &id.get_value(), current(), true, forwards_bcc);
        });
    };
    let changed = move || {
        stop_timer();
        let edited = edited();
        store(&app.account(), &id.get_value(), current(), edited, forwards_bcc);
        if !edited {
            return status.set("");
        }
        status.set("Saved in this browser");
        if !api::is_signed_out() {
            timer.set_value(set_timeout_with_handle(save_to_server, SAVE_DELAY).ok());
        }
    };
    // Stored as it opens, unedited too, so a reload comes back to it; the address names it unless this is a
    // window for one message, whose address must keep naming that. A compose window's says so, for its reload.
    let standalone = app.single().is_some();
    store(&app.account(), &id.get_value(), current(), edited(), forwards_bcc);
    if !standalone {
        let window = if app.compose_window() { crate::state::WINDOW } else { "" };
        crate::state::set_address(&format!("#draft={}{window}", id.get_value()));
    }
    let mark_alive = move || {
        let Some(copy) = stored(&app.account(), &id.get_value()).filter(|_| !handed_off.get_value()) else { return };
        store(&app.account(), &id.get_value(), copy.draft, copy.edited, copy.forwards_bcc);
    };
    let alive = set_interval_with_handle(mark_alive, ALIVE_EVERY).ok();
    OPEN.set(Some(id.get_value()));
    // The files on their way: each one's key, the bytes it will take in the message, and its request.
    let transfers = StoredValue::new_local(Vec::<(u32, u64, Rc<api::Upload>)>::new());
    on_cleanup(move || {
        OPEN.with_borrow_mut(|open| _ = open.take_if(|shown| *shown == id.get_value()));
        stop_timer();
        app.set_sending(false); // left by a way the lock does not cover: the next Compose starts free
        alive.inspect(IntervalHandle::clear);
        transfers.try_with_value(|t| t.iter().for_each(|(_, _, upload)| upload.cancel())); // as leaving Roundcube's page stops them
        if !handed_off.get_value() {
            leave(app, &id.get_value(), sending.get_untracked());
        }
        if !standalone {
            app.show_address();
        }
    });

    // As Roundcube's `set_busy` and `lock_form`: "Sending message..." and every field and tool disabled until the
    // answer, so no save makes a copy in Drafts the send does not drop. The browser copy counts as unedited
    // meanwhile, so leaving by another way (a folder) saves no draft to Drafts that the send then cannot remove; a
    // failed send keeps it again, for the restore question.
    let deliver = move |mut draft: Draft| {
        stop_timer();
        sending.set(true);
        app.set_sending(true);
        status.set("Sending message...");
        let compose = id.get_value(); // read now: Compose may be left, and `id` disposed, before the answer
        store(&app.account(), &compose, draft.clone(), false, forwards_bcc);
        spawn_local(async move {
            draft.copy = app.draft_copy(&compose, draft.copy).await; // a save still on its way makes a newer copy
            let result = api::post("api/send", &draft).await;
            app.set_sending(false);
            match result {
                Ok(()) => {
                    forget(&app.account(), &compose);
                    draft.copy.as_ref().inspect(|old| app.draft_copy_gone(old)); // the server drops it after the send
                    app.notify("Message sent successfully.".into());
                    if app.compose_window() {
                        set_timeout(move || close(app), Duration::from_secs(1)); // as Roundcube's window, once the notice was seen
                    } else if !sending.is_disposed() {
                        app.view.set(View::Mail); // back where the user was; a phone with nothing open shows the list
                        if crate::state::narrow() && app.open.get_untracked().is_none() {
                            app.pane.set(Pane::List);
                        }
                    }
                }
                Err(e) => {
                    store(&app.account(), &compose, draft, true, forwards_bcc);
                    // `try_set` hands the value back as `Some` only once Compose is gone.
                    if sending.try_set(false).is_none() {
                        status.set("Saved in this browser");
                    }
                    app.notify(format!("Not sent: {}", e.0));
                }
            }
        });
    };
    // Roundcube's checks before anything goes out, in its order and words (`check_compose_input`): files still on
    // their way, an empty subject (asked once per Compose, offering a field), an empty body, no recipient, then
    // more than five public recipients (asked once per Compose, offering Bcc).
    let (subject_asked, recipients_asked) = (StoredValue::new(false), StoredValue::new(false));
    // A frame later: the closing dialog first gives the focus back to the Send button.
    let focus = move |field: Option<web_sys::HtmlElement>| request_animation_frame(move || drop(field.map(|f| f.focus())));
    let uploading = move || {
        let waiting = !uploads.with_untracked(Vec::is_empty);
        if waiting {
            app.ask(Ask::alert("Not all attachments have been uploaded yet. Please wait or cancel the upload."), |_| {});
        }
        waiting
    };
    let recipients = move |draft: Draft| {
        if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
            let to_field = move || to_ref.get_untracked().map(Into::into);
            return app.ask(Ask::alert("Please enter at least one recipient."), move |_| focus(to_field()));
        }
        if draft.to.len() + draft.cc.len() > MAX_DISCLOSED && !recipients_asked.get_value() {
            recipients_asked.set_value(true);
            let question = "All recipients will see each others e-mail addresses. To prevent this and protect their privacy \
                            you can use the Bcc field.";
            let ask =
                Ask { alt: Some("Use Bcc"), danger: false, ..Ask::confirm("Too many public recipients", question.into(), "Send message") };
            let use_bcc = move || {
                let moved: Vec<Address> = [bcc, to, cc].iter().flat_map(|f| text::parse_addresses(&f.get_untracked())).collect();
                bcc.set(list_text(&moved));
                to.set(String::new());
                cc.set(String::new());
                bcc_shown.set(true);
                changed();
                deliver(current());
            };
            return app.ask_reply(ask, move |reply| match reply {
                Reply::Yes(_) => deliver(draft),
                Reply::Alt => use_bcc(),
                Reply::Cancel => {}
            });
        }
        deliver(draft);
    };
    let after_subject = move || {
        let draft = current();
        if draft.body.is_empty() {
            let question = "Send this message without text?".to_owned();
            let ask = Ask { danger: false, ..Ask::confirm("Send message", question, "Send message") };
            let body_field = move || body_ref.get_untracked().map(Into::into);
            return app.ask_reply(ask, move |reply| if let Reply::Yes(_) = reply { recipients(draft) } else { focus(body_field()) });
        }
        recipients(draft);
    };
    let send = move |_| {
        if sending.get_untracked() || uploading() {
            return; // the first press is on its way, or files are
        }
        if subject.with_untracked(String::is_empty) && !subject_asked.get_value() {
            subject_asked.set_value(true); // asked once: Cancel lets the next Send go without one
            let question = "The \"Subject\" field is empty. Would you like to enter one now?";
            let entered = move |text: String| {
                subject.set(text);
                changed();
                if !uploading() {
                    after_subject();
                }
            };
            let subject_field = move || subject_ref.get_untracked().map(Into::into);
            let ask = Ask::text("No subject", question, "", "Send message");
            return app.ask_reply(ask, move |reply| if let Reply::Yes(text) = reply { entered(text) } else { focus(subject_field()) });
        }
        after_subject();
    };

    // "Open in new window": the browser copy as it stands goes on in a tab of its own, as Roundcube's `extwin` posts
    // the form there, and this tab goes back to the mail without saving or forgetting it.
    let open_elsewhere = move |_| {
        stop_timer();
        store(&app.account(), &id.get_value(), current(), edited(), forwards_bcc);
        handed_off.set_value(true);
        let address = format!("#draft={}{}", id.get_value(), crate::state::WINDOW);
        crate::actions::new_tab(&address);
        app.view.set(View::Mail);
    };

    // Roundcube's `change_identity`: picking another From takes the old identity's Reply-To and Bcc out of those
    // fields and puts the new one's in.
    let switch_identity = move |text: &str| {
        let old = identity.get_value();
        let email = text::parse_addresses(text).into_iter().next().map(|a| a.email.to_lowercase());
        let Some(email) = email.filter(|email| *email != old) else { return };
        identity.set_value(email.clone());
        let prefs = |e: &str| app.settings.with_untracked(|s| s.identities.get(e).cloned().unwrap_or_default());
        let (old, new) = (prefs(&old), prefs(&email));
        let swaps = [(reply_to, reply_to_shown, old.reply_to, new.reply_to), (bcc, bcc_shown, old.bcc, new.bcc)];
        for (field, shown, out, add) in swaps {
            if out.is_empty() && add.is_empty() {
                continue;
            }
            let has = |list: &[Address], a: &Address| list.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email));
            let mut list = text::parse_addresses(&field.get_untracked());
            list.retain(|a| !has(&out, a));
            for address in add {
                if !has(&list, &address) {
                    list.push(address);
                }
            }
            field.set(list_text(&list));
            if !list.is_empty() {
                shown.set(true);
            }
        }
    };

    let next_upload = StoredValue::new(0u32);
    // Roundcube's `cancel_attachment_upload`: the entry goes at once, then the request stops.
    let cancel_upload = move |key: u32| {
        uploads.update(|u| u.retain(|u| u.key != key));
        transfers.with_value(|t| t.iter().filter(|(k, _, _)| *k == key).for_each(|(_, _, upload)| upload.cancel()));
    };
    let upload = move |files: Option<web_sys::FileList>| {
        for file in (0..files.as_ref().map_or(0, web_sys::FileList::length)).filter_map(|i| files.as_ref()?.get(i)) {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a file's size in bytes, exact in an f64
            let encoded = encoded_size(file.size() as u64, &file.type_());
            // Roundcube's check_message_size, against the mail server's own limit: the body, 10 KB of headers, the
            // files attached and on their way, and this one.
            if let Some(limit) = app.max_message_size() {
                if encoded > limit {
                    app.notify(format!("The uploaded file exceeds the maximum size of {}.", kubicle_api::size(largest_file(limit))));
                    continue;
                }
                let attached: u64 = attachments.with_untracked(|a| a.iter().map(|a| encoded_size(a.size, &a.content_type)).sum());
                // Bytes the files still on their way will take in the message, so two picked at once are counted together.
                let in_flight: u64 = transfers.with_value(|t| t.iter().map(|(_, size, _)| size).sum());
                let total = body.with_untracked(String::len) as u64 + TEXT_ALLOWANCE + attached + in_flight + encoded;
                if total > limit {
                    app.notify(format!("Failed to attach a file. Maximum size of a message ({}) exceeded.", kubicle_api::size(limit)));
                    continue;
                }
            }
            // Roundcube's list entry for a file on its way, its progress filled in as the browser sends it.
            let key = next_upload.get_value();
            next_upload.set_value(key + 1);
            let progress = move |loaded, total| {
                uploads.try_update(|u| u.iter_mut().filter(|u| u.key == key).for_each(|u| (u.loaded, u.total) = (loaded, total)));
            };
            let started = match api::Upload::start("api/upload", &file, progress) {
                Ok(started) => Rc::new(started),
                Err(e) => {
                    app.notify(format!("Could not attach {}: {}", file.name(), e.0));
                    continue;
                }
            };
            uploads.update(|u| u.push(Uploading { key, name: file.name(), loaded: 0.0, total: 0.0 }));
            transfers.update_value(|t| t.push((key, encoded, Rc::clone(&started))));
            let compose = id.get_value();
            spawn_local(async move {
                let result = started.answer::<DraftAttachment>().await;
                transfers.try_update_value(|t| t.retain(|(k, _, _)| *k != key));
                // Cancel took the entry off already; the draft left meanwhile took it all.
                let listed = uploads.try_update(|u| {
                    let listed = u.iter().any(|u| u.key == key);
                    u.retain(|u| u.key != key);
                    listed
                });
                match result {
                    Ok(attachment) if listed == Some(true) => {
                        attachments.update(|a| a.push(attachment));
                        changed();
                    }
                    Err(e) if listed == Some(true) => app.notify(format!("Could not attach {}: {}", file.name(), e.0)),
                    Ok(attachment) => drop_uploads(app, compose, vec![attachment]).await, // stored after Cancel or leaving
                    Err(_) => {}
                }
            });
        }
    };
    let file_input = NodeRef::<html::Input>::new();
    let pick_files = move || _ = file_input.get_untracked().map(|input| input.click());
    let on_files = move |_| {
        let Some(input) = file_input.get_untracked() else { return };
        upload(input.files());
        input.set_value("");
    };
    let dragging = RwSignal::new(false);
    // The upload ceiling, or the largest file the mail server's limit leaves room for, whichever is smaller.
    let ceiling = (MAX_UPLOAD_MIB as u64) << 20;
    let largest_file = app.max_message_size().map_or(ceiling, |limit| ceiling.min(largest_file(limit)));

    // Roundcube's first focus: To while it is empty, else Subject while it is empty, else the body with the caret
    // at the end, or at the start of a reply that goes above the quote ("When replying").
    let above = reply && app.settings.with_untracked(|s| matches!(s.reply_mode, ReplyMode::Above | ReplyMode::AboveNoIndent));
    let (to_empty, subject_empty) = base.with_value(|d| (d.to.is_empty(), d.subject.is_empty()));
    Effect::new(move |_| match (to_ref.get(), subject_ref.get(), body_ref.get()) {
        (Some(to), _, _) if to_empty => drop(to.focus()),
        (_, Some(subject), _) if subject_empty => drop(subject.focus()),
        // A frame later: the body's text is not in yet, and putting it in moves the caret to its end.
        (_, _, Some(area)) => request_animation_frame(move || {
            let at = if above { 0 } else { u32::try_from(area.value().encode_utf16().count()).unwrap_or(u32::MAX) };
            let _ = area.focus();
            let _ = area.set_selection_range(at, at);
            area.set_scroll_top(if above { 0 } else { area.scroll_height() });
        }),
        _ => {}
    });

    // Roundcube's Rename attachment: the name the file goes out with, empty refused.
    let rename_attachment = move |a: &DraftAttachment| {
        let id = a.id.clone();
        let rename = move |name: String| {
            let name = name.trim().to_owned();
            if !name.is_empty() {
                attachments.update(|list| list.iter_mut().filter(|x| x.id == id).for_each(|x| x.filename.clone_from(&name)));
                changed();
            }
        };
        app.ask(Ask::text("Rename attachment", "Name", &a.filename, "Save"), rename);
    };
    // Elastic's attachment menu in Compose: Open where the browser shows the file (images, as the reader's), Download
    // and Rename, whose new name is the one the file goes out with.
    let attachment = move |a: DraftAttachment| {
        let file = a.clone();
        let items = move || {
            let url = |open: bool| format!("api/upload?id={}&name={}&open={open}", enc(&file.id), enc(&file.filename));
            let (open, download, rename) = (url(true), url(false), file.clone());
            let open_tab = move || crate::actions::new_tab(&open);
            vec![
                MenuItem::action("Open", open_tab).glyph(icons::OPEN).enabled(kubicle_api::image_type(&file.content_type).is_some()),
                // The answer carries `Content-Disposition: attachment`, so Compose stays where it is.
                MenuItem::action("Download", move || drop(window().location().set_href(&download))).glyph(icons::DOWNLOAD),
                MenuItem::action("Rename", move || rename_attachment(&rename)).glyph(icons::COMPOSE),
            ]
        };
        view! {
            <li class="attachment-item">
                <Menu class="chip attachment-menu" label=a.filename.clone()
                    trigger=view! { {icon(icons::ATTACH)} <span class="chip-name">{a.filename.clone()}</span> } items=items />
                <span class="muted small">{kubicle_api::size(a.size)}</span>
                <button class="icon-button danger" aria-label=format!("Remove {}", a.filename) title="Remove" on:click=move |_| {
                    attachments.update(|list| list.retain(|x| x.id != a.id));
                    changed();
                    spawn_local(drop_uploads(app, id.get_value(), vec![a.clone()])); // Roundcube's `attachment_delete`
                    drop(attach_ref.get_untracked().map(|attach| attach.focus())); // the button goes with its file
                }>{icon(icons::CLOSE)}</button>
            </li>
        }
    };
    let uploading_item = move |u: Uploading| {
        let progress = move || uploads.with(|all| all.iter().find(|x| x.key == u.key).map(|x| upload_progress(x.loaded, x.total)));
        view! {
            <li class="attachment-item uploading">
                <button class="icon-button danger" title="Cancel" aria-label=format!("Cancel {}", u.name)
                    on:click=move |_| cancel_upload(u.key)>{icon(icons::CLOSE)}</button>
                <span class="chip-name">{u.name.clone()}</span>
                <span class="muted small">{progress}</span>
            </li>
        }
    };
    let extra_field = move |(label, value, shown): (&'static str, RwSignal<String>, RwSignal<bool>)| {
        view! {
            <Show when=move || shown.get()>
                <div class="field-line">
                    {address_field(label, value, changed, NodeRef::new())}
                    <span class="field-adds">
                    <button class="icon-button danger" title=format!("Remove {label}") aria-label=format!("Remove {label}")
                        on:click=move |_| {
                            value.set(String::new());
                            shown.set(false);
                            changed();
                            drop(to_ref.get_untracked().map(|to| to.focus())); // the button goes with its field; To's "+" adds it back
                        }>{icon(icons::CLOSE)}</button>
                    </span>
                </div>
            </Show>
        }
    };
    let is_choice = move |text: &str| from_choices.with_untracked(|c| c.as_ref().is_some_and(|c| c.iter().any(|a| a.to_string() == text)));
    let from_option = |a: Address| view! { <option value=a.to_string()></option> };
    let toggle = move |label: &'static str, signal: RwSignal<bool>| {
        view! {
            <label class="option">
                <span>{label}</span>
                <input type="checkbox" prop:checked=move || signal.get() on:change=move |e| {
                    signal.set(event_target_checked(&e));
                    changed();
                } />
            </label>
        }
    };
    view! {
        <section class="compose" aria-label="Compose">
            <header class="bar toolbar">
                <button class="icon-button phone-only" aria-label="Back" prop:disabled=move || sending.get() on:click=move |_| app.back()>
                    {icon(icons::BACK)}
                </button>
                <div class="compose-tools">
                    <Show when=move || app.folders.with(|f| f.iter().any(|f| f.roles.contains(&FolderRole::Drafts)))>
                        {tool("Save as draft now", icons::DRAFTS, "Save", sending, move || { stop_timer(); save_to_server() })}
                    </Show>
                    {tool("Attach a file", icons::ATTACH, "Attach", sending, pick_files)}
                    {tool("Discard this draft", icons::TRASH, "Discard", sending, move || {
                        // Nothing to lose in an untouched form with no copy in Drafts: close without asking.
                        if !edited() && copy.with_untracked(Option::is_none) {
                            return discard(app, &id.get_value(), None, false, attachments.get_untracked());
                        }
                        let opened = base.with_value(|d| d.opened);
                        let question = if opened && app.role_folder(FolderRole::Trash).is_some() {
                            "Discard this draft? Its copy in Drafts moves to Trash."
                        } else {
                            "Discard this draft? Its copy in Drafts is deleted too."
                        };
                        let discard = move |_| discard(app, &id.get_value(), copy.get_untracked(), opened, attachments.get_untracked());
                        app.ask(Ask::confirm("Discard draft", question.to_owned(), "Discard"), discard);
                    })}
                    {tool("Close; the draft stays in Drafts", icons::CLOSE, "Close", sending, move || close(app))}
                </div>
                <span class="compose-status muted small">{move || if uploads.with(Vec::is_empty) { status.get() } else { "Uploading..." }}</span>
            </header>
            <div class="compose-main">
                <div class="compose-form">
                    {forwards_bcc.then(crate::reader::bcc_warning)}
                    // Roundcube's `lock_form` while it sends: every field and button inside turns disabled.
                    <fieldset class="compose-lock" prop:disabled=move || sending.get()>
                        <div class="compose-fields">
                            <label class="field">
                                <span class="field-label">"From"</span>
                                <input class="input" type="text" list="from-choices" aria-label="From" autocomplete="off" spellcheck="false"
                                    title="Any address on your domains" prop:value=move || from.get() on:input=move |e| {
                                        let text = event_target_value(&e);
                                        // A pick from the list is an identity at once; typing becomes one on leaving the field.
                                        if is_choice(&text) {
                                            switch_identity(&text);
                                        }
                                        from.set(text);
                                        changed();
                                    }
                                    on:change=move |e| {
                                        if from.is_disposed() {
                                            return; // Compose closed with the field focused: the browser fires this as it goes
                                        }
                                        switch_identity(&event_target_value(&e));
                                        changed();
                                    } />
                                <span class="field-adds"></span>
                                <datalist id="from-choices">
                                    {move || from_choices.get().unwrap_or_default().into_iter().map(from_option).collect_view()}
                                </datalist>
                            </label>
                            <div class="field-line">
                                {address_field("To", to, changed, to_ref)}
                                // Roundcube's "+": the headers not on screen yet, each with its own row once chosen.
                                <span class="field-adds">
                                    <Menu class="icon-button" label="Add header" trigger=icon(icons::PLUS) align_right=true
                                        disabled=Signal::derive(move || extras.iter().all(|(_, _, shown)| shown.get()))
                                        items=move || {
                                            let hidden = extras.iter().filter(|(_, _, shown)| !shown.get_untracked());
                                            hidden.map(|&(label, _, shown)| MenuItem::action(label, move || shown.set(true))).collect()
                                        } />
                                </span>
                            </div>
                            {extras.map(extra_field).collect_view()}
                            <label class="field">
                                <span class="field-label">"Subject"</span>
                                <input class="input" type="text" aria-label="Subject" spellcheck="true" node_ref=subject_ref
                                    prop:value=move || subject.get() on:input=move |e| {
                                        subject.set(event_target_value(&e));
                                        changed();
                                    } />
                                <span class="field-adds"></span>
                            </label>
                        </div>
                        <textarea class="compose-body input" aria-label="Message" node_ref=body_ref spellcheck="true"
                            prop:value=move || body.get() on:input=move |e| {
                                body.set(event_target_value(&e));
                                changed();
                            }></textarea>
                    </fieldset>
                    <div class="compose-send">
                        // `aria-disabled`, not `disabled`, which would drop the focus to the page.
                        <button class="button primary" aria-disabled=move || sending.get().then_some("true") on:click=send>
                            {icon(icons::SENT)} "Send"
                        </button>
                        <span class="grow"></span>
                        // Elastic's, hidden on a phone and in a window that is already one.
                        {(!app.compose_window()).then(|| view! {
                            <button class="button link extwin" prop:disabled=move || sending.get() on:click=open_elsewhere>
                                {icon(icons::OPEN)} "Open in new window"
                            </button>
                        })}
                    </div>
                </div>
                <aside class="compose-side" aria-label="Options and attachments">
                    <h3 class="side-title">"Options and attachments"</h3>
                    <fieldset class="compose-lock" prop:disabled=move || sending.get()>
                        <div class="dropzone" class:over=move || dragging.get() on:dragleave=move |_| dragging.set(false)
                            on:dragover=move |e: DragEvent| {
                                e.prevent_default();
                                dragging.set(true);
                            }
                            on:drop=move |e: DragEvent| {
                                e.prevent_default();
                                dragging.set(false);
                                upload(e.data_transfer().and_then(|d| d.files()));
                            }>
                            <p class="muted small">{format!("Maximum allowed file size is {}", kubicle_api::size(largest_file))}</p>
                            <button class="button" node_ref=attach_ref on:click=move |_| pick_files()>
                                {icon(icons::ATTACH)} "Attach a file"
                            </button>
                            <input class="visually-hidden" type="file" multiple node_ref=file_input on:change=on_files />
                            <ul class="attachment-list">
                                <For each=move || attachments.get() key=|a| (a.id.clone(), a.filename.clone()) children=attachment />
                                <For each=move || uploads.get() key=|u| u.key children=uploading_item />
                            </ul>
                            <Show when=move || attachments.with(Vec::is_empty) && uploads.with(Vec::is_empty)>
                                <span class="dropzone-icon">{icon(icons::DOWNLOAD)}</span>
                                <p class="muted small">"or drop files here"</p>
                            </Show>
                        </div>
                        {toggle("Return receipt", return_receipt)}
                        {toggle("Delivery status notification", delivery_status)}
                        {toggle("Keep formatting", keep_formatting)}
                        <label class="option">
                            <span>"Priority"</span>
                            {select("Priority", || PRIORITIES, move || priority.get(), move |p| { priority.set(p); changed(); })}
                        </label>
                        {sent_copy_select(app, sent_copy, changed)}
                    </fieldset>
                </aside>
            </div>
        </section>
    }
}

/// A tool in Compose's bar. Off while it sends, as every tool: the send decides what happens to the draft.
fn tool(title: &'static str, glyph: &'static str, label: &'static str, busy: RwSignal<bool>, click: impl Fn() + 'static) -> impl IntoView {
    view! {
        <button class="tool" title=title prop:disabled=move || busy.get() on:click=move |_| click()>
            {icon(glyph)}
            <span class="tool-label">{label}</span>
        </button>
    }
}

/// A file on its way to the server, as Compose lists it.
#[derive(Clone)]
struct Uploading {
    key: u32,
    name: String,
    /// Bytes sent so far and in all, as the browser reports them; none before its first report.
    loaded: f64,
    total: f64,
}

/// Roundcube's `file_upload_msg`: "37% (11.1 of 30.0 MB)", empty before the browser reports and once all is sent.
fn upload_progress(loaded: f64, total: f64) -> String {
    if total <= 0.0 || loaded >= total {
        return String::new();
    }
    let percent = (loaded / total * 100.0).round();
    let (current, total) = match total {
        t if t >= 1_073_741_824.0 => (format!("{:.1}", loaded / 1_073_741_824.0), format!("{:.1} GB", t / 1_073_741_824.0)),
        t if t >= 1_048_576.0 => (format!("{:.1}", loaded / 1_048_576.0), format!("{:.1} MB", t / 1_048_576.0)),
        t if t >= 1024.0 => (format!("{}", (loaded / 1024.0).floor()), format!("{} KB", (t / 1024.0).floor())),
        t => (format!("{loaded}"), format!("{t} B")),
    };
    format!("{percent}% ({current} of {total})")
}

/// Roundcube's "Save sent message in": Sent, Don't save or any other folder. Redirect's dialog has it too.
pub fn sent_copy_select(app: App, sent_copy: RwSignal<SentCopy>, changed: impl Fn() + Send + Sync + 'static) -> impl IntoView {
    let options = move || {
        let folders = app.folders.with(|f| {
            let other = f.iter().filter(|f| f.selectable && !f.roles.contains(&FolderRole::Sent));
            other.map(|f| (f.name.clone(), SentCopy::Folder(f.name.clone()))).collect::<Vec<_>>()
        });
        [("Sent".to_owned(), SentCopy::Sent), ("Don't save".to_owned(), SentCopy::NotSaved)].into_iter().chain(folders)
    };
    view! {
        <label class="option">
            <span>"Save sent message in"</span>
            {select("Save sent message in", options, move || sent_copy.get(), move |choice| { sent_copy.set(choice); changed(); })}
        </label>
    }
}

thread_local! {
    /// The chip being dragged and the field it comes from, for the field it is dropped on.
    static DRAGGED: std::cell::Cell<Option<(RwSignal<String>, usize)>> = const { std::cell::Cell::new(None) };
    /// The id of the Compose on screen, which `may_leave` and `sign_out` ask about.
    static OPEN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// An address field as Elastic's recipient input, its chips and Roundcube's autocomplete keys as docs/design.md
/// (Features, Compose) lists them; Redirect's dialog has it too. `value` stays the field's text, the chips then
/// what is being typed, so a draft keeps half an address as it is. Suggestions (`api/autocomplete`) come from the
/// first letter on, as Roundcube's `autocomplete_min_length`.
#[allow(clippy::too_many_lines)] // the chips, the typed text and the suggestions are one control
pub fn address_field(
    label: &'static str, value: RwSignal<String>, changed: impl Fn() + Copy + Send + Sync + 'static, input_ref: NodeRef<html::Input>,
) -> impl IntoView {
    let app = expect_context::<App>();
    let (suggestions, active) = (RwSignal::new(Vec::<Address>::new()), RwSignal::new(0usize));
    let (list_id, input_id) = (format!("suggest-{label}"), format!("field-{label}"));
    let (first, rest) = text::recipients(&value.get_untracked());
    let (chips, typed) = (RwSignal::new(first), RwSignal::new(rest));
    // The text this field last wrote: anything else in `value` came from outside (Use Bcc, a change of From, the
    // remove button) and is split into chips again.
    let written = StoredValue::new(value.get_untracked());
    Effect::new(move |_| {
        let text = value.get();
        if text != written.get_value() {
            let (found, rest) = text::recipients(&text);
            chips.set(found);
            typed.set(rest);
            written.set_value(text);
        }
    });
    let write = move || {
        let text = chips.with_untracked(|c| list_text(c)) + &typed.get_untracked();
        written.set_value(text.clone());
        value.set(text);
        changed();
    };
    // The addresses in what was typed become chips; text that holds none stays typed.
    let commit = move |text: &str| {
        let (found, rest) = text::recipients(text);
        let any = !found.is_empty();
        if any {
            chips.update(|c| c.extend(found));
            typed.set(rest);
            write();
        }
        any
    };

    let lookup = move || {
        let token = text::last_token(&typed.get_untracked()).1.to_owned();
        if token.is_empty() {
            return suggestions.set(Vec::new());
        }
        spawn_local(async move {
            let Ok(found) = api::get::<Vec<Address>>(&format!("api/autocomplete?q={}", enc(&token))).await else { return };
            // Only for the token still being typed, and never after the draft was left.
            if typed.try_with_untracked(|v| text::last_token(v).1 == token) == Some(true) {
                active.set(0);
                suggestions.set(found);
            }
        });
    };
    let choose = move |a: &Address| {
        let kept = text::last_token(&typed.get_untracked()).0.trim_end_matches([',', ';', ' ']).to_owned();
        chips.update(|c| c.push(a.clone()));
        typed.set(kept);
        suggestions.set(Vec::new());
        write();
    };
    let choose_active = move || {
        // Cloned first: `choose` writes the list, which must not happen under its read lock.
        let picked = suggestions.with_untracked(|s| s.get(active.get_untracked()).cloned());
        picked.inspect(choose);
    };
    let on_key = move |e: KeyboardEvent| {
        let n = suggestions.with_untracked(Vec::len);
        match e.key().as_str() {
            "ArrowDown" if n > 0 => active.set((active.get_untracked() + 1).min(n - 1)),
            "ArrowUp" if n > 0 => active.set(active.get_untracked().saturating_sub(1)),
            // Tab returns before `prevent_default`: the browser moves the focus on, or back with Shift.
            "Tab" if n > 0 && e.shift_key() => return suggestions.set(Vec::new()),
            "Tab" if n > 0 => return choose_active(),
            "Enter" if n > 0 => choose_active(),
            "Escape" if n > 0 => suggestions.set(Vec::new()),
            " " | "," | ";" | "Enter" if commit(&typed.get_untracked()) => suggestions.set(Vec::new()),
            "Backspace" if typed.with_untracked(String::is_empty) && chips.with_untracked(|c| !c.is_empty()) => {
                chips.update(|c| drop(c.pop()));
                write();
            }
            _ => return,
        }
        e.prevent_default();
        e.stop_propagation();
    };
    // The browser turns new lines into spaces on paste, so the clipboard's text is read instead, as Elastic does.
    let on_paste = move |e: web_sys::ClipboardEvent| {
        let Some(pasted) = e.clipboard_data().and_then(|d| d.get_data("text").ok()) else { return };
        let Some(input) = input_ref.get_untracked() else { return };
        e.prevent_default();
        let units: Vec<u16> = input.value().encode_utf16().collect();
        let at = |n: Option<u32>| n.map_or(units.len(), |n| (n as usize).min(units.len()));
        let (start, end) = (at(input.selection_start().ok().flatten()), at(input.selection_end().ok().flatten()));
        let text = String::from_utf16_lossy(&units[..start]) + &pasted + &String::from_utf16_lossy(&units[end..]);
        if !commit(&text) {
            typed.set(text);
            write();
        }
    };
    // Elastic's `recipient_input_edit_dialog`; inside another dialog, which a question would replace, the chip's
    // text goes back into the input instead.
    let edit = move |i: usize| {
        let Some(address) = chips.with_untracked(|c| c.get(i).cloned()) else { return };
        if app.modal.with_untracked(Option::is_some) {
            chips.update(|c| drop(c.remove(i)));
            typed.update(|t| *t = format!("{address}{}", if t.is_empty() { "" } else { ", " }) + t);
            return write();
        }
        let replace = move |text: String| {
            let (found, rest) = text::recipients(&text);
            let ([one], true) = (found.as_slice(), rest.is_empty()) else { return };
            chips.update(|c| c.get_mut(i).into_iter().for_each(|a| a.clone_from(one)));
            write();
        };
        app.ask(Ask::text("Recipient edit", "Recipient", &address.to_string(), "Save"), replace);
    };
    let chip = move |(i, a): (usize, Address)| {
        let (label, title) = (text::reader_label(&a, false), a.name.is_some().then(|| a.to_string())); // never a name dressed as an address
        let remove = format!("Remove {}", a.email);
        view! {
            <li class="recipient" title=title draggable="true" on:dblclick=move |_| edit(i) on:dragstart=move |e: DragEvent| {
                DRAGGED.set(Some((value, i)));
                let Some(data) = e.data_transfer() else { return };
                data.set_effect_allowed("move");
                let _ = data.set_data(CHIP, &a.to_string());
            }>
                <span class="recipient-name">{label}</span>
                <button type="button" class="recipient-remove" tabindex="-1" aria-label=remove on:click=move |e| {
                    e.stop_propagation();
                    chips.update(|c| drop(c.remove(i)));
                    write();
                    drop(input_ref.get_untracked().map(|input| input.focus()));
                }>{icon(icons::CLOSE)}</button>
            </li>
        }
    };
    let carries_chip = |e: &DragEvent| e.data_transfer().is_some_and(|d| d.types().includes(&CHIP.into(), 0));
    let on_drop = move |e: DragEvent| {
        let Some((from, at)) = DRAGGED.take().filter(|_| carries_chip(&e)) else { return };
        e.prevent_default();
        e.stop_propagation();
        if from == value {
            chips.update(|c| {
                if at < c.len() {
                    let moved = c.remove(at);
                    c.push(moved);
                }
            });
        } else {
            let (mut theirs, rest) = text::recipients(&from.get_untracked());
            if at >= theirs.len() {
                return;
            }
            let moved = theirs.remove(at);
            from.set(list_text(&theirs) + &rest);
            chips.update(|c| c.push(moved));
        }
        write();
    };
    let option = move |(i, a): (usize, Address)| {
        let (who, email) = (text::who(&a).to_owned(), a.email.clone());
        view! {
            <li role="option" class="menu-item" class:active=move || active.get() == i
                aria-selected=move || if active.get() == i { "true" } else { "false" } on:mousedown=move |e| {
                    e.prevent_default();
                    choose(&a);
                }>
                <span>{who}</span>
                <span class="muted small">{email}</span>
            </li>
        }
    };
    let options = move || suggestions.get().into_iter().enumerate().map(option).collect_view();
    view! {
        <div class="field">
            <label class="field-label" for=input_id.clone()>{label}</label>
            <span class="suggest">
                <ul class="recipient-input input" on:click=move |_| drop(input_ref.get_untracked().map(|i| i.focus()))
                    on:drop=on_drop on:dragover=move |e: DragEvent| if carries_chip(&e) { e.prevent_default() }>
                    {move || chips.get().into_iter().enumerate().map(chip).collect_view()}
                    <li class="recipient-typed">
                        <input id=input_id type="text" autocomplete="off" spellcheck="false" role="combobox" aria-label=label
                            aria-autocomplete="list" aria-controls=list_id.clone()
                            aria-expanded=move || if suggestions.with(Vec::is_empty) { "false" } else { "true" }
                            node_ref=input_ref prop:value=move || typed.get() on:keydown=on_key on:paste=on_paste
                            on:input=move |e| {
                                typed.set(event_target_value(&e));
                                write();
                                lookup();
                            }
                            on:change=move |e| {
                                if typed.is_disposed() {
                                    return; // Compose closed with the field focused: the browser fires this as it goes
                                }
                                let _ = commit(&event_target_value(&e));
                            }
                            on:blur=move |_| set_timeout(move || drop(suggestions.try_set(Vec::new())), Duration::from_millis(150))
                        />
                    </li>
                </ul>
                <Show when=move || !suggestions.with(Vec::is_empty)>
                    <ul class="menu suggest-list" role="listbox" id=list_id.clone()>{options}</ul>
                </Show>
            </span>
        </div>
    }
}

/// The drag data type that marks a recipient chip, as Elastic's chips drag only between recipient fields.
const CHIP: &str = "application/x-kubicle-recipient";
