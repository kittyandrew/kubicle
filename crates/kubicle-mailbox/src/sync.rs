//! The engine: one task per mailbox owning one IMAP connection, which answers every list page and search from
//! Dovecot and carries out what the user does; and `Mailbox`, the handle the HTTP layer calls (docs/design.md,
//! IMAP). Between jobs it IDLEs on the viewed folder, so new mail there reaches the list within a second.

use std::{cmp::Reverse, collections::BTreeMap, collections::HashMap, collections::HashSet, collections::VecDeque, panic::AssertUnwindSafe};
use std::{path::Path, sync::Arc, sync::Mutex, sync::PoisonError, sync::Weak, time::Duration};

use axum::http::StatusCode;
use bytes::Bytes;
use futures_util::{FutureExt, future::BoxFuture};
use kubicle_api::{ArchiveType, Change, ComposeMode, DraftCopy, Event, Flag, Folder, FolderRole, FolderSizes, Hit, ListPage, ListQuery};
use kubicle_api::{Messages, Priority, Quota, Row, Search, SearchFilter, SearchInterval, SearchScope, SentCopy, Settings, SortColumn};
use kubicle_api::{SortOrder, SourceRef, Terms};
use kubicle_imap::{MailboxStatus, client::Client, client::Criterion, header::Fetched};
use kubicle_mail::render::list_post;
use tokio::{sync::OwnedSemaphorePermit, sync::Semaphore, sync::broadcast, sync::mpsc, sync::oneshot, time::Instant};
use tokio::{time::MissedTickBehavior, time::interval_at, time::sleep};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use crate::{Error, MailServer, compose::base_subject, threads, threads::Threaded, uploads::Uploads};

/// How often a STATUS round checks the folders IDLE does not watch; a changed one is then reported. A round
/// costs Dovecot about 0.1 ms per folder, INBOX too with `mailbox_list_index_include_inbox` (docs/design.md,
/// IMAP). NOTIFY (RFC 5465) would push the changes within 0.5 s instead, but imap-types cannot express it and
/// kubicle sends no hand-written command.
const POLL: Duration = Duration::from_secs(15);
/// The UIDs all kept orders (`Engine::orders`) hold at most, 12 bytes each: 12 MB, the oldest order going first.
const KEPT_UIDS: usize = 1_000_000;
/// The hits of each folder a sorted search over several merges, Roundcube's 1,000 (`list_search_messages`): the
/// merge holds each one's header, and the last page of a large search held every hit of every folder. The rows and
/// the total are those merged hits, as Roundcube's index is: a folder's later hits never show.
const MERGED: usize = 1000;
/// A user's uploads and imports read into memory at once: four 30 MB uploads had all four in memory.
const BUFFERED_REQUESTS: usize = 2;
/// Draft-copy successors kept for a concurrent save that still names a copy a save replaced (`Engine::replaced`).
/// The realistic race resolves within a save or two and consumes its entry, so past this cap the map is all a
/// serial client's never-read trail; dropping it bounds the memory one engine's lifetime of saves can hold.
const MAX_REPLACED: usize = 1024;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A session's Mailu token, which the session holds and the engine sees as long as the session lives.
pub type Token = Arc<Zeroizing<String>>;

/// What `Mailbox::run` takes: an operation on the engine and its IMAP connection.
pub trait Op<T>: for<'e> FnOnce(&'e mut Engine) -> BoxFuture<'e, Result<T, Error>> + Send + 'static {}
impl<T, F: for<'e> FnOnce(&'e mut Engine) -> BoxFuture<'e, Result<T, Error>> + Send + 'static> Op<T> for F {}

/// An operation queued for the engine's IMAP connection. It sends its own reply to the caller, and returns `Err`
/// only when the connection broke, which makes the engine reconnect.
type Job = Box<dyn Op<()>>;

/// What the `Mailbox` handles and the engine share.
struct Shared {
    user: String,
    events: broadcast::Sender<Event>,
    uploads: Uploads,
    /// Every folder as LIST and the last STATUS of it gave it, with the server's SPECIAL-USE roles.
    folders: Mutex<Vec<Folder>>,
    /// The user's own choice of special folders (`Settings::special_folders`), over the server's roles.
    special: Mutex<BTreeMap<FolderRole, Option<String>>>,
    /// Each live session's token, newest last. A reconnect proves every one and forgets those refused, which ends
    /// their sessions (`Mailbox::accepts`), so one session's token never keeps another alive.
    tokens: Mutex<Vec<Weak<Zeroizing<String>>>>,
    /// The storage quota as last read (`Engine::check_quota`).
    quota: Mutex<Option<Quota>>,
    /// Turns to hold a request's files in memory (`Mailbox::buffer_turn`).
    buffering: Arc<Semaphore>,
}

impl Shared {
    /// Every folder with its roles as the user sees them: a role the user chose goes to that folder, or to none,
    /// away from the folder the server's SPECIAL-USE flag gave it; a choice of a folder gone since keeps the server's.
    /// One folder may hold several, as Roundcube's `<role>_mbox` preferences may name one; INBOX holds Inbox alone.
    fn folders(&self) -> Vec<Folder> {
        let mut folders = self.folders.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let special = self.special.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let chosen = special
            .into_iter()
            .filter(|(role, name)| *role != FolderRole::Inbox && name.as_ref().is_none_or(|n| folders.iter().any(|f| f.name == *n)));
        let chosen: Vec<(FolderRole, Option<String>)> = chosen.collect();
        for folder in folders.iter_mut().filter(|f| !f.roles.contains(&FolderRole::Inbox)) {
            for (role, name) in &chosen {
                _ = if name.as_ref() == Some(&folder.name) { folder.roles.insert(*role) } else { folder.roles.remove(role) };
            }
        }
        folders
    }
}

/// One user's mailbox. Cheap to clone; the engine stops, logging out, when the last clone is dropped. Its end,
/// however it came (every token refused, a panic), closes `jobs`, which ends every session on it (`accepts`).
#[derive(Clone)]
pub struct Mailbox {
    jobs: mpsc::Sender<Job>,
    shared: Arc<Shared>,
}

/// The session store's hold on a user's engine, kept after the last session ends. A new sign-in reuses this engine
/// while it takes work, else waits for its last writes to finish.
pub struct Running {
    jobs: mpsc::WeakSender<Job>,
    shared: Weak<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// A new handle, if the engine still takes work: a handle is alive, and the engine did not end.
    #[must_use]
    pub fn mailbox(&self) -> Option<Mailbox> {
        let mailbox = Mailbox { jobs: self.jobs.upgrade()?, shared: self.shared.upgrade()? };
        mailbox.alive().then_some(mailbox)
    }

    /// The engine's task has not returned yet, though it may take no more work.
    #[must_use]
    pub fn running(&self) -> bool { !self.task.is_finished() }

    /// Waits for the engine's task to return: its jobs done, or ended by a panic.
    pub async fn stopped(self) { drop(self.task.await) }
}

impl Mailbox {
    /// Signs in to IMAP and starts the engine; also answers INBOX's UIDVALIDITY, read on its new connection. Its
    /// special folders are the server's until `settings_saved`.
    /// `kubicle_imap::Error::Expired`: IMAP refused the token, which is how `sso.php` proves the identity front vouched for.
    pub async fn start(mail: Arc<MailServer>, data_dir: &Path, user: &str, token: &Token) -> Result<(Self, Running, u32), Error> {
        let mut client = Client::connect(&mail.host, mail.imap_port, &mail.tls, &mail.tls_name, user, token).await?;
        let validity = client.inbox_validity().await?;
        let (jobs, receiver) = mpsc::channel(64);
        let shared = Arc::new(Shared {
            user: user.to_owned(),
            events: broadcast::channel(64).0,
            uploads: Uploads::open(data_dir, user)?,
            folders: Mutex::default(),
            special: Mutex::default(),
            tokens: Mutex::new(vec![Arc::downgrade(token)]),
            quota: Mutex::new(None),
            buffering: Arc::new(Semaphore::new(BUFFERED_REQUESTS)),
        });
        let engine = Engine {
            mail,
            client,
            shared: Arc::clone(&shared),
            jobs: receiver,
            viewed: None,
            status: HashMap::new(),
            orders: HashMap::new(),
            sent: None,
            replaced: HashMap::new(),
        };
        let running = Running { jobs: jobs.downgrade(), shared: Arc::downgrade(&shared), task: tokio::spawn(engine.run()) };
        Ok((Self { jobs, shared }, running, validity))
    }

