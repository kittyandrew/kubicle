//! Typed IMAP commands over one authenticated connection. Every command is an imap-types value; user text
//! reaches the wire only as an imap-types string, which picks the atom, quoted or literal form that can carry it.

use std::{collections::BTreeSet, collections::HashSet, num::NonZeroU32, sync::Arc, time::Instant};

use bytes::Bytes;
use imap_codec::{CommandCodec, IdleDoneCodec, encode::Encoder, encode::Fragment, imap_types::IntoStatic, imap_types::auth::AuthMechanism};
use imap_codec::{imap_types::command::Command, imap_types::command::CommandBody, imap_types::core::AString, imap_types::core::Atom};
use imap_codec::{imap_types::core::Charset, imap_types::core::Literal, imap_types::core::LiteralMode, imap_types::core::Vec1};
use imap_codec::{imap_types::datetime::NaiveDate, imap_types::extensions::binary::LiteralOrLiteral8};
use imap_codec::{imap_types::extensions::idle::IdleDone, imap_types::extensions::quota::Resource};
use imap_codec::{imap_types::extensions::sort::SortCriterion, imap_types::extensions::sort::SortKey};
use imap_codec::{imap_types::extensions::thread::ThreadingAlgorithm, imap_types::fetch::MessageDataItem};
use imap_codec::{imap_types::fetch::MessageDataItemName, imap_types::fetch::Part, imap_types::fetch::Section};
use imap_codec::{imap_types::flag::Flag as ImapFlag, imap_types::flag::FlagNameAttribute, imap_types::flag::StoreResponse};
use imap_codec::{imap_types::flag::StoreType, imap_types::mailbox::Mailbox, imap_types::response::Capability, imap_types::response::Code};
use imap_codec::{imap_types::response::Data, imap_types::response::GreetingKind, imap_types::response::Response};
use imap_codec::{
    imap_types::response::Status, imap_types::response::StatusBody, imap_types::response::StatusKind, imap_types::response::Tagged,
};
use imap_codec::{imap_types::search::SearchKey, imap_types::sequence::SequenceSet};
use imap_codec::{imap_types::status::StatusDataItem, imap_types::status::StatusDataItemName};
use kubicle_api::{Flag, Folder, FolderRole, Messages, Quota, SortColumn};
use rustls::{ClientConfig, pki_types::ServerName};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::debug;
use zeroize::{Zeroize, Zeroizing};

use crate::{Error, MailboxStatus, Thread, header::Fetched, header::Items, stream::Stream, stream::part_of, utf7};

/// What a UID SEARCH, SORT or THREAD looks for; a list of them must all match. Mailu answers `Body` and `Text` from
/// its full-text index.
#[derive(Debug, Clone, Copy)]
pub enum Criterion<'a> {
    Body(&'a str),
    /// The headers and the body.
    Text(&'a str),
    Header(&'static str, &'a str),
    Not(&'a Criterion<'a>),
    /// Any one of these.
    Any(&'a [Criterion<'a>]),
    Unseen,
    Flagged,
    Unanswered,
    /// Arrived on or after this day, or before it, in days since 1970-01-01: IMAP compares dates, never times.
    Since(i64),
    Before(i64),
}

pub struct Client {
    stream: Stream<TlsStream<TcpStream>>,
    /// Upper case, as the server last listed them.
    capabilities: Vec<String>,
    tag: u32,
    selected: Option<String>,
    idle_tag: Option<String>,
    /// The server reported a change to the selected mailbox that no handler claimed (`take_changed`).
    changed: bool,
    /// The running command's `* THREAD` lines that imap-codec could not decode, which `thread` reads itself. No
    /// other undecoded frame is kept: a sender's mail sizes them.
    unparsed_threads: Vec<Bytes>,
    /// The selected mailbox's UIDVALIDITY, from its SELECT's `* OK [UIDVALIDITY n]`.
    uid_validity: Option<NonZeroU32>,
}

impl Client {
    /// Connects to `host`, upgrades to TLS checked against `tls_name` and signs in. `Error::Expired` means the token
    /// is no longer valid.
    pub async fn connect(
        host: &str, port: u16, tls: &Arc<ClientConfig>, tls_name: &ServerName<'static>, user: &str, token: &str,
    ) -> Result<Self, Error> {
        let tcp = TcpStream::connect((host, port)).await?;
        tcp.set_nodelay(true)?; // every write is a whole command or a pipelined batch: nothing to coalesce
        let mut plain = Stream::new(tcp);
        if plain.greeting().await?.kind != GreetingKind::Ok {
            return Err(Error::Malformed("greeting"));
        }
        let starttls = Command::new("s", CommandBody::StartTLS).expect("`s` is a valid tag");
        plain.write(&CommandCodec::new().encode(&starttls).dump()).await?;
        plain.flush().await?;
        loop {
            match plain.response().await {
                Ok((Response::Status(Status::Tagged(Tagged { body, .. })), _)) if body.kind == StatusKind::Ok => break,
                Ok((Response::Status(Status::Tagged(Tagged { body, .. })), _)) =>
                    return Err(Error::Rejected { command: "STARTTLS", text: body.text.inner().to_owned() }),
                Ok((Response::Status(Status::Bye(_)), _)) => return Err(Error::Closed),
                Ok(_) | Err(Error::Unparsed) => {}
                Err(e) => return Err(e),
            }
        }
        let tls = TlsConnector::from(Arc::clone(tls)).connect(tls_name.clone(), plain.into_inner()?).await?;
        let mut client = Self {
            stream: Stream::new(tls),
            capabilities: Vec::new(),
            tag: 0,
            selected: None,
            idle_tag: None,
            changed: false,
            unparsed_threads: Vec::new(),
            uid_validity: None,
        };
        client.refresh_capabilities().await?; // the ones sent before TLS could have been altered on the way
        client.authenticate(user, token).await?; // first: Mailu's Dovecot lists SORT, THREAD and MOVE only after sign-in
        // Mailu's Dovecot has all of these, so kubicle keeps no fallback for a server without one.
        if let Some(missing) =
            ["IDLE", "MOVE", "UIDPLUS", "CONDSTORE", "SORT", "SORT=DISPLAY", "THREAD=REFS"].into_iter().find(|c| !client.has(c))
        {
            return Err(Error::Missing(missing));
        }
        Ok(client)
    }

