//! Sessions created on `sso.php` from the identity Mailu front vouches for. They live in memory only, as does
//! each one's Mailu token, a credential: a restart costs one redirect through `sso.php`.

use std::{collections::HashMap, path::Path, sync::Arc, sync::atomic::AtomicI64, sync::atomic::Ordering, time::Duration};

use jiff::Timestamp;
use kubicle_api::Chosen;
use kubicle_imap::client::Client;
use kubicle_mailbox::{Error, MailServer, random_id, settings::Store, sync::Mailbox, sync::Running, sync::Token};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tracing::{debug, info};
use zeroize::Zeroizing;

use crate::sign::Handle;

/// The cookie name Mailu's logout clears (docs/design.md, Mailu integration): a Mailu sign-out also signs the
/// browser out of kubicle. The session itself lasts until a reconnect finds its token refused, or `IDLE_LIMIT`.
pub const COOKIE: &str = "roundcube_sessid";

/// A session unused this long is dropped; the next request goes through `sso.php` and gets a fresh one.
const IDLE_LIMIT: Duration = Duration::from_hours(12);
/// A user's sessions at most, about one per browser: past it the least recently used goes, and its browser passes
/// `sso.php` again. Each held token costs a login at every reconnect.
const MAX_SESSIONS: usize = 32;

pub struct Session {
    /// The cookie value: whoever holds it acts as the user, so it never appears in a URL.
    pub id: String,
    /// What signed URLs carry instead (`sign.rs`): it identifies the session but does not authenticate a request.
    pub handle: String,
    pub user: String,
    pub csrf: String,
    /// The Mailu token its `sso.php` proved, for its submissions; the session ends once a reconnect finds it refused.
    pub token: Token,
    /// Shared by all of the user's sessions: one engine, and one IMAP connection, per user.
    pub mailbox: Mailbox,
    /// Per message (folder, UIDVALIDITY, UID), for the session, as Roundcube's `safe_messages` and `msg_formats`. With
    /// the UIDVALIDITY, a folder recreated since names its new messages anew.
    pub chosen: std::sync::Mutex<HashMap<(String, u32, u32), Chosen>>,
    last_seen: AtomicI64,
}

impl Session {
    pub fn csrf_matches(&self, presented: &str) -> bool { self.csrf.as_bytes().ct_eq(presented.as_bytes()).into() }

    /// How a message shows: with remote images once they were allowed in this session, and with the body asked
    /// for now, else the one chosen last.
    pub fn chosen(&self, folder: &str, validity: u32, uid: u32, images: bool, text: Option<bool>) -> Chosen {
        let mut all = self.chosen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (folder.to_owned(), validity, uid);
        if !images && text.is_none() {
            return all.get(&key).copied().unwrap_or_default(); // a plain view adds no entry
        }
        let chosen = all.entry(key).or_default();
        chosen.images |= images;
        chosen.text = text.or(chosen.text);
        *chosen
    }
}

/// Every live session by id. Their mailboxes go with them: an engine stops when no session holds it. A plain mutex,
/// whose guard cannot be held across an `.await` in a request: a login under it would stall every user's requests.
#[derive(Default)]
pub struct Sessions(std::sync::Mutex<State>);

#[derive(Default)]
struct State {
    sessions: HashMap<String, Arc<Session>>,
    /// Each user's engine, past its last session too, so a sign-in never starts a second one beside it: one
    /// engine runs a user's jobs in the order asked. A sign-in holds the user's lock across its login, so that
    /// user's sign-ins run one at a time.
    engines: HashMap<String, Arc<Mutex<Option<Running>>>>,
}