    /// Ends every session of this engine and keeps it from connecting again: the address holds another account now,
    /// and only its open connection still sees the one before's mail. It stops when its last handle goes, as the
    /// caller drops its sessions; a reconnect meanwhile finds no token.
    pub fn retire(&self) { self.shared.tokens.lock().unwrap_or_else(PoisonError::into_inner).clear() }

    /// Server-sent events for this mailbox.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<Event> { self.shared.events.subscribe() }

    /// Shows `text` in every open tab of the user: for a background failure no request is waiting on.
    pub fn notice(&self, text: String) { drop(self.shared.events.send(Event::Notice(text))) } // no listener is fine

    /// Call after any settings save, a tab's or a folder rename's: it applies the special-folder roles and sends
    /// every open tab the folders and settings again.
    pub fn settings_saved(&self, settings: Settings) {
        *self.shared.special.lock().unwrap_or_else(PoisonError::into_inner) = settings.special_folders.clone();
        let _ = self.shared.events.send(Event::Folders(self.folders())); // no listener is fine
        let _ = self.shared.events.send(Event::Settings(Box::new(settings)));
    }

    pub fn quota(&self) -> Option<Quota> { *self.shared.quota.lock().unwrap_or_else(PoisonError::into_inner) }

    /// The engine has not ended, so it takes this handle's jobs.
    #[must_use]
    fn alive(&self) -> bool { !self.jobs.is_closed() }

    /// Another session's token, proved by its `sso.php`; reconnects prove it too.
    pub fn hold(&self, token: &Token) {
        let mut tokens = self.shared.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        tokens.retain(|t| t.strong_count() > 0);
        tokens.push(Arc::downgrade(token));
    }

    /// The token was not refused since its sign-in, and the engine did not end.
    #[must_use]
    pub fn accepts(&self, token: &Token) -> bool {
        self.alive() && self.shared.tokens.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|t| t.as_ptr() == Arc::as_ptr(token))
    }

    /// Every folder with its counts and the role the user sees.
    #[must_use]
    pub fn folders(&self) -> Vec<Folder> { self.shared.folders() }

    #[must_use]
    pub fn uploads(&self) -> &Uploads { &self.shared.uploads }

    /// A turn to read an upload or an import into memory, up to `kubicle_api::MAX_UPLOAD_MIB` each, to be held
    /// until its files are dropped: `BUFFERED_REQUESTS` of the user's at once, the rest waiting in turn.
    pub async fn buffer_turn(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.shared.buffering).acquire_owned().await.expect("the semaphore is never closed")
    }

    /// Runs `op` on the engine's connection, then reports what the server said changed meanwhile. Jobs run in the
    /// order asked, so a page asked after a change was accepted (a reloaded tab's included) never shows what it
    /// removed. An error that breaks the connection reaches the caller as `Unavailable` while the engine reconnects.
    pub async fn run<T: Send + 'static>(&self, op: impl Op<T>) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        let job = job(move |engine| {
            Box::pin(async move {
                let (result, reported) = (op(engine).await, engine.report_changes().await);
                let (answer, ended) = match (result, reported) {
                    (Err(e), _) if e.breaks_connection() => (Err(kubicle_imap::Error::Unavailable.into()), Err(e)),
                    // The op's own answer: a change that landed before the link dropped is reported as done.
                    (result, Err(e)) if e.breaks_connection() => (result, Err(e)),
                    (result, reported) => (reported.and(result), Ok(())),
                };
                let _ = reply.send(answer); // a caller that gave up is fine
                ended
            })
        });
        // An ended engine sends the client through `sso.php`, which starts another; one offline dropped the job.
        let gone = || Error::Imap(if self.alive() { kubicle_imap::Error::Unavailable } else { kubicle_imap::Error::Expired });
        self.jobs.send(job).await.map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }

    /// The rows of these messages of `folder`, in the order given; messages gone are left out.
    pub async fn rows(&self, folder: &str, uids: &[u32]) -> Result<Vec<Row>, Error> {
        let (folder, places): (String, Vec<Threaded>) =
            (folder.to_owned(), uids.iter().map(|&uid| Threaded { uid, depth: 0, replies: 0 }).collect());
        self.run(move |e| Box::pin(async move { e.rows(&folder, &places).await })).await
    }

    /// A message's raw bytes and its arrival time (IMAP's INTERNALDATE), from one FETCH, and its folder's UIDVALIDITY,
    /// which its signed part URLs carry; `NOT_FOUND` for a message that is gone.
    pub async fn message(&self, folder: &str, uid: u32) -> Result<(Bytes, i64, u32), Error> {
        let folder = folder.to_owned();
        self.run(move |e| {
            Box::pin(async move {
                let found = e.fetch(folder.clone(), uid).await?;
                let (raw, arrival) = found.ok_or_else(|| Error::status(StatusCode::NOT_FOUND, "no such message"))?;
                Ok((raw, arrival, e.validity(&folder))) // `fetch` selected it, which reads it
            })
        })
        .await
    }

    /// One part of a message, its MIME header and body, and the folder's UIDVALIDITY: all an inline image needs.
    pub async fn section(&self, folder: &str, uid: u32, number: &str) -> Result<Option<(Bytes, Bytes, u32)>, Error> {
        let (folder, number) = (folder.to_owned(), number.to_owned());
        self.run(move |e| {
            Box::pin(async move {
                e.ensure(&folder).await?;
                let validity = e.validity(&folder);
                Ok(e.client.fetch_section(uid, &number).await?.map(|(mime, body)| (mime, body, validity)))
            })
        })
        .await
    }

    /// Several messages' raw bytes from one job, `None` for one gone. A job each would send the engine back to the
    /// viewed folder in between: two SELECTs a message when `folder` is another one.
    pub async fn messages(&self, folder: &str, uids: Vec<u32>) -> Result<Vec<Option<Bytes>>, Error> {
        let folder = folder.to_owned();
        self.run(move |e| {
            Box::pin(async move {
                let mut found = Vec::with_capacity(uids.len());
                for uid in uids {
                    found.push(e.fetch(folder.clone(), uid).await?.map(|(raw, _)| raw));
                }
                Ok(found)
            })
        })
        .await
    }

    /// The headers of these messages: what a download names its files by.
    pub async fn headers(&self, folder: String, messages: Messages) -> Result<Vec<Fetched>, Error> {
        self.run(move |e| Box::pin(async move { e.headers(&folder, &messages).await })).await
    }

    /// The headers of the mail in Sent, for autocomplete and the From choices.
    pub async fn sent(&self) -> Result<Arc<Vec<Fetched>>, Error> { self.run(|e| Box::pin(e.sent())).await }

    /// Each folder's size, the sum of its messages' `RFC822.SIZE`.
    pub async fn sizes(&self) -> Result<FolderSizes, Error> { self.run(|e| Box::pin(e.sizes())).await }

    /// The page a `ListQuery` asks for: of its folder, or of its search's hits in the scope's folders, threaded
    /// if it asks for threads and covers one folder, as Roundcube runs THREAD over the hits. `around` asks for the
    /// page holding that message, not `query.page`; the answer's query says which page and whether it is threaded.
    /// A search stops between folders once `current` says its request ended, so typing never queues searches.
    pub async fn list(&self, query: ListQuery, around: Option<u32>, current: impl Fn() -> bool + Send + 'static) -> Result<ListPage, Error> {
        self.run(move |e| Box::pin(e.list(query, around, current))).await
    }
}