    /// The mailbox the last successful SELECT opened.
    pub fn selected(&self) -> Option<&str> { self.selected.as_deref() }

    /// Whether the selected mailbox changed (new mail, an expunge, other flags) since the last call.
    pub fn take_changed(&mut self) -> bool { std::mem::take(&mut self.changed) }

    fn has(&self, capability: &str) -> bool { self.capabilities.iter().any(|c| c == capability) }

    /// PLAIN with SASL-IR: front's Dovecot proxy offers both after STARTTLS, so no LOGIN fallback is kept.
    async fn authenticate(&mut self, user: &str, token: &str) -> Result<(), Error> {
        if !self.has("AUTH=PLAIN") || !self.has("SASL-IR") {
            return Err(Error::Missing("AUTH=PLAIN with SASL-IR"));
        }
        let response = Zeroizing::new([&[0], user.as_bytes(), &[0], token.as_bytes()].concat());
        let body = CommandBody::authenticate_with_ir(AuthMechanism::Plain, response.as_slice());
        match self.run(body, "sign-in", |_| false).await {
            Ok(Some(Code::Capability(list))) => self.capabilities = upper(list.as_ref()),
            Ok(_) => self.refresh_capabilities().await?,
            Err(Error::Rejected { .. }) => return Err(Error::Expired),
            Err(e) => return Err(e),
        }
        Ok(())
    }

    async fn refresh_capabilities(&mut self) -> Result<(), Error> {
        let mut found = None;
        self.run(CommandBody::Capability, "CAPABILITY", |item| {
            let Data::Capability(list) = item else { return false };
            found = Some(upper(list.as_ref()));
            true
        })
        .await?;
        self.capabilities = found.ok_or(Error::Malformed("CAPABILITY answer"))?;
        Ok(())
    }

    /// Every mailbox, with its roles from the SPECIAL-USE attributes plain LIST carries, and whether it is
    /// subscribed; the counts stay 0, and `uid_next` 1, for `status` to fill. Names that are not valid modified
    /// UTF-7 are left out.
    pub async fn list(&mut self) -> Result<Vec<Folder>, Error> {
        let mut entries = Vec::new();
        let body = CommandBody::list("", "*").expect("an empty reference and `*` are valid");
        self.run(body, "LIST", |item| {
            let Data::List { items, delimiter, mailbox } = item else { return false };
            if let Some(name) = mailbox_name(mailbox) {
                let roles = if matches!(mailbox, Mailbox::Inbox) { BTreeSet::from([FolderRole::Inbox]) } else { roles(items) };
                let selectable = !items.iter().any(|a| matches!(a, FlagNameAttribute::Noselect) || is(a, "\\NonExistent"));
                let delimiter = delimiter.map(|d| d.inner());
                entries.push(Folder { name, delimiter, roles, selectable, uid_next: 1, ..Folder::default() });
            }
            true
        })
        .await?;
        let mut subscribed = HashSet::new();
        let body = CommandBody::lsub("", "*").expect("an empty reference and `*` are valid");
        self.run(body, "LSUB", |item| {
            let Data::Lsub { mailbox, .. } = item else { return false };
            subscribed.extend(mailbox_name(mailbox));
            true
        })
        .await?;
        for entry in &mut entries {
            // INBOX always shows, whatever LSUB says, so settings shows it subscribed too.
            entry.subscribed = entry.roles.contains(&FolderRole::Inbox) || subscribed.contains(&entry.name);
        }
        Ok(entries)
    }