impl Sessions {
    /// A session for a user front vouched for, once an IMAP login proves the token: the user's running engine
    /// holds it too, or a new one starts. A live session holding the same token is given back, the browser's
    /// `current` one first, so its other tabs keep their CSRF token: Mailu's Webmail link opened in a second tab
    /// passes here again, and Mailu gives a session one token. `kubicle_imap::Error::Expired` when the mail server
    /// refuses it.
    ///
    /// The login also reads INBOX's UIDVALIDITY, which `Store::incarnation` ties the user's state to: another one
    /// means the address holds another account than before, so the account before's sessions, engine (whose
    /// connection still sees its mail), uploads and settings go.
    pub async fn sign_in(
        &self, mail: &Arc<MailServer>, data_dir: &Path, store: &Store, user: &str, token: &str, current: Option<&str>,
    ) -> Result<Arc<Session>, Error> {
        let token = Arc::new(Zeroizing::new(token.to_owned()));
        // One sign-in of a user at a time, so a user never gets two engines. An entry goes once the map holds it alone,
        // so no sign-in does, and its engine stopped: a user name front never vouched for (`sso.php` reads it before
        // the login proves it) leaves nothing behind.
        let slot = {
            let state = &mut *self.state();
            state.engines.retain(|_, slot| Arc::get_mut(slot).is_none_or(|alone| alone.get_mut().as_ref().is_some_and(Running::running)));
            let slot = Arc::clone(state.engines.entry(user.to_owned()).or_default());
            debug!(users = state.engines.len(), "a sign-in started"); // the names held now, which a VM test reads
            slot
        };
        let mut engine = slot.lock().await;
        // The handle, held to the end of this sign-in, keeps the engine taking work meanwhile. An engine whose last
        // handle went is let finish its writes first, so they never run beside a new engine's.
        let (running, mut reused) = (engine.as_ref().and_then(Running::mailbox), None);
        if running.is_none()
            && let Some(stopping) = engine.take()
        {
            stopping.stopped().await;
        }
        let known = store.validity(user)?;
        if let Some(mailbox) = running {
            // A connection of its own: the engine's may hold the account before's INBOX open (`inbox_validity`).
            let mut proving = Client::connect(&mail.host, mail.imap_port, &mail.tls, &mail.tls_name, user, &token).await?;
            let validity = proving.inbox_validity().await;
            proving.logout().await;
            let mut state = self.state();
            if validity? == known {
                let same = |s: &&Arc<Session>| {
                    s.user == user && bool::from(s.token.as_bytes().ct_eq(token.as_bytes())) && mailbox.accepts(&s.token)
                };
                if let Some(session) =
                    current.and_then(|id| state.sessions.get(id)).filter(same).or_else(|| state.sessions.values().find(same))
                {
                    session.last_seen.store(Timestamp::now().as_second(), Ordering::Relaxed);
                    return Ok(Arc::clone(session));
                }
                mailbox.hold(&token);
                reused = Some(mailbox);
            } else {
                mailbox.retire();
                state.sessions.retain(|_, s| s.user != user);
                *engine = None;
            }
        }
        let mailbox = if let Some(mailbox) = reused {
            mailbox
        } else {
            let (mailbox, running, validity) = Mailbox::start(Arc::clone(mail), data_dir, user, &token).await?;
            *engine = Some(running);
            if validity != known {
                // Uploads first: should either step fail, the old value stays recorded and the next sign-in tries again.
                if known != 0 {
                    info!(user, "the address holds another account than before; its settings and uploads went");
                    mailbox.uploads().clear()?;
                }
                store.incarnation(user, validity)?;
            }
            mailbox.settings_saved(store.settings(user)?); // its special folders
            // A job runs after the engine's first folder read: else `api/session` could answer no folders, and the
            // folder the address named fell back to INBOX.
            mailbox.run(|_| Box::pin(async { Ok(()) })).await?;
            mailbox
        };
        let session = Arc::new(Session {
            id: random_id(),
            handle: random_id(),
            user: user.to_owned(),
            csrf: random_id(),
            token,
            mailbox,
            chosen: std::sync::Mutex::default(),
            last_seen: AtomicI64::new(Timestamp::now().as_second()),
        });
        let sessions = &mut self.state().sessions;
        let mine: Vec<(i64, String)> =
            sessions.values().filter(|s| s.user == user).map(|s| (s.last_seen.load(Ordering::Relaxed), s.id.clone())).collect();
        if mine.len() >= MAX_SESSIONS
            && let Some((_, oldest)) = mine.into_iter().min()
        {
            sessions.remove(&oldest);
        }
        sessions.insert(session.id.clone(), Arc::clone(&session));
        Ok(session)
    }

    /// The session with this cookie id, which counts as its use. `None` once a reconnect found its token refused
    /// (its Mailu session ended), or past `IDLE_LIMIT`, which ends it.
    pub fn find(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.live(|sessions| sessions.get(id).cloned())?;
        session.last_seen.store(Timestamp::now().as_second(), Ordering::Relaxed);
        Some(session)
    }

    /// The session a signed URL names by its public handle. Not a use: a held URL must not keep a session past its
    /// idle limit.
    pub fn find_handle(&self, handle: &Handle) -> Option<Arc<Session>> {
        self.live(|sessions| sessions.values().find(|s| s.handle == handle.as_str()).cloned())
    }

    fn live(&self, pick: impl FnOnce(&HashMap<String, Arc<Session>>) -> Option<Arc<Session>>) -> Option<Arc<Session>> {
        let sessions = &mut self.state().sessions;
        let session = pick(sessions)?;
        if !session.mailbox.accepts(&session.token) || session.last_seen.load(Ordering::Relaxed) < idle_cutoff() {
            sessions.remove(&session.id);
            return None;
        }
        Some(session)
    }

    pub fn remove(&self, id: &str) { self.state().sessions.remove(id); }

    /// Drops idle sessions.
    pub fn sweep(&self) { self.state().sessions.retain(|_, s| s.last_seen.load(Ordering::Relaxed) >= idle_cutoff()); }

    fn state(&self) -> std::sync::MutexGuard<'_, State> { self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
}

fn idle_cutoff() -> i64 { Timestamp::now().as_second() - i64::try_from(IDLE_LIMIT.as_secs()).expect("the idle limit fits in i64") }

/// The session id from a `Cookie` header, if it carries ours.
pub fn from_cookie_header(header: &str) -> Option<&str> {
    header.split(';').filter_map(|pair| pair.trim().split_once('=')).find(|(name, _)| *name == COOKIE).map(|(_, v)| v)
}

/// `Set-Cookie` for a new session. `Path=/` so Mailu's logout, which clears this name at `/`, reaches it.
pub fn set_cookie(id: &str, secure: bool) -> String {
    format!("{COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict{}", if secure { "; Secure" } else { "" })
}

pub fn clear_cookie() -> String { format!("{COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict") }