/// What a search over several folders compares by `column`: a number, or lowercased text.
fn sort_key(h: &Fetched, column: SortColumn) -> (i64, String) {
    let name = |a: Option<&kubicle_api::Address>| a.map(|a| a.name.as_deref().unwrap_or(&a.email).to_lowercase()).unwrap_or_default();
    match column {
        SortColumn::Unsorted => (0, String::new()),
        SortColumn::Arrival => (h.arrival, String::new()),
        SortColumn::Date => (h.date, String::new()),
        SortColumn::Size => (i64::from(h.size), String::new()),
        SortColumn::Subject => (0, base_subject(&h.subject, &["re", "fwd", "fw"], true).to_lowercase()),
        SortColumn::From | SortColumn::FromTo => (0, name(h.from.as_ref())),
        SortColumn::To => (0, name(h.to.first())),
        SortColumn::Cc => (0, name(h.cc.first())),
    }
}

/// One of `Engine::orders`.
struct Kept {
    order: Arc<Vec<Threaded>>,
    /// Its search reads flags (`reads_flags`), so it also goes when they change.
    flags: bool,
    made: Instant,
}

/// Whether a search by `criteria` reads flags. Every other search key, every sort key and THREAD=REFS hold while
/// only flags change, so only such an order goes when they do.
fn reads_flags(criteria: &[Criterion<'_>]) -> bool {
    criteria.iter().any(|c| match c {
        Criterion::Unseen | Criterion::Flagged | Criterion::Unanswered => true,
        Criterion::Not(inner) => reads_flags(std::slice::from_ref(*inner)),
        Criterion::Any(any) => reads_flags(any),
        Criterion::Body(_) | Criterion::Text(_) | Criterion::Header(..) | Criterion::Since(_) | Criterion::Before(_) => false,
    })
}

/// Recipients a list row carries. The list shows three and a "..." for any beyond (web `people`), so a row
/// needs four: three to show and one to keep the "...". Without the cap a mail addressed to tens of thousands
/// makes one list page megabytes the client reads three names of; the reader fetches the full list itself
/// (`render::message`).
const ROW_RECIPIENTS: usize = 4;

/// A fetched header as a list row, at its place in its thread.
fn row(h: Fetched, place: &Threaded) -> Row {
    let list_post = h.list_post.as_deref().and_then(list_post).is_some(); // a List-Post address kubicle can send to
    Row {
        uid: h.uid,
        date: h.date,
        arrival: h.arrival,
        from: h.from.unwrap_or_default(),
        to: h.to.into_iter().take(ROW_RECIPIENTS).collect(),
        subject: h.subject,
        size: h.size,
        flags: h.flags,
        attached: h.attached,
        depth: place.depth,
        replies: place.replies,
        list_post,
    }
}

/// Gives a closure the higher-ranked signature `Job` needs.
fn job(job: impl Op<()>) -> Job { Box::new(job) }

enum Wake {
    Job(Option<Job>),
    Change,
    Poll,
}

pub struct Engine {
    mail: Arc<MailServer>,
    client: Client,
    shared: Arc<Shared>,
    jobs: mpsc::Receiver<Job>,
    viewed: Option<String>,
    /// Each folder's STATUS as last read: one that differs next time changed meanwhile.
    status: HashMap<String, MailboxStatus>,
    /// Message orders a view or search asked for, by folder and what was asked, until a message arrives in the
    /// folder or leaves it: a page turn then costs one FETCH of its rows, not a SORT of the whole folder.
    orders: HashMap<(String, String), Kept>,
    /// The Sent folder's name and the headers of its mail, until it changes (`Engine::sent`).
    sent: Option<(String, Arc<Vec<Fetched>>)>,
    /// Draft copies a save replaced, old to new: a save that raced another one from the same draft then deletes
    /// the newer copy instead of leaving it behind.
    replaced: HashMap<DraftCopy, DraftCopy>,
}

impl Engine {
    /// A connection with the newest token IMAP accepts, after proving the others, so a session whose Mailu session
    /// ended goes at the first reconnect. `Expired` when none is accepted.
    async fn reconnect(&self) -> Result<Client, Error> {
        let (mail, user) = (&self.mail, &self.shared.user);
        let live: Vec<Token> = self.shared.tokens.lock().unwrap_or_else(PoisonError::into_inner).iter().filter_map(Weak::upgrade).collect();
        let (mut client, mut refused) = (None, Vec::new());
        for token in live.iter().rev() {
            match Client::connect(&mail.host, mail.imap_port, &mail.tls, &mail.tls_name, user, token).await {
                Ok(mut proved) if client.is_some() => proved.logout().await,
                Ok(proved) => client = Some(proved),
                Err(kubicle_imap::Error::Expired) => refused.push(token),
                Err(e) => return Err(e.into()),
            }
        }
        let mut tokens = self.shared.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        tokens.retain(|t| t.strong_count() > 0 && !refused.iter().any(|r| t.as_ptr() == Arc::as_ptr(r)));
        client.ok_or(Error::Imap(kubicle_imap::Error::Expired))
    }

    async fn run(mut self) {
        let mut backoff = MIN_BACKOFF;
        let mut connected = true;
        loop {
            let started = Instant::now();
            let ended = async {
                if !connected {
                    self.client = self.reconnect().await?;
                }
                self.session().await
            };
            match ended.await {
                Ok(()) => return,
                Err(Error::Imap(kubicle_imap::Error::Expired)) => {
                    let _ = self.shared.events.send(Event::SessionExpired);
                    info!(user = self.shared.user, "the Mailu session ended, or the account was made anew; the engine stops");
                    return;
                }
                Err(e) => {
                    connected = false; // status and orders stay: the next session's STATUS reports what changed meanwhile
                    if started.elapsed() > MAX_BACKOFF {
                        backoff = MIN_BACKOFF; // the connection worked for a while: this is a new outage
                    }
                    warn!(user = self.shared.user, error = %e, "IMAP connection lost, reconnecting in {backoff:?}");
                    if !self.offline_for(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// Waits before a reconnect, failing jobs meanwhile. False when every handle is gone.
    async fn offline_for(&mut self, delay: Duration) -> bool {
        let deadline = sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return true,
                job = self.jobs.recv() => if job.is_none() {
                    return false; // a job dropped unanswered reads as Unavailable to its caller
                },
            }
        }
    }

    async fn session(&mut self) -> Result<(), Error> {
        self.refresh_folders().await?;
        // One clock for the whole session: jobs and IDLE wake the loop far more often than every 15 s while the user
        // works, and a round timed from each wake would never come. A round missed while busy runs once, then waits.
        let mut rounds = interval_at(Instant::now() + POLL, POLL);
        rounds.set_missed_tick_behavior(MissedTickBehavior::Delay);
        'session: loop {
            loop {
                match self.jobs.try_recv() {
                    Ok(job) => self.take(job).await?,
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    // Every handle is gone: stop before the next write, as a new sign-in waits for this engine.
                    Err(mpsc::error::TryRecvError::Disconnected) => break 'session,
                }
            }
            let target = self.viewed.clone().unwrap_or_else(|| "INBOX".to_owned());
            if !self.select(&target).await? {
                if self.viewed.take().is_none() {
                    return Err(kubicle_imap::Error::Unavailable.into()); // INBOX itself refused: reconnect after a pause
                }
                continue; // the viewed folder is gone; INBOX takes its place
            }
            self.client.idle().await?;
            let wake = tokio::select! {
                job = self.jobs.recv() => Wake::Job(job),
                result = self.client.idle_wait() => {
                    result?;
                    Wake::Change
                }
                _ = rounds.tick() => Wake::Poll,
            };
            self.client.idle_done().await?;
            self.report_changes().await?;
            match wake {
                Wake::Job(None) => break,
                Wake::Job(Some(job)) => self.take(job).await?,
                Wake::Change => {}
                Wake::Poll => self.poll().await?,
            }
        }
        self.client.logout().await;
        Ok(())
    }

    /// Runs a job. One that panics, as a parser may on a sender's header, fails alone: its caller hears `Unavailable`,
    /// the connection it may have left mid-reply is replaced, and the engine goes on with the jobs after it.
    async fn take(&mut self, job: Job) -> Result<(), Error> {
        AssertUnwindSafe(job(self)).catch_unwind().await.unwrap_or_else(|_| Err(std::io::Error::other("a job panicked").into()))
    }

    /// LIST, then STATUS of every folder: the folder list and its counts show at once.
    async fn refresh_folders(&mut self) -> Result<(), Error> {
        let mut folders = self.client.list().await?;
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<String> = folders.iter().filter(|f| f.selectable).map(|f| f.name.clone()).collect();
        self.status.retain(|name, _| names.contains(name));
        self.orders.retain(|(name, _), _| names.contains(name));
        *self.shared.folders.lock().unwrap_or_else(PoisonError::into_inner) = folders;
        self.check(&names).await?;
        self.check_quota().await?;
        self.publish_folders();
        Ok(())
    }

    /// STATUS of every folder, which reports what changed in them since the last round.
    async fn poll(&mut self) -> Result<(), Error> {
        // LIST too: a folder another client created, renamed, deleted or (un)subscribed then shows without a reload.
        let mut listed = self.client.list().await?;
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let known = self.shared.folders.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let shape = |f: &Folder| (f.name.clone(), f.subscribed, f.selectable);
        if listed.iter().map(shape).ne(known.iter().map(shape)) {
            return self.refresh_folders().await;
        }
        let names: Vec<String> = known.into_iter().filter(|f| f.selectable).map(|f| f.name).collect();
        self.check(&names).await?;
        self.check_quota().await
    }

    /// Reads the STATUS of `names`: their counts, and each one that changed since the last read is reported.
    async fn check(&mut self, names: &[String]) -> Result<(), Error> {
        let mut changed = Vec::new();
        for (name, status) in self.client.status(names).await? {
            let mut folders = self.shared.folders.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(f) = folders.iter_mut().find(|f| f.name == name) {
                (f.total, f.unseen, f.uid_next) = (status.messages, status.unseen, status.uid_next);
            }
            drop(folders);
            if let Some(old) = self.status.insert(name.clone(), status).filter(|old| *old != status) {
                // The same count, next UID and validity: no message came or went, only flags changed.
                let moved = (old.messages, old.uid_next, old.uid_validity) != (status.messages, status.uid_next, status.uid_validity);
                changed.push((name, moved));
            }
        }
        for (name, moved) in &changed {
            self.orders.retain(|(folder, _), kept| folder != name || !moved && !kept.flags);
            self.sent.take_if(|(sent, _)| sent == name);
            let _ = self.shared.events.send(Event::FolderChanged { folder: name.clone() }); // no listener is fine
        }
        if !changed.is_empty() {
            self.publish_folders();
        }
        Ok(())
    }

    /// Reports what the server said changed in the selected folder since the last call.
    async fn report_changes(&mut self) -> Result<(), Error> {
        match self.client.selected().map(str::to_owned) {
            Some(folder) if self.client.take_changed() => self.check(&[folder]).await,
            _ => Ok(()),
        }
    }

    /// Selects `folder` unless it is selected, then reads its STATUS, which tells what changed while it was not.
    /// False when the folder is not in the folder list, or the server refuses to open it, which rereads the list.
    async fn select(&mut self, folder: &str) -> Result<bool, Error> {
        if self.client.selected() == Some(folder) {
            return Ok(true);
        }
        if !self.shared.folders.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|f| f.selectable && f.name == folder) {
            return Ok(false);
        }
        match self.client.select(folder).await {
            Err(kubicle_imap::Error::Rejected { text, .. }) => {
                warn!(folder, text, "folder refused; rereading the folder list");
                self.refresh_folders().await?;
                Ok(false)
            }
            selected => {
                selected?;
                self.check(&[folder.to_owned()]).await?;
                Ok(true)
            }
        }
    }

    /// Selects `folder` for a request that reads or changes it, without moving the IDLE watch.
    async fn ensure(&mut self, folder: &str) -> Result<(), Error> {
        if self.select(folder).await? { Ok(()) } else { Err(Error::status(StatusCode::NOT_FOUND, format!("no folder named {folder:?}"))) }
    }

    /// The folder the user looks at, the one the engine IDLEs on.
    pub async fn view(&mut self, folder: String) -> Result<(), Error> {
        self.ensure(&folder).await?;
        self.viewed = Some(folder);
        Ok(())
    }

    /// A folder's UIDVALIDITY as last selected or asked, 0 before then.
    pub fn validity(&self, folder: &str) -> u32 { self.status.get(folder).map_or(0, |s| s.uid_validity) }

    /// The raw message and its arrival time, for rendering and downloads. Does not mark it read.
    pub async fn fetch(&mut self, folder: String, uid: u32) -> Result<Option<(Bytes, i64)>, Error> {
        self.ensure(&folder).await?;
        if let Some(message) = self.client.fetch_message(uid).await? {
            return Ok(Some(message));
        }
        self.client.noop().await?; // mail another client added shows on this connection only once the server told it so
        Ok(self.client.fetch_message(uid).await?)
    }

    /// The messages of `folder` matching `criteria` in the order a view shows them, by `column` (`Unsorted`: by
    /// UID) and with `threads` as threads; kept as `Engine::orders` says.
    async fn sorted(
        &mut self, folder: &str, column: SortColumn, order: SortOrder, threads: bool, criteria: &[Criterion<'_>],
    ) -> Result<Arc<Vec<Threaded>>, Error> {
        let key = (folder.to_owned(), format!("{column:?} {order:?} {threads} {criteria:?}"));
        if let Some(kept) = self.orders.get(&key) {
            return Ok(Arc::clone(&kept.order));
        }
        self.ensure(folder).await?;
        let sorted = self.client.sort(column, order == SortOrder::Descending, criteria).await?;
        let found = if threads {
            threads::order(&self.client.thread(criteria).await?, &sorted)
        } else {
            sorted.into_iter().map(|uid| Threaded { uid, depth: 0, replies: 0 }).collect()
        };
        let mut kept: usize = self.orders.values().map(|k| k.order.len()).sum();
        while kept + found.len() > KEPT_UIDS
            && let Some(oldest) = self.orders.iter().min_by_key(|(_, k)| k.made).map(|(key, _)| key.clone())
        {
            kept -= self.orders.remove(&oldest).map_or(0, |k| k.order.len());
        }
        let made = Kept { order: Arc::new(found), flags: reads_flags(criteria), made: Instant::now() };
        Ok(Arc::clone(&self.orders.entry(key).or_insert(made).order))
    }

    /// One page of `order`, `size` messages or with `threads` `size` whole threads, as Roundcube pages a threaded
    /// list; and how many messages or threads there are in all.
    async fn page(&mut self, folder: &str, order: &[Threaded], page: u32, size: u32, threads: bool) -> Result<(u32, Vec<Row>), Error> {
        let first = (page as usize).saturating_mul(size as usize);
        let roots: Vec<usize> =
            if threads { order.iter().enumerate().filter(|(_, t)| t.depth == 0).map(|(i, _)| i).collect() } else { Vec::new() };
        // Where the list's item n starts in `order`: thread n's root, or row n; past the last item, the end.
        let at = |n: usize| if threads { roots.get(n).copied().unwrap_or(order.len()) } else { n.min(order.len()) };
        let total = if threads { roots.len() } else { order.len() };
        let rows = self.rows(folder, &order[at(first)..at(first.saturating_add(size as usize))]).await?;
        Ok((u32::try_from(total).unwrap_or(u32::MAX), rows))
    }

    /// The rows of these messages in the order given, at their places in their threads; messages gone are left out.
    async fn rows(&mut self, folder: &str, places: &[Threaded]) -> Result<Vec<Row>, Error> {
        let mut found = self.fetched(folder, places).await?;
        Ok(places.iter().filter_map(|t| found.remove(&t.uid).map(|h| row(h, t))).collect())
    }

    /// The headers of these messages by UID; messages gone are left out.
    async fn fetched(&mut self, folder: &str, places: &[Threaded]) -> Result<HashMap<u32, Fetched>, Error> {
        let uids = Messages::Uids(places.iter().map(|t| t.uid).collect());
        Ok(self.headers(folder, &uids).await?.into_iter().map(|h| (h.uid, h)).collect())
    }

    async fn headers(&mut self, folder: &str, messages: &Messages) -> Result<Vec<Fetched>, Error> {
        self.ensure(folder).await?;
        Ok(self.client.fetch_rows(messages).await?)
    }

    /// The headers of the mail in Sent, kept until Sent changes: autocomplete asks at every key.
    pub async fn sent(&mut self) -> Result<Arc<Vec<Fetched>>, Error> {
        let Ok(folder) = self.role(FolderRole::Sent) else { return Ok(Arc::default()) };
        if let Some((_, sent)) = self.sent.as_ref().filter(|(name, _)| *name == folder) {
            return Ok(Arc::clone(sent));
        }
        let sent = Arc::new(self.headers(&folder, &Messages::All).await?);
        self.sent = Some((folder, Arc::clone(&sent)));
        Ok(sent)
    }

    async fn sizes(&mut self) -> Result<FolderSizes, Error> {
        let mut sizes = FolderSizes::new();
        for folder in self.shared.folders().into_iter().filter(|f| f.selectable) {
            if self.select(&folder.name).await? {
                // false: another client deleted it since the folder list was read
                sizes.insert(folder.name, self.client.fetch_sizes().await?);
            }
        }
        Ok(sizes)
    }

    /// `Mailbox::list`'s job: every folder's order and the page's rows in one go, so the engine's IDLE folder is
    /// selected again only once it is done.
    async fn list(&mut self, mut query: ListQuery, around: Option<u32>, current: impl Fn() -> bool) -> Result<ListPage, Error> {
        let all = self.shared.folders();
        let sent: Vec<String> = all.iter().filter(|f| kubicle_api::lists_recipients(&all, &f.name)).map(|f| f.name.clone()).collect();
        // Wider scopes cover the subscribed folders, those the pane shows, as Roundcube's search does and in its
        // order: the folder and those below it, or all folders by name ignoring case (`natcasesort`).
        let shown = |f: &Folder| f.subscribed || f.name == query.folder;
        let mut scoped: Vec<Folder> = match query.search.as_ref().map(|s| s.scope) {
            None | Some(SearchScope::Folder) => all.into_iter().filter(|f| f.name == query.folder).collect(),
            Some(SearchScope::Subfolders) => family(all, &query.folder).into_iter().filter(shown).collect(),
            Some(SearchScope::All) => all.into_iter().filter(shown).collect(),
        };
        if query.search.as_ref().is_some_and(|s| s.scope == SearchScope::All) {
            scoped.sort_by_key(|f| f.name.to_lowercase());
        }
        let folders: Vec<String> = scoped.into_iter().filter(|f| f.selectable).map(|f| f.name).collect();
        if query.search.as_ref().is_some_and(Search::is_blank) || folders.is_empty() && query.search.is_some() {
            return Ok(ListPage { query, total: 0, rows: Vec::new() });
        }
        query.threads &= folders.len() == 1;
        let hit = |folder: &String| {
            let folder = folder.clone();
            move |row| Hit { folder: folder.clone(), row }
        };
        let column = |folder: &String| if query.sort == SortColumn::FromTo && sent.contains(folder) { SortColumn::To } else { query.sort };
        if let [folder] = folders.as_slice() {
            let order = self.order(folder.clone(), query.search.clone(), column(folder), query.order, query.threads).await?;
            if let Some(at) = around.and_then(|uid| order.iter().position(|t| t.uid == uid)) {
                // The index of its thread, or of the message itself in a flat list.
                let place = if query.threads { order[..=at].iter().filter(|t| t.depth == 0).count().saturating_sub(1) } else { at };
                query.page = u32::try_from(place / query.size as usize).unwrap_or(0);
            }
            let (total, rows) = self.page(folder, &order, query.page, query.size, query.threads).await?;
            return Ok(ListPage { rows: rows.into_iter().map(hit(folder)).collect(), total, query });
        }
        let (first, end) =
            ((query.page as usize).saturating_mul(query.size as usize), (query.page as usize + 1).saturating_mul(query.size as usize));
        let (descending, unsorted) = (query.order == SortOrder::Descending, query.sort == SortColumn::Unsorted);
        // The selected folder is never selected again, which for 50,000 messages costs Dovecot 23 ms (docs/design.md,
        // IMAP). Sorted, it goes first, and each visit fetches the folder's part of the merge below; unsorted, it goes
        // last, as each folder's part of the page is known only once every folder is counted.
        let home = folders.iter().position(|f| self.client.selected() == Some(f.as_str()));
        let mut visits: Vec<usize> = (0..folders.len()).collect();
        visits.sort_by_key(|&i| (Some(i) == home) == unsorted);
        let (mut orders, mut fetched) = (vec![Arc::default(); folders.len()], vec![HashMap::new(); folders.len()]);
        for i in visits {
            if !current() {
                debug!(folder = folders[i], "a search stopped: its request ended");
                return Err(Error::status(StatusCode::CONFLICT, "the search's request ended"));
            }
            orders[i] = self.order(folders[i].clone(), query.search.clone(), column(&folders[i]), query.order, false).await?;
            if !unsorted {
                fetched[i] = self.fetched(&folders[i], &orders[i][..end.min(MERGED).min(orders[i].len())]).await?;
            }
        }
        let merged = |order: &Arc<Vec<Threaded>>| if unsorted { order.len() } else { order.len().min(MERGED) }; // all a merge shows
        let total = u32::try_from(orders.iter().map(merged).sum::<usize>()).unwrap_or(u32::MAX);
        let mut rows = Vec::new();
        if unsorted {
            // Each folder's hits together in the order of the folders, which descending reverses whole
            // (`rcube_result_multifolder`).
            let mut shown: Vec<usize> = (0..folders.len()).collect();
            if descending {
                shown.reverse();
            }
            let mut before = 0;
            let parts: Vec<(usize, &[Threaded])> = shown
                .into_iter()
                .map(|i| {
                    let n = orders[i].len();
                    before += n;
                    // The page's window over all hits, less the `before - n` hits of the folders ahead of this one.
                    (i, &orders[i][first.saturating_sub(before - n).min(n)..end.saturating_sub(before - n).min(n)])
                })
                .filter(|(_, part)| !part.is_empty())
                .collect();
            let mut fetching = parts.clone();
            // The folder the visits above left selected, home, goes first, which saves one SELECT.
            fetching.sort_by_key(|(i, _)| self.client.selected() != Some(folders[*i].as_str()));
            for (i, part) in fetching {
                fetched[i] = self.fetched(&folders[i], part).await?;
            }
            for (i, part) in parts {
                rows.extend(part.iter().filter_map(|t| fetched[i].remove(&t.uid).map(|h| hit(&folders[i])(row(h, t)))));
            }
            return Ok(ListPage { query, total, rows });
        }
        // Across folders by the sort column: each folder's first `end` hits, up to `MERGED`, stay in Dovecot's order, and
        // each row is the next hit of the folder whose next hit sorts first, the earlier folder on a tie. Sorting them
        // all by our own key instead reorders a folder's hits wherever it disagrees with Dovecot's (base subjects, ties
        // under REVERSE), and the pages then repeat some hits and never show others.
        let mut queues = Vec::with_capacity(folders.len());
        for (i, (order, mut fetched)) in orders.iter().zip(fetched).enumerate() {
            let hits: VecDeque<_> = order
                .iter()
                .take(end.min(MERGED))
                .filter_map(|t| fetched.remove(&t.uid))
                .map(|h| (sort_key(&h, column(&folders[i])), h))
                .collect();
            queues.push((folders[i].clone(), hits));
        }
        let flat = Threaded { uid: 0, depth: 0, replies: 0 };
        for taken in 0..end {
            let next = queues.iter().enumerate().filter_map(|(i, (_, hits))| Some((&hits.front()?.0, i)));
            let Some((_, i)) = (if descending { next.max_by_key(|&(key, i)| (key, Reverse(i))) } else { next.min() }) else { break };
            let (_, header) = queues[i].1.pop_front().expect("the folder whose next hit was just read");
            if taken >= first {
                rows.push(Hit { folder: queues[i].0.clone(), row: row(header, &flat) });
            }
        }
        Ok(ListPage { query, total, rows })
    }

    /// The messages of `folder` a view shows, or of its hits for `search`: the text in the fields it names, the
    /// type and the date, as Roundcube's search asks IMAP. The header phase (`full` false) leaves out the fields that
    /// read the message text, which Mailu answers from its full-text index.
    async fn order(
        &mut self, folder: String, search: Option<Search>, column: SortColumn, order: SortOrder, threads: bool,
    ) -> Result<Arc<Vec<Threaded>>, Error> {
        let Some(search) = search else { return self.sorted(&folder, column, order, threads, &[]).await };
        let (Terms { fields: f, text, prefixed }, full) = (search.terms(), search.full);
        // Roundcube's `reply-to:` prefix is `HEADER REPLY-TO` alone; the ticked Sender field also takes Mail-Reply-To.
        let both = [Criterion::Header("Reply-To", text), Criterion::Header("Mail-Reply-To", text)];
        let reply_to = if prefixed { &both[..1] } else { &both[..] };
        let followup = [Criterion::Header("Followup-To", text), Criterion::Header("Mail-Followup-To", text)];
        let fields: [(bool, &[Criterion<'_>]); 9] = [
            (f.subject, &[Criterion::Header("Subject", text)]),
            (f.from, &[Criterion::Header("From", text)]),
            (f.to, &[Criterion::Header("To", text)]),
            (f.cc, &[Criterion::Header("Cc", text)]),
            (f.text && full, &[Criterion::Text(text)]),
            (f.body && full, &[Criterion::Body(text)]),
            (f.reply_to && full, reply_to),
            (f.followup_to && full, &followup),
            (f.bcc && full, &[Criterion::Header("Bcc", text)]),
        ];
        let any: Vec<Criterion<'_>> = fields.iter().filter(|(on, _)| *on).flat_map(|(_, c)| c.iter().copied()).collect();
        // Roundcube's `X-Priority` tests; Normal means none of the other levels.
        let levels = ["1", "2", "3", "4", "5"].map(|n| Criterion::Header("X-Priority", n));
        // Roundcube's "With attachment": a guess from the top Content-Type, as IMAP cannot search the structure.
        let attachment =
            ["application/", "multipart/mixed", "multipart/signed", "multipart/report"].map(|t| Criterion::Header("Content-Type", t));
        let mut all: Vec<Criterion<'_>> = Vec::new();
        if !text.is_empty() {
            all.push(Criterion::Any(&any));
        }
        match search.filter {
            SearchFilter::All => {}
            SearchFilter::Unread => all.push(Criterion::Unseen),
            SearchFilter::Flagged => all.push(Criterion::Flagged),
            SearchFilter::Unanswered => all.push(Criterion::Unanswered),
            SearchFilter::WithAttachment => all.push(Criterion::Any(&attachment)),
            SearchFilter::Priority(Priority::Normal) => all.extend([0, 1, 3, 4].map(|i| Criterion::Not(&levels[i]))),
            SearchFilter::Priority(level) => all.push(levels[level as usize]),
        }
        // Today in UTC, as Roundcube's PHP dates it under Mailu; a day number keeps this search's order key all day.
        let today = jiff::Timestamp::now().as_second().div_euclid(86_400);
        match search.interval {
            SearchInterval::Any => {}
            SearchInterval::YoungerThan(p) => all.push(Criterion::Since(today - p.days())),
            SearchInterval::OlderThan(p) => all.push(Criterion::Before(today - p.days())),
        }
        self.sorted(&folder, column, order, threads, &all).await
    }

    /// Carries out `change` as the user's settings shape it. Roundcube's `read_when_deleted` (any move into Trash:
    /// Delete, a drag, Move to, Empty) and `read_on_archive` mark the mail read; the \Seen STORE runs in the move's
    /// own job just before it, so the two never arrive reversed. `archive_type` and `zone`: how Archive divides.
    /// Answers a rename's old and new name, which the caller moves the folder's preferences by.
    pub async fn change(&mut self, change: Change, settings: Settings, zone: jiff::tz::TimeZone) -> Result<Option<(String, String)>, Error> {
        let mut renamed = None;
        match change {
            Change::Flag { folder, messages, flag, set } => {
                self.ensure(&folder).await?;
                Ok(self.client.store(&messages, flag, set).await?)
            }
            Change::Move { folder, messages, to } => {
                let seen = settings.read_when_deleted && self.role(FolderRole::Trash).is_ok_and(|trash| trash == to);
                self.move_messages(&folder, &messages, to, seen).await
            }
            Change::Archive { folder, messages, .. } => {
                let archive = self.role(FolderRole::Archive)?;
                let folders = self.shared.folders();
                let delimiter = folders.iter().find(|f| f.name == folder).and_then(|f| f.delimiter);
                if kubicle_api::within(&folder, &archive, delimiter) {
                    return Ok(None); // already archived, as the plugin skips the archive folder and those below it
                }
                // Dividing reads each message's date or sender.
                let headers = match settings.archive_type {
                    ArchiveType::None | ArchiveType::Folder => Vec::new(),
                    _ => self.headers(&folder, &messages).await?,
                };
                let delimiter = folders.iter().find(|f| f.name == archive).and_then(|f| f.delimiter);
                // The folder list read once, and again once at the end, as the plugin's `subfolder_worker`: a refresh
                // per target sent N^2/2 STATUS for N new senders.
                let (mut names, mut created) = (folders.into_iter().map(|f| f.name).collect(), false);
                for (to, messages) in archive_targets(&headers, &folder, messages, &archive, delimiter, settings.archive_type, &zone) {
                    created |= self.create_path(&mut names, &archive, &to, delimiter).await?;
                    self.move_messages(&folder, &messages, to, settings.read_on_archive).await?;
                }
                if created { self.refresh_folders().await } else { Ok(()) }
            }
            Change::Report { folder, messages, spam } => {
                let junk = self.role(FolderRole::Junk)?;
                if spam == (folder == junk) {
                    return Ok(None); // Junk on mail in Junk, or Not junk on mail outside it: nothing to report
                }
                let (add, remove, to) = if spam { ("Junk", "NonJunk", junk) } else { ("NonJunk", "Junk", "INBOX".to_owned()) };
                self.ensure(&folder).await?;
                self.client.store_keywords(&messages, add, remove).await?;
                self.move_messages(&folder, &messages, to, false).await
            }
            Change::Copy { folder, messages, to } => {
                self.ensure(&folder).await?;
                self.client.copy(&messages, &to).await?;
                self.check(&[to]).await?;
                self.check_quota().await
            }
            Change::Delete { folder, messages } => {
                self.ensure(&folder).await?;
                self.client.expunge(&messages).await?;
                self.check_quota().await
            }
            Change::CreateFolder { name, parent } => {
                let name = folder_name(&self.shared.folders(), parent.as_deref(), &name, None)?;
                self.client.create(&name).await?;
                self.client.subscribe(&name, true).await?; // else it never shows: the folder column lists subscribed ones
                self.refresh_folders().await
            }
            Change::RenameFolder { from, name, parent } => {
                let to = folder_name(&self.shared.folders(), parent.as_deref(), &name, Some(&from))?;
                if to == from {
                    return Ok(None);
                }
                // RENAME leaves subscriptions on the old names, so move them, children included, as Roundcube's
                // rcube_imap::rename_folder does.
                let moved: Vec<String> = family(self.shared.folders(), &from).into_iter().filter(|f| f.subscribed).map(|f| f.name).collect();
                self.client.rename(&from, &to).await?;
                for old in moved {
                    self.client.subscribe(&old, false).await?;
                    self.client.subscribe(&format!("{to}{}", &old[from.len()..]), true).await?;
                }
                renamed = Some((from, to));
                self.refresh_folders().await
            }
            Change::DeleteFolder { name } => {
                // Children first, deepest first, as Roundcube's rcube_imap::delete_folder: a server like Dovecot
                // keeps a deleted parent as \NoSelect while it still has children. Each is unsubscribed only once
                // deleted, so a refused DELETE leaves its folder in the folder pane.
                let mut doomed: Vec<String> = family(self.shared.folders(), &name).into_iter().map(|f| f.name).collect();
                doomed.sort_by_key(|n| std::cmp::Reverse(n.len()));
                for folder in doomed {
                    self.client.delete(&folder).await?;
                    self.client.subscribe(&folder, false).await?;
                }
                self.refresh_folders().await
            }
            Change::Subscribe { name, subscribed } => {
                self.client.subscribe(&name, subscribed).await?;
                self.refresh_folders().await
            }
        }?;
        Ok(renamed)
    }

    /// Creates and subscribes each folder from below `archive` down to `to` that is not in `names` yet, as the archive
    /// plugin's `subfolder_worker`, and adds it there; true when it created one.
    async fn create_path(&mut self, names: &mut HashSet<String>, archive: &str, to: &str, delimiter: Option<char>) -> Result<bool, Error> {
        let Some(d) = delimiter.filter(|_| to != archive) else { return Ok(false) };
        let mut created = false;
        // Each path from `archive`, which exists, to `to`: the delimiters after archive's own, then `to` itself.
        for (end, _) in to.match_indices(d).skip(archive.matches(d).count()).chain([(to.len(), "")]) {
            let path = &to[..end];
            if !names.contains(path) {
                self.client.create(path).await?;
                self.client.subscribe(path, true).await?;
                names.insert(path.to_owned());
                created = true;
            }
        }
        Ok(created)
    }

    /// Moves messages, marked read first in the same job when `seen`, and reads the quota after.
    async fn move_messages(&mut self, folder: &str, messages: &Messages, to: String, seen: bool) -> Result<(), Error> {
        self.ensure(folder).await?;
        if seen {
            self.client.store(messages, Flag::Seen, true).await?;
        }
        self.client.r#move(messages, &to).await?;
        self.check(&[to]).await?;
        self.check_quota().await
    }

    /// Roundcube's "Clear Trash on logout" (`rcmail::logout_actions`): every message in Trash and below it, or those
    /// that arrived before the day `before` (`Criterion::Before`); a folder below Trash goes once none of its
    /// messages stay and nothing below it stayed.
    pub async fn purge_trash(&mut self, before: Option<i64>) -> Result<(), Error> {
        let trash = self.role(FolderRole::Trash)?;
        let mut folders = family(self.shared.folders(), &trash);
        folders.sort_by(|a, b| b.name.cmp(&a.name)); // children before their parents
        let mut kept: Vec<Folder> = Vec::new();
        for folder in folders {
            if folder.selectable {
                self.ensure(&folder.name).await?;
            }
            let (everything, doomed) = match (folder.selectable, before) {
                (false, _) => (true, Messages::Uids(Vec::new())),
                (true, None) => (true, Messages::All),
                (true, Some(before)) => {
                    let (uids, total) = (self.client.search(&[Criterion::Before(before)]).await?, self.client.search(&[]).await?.len());
                    (uids.len() == total, Messages::Uids(uids))
                }
            };
            let below_kept = kept.iter().any(|k| k.name != folder.name && kubicle_api::within(&k.name, &folder.name, folder.delimiter));
            if folder.name != trash && everything && !below_kept {
                self.client.delete(&folder.name).await?;
                self.client.subscribe(&folder.name, false).await?;
                continue;
            }
            if folder.selectable {
                self.client.expunge(&doomed).await?;
            }
            kept.push(folder);
        }
        self.refresh_folders().await
    }

    /// Reads the storage quota and tells every tab when it changed. A server that refuses GETQUOTAROOT, as one without
    /// QUOTA does (the dev fixture), keeps the last.
    async fn check_quota(&mut self) -> Result<(), Error> {
        let quota = match self.client.quota().await {
            Err(kubicle_imap::Error::Rejected { .. }) => return Ok(()),
            quota => quota?,
        };
        let mut last = self.shared.quota.lock().unwrap_or_else(PoisonError::into_inner);
        if *last != quota {
            *last = quota;
            let _ = self.shared.events.send(Event::Quota(quota)); // no listener is fine
        }
        Ok(())
    }

    /// Stores a draft's copy in place of its previous one, in that one's folder, else in Drafts; returns the new copy.
    pub async fn save_draft(&mut self, message: Vec<u8>, replaces: Option<DraftCopy>) -> Result<DraftCopy, Error> {
        let folder = match &replaces {
            Some(old) => old.folder.clone(),
            None => self.role(FolderRole::Drafts)?,
        };
        let uid = self.append(&folder, &[Flag::Draft, Flag::Seen], &message).await?;
        let copy = DraftCopy { folder, uid };
        if let Some(old) = replaces {
            self.drop_draft(old, Some(copy.clone())).await?;
        }
        Ok(copy)
    }

    /// Files a sent message's copy in Sent, flags what it answered or forwarded and deletes the draft's copy.
    pub async fn file_sent(
        &mut self, copy: Vec<u8>, sent_copy: SentCopy, sources: Vec<SourceRef>, draft: Option<DraftCopy>,
    ) -> Result<(), Error> {
        // Each step runs whether or not the one before it failed; the first failure is the answer.
        let filed = match sent_copy {
            SentCopy::Sent => match self.role(FolderRole::Sent) {
                Ok(folder) => self.file_copy(&folder, &copy).await,
                Err(e) => Err(e),
            },
            SentCopy::Folder(folder) => self.file_copy(&folder, &copy).await,
            SentCopy::NotSaved => Ok(()),
        };
        let flagged = self.flag_sources(sources).await;
        let dropped = match draft {
            Some(draft) => self.drop_draft(draft, None).await,
            None => Ok(()),
        };
        filed.and(flagged).and(dropped)
    }

    /// Stores a sent message's copy. Into Sent, when the copy is the only change there since `sent` read its
    /// headers, the copy's header joins them, so the next Reply or autocomplete does not read all of Sent again.
    async fn file_copy(&mut self, folder: &str, copy: &[u8]) -> Result<(), Error> {
        // Sent's headers are taken out first: the APPEND's STATUS check would drop them as a change to Sent.
        let (before, cached) = (self.status.get(folder).copied(), self.sent.take_if(|(name, _)| name == folder));
        let uid = self.append(folder, &[Flag::Seen], copy).await?;
        let Some((name, headers)) = cached else { return Ok(()) };
        let only = match (before, self.status.get(folder)) {
            (Some(before), Some(after)) =>
                after.uid_validity == before.uid_validity && after.messages == before.messages + 1 && after.uid_next == uid + 1,
            _ => false,
        };
        if only {
            let mut headers = Arc::unwrap_or_clone(headers);
            headers.extend(self.headers(folder, &Messages::Uids(vec![uid])).await?);
            self.sent = Some((name, Arc::new(headers)));
        }
        Ok(())
    }

    /// `\Answered` on what a sent message replied to, `$Forwarded` on what it forwarded, a folder at a time as
    /// Roundcube's `rcmail::get_uids` groups them; every folder is tried, the first failure is the answer.
    async fn flag_sources(&mut self, sources: Vec<SourceRef>) -> Result<(), Error> {
        let mut groups: Vec<(String, Flag, Vec<u32>)> = Vec::new();
        for SourceRef { folder, uid, mode } in sources {
            let forward = matches!(mode, ComposeMode::Forward | ComposeMode::ForwardAttachment);
            let flag = if forward { Flag::Forwarded } else { Flag::Answered };
            match groups.iter_mut().find(|(f, g, _)| *f == folder && *g == flag) {
                Some((_, _, uids)) => uids.push(uid),
                None => groups.push((folder, flag, vec![uid])),
            }
        }
        let mut result = Ok(());
        for (folder, flag, uids) in groups {
            let flagged = match self.ensure(&folder).await {
                Ok(()) => self.client.store(&Messages::Uids(uids), flag, true).await.map_err(Error::from),
                Err(e) => Err(e),
            };
            result = result.and(flagged);
        }
        result
    }

    /// Deletes a draft's copy, or the newer copy a raced save put in its place.
    async fn drop_draft(&mut self, mut copy: DraftCopy, successor: Option<DraftCopy>) -> Result<(), Error> {
        while let Some(newer) = self.replaced.remove(&copy) {
            copy = newer;
        }
        if let Some(successor) = successor {
            // Keys are expunged copies a concurrent save might still name. A referenced one is consumed above, so
            // only a serial client's trail piles up; past the cap it is all trail. Dropping it keeps the just-made
            // entry, so a save racing the one before still resolves; a straggler many saves behind would orphan a
            // draft copy, as a third concurrent save already does.
            if self.replaced.len() >= MAX_REPLACED {
                self.replaced.clear();
            }
            self.replaced.insert(copy.clone(), successor);
        }
        self.ensure(&copy.folder).await?;
        Ok(self.client.expunge(&Messages::Uids(vec![copy.uid])).await?)
    }

    /// Import: appends each message to `folder` unflagged, as Roundcube's import does, `MAX_UIDS` at most (a 36 MiB file
    /// of tiny ones made millions of APPENDs in this one job, ahead of all else the user asks); returns how many it
    /// stored. The folder and quota are read once after: the server refuses an APPEND past the quota itself.
    pub async fn import(&mut self, folder: String, messages: impl Iterator<Item = Vec<u8>> + Send) -> Result<u32, Error> {
        let mut stored = 0;
        for message in messages.take(kubicle_api::MAX_UIDS) {
            self.client.append(&folder, &[], &message).await?;
            stored += 1;
        }
        if self.client.selected() != Some(folder.as_str()) {
            self.check(std::slice::from_ref(&folder)).await?;
        }
        self.check_quota().await?;
        Ok(stored)
    }

    /// Stores a message; the selected folder hears of it by EXISTS, any other by its STATUS.
    async fn append(&mut self, folder: &str, flags: &[Flag], message: &[u8]) -> Result<u32, Error> {
        let uid = self.client.append(folder, flags, message).await?;
        if self.client.selected() != Some(folder) {
            self.check(&[folder.to_owned()]).await?;
        }
        self.check_quota().await?;
        Ok(uid)
    }

    /// The name of the folder with a role, such as Drafts or Sent.
    fn role(&self, role: FolderRole) -> Result<String, Error> {
        let folder = self.shared.folders().into_iter().find(|f| f.roles.contains(&role));
        folder.map(|f| f.name).ok_or_else(|| Error::status(StatusCode::NOT_FOUND, format!("no {role:?} folder")))
    }

    fn publish_folders(&self) { drop(self.shared.events.send(Event::Folders(self.shared.folders()))) } // no listener is fine
}

/// Where Archive files `messages` of `folder`, as the archive plugin's `move_messages` divides them by
/// `archive_type`: one target for none and for the original folder, else a target per year, month or sender, read
/// from their `headers`. Without a hierarchy delimiter everything goes to Archive itself, as Roundcube offers no
/// division where folders cannot hold folders.
fn archive_targets(
    headers: &[Fetched], folder: &str, messages: Messages, archive: &str, delimiter: Option<char>, archive_type: ArchiveType,
    zone: &jiff::tz::TimeZone,
) -> Vec<(String, Messages)> {
    let Some(d) = delimiter else { return vec![(archive.to_owned(), messages)] };
    let path = |parts: &[&str]| std::iter::once(archive).chain(parts.iter().copied()).collect::<Vec<_>>().join(&d.to_string());
    let target = |date: i64, from: &str| {
        let at = jiff::Timestamp::from_second(date).unwrap_or_default().to_zoned(zone.clone());
        let (y, m) = (format!("{:04}", at.year()), format!("{:02}", at.month()));
        match archive_type {
            ArchiveType::None => archive.to_owned(),
            ArchiveType::Folder => path(&[folder]),
            ArchiveType::Year => path(&[&y]),
            ArchiveType::Month => path(&[&y, &m]),
            ArchiveType::ThunderbirdMonth => path(&[&y, &format!("{y}-{m}")]),
            ArchiveType::Sender => path(&[&sender_folder(from, d)]),
            ArchiveType::FolderYear => path(&[&y, folder]),
            ArchiveType::FolderMonth => path(&[&y, &m, folder]),
        }
    };
    if matches!(archive_type, ArchiveType::None | ArchiveType::Folder) {
        return vec![(target(0, ""), messages)]; // the same for every message: `Messages::All` stays `1:*`
    }
    let mut targets: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for h in headers {
        let from = h.from.as_ref().map_or("", |a| a.email.as_str());
        targets.entry(target(h.date, from)).or_default().push(h.uid);
    }
    targets.into_iter().map(|(to, uids)| (to, Messages::Uids(uids))).collect()
}

/// The archive plugin's `sender_subfolder`: the sender's address less control characters, `%` and `*`, the
/// hierarchy delimiter as `-` (`_` where the delimiter is `-`), and `unknown` without an address.
fn sender_folder(from: &str, delimiter: char) -> String {
    if !from.contains('@') {
        return "unknown".into();
    }
    let replace = if delimiter == '-' { '_' } else { '-' };
    from.chars().filter(|c| !c.is_ascii_control() && !matches!(c, '%' | '*')).map(|c| if c == delimiter { replace } else { c }).collect()
}

/// The full name `name` gets inside `parent`, after Roundcube's name checks. The delimiter comes from `parent`,
/// else the folder being renamed, else INBOX; Mailu's Dovecot has one namespace, so all three agree.
fn folder_name(folders: &[Folder], parent: Option<&str>, name: &str, renaming: Option<&str>) -> Result<String, Error> {
    let delimiter_of = |n: &str| folders.iter().find(|f| f.name == n).and_then(|f| f.delimiter);
    let delimiter = parent.or(renaming).map_or_else(|| delimiter_of("INBOX"), delimiter_of);
    kubicle_api::check_folder_name(name, delimiter).map_err(|e| Error::status(StatusCode::BAD_REQUEST, e))?;
    if let Some(parent) = parent {
        if !folders.iter().any(|f| f.name == parent) {
            return Err(Error::status(StatusCode::NOT_FOUND, format!("no folder named {parent:?}")));
        }
        if renaming.is_some_and(|from| kubicle_api::within(parent, from, delimiter)) {
            return Err(Error::status(StatusCode::BAD_REQUEST, "a folder cannot move inside itself"));
        }
    }
    Ok(kubicle_api::folder_path(parent, name, delimiter))
}

/// `name` and every folder below it.
fn family(folders: Vec<Folder>, name: &str) -> Vec<Folder> {
    let delimiter = folders.iter().find(|f| f.name == name).and_then(|f| f.delimiter);
    folders.into_iter().filter(|f| kubicle_api::within(&f.name, name, delimiter)).collect()
}