    /// STATUS of each mailbox, pipelined so the batch costs one round trip. LIST-STATUS would save the extra
    /// command, but imap-types' LIST has no RETURN options. A mailbox gone meanwhile is left out.
    pub async fn status(&mut self, names: &[String]) -> Result<Vec<(String, MailboxStatus)>, Error> {
        use StatusDataItemName::{HighestModSeq, Messages, UidNext, UidValidity, Unseen};
        // HIGHESTMODSEQ (CONDSTORE) is the one item that moves when only a flag other than \Seen changes.
        let items = [Messages, Unseen, UidNext, UidValidity, HighestModSeq];
        let mut tags = Vec::with_capacity(names.len());
        for name in names {
            let body = CommandBody::Status { mailbox: mailbox(name)?, item_names: items.as_slice().into() };
            tags.push(self.send(body, false).await?);
        }
        self.stream.flush().await?;
        let mut found = Vec::with_capacity(names.len());
        for tag in tags {
            let result = self.finish(&tag, "STATUS", false, |item, _| {
                let Data::Status { mailbox, items } = item else { return false };
                found.extend(mailbox_name(mailbox).map(|name| (name, status_of(items))));
                true
            });
            result.await.or_else(rejected_is_none)?;
        }
        Ok(found)
    }

    /// INBOX's UIDVALIDITY, from selecting it on a fresh connection, as an open view that a maildir moved aside since
    /// keeps the old one. Dovecot gives a mailbox made anew another, so it tells which account holds an address now.
    /// Not STATUS: it answers 0 for a mailbox no session synced yet (index-status.c), as when several sign in to a
    /// new account at once, where SELECT syncs first.
    pub async fn inbox_validity(&mut self) -> Result<u32, Error> {
        self.select("INBOX").await?;
        self.uid_validity.map(NonZeroU32::get).ok_or(Error::Malformed("SELECT INBOX answer"))
    }

    /// Opens a mailbox read-write. What changes in it later, by this connection or another, sets `take_changed`.
    pub async fn select(&mut self, name: &str) -> Result<(), Error> {
        self.selected = None; // a refused SELECT also closes the mailbox open before it (RFC 3501, 6.3.1)
        self.uid_validity = None;
        self.run(CommandBody::Select { mailbox: mailbox(name)?, parameters: Vec::new() }, "SELECT", |_| true).await?;
        self.changed = false; // what arrived so far was about the mailbox being closed, or the SELECT itself
        self.selected = Some(name.to_owned());
        Ok(())
    }

    /// The headers of these messages of the selected mailbox, in the server's order (UIDs ascending).
    pub async fn fetch_rows(&mut self, messages: &Messages) -> Result<Vec<Fetched>, Error> {
        use MessageDataItemName::{Envelope, Flags, InternalDate, Rfc822Size, Uid};
        let (mut found, mut got) = (Vec::new(), HashSet::new());
        let Some(set) = set_of(messages) else { return Ok(found) };
        let Ok(body) = CommandBody::fetch(set, vec![Uid, Flags, InternalDate, Rfc822Size, Envelope, header_fields_item()], true);
        self.run(body, "FETCH", |item| {
            let Data::Fetch { items, .. } = item else { return false };
            // An unsolicited FETCH, as of another client's flag change, has no envelope: unclaimed, it is a change.
            let Some(header) = Items::of(items.as_ref()).fetched() else { return false };
            if got.insert(header.uid) {
                found.push(header);
            }
            true
        })
        .await?;
        Ok(found)
    }

    /// The sum of the sizes of the selected mailbox's messages.
    pub async fn fetch_sizes(&mut self) -> Result<u64, Error> {
        let Ok(body) = CommandBody::fetch(SequenceSet::from(..), vec![MessageDataItemName::Rfc822Size], true);
        let mut total = 0;
        self.run(body, "FETCH", |item| {
            let Data::Fetch { items, .. } = item else { return false };
            total += items.as_ref().iter().map(|i| if let MessageDataItem::Rfc822Size(n) = i { u64::from(*n) } else { 0 }).sum::<u64>();
            true
        })
        .await?;
        Ok(total)
    }

    /// The whole message, without setting `\Seen`, in the response's own buffer, and its arrival time (INTERNALDATE)
    /// in seconds since the epoch. `None` when no message has that UID.
    pub async fn fetch_message(&mut self, uid: u32) -> Result<Option<(Bytes, i64)>, Error> {
        use MessageDataItemName::{BodyExt, InternalDate};
        let Some(set) = NonZeroU32::new(uid).map(SequenceSet::from) else { return Ok(None) };
        let Ok(body) = CommandBody::fetch(set, vec![InternalDate, BodyExt { section: None, partial: None, peek: true }], true);
        let mut message = None;
        self.run_framed(body, "FETCH", |item, frame| {
            let Data::Fetch { items, .. } = item else { return false };
            let data = items.as_ref().iter().find_map(|item| match item {
                MessageDataItem::BodyExt { section: None, data, .. } => data.0.as_ref(),
                _ => None,
            });
            let arrival = items.as_ref().iter().find_map(|item| match item {
                MessageDataItem::InternalDate(internal) => Some(internal.as_ref().timestamp()),
                _ => None,
            });
            data.zip(arrival).map(|(data, arrival)| message = Some((part_of(frame, data.as_ref()), arrival))).is_some()
        })
        .await?;
        Ok(message)
    }

    /// One part's MIME header and body (`BODY.PEEK[n.MIME]`, `BODY.PEEK[n]`), still transfer-encoded and without
    /// setting `\Seen`: an inline image without the rest of its message. `None` when either is missing.
    pub async fn fetch_section(&mut self, uid: u32, number: &str) -> Result<Option<(Bytes, Bytes)>, Error> {
        let numbers: Option<Vec<NonZeroU32>> = number.split('.').map(|n| n.parse().ok()).collect();
        let (Some(set), Some(Ok(part))) = (NonZeroU32::new(uid).map(SequenceSet::from), numbers.map(Vec1::try_from)) else {
            return Ok(None);
        };
        let item = |section| MessageDataItemName::BodyExt { section: Some(section), partial: None, peek: true };
        let Ok(body) = CommandBody::fetch(set, vec![item(Section::Mime(Part(part.clone()))), item(Section::Part(Part(part)))], true);
        let mut found = None;
        self.run_framed(body, "FETCH", |item, frame| {
            let Data::Fetch { items, .. } = item else { return false };
            let data = |mime: bool| {
                items.as_ref().iter().find_map(|item| match item {
                    MessageDataItem::BodyExt { section: Some(Section::Mime(_)), data, .. } if mime => data.0.as_ref(),
                    MessageDataItem::BodyExt { section: Some(Section::Part(_)), data, .. } if !mime => data.0.as_ref(),
                    _ => None,
                })
            };
            data(true).zip(data(false)).map(|(m, b)| found = Some((part_of(frame, m.as_ref()), part_of(frame, b.as_ref())))).is_some()
        })
        .await?;
        Ok(found)
    }

    /// Adds or removes a flag. The server answers with the new flags, which sets `take_changed`.
    pub async fn store(&mut self, messages: &Messages, flag: Flag, add: bool) -> Result<(), Error> {
        let kind = if add { StoreType::Add } else { StoreType::Remove };
        for set in sets_of(messages) {
            let Ok(body) = CommandBody::store(set, kind, StoreResponse::Answer, vec![imap_flag(flag)], true);
            self.run(body, "STORE", |_| false).await?;
        }
        Ok(())
    }

    /// Adds and removes keywords, as markasjunk's `Junk` and `NonJunk`. A server that refuses keywords answers NO,
    /// which is left at that, as Roundcube goes on with its move.
    pub async fn store_keywords(&mut self, messages: &Messages, add: &str, remove: &str) -> Result<(), Error> {
        for (keyword, kind) in [(add, StoreType::Add), (remove, StoreType::Remove)] {
            let flag = ImapFlag::Keyword(Atom::try_from(keyword.to_owned()).expect("the keywords are atoms"));
            for set in sets_of(messages) {
                let Ok(body) = CommandBody::store(set, kind, StoreResponse::Silent, vec![flag.clone()], true);
                self.run(body, "STORE", |_| false).await.or_else(rejected_is_none)?;
            }
        }
        Ok(())
    }

    /// Moves messages out of the selected mailbox, which sets `take_changed`.
    pub async fn r#move(&mut self, messages: &Messages, to: &str) -> Result<(), Error> {
        let mailbox = mailbox(to)?;
        for set in sets_of(messages) {
            self.run(CommandBody::Move { sequence_set: set, mailbox: mailbox.clone(), uid: true }, "MOVE", |_| false).await?;
        }
        Ok(())
    }

    /// Copies messages of the selected mailbox to `to`; they stay where they are.
    pub async fn copy(&mut self, messages: &Messages, to: &str) -> Result<(), Error> {
        let mailbox = mailbox(to)?;
        for set in sets_of(messages) {
            self.run(CommandBody::Copy { sequence_set: set, mailbox: mailbox.clone(), uid: true }, "COPY", |_| false).await?;
        }
        Ok(())
    }

    /// Deletes for good: flags `\Deleted`, then expunges exactly these UIDs, never others another client flagged.
    pub async fn expunge(&mut self, messages: &Messages) -> Result<(), Error> {
        for set in sets_of(messages) {
            let Ok(store) = CommandBody::store(set.clone(), StoreType::Add, StoreResponse::Silent, vec![ImapFlag::Deleted], true);
            self.run(store, "STORE", |_| false).await?;
            self.run(CommandBody::ExpungeUid { sequence_set: set }, "UID EXPUNGE", |_| false).await?;
        }
        Ok(())
    }

    /// The account's storage quota (GETQUOTAROOT of INBOX), as Roundcube's `getQuota`: of the STORAGE quotas the
    /// roots report, the one with the least room left; `None` when no root has a limit. A server without `QUOTA`
    /// answers `Error::Rejected`.
    pub async fn quota(&mut self) -> Result<Option<Quota>, Error> {
        let mut found: Option<Quota> = None;
        self.run(CommandBody::GetQuotaRoot { mailbox: Mailbox::Inbox }, "GETQUOTAROOT", |item| match item {
            Data::Quota { quotas, .. } => {
                for q in quotas.as_ref().iter().filter(|q| q.resource == Resource::Storage && q.limit > 0) {
                    let quota = Quota { used: q.usage.saturating_mul(1024), limit: q.limit.saturating_mul(1024) }; // in KiB (RFC 9208)
                    let room = |q: &Quota| q.limit.saturating_sub(q.used);
                    if found.is_none_or(|f| room(&quota) < room(&f)) {
                        found = Some(quota);
                    }
                }
                true
            }
            Data::QuotaRoot { .. } => true,
            _ => false,
        })
        .await?;
        Ok(found)
    }

    /// Stores a message; returns its UID (UIDPLUS's APPENDUID).
    pub async fn append(&mut self, name: &str, flags: &[Flag], message: &[u8]) -> Result<u32, Error> {
        let literal = Literal::try_from(message).map_err(|_| Error::Invalid("a NUL byte in the message"))?;
        let flags = flags.iter().copied().map(imap_flag).collect();
        let body = CommandBody::Append { mailbox: mailbox(name)?, flags, date: None, message: LiteralOrLiteral8::Literal(literal) };
        match self.run(body, "APPEND", |_| false).await? {
            Some(Code::AppendUid { uid, .. }) => Ok(uid.get()),
            _ => Err(Error::Malformed("APPEND answer without APPENDUID")),
        }
    }

    /// UIDs in the selected mailbox matching every criterion (all messages for none), ascending. Text goes as one
    /// IMAP string (atom, quoted or literal, as imap-types picks), never spliced into the command.
    pub async fn search(&mut self, all: &[Criterion<'_>]) -> Result<Vec<u32>, Error> {
        let (charset, keys) = keys(all)?;
        let mut uids = Vec::new();
        self.run(CommandBody::search(charset, keys, true), "SEARCH", |item| {
            let Data::Search(found, ..) = item else { return false };
            uids.extend(found.iter().map(|uid| uid.get()));
            true
        })
        .await?;
        Ok(uids)
    }

    /// UIDs matching `all` in the order of `column` (SORT, RFC 5256; names by SORT=DISPLAY), reversed with `reverse`.
    /// `Unsorted` is the mailbox's own order, by UID.
    pub async fn sort(&mut self, column: SortColumn, reverse: bool, all: &[Criterion<'_>]) -> Result<Vec<u32>, Error> {
        let key = match column {
            SortColumn::Unsorted => {
                let mut uids = self.search(all).await?;
                if reverse {
                    uids.reverse();
                }
                return Ok(uids);
            }
            SortColumn::Arrival => SortKey::Arrival,
            SortColumn::Date => SortKey::Date,
            SortColumn::Subject => SortKey::Subject,
            SortColumn::From | SortColumn::FromTo => SortKey::DisplayFrom,
            SortColumn::To => SortKey::DisplayTo,
            SortColumn::Cc => SortKey::Cc,
            SortColumn::Size => SortKey::Size,
        };
        let (charset, search_criteria) = keys(all)?;
        let charset = charset.unwrap_or_else(|| Charset::try_from("UTF-8").expect("UTF-8 is an atom"));
        let sort_criteria = Vec1::from(SortCriterion { reverse, key });
        let mut uids = Vec::new();
        self.run(CommandBody::Sort { sort_criteria, charset, search_criteria, uid: true }, "SORT", |item| {
            let Data::Sort(found, ..) = item else { return false };
            uids.extend(found.iter().map(|uid| uid.get()));
            true
        })
        .await?;
        Ok(uids)
    }

    /// The threads of the messages matching `all` (THREAD=REFS, as Roundcube asks Dovecot for).
    pub async fn thread(&mut self, all: &[Criterion<'_>]) -> Result<Vec<Thread>, Error> {
        let (charset, search_criteria) = keys(all)?;
        let charset = charset.unwrap_or_else(|| Charset::try_from("UTF-8").expect("UTF-8 is an atom"));
        let algorithm = ThreadingAlgorithm::from(Atom::try_from("REFS").expect("REFS is an atom"));
        let mut lines = Vec::new();
        self.run_framed(CommandBody::Thread { algorithm, charset, search_criteria, uid: true }, "THREAD", |item, frame| {
            lines.push(frame.clone());
            matches!(item, Data::Thread(_))
        })
        .await?;
        lines.retain(|l| l.starts_with(b"* THREAD"));
        lines.append(&mut self.unparsed_threads); // imap-codec stops at 8 levels
        let mut nodes = Vec::new();
        for line in lines {
            let start = nodes.len();
            nodes.extend(
                thread_nodes(&line).ok_or(Error::Unparsed)?.into_iter().map(|t| Thread { parent: t.parent.map(|p| p + start), ..t }),
            );
        }
        Ok(nodes)
    }

    /// Asks the server for what changed in the selected mailbox; it sets `take_changed`.
    pub async fn noop(&mut self) -> Result<(), Error> { self.run(CommandBody::Noop, "NOOP", |_| false).await.map(drop) }

    pub async fn create(&mut self, name: &str) -> Result<(), Error> {
        self.run(CommandBody::Create { mailbox: mailbox(name)? }, "CREATE", |_| false).await.map(drop)
    }

    pub async fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        self.leave(from).await?;
        self.run(CommandBody::Rename { from: mailbox(from)?, to: mailbox(to)? }, "RENAME", |_| false).await.map(drop)
    }

    pub async fn delete(&mut self, name: &str) -> Result<(), Error> {
        self.leave(name).await?;
        self.run(CommandBody::Delete { mailbox: mailbox(name)? }, "DELETE", |_| false).await.map(drop)
    }

    /// Closes the selected mailbox if its name starts with `name`, as those below it do: Dovecot disconnects
    /// a client whose selected mailbox is renamed or deleted. UNSELECT, unlike CLOSE, expunges nothing.
    async fn leave(&mut self, name: &str) -> Result<(), Error> {
        if self.selected.as_deref().is_some_and(|selected| selected.starts_with(name)) {
            self.run(CommandBody::Unselect, "UNSELECT", |_| false).await?;
            self.selected = None;
            self.changed = false;
        }
        Ok(())
    }

    pub async fn subscribe(&mut self, name: &str, subscribed: bool) -> Result<(), Error> {
        let mailbox = mailbox(name)?;
        let body = if subscribed { CommandBody::Subscribe { mailbox } } else { CommandBody::Unsubscribe { mailbox } };
        self.run(body, if subscribed { "SUBSCRIBE" } else { "UNSUBSCRIBE" }, |_| false).await.map(drop)
    }

    /// Starts IDLE on the selected mailbox. Until `idle_done`, only `idle_wait` may run.
    pub async fn idle(&mut self) -> Result<(), Error> {
        let tag = self.send(CommandBody::Idle, true).await?;
        self.finish(&tag, "IDLE", true, |_, _| false).await?;
        self.idle_tag = Some(tag);
        Ok(())
    }

    /// Waits during IDLE until the server reports a change. Cancel-safe: it can race other events, and no
    /// response is lost when another branch wins.
    pub async fn idle_wait(&mut self) -> Result<(), Error> {
        loop {
            match self.stream.idle_response().await {
                Ok(Response::Data(data)) if absorb(&data) => {
                    self.changed = true;
                    return Ok(());
                }
                Ok(Response::Status(Status::Bye(_))) => return Err(Error::Closed),
                Ok(Response::Status(Status::Tagged(_)) | Response::CommandContinuationRequest(_)) =>
                    return Err(Error::Malformed("response during IDLE")),
                Ok(Response::Data(_) | Response::Status(Status::Untagged(_))) | Err(Error::Unparsed) => {} // Dovecot's "Still here"
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn idle_done(&mut self) -> Result<(), Error> {
        let tag = self.idle_tag.take().expect("idle_done follows a successful idle");
        self.stream.write(&IdleDoneCodec::new().encode(&IdleDone).dump()).await?;
        self.stream.flush().await?;
        self.finish(&tag, "IDLE", false, |_, _| false).await.map(drop)
    }

    pub async fn logout(&mut self) {
        if let Ok(tag) = self.send(CommandBody::Logout, true).await {
            let _ = self.finish(&tag, "LOGOUT", false, |_, _| false).await; // BYE comes first and ends it as Closed
        }
    }

    async fn run<F>(&mut self, body: CommandBody<'_>, command: &'static str, mut handle: F) -> Result<Option<Code<'static>>, Error>
    where F: FnMut(&Data<'_>) -> bool {
        self.run_framed(body, command, |item, _| handle(item)).await
    }

    /// `run` whose handler also gets the bytes each response was decoded from, to keep a part of them (`part_of`).
    async fn run_framed<F>(&mut self, body: CommandBody<'_>, command: &'static str, handle: F) -> Result<Option<Code<'static>>, Error>
    where F: FnMut(&Data<'_>, &Bytes) -> bool {
        let started = Instant::now();
        self.unparsed_threads.clear();
        let tag = self.send(body, true).await?;
        let result = self.finish(&tag, command, false, handle).await;
        debug!(command, ms = started.elapsed().as_secs_f64() * 1000.0, ok = result.is_ok(), "IMAP command");
        result
    }

    /// Writes a command under a fresh tag. A literal waits for the server's go-ahead; without `flush` the command
    /// stays buffered for pipelining, which only STATUS asks for: its folder name, printable ASCII once in modified
    /// UTF-7, goes as an atom or a quoted string, never as a literal.
    async fn send(&mut self, body: CommandBody<'_>, flush: bool) -> Result<String, Error> {
        self.tag += 1;
        let tag = format!("k{}", self.tag);
        let command = Command::new(tag.as_str(), body).expect("k followed by digits is a valid tag");
        for fragment in CommandCodec::new().encode(&command) {
            match fragment {
                Fragment::Line { mut data } | Fragment::Literal { mut data, mode: LiteralMode::NonSync } => {
                    let written = self.stream.write(&data).await;
                    data.zeroize(); // the AUTHENTICATE line carries the token
                    written?;
                }
                Fragment::Literal { data, mode: LiteralMode::Sync } => {
                    self.stream.flush().await?;
                    self.finish(&tag, command.name(), true, |_, _| false).await?;
                    self.stream.write(&data).await?;
                }
            }
        }
        if flush {
            self.stream.flush().await?;
        }
        Ok(tag)
    }

    /// Reads responses up to the tagged completion of `tag`, or with `continuation` up to the server's go-ahead
    /// for it. Data the handler does not claim is absorbed as a change to the selected mailbox. Returns the
    /// completion's response code.
    async fn finish(
        &mut self, tag: &str, command: &'static str, continuation: bool, mut handle: impl FnMut(&Data<'_>, &Bytes) -> bool,
    ) -> Result<Option<Code<'static>>, Error> {
        loop {
            let (response, frame) = match self.stream.response().await {
                Ok(response) => response,
                Err(Error::Unparsed) => {
                    if self.stream.last_frame().starts_with(b"* THREAD") {
                        self.unparsed_threads.push(self.stream.last_frame().clone());
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            match response {
                Response::Data(data) =>
                    if !handle(&data, frame) {
                        self.changed |= absorb(&data);
                    },
                Response::Status(Status::Untagged(StatusBody { code: Some(Code::UidValidity(n)), .. })) => self.uid_validity = Some(n),
                Response::Status(Status::Untagged(_)) => {}
                Response::Status(Status::Tagged(Tagged { tag: got, body })) if got.inner() == tag =>
                    return match body.kind {
                        StatusKind::Ok if !continuation => Ok(body.code.map(IntoStatic::into_static)),
                        StatusKind::No if matches!(&body.code, Some(Code::Other(other)) if other.inner().starts_with(b"UNAVAILABLE")) =>
                            Err(Error::Unavailable),
                        _ => Err(Error::Rejected { command, text: body.text.inner().to_owned() }),
                    },
                Response::Status(Status::Tagged(_)) => return Err(Error::Malformed("tag")),
                Response::Status(Status::Bye(_)) => return Err(Error::Closed),
                Response::CommandContinuationRequest(_) if continuation => return Ok(None),
                Response::CommandContinuationRequest(_) => return Err(Error::Malformed("continuation")),
            }
        }
    }
}

/// A NO or BAD, as `Ok`: the caller goes on without what it asked for.
fn rejected_is_none(e: Error) -> Result<Option<Code<'static>>, Error> { if let Error::Rejected { .. } = e { Ok(None) } else { Err(e) } }

/// Whether unsolicited data says the selected mailbox changed: new mail, an expunge, or other flags.
fn absorb(data: &Data<'_>) -> bool { matches!(data, Data::Exists(_) | Data::Expunge(_) | Data::Vanished { .. } | Data::Fetch { .. }) }

fn upper(capabilities: &[Capability<'_>]) -> Vec<String> { capabilities.iter().map(|c| c.to_string().to_ascii_uppercase()).collect() }

/// Ours as imap-types' wire value, where `Forwarded` is a keyword.
fn imap_flag(flag: Flag) -> ImapFlag<'static> {
    match flag {
        Flag::Seen => ImapFlag::Seen,
        Flag::Flagged => ImapFlag::Flagged,
        Flag::Answered => ImapFlag::Answered,
        Flag::Forwarded => ImapFlag::Keyword(Atom::try_from("$Forwarded").expect("$Forwarded is an atom")),
        Flag::Draft => ImapFlag::Draft,
    }
}

/// `BODY.PEEK[HEADER.FIELDS (List-Post Content-Type)]`: the header fields the envelope lacks, as Roundcube's
/// list fetches them.
fn header_fields_item() -> MessageDataItemName<'static> {
    let name = |name: &'static str| AString::try_from(name).expect("a header name is an atom");
    let names = Vec1::try_from(vec![name("List-Post"), name("Content-Type")]).expect("two names");
    MessageDataItemName::BodyExt { section: Some(Section::HeaderFields(None, names)), partial: None, peek: true }
}

/// A `* THREAD` line (RFC 5256) as flat nodes, each parent before its children: in a list each number is the
/// parent of the next and nested lists hang off its last, and lists alone hang off a message the mailbox lacks.
/// Read here, iteratively, as imap-codec stops at 8 levels; `None` for what is not one.
fn thread_nodes(line: &[u8]) -> Option<Vec<Thread>> {
    let (mut nodes, mut open, mut number) = (Vec::new(), Vec::<(Option<usize>, Option<usize>)>::new(), None::<u32>); // (parent, last) per list
    for &c in line.strip_prefix(b"* THREAD")? {
        if c.is_ascii_digit() {
            number = Some(number.unwrap_or(0).checked_mul(10)?.checked_add(u32::from(c - b'0'))?);
            continue;
        }
        if let Some(uid) = number.take().filter(|&n| n != 0) {
            let (parent, last) = open.last_mut()?;
            nodes.push(Thread { uid: Some(uid), parent: last.or(*parent) });
            *last = Some(nodes.len() - 1);
        }
        match c {
            b'(' => {
                let parent = open.last_mut().map(|(parent, last)| {
                    *last.get_or_insert_with(|| {
                        nodes.push(Thread { uid: None, parent: *parent });
                        nodes.len() - 1
                    })
                });
                open.push((parent, None));
            }
            b')' => drop(open.pop()?),
            b' ' | b'\r' | b'\n' => {}
            _ => return None,
        }
    }
    open.is_empty().then_some(nodes)
}

/// The one place a folder name becomes IMAP: imap-types picks a form that carries it, or refuses it.
fn mailbox(name: &str) -> Result<Mailbox<'static>, Error> {
    Mailbox::try_from(utf7::encode(name)).map_err(|_| Error::Invalid("a folder name IMAP cannot carry"))
}

fn mailbox_name(mailbox: &Mailbox<'_>) -> Option<String> {
    if let Mailbox::Other(other) = mailbox { utf7::decode(other.as_ref()) } else { Some("INBOX".to_owned()) }
}

fn is(attribute: &FlagNameAttribute<'_>, name: &str) -> bool { attribute.to_string().eq_ignore_ascii_case(name) }

fn roles(attributes: &[FlagNameAttribute<'_>]) -> BTreeSet<FolderRole> {
    let roles = [
        ("\\Drafts", FolderRole::Drafts),
        ("\\Sent", FolderRole::Sent),
        ("\\Junk", FolderRole::Junk),
        ("\\Trash", FolderRole::Trash),
        ("\\Archive", FolderRole::Archive),
    ];
    roles.into_iter().filter(|(name, _)| attributes.iter().any(|a| is(a, name))).map(|(_, role)| role).collect()
}

fn status_of(items: &[StatusDataItem]) -> MailboxStatus {
    let mut status = MailboxStatus { messages: 0, unseen: 0, uid_next: 0, uid_validity: 0, highest_modseq: None };
    for item in items {
        match item {
            StatusDataItem::Messages(n) => status.messages = *n,
            StatusDataItem::Unseen(n) => status.unseen = *n,
            StatusDataItem::UidNext(n) => status.uid_next = n.get(),
            StatusDataItem::UidValidity(n) => status.uid_validity = n.get(),
            StatusDataItem::HighestModSeq(n) => status.highest_modseq = Some(*n).filter(|&n| n > 0),
            _ => {}
        }
    }
    status
}

/// The search keys for `all` (ALL for none), and the charset to name when one holds text beyond ASCII.
fn keys(all: &[Criterion<'_>]) -> Result<(Option<Charset<'static>>, Vec1<SearchKey<'static>>), Error> {
    let mut utf8 = false;
    let keys = all.iter().map(|c| search_key(c, &mut utf8)).collect::<Result<Vec<_>, _>>()?;
    let charset = utf8.then(|| Charset::try_from("UTF-8").expect("UTF-8 is an atom"));
    Ok((charset, Vec1::try_from(keys).unwrap_or_else(|_| Vec1::from(SearchKey::All))))
}

fn search_key(criterion: &Criterion<'_>, utf8: &mut bool) -> Result<SearchKey<'static>, Error> {
    let mut string = |text: &str| {
        *utf8 |= !text.is_ascii();
        AString::try_from(text.to_owned()).map_err(|_| Error::Invalid("a NUL in the search"))
    };
    let day = |days: i64| {
        let day = chrono::DateTime::from_timestamp(days.saturating_mul(86_400), 0).unwrap_or_default().date_naive();
        NaiveDate::try_from(day).map_err(|_| Error::Invalid("a date IMAP cannot carry"))
    };
    Ok(match criterion {
        Criterion::Body(text) => SearchKey::Body(string(text)?),
        Criterion::Text(text) => SearchKey::Text(string(text)?),
        Criterion::Header(name, text) => SearchKey::Header(string(name)?, string(text)?),
        Criterion::Not(inner) => SearchKey::Not(Box::new(search_key(inner, utf8)?)),
        Criterion::Any(any) => {
            let any = any.iter().map(|c| search_key(c, utf8)).collect::<Result<Vec<_>, _>>()?;
            // An empty Any finds nothing: no field ticked, or a header phase whose fields all read the text.
            any.into_iter().reduce(|a, b| SearchKey::Or(Box::new(a), Box::new(b))).unwrap_or(SearchKey::Not(Box::new(SearchKey::All)))
        }
        Criterion::Unseen => SearchKey::Unseen,
        Criterion::Flagged => SearchKey::Flagged,
        Criterion::Unanswered => SearchKey::Unanswered,
        Criterion::Since(days) => SearchKey::Since(day(*days)?),
        Criterion::Before(days) => SearchKey::Before(day(*days)?),
    })
}

/// `All` is `1:*`, so Select all sends no list of UIDs; `None` for no (nonzero) UIDs.
fn set_of(messages: &Messages) -> Option<SequenceSet> {
    match messages {
        Messages::All => Some(SequenceSet::from(..)), // where `uid_set`'s `*` would be the last message alone
        Messages::Uids(_) => SequenceSet::try_from(kubicle_api::uid_set(messages).as_str()).ok(),
    }
}

/// Size-bounded sequence sets for a mutating command (STORE, MOVE, COPY, EXPUNGE). A sparse selection of thousands
/// of UIDs serialises past Dovecot's `imap_max_line_length` as one set and the whole command is refused, so the
/// command runs once per chunk instead. A contiguous selection stays one `1:5` and so one command; an empty one
/// yields none. `All` is `1:*` (`SequenceSet::from(..)`), never `uid_set`'s `*`, which is the last message alone.
/// `SET_BYTES` leaves ample room for the command word and a mailbox name.
fn sets_of(messages: &Messages) -> Vec<SequenceSet> {
    const SET_BYTES: usize = 4096;
    match messages {
        Messages::All => vec![SequenceSet::from(..)],
        Messages::Uids(_) =>
            kubicle_api::uid_set_chunks(messages, SET_BYTES).into_iter().filter_map(|s| SequenceSet::try_from(s.as_str()).ok()).collect(),
    }
}
