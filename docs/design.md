# kubicle design

kubicle is a webmail for [Mailu](https://mailu.io). It replaces Roundcube in Mailu's webmail slot: same address,
same sign-in, no PHP. This document holds every design decision and the reasons for it, the limits, the threat
model and what kubicle deliberately does not do. The README covers setup; the code covers the mechanics. Change
this document in the same commit as the behaviour it describes.

The decisions came from a design interview on Oct 21, 2026, the day a PHP core RCE (CVE-2026-109618/109619)
made the owner stop Roundcube. Research behind them: Roundcube 1.6.18's source and CVE history, Mailu
2024.06.64's source, a survey of non-PHP webmails, and measurements against Dovecot 2.3.21 with 50,000 messages.

## Scope

- **One mailbox per session, the Mailu way.** Whoever signs in through Mailu gets their own mailbox. There is
  no account switcher and no unified inbox: Mailu ties a session to one user, and holding credentials for
  more mailboxes would need a second login system beside Mailu's.
- **For people, not agents.** kubicle has no API meant for programs. Agent access to mail is a separate
  concern at the Mailu level.
- **Web only.** No native desktop app: on the desktop it runs as a browser tab or an installed web app. It is
  installable on a phone (a web app manifest) and has no offline mode.
- **English only.** Dates and times show in the browser's own time zone, or the one chosen in Settings (Roundcube
  parity has why not the server's). Outgoing mail is dated in the zone chosen in Settings, else the server's.
- **Feature set: what the owner used in Roundcube**, measured from his Roundcube database and mailbox. See
  "Features" for the list and "Not building" for everything left out.

## Architecture

```
browser -> Caddy -> Mailu front (nginx, Mailu SSO check) -> kubicle server (axum)
                                                              |-> front:10143 IMAP (STARTTLS)
                                                              |-> front:10025 submission (STARTTLS)
                                                              '-> SQLite: settings, unsent uploads
```

- **One Rust workspace, split by layer.** `kubicle-api` holds the types the server and the client exchange, so
  the two cannot drift. `kubicle-server` is the axum server: routes, sessions and signed URLs, built on
  `kubicle-mailbox` (a user's mailbox without the routes: the engine, Compose's sending, uploads and settings),
  which is built on `kubicle-imap` (its IMAP client) and `kubicle-mail` (MIME to the message the reader shows,
  with no I/O). `kubicle-web` is the Leptos client, compiled to WebAssembly and rendered only in the browser,
  built on `kubicle-ui` (icons, menus, the dialog) and `kubicle-browser` (requests, text, dates). Each crate but
  `kubicle-web` stays at 2,000 lines or fewer, so the compiler checks each boundary: a crate sees only what
  another makes public.
- **Client-side rendering, no server rendering.** The installed web app caches its files after the first
  visit, so a later start loads no code over the network. Server rendering with hydration would be a second
  rendering path to keep in step for a page that search engines never see.
- **The client keeps state, Dovecot keeps the index.** Mailu front is 64 ms from the owner's desktop. The client
  holds the loaded pages of the list and the next messages' bodies, so arrow keys, paging and folder switches never
  wait for the network. The server keeps no copy of the mail: Dovecot sorts, threads and searches, and its own
  index makes that fast (IMAP).
- **IMAP is the only copy.** Bodies come from IMAP when a message opens, and the client prefetches the next few.
  Only the settings database is backed up.

## Mailu integration

kubicle runs as the container behind Mailu front's `webmail` alias, where Roundcube ran. Mailu keeps
`WEBMAIL` set to a value other than `none`, because front only creates the webmail locations when it is set.

- **Address.** Front serves the webmail under `WEB_WEBMAIL` (`/login` on the owner's server) and strips that
  prefix before proxying, so kubicle serves `/` and the client builds every URL from the base path it was
  loaded under.
- **Sign-in is Mailu's.** Every request under `WEB_WEBMAIL` passes front's `auth_request`; a missing or
  expired Mailu session redirects to Mailu's sign-in page. kubicle has no login page of its own. That includes the
  web app manifest, which Chrome fetches without cookies unless the link asks for them: `index.html` links it with
  `crossorigin="use-credentials"`, else Chrome parses the sign-in page as the manifest and refuses to install.
- **Identity comes from one route only.** Front adds `X-Remote-User` and `X-Remote-User-Token` only on
  `/sso.php`. On every other path a browser could send those headers itself, so kubicle reads them on
  `/sso.php` and nowhere else. A page without a kubicle session goes to `sso.php`: at once before its first session,
  after the notice once it had one (both below).
- **The identity is proven, not trusted.** Before creating a session, `sso.php` logs in to IMAP with the user
  and token it received; a header forged by anything that can reach kubicle has no live Mailu token, so it gets
  a 403. The peer range check (`KUBICLE_TRUSTED_PROXY`, required) is a second guard there, and the only one kubicle
  has against password guessing through `sso.php` (Rate limits).
- **One engine per user.** All of a user's sessions share it and its one IMAP connection. Each session keeps the
  token its `sso.php` proved, and sends with it. One Mailu session has one token, so an `sso.php` with a token a
  live session holds gets that session back, its cookie sent or not; a user keeps 32 sessions at most, the least
  recently used going first. A reconnect proves every session's token and uses the newest one accepted; a session
  whose token is refused ends, the others go on, so no session outlives its Mailu session on another's token.
- **A session lasts until a reconnect finds its token refused, or 12 hours unused.** The engine proves tokens only
  when its connection drops, so a Mailu sign-out or `SESSION_TIMEOUT` leaves the kubicle session working on the
  open connection: that browser's cookie went with Mailu's sign-out, but a copied cookie or a held signed URL does
  not (Andrew's choice over a proof every minute, Oct 26, 2026). Roundcube logs in on every request. A signed URL
  never counts as a use.
- **An address can hold another account.** Mailu keys accounts by address alone, and keeps the maildir of one it
  deleted. kubicle records INBOX's UIDVALIDITY at an address's first sign-in, from a SELECT on the connection that
  proves the token: the engine's own may hold the account before's INBOX open, a view that keeps the old value, and
  a STATUS answers 0 for an INBOX no session synced yet, as when several sign in to a new account at once. A later
  sign-in that finds another one, a new INBOX for an account made anew, ends the address's sessions and its engine,
  drops its settings and uploads, and the browser keeps draft copies under the address and that value, so the
  account before's are never found. Mailu drops no sessions when it deletes an account, so the old holder's Mailu
  session can still sign in to the address until its `SESSION_TIMEOUT`.
- **The token is the IMAP and SMTP password.** It is `token-` plus random text, valid while the Mailu session
  lives, on front's internal ports 10143 (IMAP), 10025 (submission) and 14190 (ManageSieve). kubicle keeps it
  in server memory with the session and never writes it to disk. A server restart drops it; the next request
  goes through `sso.php` again.
- **Logout follows Mailu.** kubicle's session cookie is named `roundcube_sessid` with `Path=/`, because
  Mailu's logout clears exactly that cookie name (Mailu's `core/admin/mailu/sso/views/base.py`).
  kubicle's logout button clears its session, then sends the browser to Mailu's `/sso/logout`.
- **TLS to front.** Both internal ports require STARTTLS. kubicle verifies front's certificate against the
  mail hostname it is configured with, instead of turning verification off as Mailu's Roundcube config does.
- **Rate limits.** Ports 10143 and 10025 skip Mailu's authentication rate limits, and Mailu takes any password
  there, not only a token (`AUTH_REQUIRE_TOKENS` is off by default). kubicle passes on whatever
  `X-Remote-User-Token` `sso.php` got, unchecked: through front it is always the token front issued, but anything
  else that reaches kubicle could try passwords there without a limit. Hence only front may reach kubicle
  (Deployment). A check of the token's shape would hold the claim in code, but would lock everyone out if Mailu ever
  changed the shape.
- **Known limit.** Mailu idles a session out after an hour without requests (`SESSION_TIMEOUT`). A kubicle
  tab left open keeps its IMAP connection, which stays valid; a reconnect after that hour needs a fresh
  `sso.php` round, which the client triggers on the next request.
- **A page that never had a session signs in at once.** The service worker answers with the stored app without
  waiting on front, so the app starts after Mailu's session ended; kubicle serves its page without a session, so it
  starts after kubicle restarted; and Mailu's sign-in clears `roundcube_sessid` (`_clear_webmail_cookies`), so the
  first page after it has no kubicle session. Refused before its first session, the page has nothing open to lose
  and goes to `sso.php` at once, as Mailu's Roundcube plugin (`webmails/roundcube/login/mailu.php`) sends a page
  request without a session there. A mark in the tab's session storage lasts from that jump until a session answers.
  Refused again with it set, the jump brought no session (kubicle's cookie did not stick, the sign-in at Mailu was
  left unfinished, or `sso.php` refused), and the notice below shows. Its 2 s sign-in still goes round, at
  Roundcube's pace and with the reason in sight, where going at once could loop with no pause. With session storage
  off, the notice always shows. Before, every such start showed the notice for its 2 s: the stored app opened after
  Mailu's session ended showed the sign-in form after 3.2 s and the mail after 4.7 s, now after 1.2 and 2.7 s, in a
  VM with 100 ms added per round trip and the CPU slowed four times (Oct 27, 2026).
- **Session end, as Roundcube's `session_error`.** Once the page had a session, an answer of 401 (no kubicle
  session) or a redirected one (front's sign-in page, once Mailu's session ended) shows Roundcube's persistent
  "Your session is invalid or expired." with a Sign in again button, alone, as Roundcube's (no other notice shows
  then), and the page sends nothing more of its own: the event stream stays closed and Compose's save to Drafts
  waits, its text kept in the browser. Compose stays, so nothing typed is lost; anywhere else, and once Compose is
  left, the sign-in follows 2 s later. A Compose left meanwhile goes to Drafts from its browser copy once the user
  is signed in again, unasked. The button and that sign-in keep the address's hash in the tab's session storage
  across `sso.php` and Mailu's sign-in, and the page puts it back before it starts, so it returns to the same
  Compose with its text, or the same folder and page, where Roundcube under Mailu opens the Inbox and a new Compose.

## IMAP

- **The IMAP client is our own, on imap-codec's 2.0 alpha line.** Commands are typed imap-types values, so no user
  text is ever spliced into IMAP. imap-codec 1.0 lacks pieces kubicle uses (SORT, THREAD, STATUS HIGHESTMODSEQ);
  async-imap takes commands as strings. The alpha is pinned exactly in `kubicle-imap`'s `Cargo.toml` and read before
  each bump. It decodes thread lists and body structures only 8 levels deep, and a response it cannot decode is
  skipped: kubicle reads `* THREAD` itself, into a flat list, and asks no `BODYSTRUCTURE`, as Roundcube's list asks
  none. No other such frame is kept, as a sender's mail sizes it.
- **Sign-in is AUTHENTICATE PLAIN with SASL-IR (RFC 4959)**, one round trip, which front's Dovecot proxy offers
  after STARTTLS; a server without both is refused, with no LOGIN fallback.
- **One IMAP connection per user, kept open.** It authenticates once and listens with IDLE, so new mail
  reaches the open list within a second. Roundcube logs in on every request and polls every 60 s. Every request
  that reads or changes mail is a job on it, run in the order asked.
- **Dovecot is the index (Oct 25, 2026).** A list page is a SORT (or THREAD=REFS and SORT for threads) of the
  folder, then one FETCH of the page's rows; a search is the same SORT with the search's criteria, the keys
  Roundcube sends. The order a view asked for stays in server memory until a message arrives in its folder or
  leaves it, so a page turn, and the page reloaded after a read, is one FETCH: 1.3 ms on the 50,000-message
  fixture, where a SORT again took 36 ms. The default view (None, by UID) needs no SORT at all. The
  first SORT of a large folder costs 10 to 20 ms on the owner's mail (12,700 messages) and 38 to 66 ms on 50,000,
  and a sort key Dovecot has not cached yet once up to a second; threads 21 ms and 92 ms. Header search takes 1
  to 5 ms in a folder of the owner's mail and about 30 ms over all of it. An SQLite header cache did this before:
  it cost a full sync per new or changed cache, a quarter second for 50,000 headers, a References
  backfill, 55 MB per user, and its own sort, thread and search rules that Roundcube does not have.
- **Roundcube's search and sort, because they are Dovecot's.** Sort keys are RFC 5256's (base subject, ties by
  arrival in the folder), names by SORT=DISPLAY. Threads are THREAD=REFS, each placed where its first message
  falls in the sort, a missing root giving way to its oldest reply. Search is Roundcube's `HEADER SUBJECT`, `FROM`,
  `TO`, `CC` and the rest, which Mailu's full-text index answers by word: "andr" finds Andrew, "ndrew" finds
  nothing. "With attachment" is Roundcube's guess from the top Content-Type, as IMAP cannot search the structure.
  A search over several folders merges their first hits by fetched headers, each folder's kept in Dovecot's own
  order: kubicle's key cannot match Dovecot's comparator, and a re-sort by it would make pages repeat and skip hits.
  It merges 1,000 hits of a folder at most, as Roundcube's `list_search_messages` fetches 1,000 into its index,
  and the rows and the total are those: the last page of a large search held every hit's header. A folder's hits
  past its 1,000th never show. Unsorted, only the page's rows are fetched. It never selects the folder already
  selected again: selecting a folder of 50,000 messages anew costs Dovecot 23 ms (Oct 25, 2026).
- **A dead link is noticed.** A command's reply may go 60 s without a byte before the engine drops the
  connection and reconnects with backoff; the limit counts between reads, so a large FETCH that keeps arriving is
  never cut, and a slow SEARCH has room. IDLE waits without a limit, since the 15 s STATUS round ends it.
- **Live updates have no gaps.** The client listens on a server-sent event stream. Every stream, and every
  listener that fell behind, gets the full folder state first, on which the client reloads the page and any
  search it shows, so a reconnect cannot leave the list stale. Each folder's cached first page stays until a
  fresh copy fetched in the background replaces it, so a folder switch meanwhile still shows at once; a page
  load's first stream opened before its pages loaded, so its resync reloads nothing. The server sends a
  keepalive event after 15 s of quiet: `EventSource` hides SSE comments, so only a real event shows the stream
  is alive. The client opens the stream again after 45 s without a message, when the browser comes online, and
  when the page becomes visible after 20 s without one, as its stream may have died while hidden; each of those
  resyncs too. A page shown sooner keeps its live stream, whose resync would send 13 requests, every folder's
  first page among them, ahead of the user's next click: a page jump 100 ms after the page showed took 84 ms,
  227 at worst, against 74 (64 ms round trip, Oct 25, 2026). The stream carries `X-Accel-Buffering: no`, as
  Mailu front's nginx buffers proxied responses by default and would hold events back.
- **A change is a STATUS that moved.** IDLE says that the viewed folder changed; the 15 s STATUS round, and the
  STATUS read on each SELECT, say it for the others, by their message count, unread count, next UID and
  HIGHESTMODSEQ. Either way every tab hears `FolderChanged`, and the folder's kept orders go, unless only
  flags changed (the count, next UID and UIDVALIDITY held) and the order's search does not read flags
  (Unread, Flagged, Unanswered): no sort key or thread depends on them.
- **What the client changed stays changed.** A delete, move or mark shows at once, before the server took it, so
  a page of that folder asked before the change was sent, or while it was on its way, may still hold what it took
  off; such a page is not shown, and the folder loads again once the change is answered. The same holds for a
  message window's page and a search. A page asked before the server reported a change of its folder
  (`FolderChanged`) and answered after the report is not kept either: the server may have read the folder before
  the change, and the reload the report made has passed. A page asked after the server took a change is a job
  behind it, so a tab reloaded meanwhile, which knows nothing of it, never lists what it moved. A change also
  waits for a Compose start asked before it, so the server reads the message being answered or forwarded before a
  later delete moves it, in the order the user asked.
- **Only the folder on screen is watched.** The client names it with `api/view`; loading other folders' pages
  ahead of time never moves the engine's IDLE away from it.
- **IDLE, MOVE, UIDPLUS, CONDSTORE, SORT, SORT=DISPLAY and THREAD=REFS are required.** Mailu's Dovecot has them
  all, so there is no fallback path without them.
- **Other folders are checked by a STATUS round every 15 s**, pipelined, while IDLE watches the viewed folder.
  A round costs Dovecot about 0.1 ms per folder from its mailbox list index. Mailu leaves INBOX out of that
  index, so the owner's Mailu override sets `mailbox_list_index_include_inbox = yes`: without it, STATUS of an
  INBOX of 50,000 messages adds 24 ms while another folder is viewed. NOTIFY would report within 0.5 s, but
  imap-types cannot express it and kubicle sends no hand-written IMAP.
- **Body search** goes to IMAP SEARCH too, which Mailu answers from Dovecot's full-text index (fts_flatcurve,
  on by default), once typing pauses.

## Features

The baseline is Roundcube's behaviour; each item names where kubicle differs.

- **Layout.** Roundcube Elastic's widescreen layout: folders, list, message. One pane at a time on a phone.
  The look follows Elastic without copying its stylesheets. The columns size by Elastic's rules
  (`skins/elastic/styles/layout.less`): folders `flex: 2` within 220 px and 30%, list `flex: 3` within 300 px and
  30%, message `flex: 6`; at 1200 px and below both side columns take `flex: 3` and at least 260 px. The screen
  modes follow Elastic's `ui.js`: above 1200 px every column; from 769 to 1200 px the folder column is hidden,
  and the list header's Folders button shows it in the list's place until a folder is picked; at 768 px and
  below one pane at a time. Elastic has a 78 px rail kubicle lacks, so the message column is wider by that much,
  and the floating Compose shows at 1200 px and below (Elastic shows it at 768 px and below, its rail holding
  Compose above that), an exception Andrew approved. Elastic's drag handles between the columns are left out.
- **Account.** The full address heads the folder column, centred in the room the folder actions menu (⋮) leaves
  beside it, so a 220 px column still shows a 20 character address whole, with a tooltip when it must shorten. The
  menu holds Roundcube's Empty, Mark all as read, Manage folders and About. About is Roundcube's About dialog (the
  version, the licence with a link to the source when `KUBICLE_SOURCE_URL` names one, and its "Get support" button
  to Mailu's own pages, `KUBICLE_ADMIN_URL`, `/admin` by default, as Mailu sets Roundcube's `support_url`);
  Elastic's rail, which kubicle has not, holds it in Roundcube. Above the bottom bar sits Roundcube's quota widget,
  a disk, a bar and its percent with "Disk usage: 120 MB / 953 MB (13%)" as its tooltip, red from 90%; only for an
  account with a limit (IMAP `QUOTA`, the STORAGE root with the least room left), as Andrew chose: an unlimited
  one's empty bar says nothing. The engine reads it in each STATUS round and after each change. The bottom bar
  holds four labelled buttons, centred: Settings, Keys (the keyboard shortcuts), Light/Dark (which pins the other
  of the user's two themes, saved in the settings) and Sign out in the theme's danger colour. The labels are short
  so that they keep 6 px apart in the narrowest folder column; each button's tooltip names it in full.
- **Folders.** Unread counts, and the names of folders with unread mail in bold, a collapsed folder's also while a
  subscribed folder below it has some (its count stays its own), as Roundcube's `set_unread_count_display`. The
  special folders show under their role's name, Inbox, Drafts, Sent, Junk, Trash and Archive, whatever folder holds
  the role, in the pane, the tab title and the list header, as Roundcube's while `show_real_foldernames` is off
  (Mailu leaves it off); Settings' Special Folders choices show real names, as Roundcube's. A folder may hold
  several roles: it takes the first name of Inbox, Sent, Drafts, Trash and Junk it holds, as Roundcube's
  `folder_classname`, else Archive; Archive's icon whenever it holds that role, whose rule Elastic puts last; and
  the place of its first role in the folder order, as `sort_folder_list`. A folder other than the open one whose
  unread count and newest UID both grew (new mail arrived, not mail marked unread) takes Roundcube's "recent" accent
  until it is opened or read; every folder, as Roundcube's after a Refresh, since kubicle has no Refresh. Trash's
  icon hollows while Trash itself holds nothing, as Elastic's. A folder with subfolders collapses with an arrow left
  of its icon, as in Roundcube, remembered per folder in the settings; collapsing a folder that holds the open one
  opens it, unless it cannot be opened, as Roundcube's `folder_collapsed`; with Shift, Left and Right fold the
  folder and every folder below it, saved. A message drag resting on a collapsed folder for a second opens it until
  the page reloads, unsaved, as Roundcube's tree. INBOX always counts as subscribed, whatever the server's LSUB
  says. A parent that is unsubscribed or missing on the server shows above its subscribed subfolders as a folder
  that does not open and counts nothing, Roundcube's virtual folder, so a `\Noselect` parent needs no subscription.
  Create, rename, delete and subscribe; drag messages onto a folder to move them, or with Shift held to choose Move
  or Copy, as Roundcube's drag menu (Ctrl moves). A right-click gives Open, Mark all as read, Empty, New subfolder,
  Rename and Delete. Create and Rename are Roundcube's form: Folder name and Parent folder ("---" for the top),
  which moves a folder when it changes; a name is checked as Roundcube's (`folder_save.php`), on both sides with one
  function: not empty, at most 128 characters, no hierarchy delimiter, control character, `%` or `*`, in its words.
  A rename takes the folder's preferences (and those of the folders below it) to the new name, and the open folder
  stays open under it. A delete unsubscribes each folder only once it is gone, so a refused delete leaves it in the
  pane. The open folder and page are in the address (`#folder=INBOX&page=3`, Roundcube's `_mbox` and `_page`), so a
  reload stays there; a name that no longer exists falls back to INBOX.
- **List.** Pages of 50, Roundcube's "Rows per page" in Mailbox View: 2 to 200, and a number outside that, or text
  that is none, saves as the nearer end, as Roundcube's save clamps it; the field shows what was saved. The list
  header, centred, has Roundcube's menus without Refresh, since the list is live: Select (Selection, which shows a
  checkbox column; All, every message of the folder on every page, which actions reach as IMAP `1:*`; Current page;
  Unread; Flagged; Invert; None), Threads (Expand unread, Expand all, Collapse all; the button disabled where
  threads are off) and Options, Roundcube's dialog with the sorting column (None, Arrival date, Sent date, Subject,
  From/To, From, To, Cc, Size), the order and the list mode, applied on Save. The sort and list mode are remembered
  per folder, default Roundcube's: no sort column, newest first, as a list. A sort change keeps the page shown, as
  Roundcube keeps `_page`; a list mode change goes to page 1, the address with it. The Threads menu's choice holds
  for that folder until the page reloads, as Roundcube's, over the setting; a thread the user or that choice
  expanded or collapsed stays so when the list refreshes. Select all holds across pages of its folder. The checkbox
  column is remembered in the browser, as Elastic remembers it. Emptying the last page shows the one before. A sort
  change costs one round trip: the list request names the sort itself, and the settings save goes alongside. Where
  the folder column is hidden, the folder name shows centred above the list and the three menus fold into one List
  actions menu (⋮), as in Elastic's normal mode. The footer is Roundcube's: first and previous page, "1–50 of 179"
  (Roundcube's "Messages 1 to 50 of 179" as its tooltip), a page box that jumps on Enter, next and last page. Each
  row is Roundcube's, line for line within about a pixel: the sender (the recipients in Sent, Drafts and their
  subfolders, where From/To also sorts by To, as Roundcube's `message_list_smart_column_name`) with the date on line
  1, the size instead on hover (up to three names, `...` after the third, each with its address as its tooltip, a
  name holding an `@` giving way to the address, as Roundcube's `address_string`); the status mark (replied,
  forwarded or both, else the unread dot, else a hollow dot for a collapsed thread with unread replies, else a small
  dot; a click marks the message read or unread, as Roundcube's `toggle_status`) and the subject on line 2, the
  sender and subject sharing one left edge; the flag and the attachment, report or encrypted mark in their own cell
  at the right, one per line, each Roundcube's guess from the top Content-Type (a multipart/mixed of text alone gets
  the attachment mark, as there). Unread subjects are bold, flagged rows red, the read first message of a collapsed
  thread with unread replies underlined (an unread one is bold instead, as Roundcube's `unroot`). A double-click
  opens Roundcube's message view in the same tab (a draft opens Compose): the reader alone, with Previous and Next
  after More, across pages; Esc or Back returns to the list. On a phone, a footer under the message holds Previous,
  Reply and Next, as Elastic moves Reply there. Dates show the time today, the weekday and time this week, `Mar 30`
  this year and `Jan 17, 2012` before; the date is the arrival time under Arrival sort, else the Date header, as
  Roundcube's.
- **Context menus.** Not in Roundcube; an exception Andrew asked for. A right-click on a row (or the ContextMenu
  key or Shift+F10 on the list) opens a menu for the selection, a row outside it becoming the selection first:
  Open in new window (not on a phone or in a message window), Edit (in Drafts), Reply, Reply all, Reply list,
  Forward, Forward as attachment, Resend (bounce), Mark (the toolbar's four entries), Archive, Move to..., Copy
  to..., Junk or Not junk, Delete, Print this message, Download... (see Download), Edit as new and Show source,
  each with its key where it has one and disabled where it does not apply. It shares its entries and their rules
  with the toolbar. Text fields and the mail frame keep the browser's own menu. A delete, move, mark or drag of a
  collapsed thread's root takes its replies too, as Roundcube's selection does.
- **Menus and dialogs.** A menu opens below its button, or at the pointer, and flips to the other side of it where
  it would cross the window's edge.
  Its entries line up whether or not some have a mark or an icon, and its button shows pressed while it is open.
  A dialog takes the focus to its first field, keeps Tab inside it and gives the focus back when it closes; a
  question an answer asks next, as Send's checks do, opens once the one before has closed. Where the control that
  held the focus goes away or turns disabled, as the toolbar once no message is open, the list takes it, or where
  the list is hidden the folders or the toolbar in its place; one that went away (Compose, closed by its Discard)
  or turned disabled (Next, once a page turn's answer shows the last page) hands it on in the same task, so no key
  finds the focus on the page. A dialog belongs to the view it opened over, as Roundcube's to its page: a view
  replaced under it (Compose, by a Forward answered late) takes it along unanswered.
- **Marks.** Roundcube's Mark menu: As read, As unread, As flagged, As unflagged, each disabled where it would
  change nothing (under Select all every one applies, since the other pages' flags are not loaded), and only the
  messages it changes are sent. Mark all as read in the folder menus, on while any subscribed folder has unread
  mail: at once when only this folder has some, else Roundcube's dialog of "Selected folder only", "Selected folder
  and its subfolders" and "All folders", each enabled where it finds unread mail. "Selected folder only" follows the
  folder's own count, not its collapsed subtree's as Roundcube's does, so it never marks a folder with nothing
  unread. A message counts as read once shown in the preview, and the next message opens after a delete or move, as
  in Roundcube. A quick run of arrow keys shows each message at once but marks read only where it stops, 400 ms on,
  as Roundcube's `preview_delay_select` leaves the rows passed unread; a selection Ctrl or Shift made shows no
  message, so it marks nothing.
- **Delete.** Moves to Trash. In Trash, in Junk with "Directly delete messages in Junk", or with no Trash folder
  (Special Folders' None, or a server that marks none), deletes for good at once; Shift+Delete anywhere else deletes
  for good after a confirmation, as Roundcube's `delete_messages`. "Mark the message as read on delete" marks inside
  the server's move into Trash, whatever moved it (Delete, a drag, Move to..., Empty, under Select all too), as
  Roundcube's IMAP layer does; the STORE and the MOVE run in one job. Every folder has Empty, as Roundcube's
  `folder_purge.php`: in Trash and below it every message goes for good, anywhere else (Junk included) every
  message moves to Trash. Deleting a folder deletes its subfolders first.
- **Questions are kubicle's own dialog,** never the browser's `confirm`, `prompt` or `alert`: delete for good,
  Empty, Discard, Compose's checks at Send (Roundcube's alerts among them), renaming an attachment or editing a
  recipient, Mark all as read's scope, and creating, renaming or deleting a folder.
- **Busy notices.** Roundcube's, from the click until the server took the change: "Moving message(s)..." (Move,
  Delete to Trash, Archive), "Copying message(s)...", "Deleting message(s)...", "Marking message(s)..." and
  "Loading..." for Junk and Empty. A change may wait seconds behind a Compose start, and without it
  nothing on the page would show that the click was taken.
- **Confirmations.** Roundcube's, once the server took the change: "Message(s) moved successfully." (to Trash:
  "Message(s) moved to Trash successfully."), "Message(s) deleted successfully.", "Message(s) copied successfully.",
  "Message(s) marked successfully." (not the automatic read on preview), "Folder successfully emptied.",
  "Successfully archived", "Successfully reported as junk" (or "...as not junk") and "Message sent successfully.";
  one per action, however many folders it spans. Sizes read as Roundcube's `show_bytes`: KB, then MB and GB with a
  decimal below 10.
- **Archive and Junk.** Archive moves to the Archive folder, off inside Archive and its subfolders, and leaves
  messages already in that subtree where they are (a search over several folders included), as Roundcube's
  plugin; under "Mark the message as read on archive" the server marks them read inside the move. "Divide archive
  by" has the plugin's eight choices and folder names: none, year (`Archive/2012`), month (`Archive/2012/06`),
  Thunderbird's month (`Archive/2012/2012-06`), sender email (the address, the hierarchy delimiter as `-`, `%`, `*`
  and control characters dropped, `unknown` without one), original folder (`Archive/INBOX/github`), and year or
  year and month before the original folder. Dates are the Date header (the arrival without one) in the zone the
  user sees, which the client names in the change. The server groups the messages by target from their fetched
  headers, creates and subscribes each missing folder, then moves each group, as the plugin's `move_messages`.
  Junk and Not junk are markasjunk's: the keyword `Junk` (or `NonJunk`) goes on in the same job, before the move to
  Junk (or INBOX), so other mail apps see the verdict; Mailu's rspamd learns from the moves themselves. Junk shows
  outside Junk and Not junk in it, both over results from several folders, each taking only its own side of them.
- **Copy to...** IMAP COPY into the folder picked from the same chooser as Move to...; the messages stay.
- **Resend (bounce).** Roundcube's bounce: a dialog asks for From (one of the user's addresses), To (Cc and Bcc
  behind its "+", each with address suggestions) and where the copy goes, "Don't save" by default as in
  Roundcube, and the message goes out unchanged with Resent-From, Resent-To, Resent-Cc, Resent-Date and
  Resent-Message-ID on top. Resent-Bcc stays off the wire and in the filed copy. The From ownership check of
  sending applies to Resent-From. A message with its own Bcc or Resent-Bcc lines (a copy from Sent) goes on with
  them, here and in Forward as attachment, under Roundcube's warning "The forwarded message(s) contain
  confidential recipients (Bcc)." (Andrew chose this over leaving the lines out, Oct 25, 2026.)
- **Download and Import.** zipdownload's "Download..." menu: Source (.eml) saves one message as a file named after
  its subject, cleaned as Roundcube's `filename_from_subject` (colons and slashes to spaces, dots and underscores
  off the ends: "Re: Invoice 4/2026" is `Re Invoice 4 2026.eml`); for several, Mbox format (.zip) holds one mailbox
  file (each message after a `From ` line with its sender and arrival, `>From ` lines quoted once more, mboxrd) and
  Maildir format (.zip) one `.eml` per message named by UID and the subject's first 16 characters, under folder
  paths across folders (a name taken twice gets a `_` before `.eml`); the zip is named after the folder, or
  `messages`. Select all takes the whole folder, and a download 100,000 messages at most. The server streams the
  archive as the mail comes from IMAP, up to 4 MB of it per engine job, stored uncompressed, ZIP64 past 4 GiB or
  65,535 messages, so memory stays flat (the `zip` crate). A job per message cost two SELECTs a message outside the
  folder on screen: 26 s for the fixture's 800 in `INBOX.finance`, 0.24 s in batches (Oct 25, 2026). Open issue (Oct
  25, 2026): streamed, the crate gives each stored entry a data descriptor, which Java's `ZipInputStream` refuses,
  so these zips do not open in Java-based tools; Python, unzip, bsdtar and 7-Zip read them. Import, in More, appends
  one or more `.eml` files or mbox mailboxes to the open folder as they are, unread, as Roundcube's `import.php`
  splits them (an mbox at its `From ` lines, `>From ` unquoted), within the upload limit.
- **Reader.** Every mail in a sandboxed frame (see Security), as Roundcube's preview frame: HTML mail on a light
  "paper" background in every theme, plain-text mail in the theme's colours and monospace font (the app passes its
  tokens into the frame's document); the paper is as tall as the mail and the pane shows below it, in line with the
  header as Elastic's message part is. The header follows Roundcube's: the subject with "Open in new window" (the
  one message in a tab of its own, from an `#open=UID/FOLDER` address, with the toolbar's actions, Compose and the
  dialogs; `g` alone is off there, as it has no list), and Elastic's red flag before it while the message is
  flagged; one line "From Name on date" ("To Name" in Sent and Drafts, else the Cc, else the Bcc, and "On date" with
  nobody, as Roundcube's `message_summary`), a second address as "N more..."; then Details (Roundcube's rows: From,
  Sender, To, Cc, Bcc, Reply-To, Mail-Reply-To, Followup-To, Date and Priority, each where the mail has it, Sender
  and the Reply-To rows only where they name someone else; ten addresses to a row, "N more..." opening the whole row
  in a dialog; open or closed as last left, in the settings), Headers (Roundcube's "Message headers" dialog: the raw
  header block, each name in bold, a folded line indented by four spaces, no wrapping) and, on any mail with an HTML
  part (Roundcube's `has_html_part`), Plain text or HTML, which the server remembers per message for the session, as
  Roundcube's `msg_formats`: print and a new window follow it. Every address is a link to Compose to and Copy
  address, showing the name with the address as its tooltip; a name holding an `@` gives way to the address, as a
  phisher's `"support@bank.example" <x@evil.example>` would otherwise pass, and the setting "Show email address with
  display name" shows `Name <email>` instead. A reply or forward quotes the part the reader shows, as Roundcube's.
  Print view. Show raw source. While several messages are selected, or Select all is on, the reader shows "N
  messages selected" instead of a preview, as Roundcube clears it; the toolbar acts on the selection. A message the
  server cannot load shows Roundcube's "Could not load message from server." in its place.
- **What the reader shows follows Roundcube's `parse_structure`** (`structure.rs`), walked over mail-parser's
  part tree, not mail-parser's own body lists, which follow JMAP's rules. A forwarded message (`message/rfc822`)
  shows below the text that carries it with its own header table, and its files join the list; a bounce's
  delivery-status, rfc822-headers and read-receipt parts show as text below its human part; text/enriched shows
  formatted; uuencoded blocks in a plain-text mail are unpacked into the list, and a TNEF part (`winmail.dat`) is
  listed as one file (Not building). An encrypted part (PGP/MIME, S/MIME) shows Roundcube's "This is an encrypted
  message and can not be displayed. Sorry!", its payload listed; kubicle never decrypts. Every mail is one
  document in the frame, its text parts and header tables drawn in it.
- **HTML as text.** "Plain text" on an HTML mail, and every HTML mail while Display HTML is off, shows the HTML
  converted as Roundcube's `rcube_html2text` writes it, the converter replies quote with: the html2text crate's raw
  layout, a table's cells each on a line of their own as Roundcube's (without its tab indent and the empty line
  between rows), no images, a link's text followed by ` [n]` and its address listed under "Links:". Column layout
  padded every row to the widest cell, so 68 KB of table made 20 MB of text. Lines are not cut at 75 columns as
  Roundcube's are: a cut breaks long addresses, and the reader wraps on screen.
- **Plain text as Roundcube's `text2html` draws it.** The server writes a text part into the frame's document
  (`plain.rs`): format=flowed unfolded, so paragraphs reflow to the pane; `>` levels as nested quote bars in three
  colours (the theme's `--quote-1` to `--quote-3`, Elastic's blue, green and red); the signature, from the last
  `-- ` line with fewer than 15 lines after it, muted; a unified diff unwrapped; links for `http`, `https`, `ftp`
  and `file` in any case, `www.` and addresses, an address opening Compose to it in a tab of its own, as a `mailto:`
  link in HTML mail does. Unlike Roundcube's `rcube_string_replacer`, a reference such as `[1]` is not linked to the
  `[1] url` line it points at (the "Links:" list HTML as text ends with): only that line's address is a link. Every
  piece is escaped, and the frame runs no script.
- **Attachments, by Roundcube's list.** An image with a Content-ID and an inline disposition, or in a
  multipart/related below the top, belongs to the mail's layout and is not listed; every other file is, iPhone
  photos sent inline included, and an HTML mail shows its listed images below it, as Roundcube's `inline_images`.
  Types are cleaned as Roundcube's `fix_mimetype` (`image/jpg` and `image/pjpeg` are JPEG, Outlook's
  `application/pdf.<id>` is PDF), so such an image shows and is served. Each file opens a menu: Open, for an image
  in a tab of its own and for text, HTML and a forwarded message as a sandboxed page (Security), and Download for
  all. A PDF downloads: whether a PDF opened in the browser can run script or reach the network could not be shown
  (the test browsers are headless Chromium, which has no PDF viewer). An empty file has no menu: a click says
  "This attachment appears to be empty.", Roundcube's alert. An invite (`text/calendar`) without a name, disposition
  or Content-ID outside a multipart/alternative is `calendar.ics`, as Roundcube names it.
- **Notices above the mail**, in Roundcube's order: "This is a draft message." with Edit, in Drafts and its
  subfolders; kubicle's sender warning; remote images left out; and Roundcube's "This message contains suspicious
  email addresses that may be fraudulent." when a shown address's domain mixes scripts, as a look-alike does.
  Roundcube asks ICU's spoof checker at its highly restrictive level, which allows one script, or Latin with the
  Chinese, Japanese or Korean scripts; kubicle applies that rule to Unicode's script data (the `unicode-security`
  crate, UTS 39), a punycode domain read in Unicode first. A domain all in Cyrillic, as `пошта.укр`, gets no notice.
- **Message window.** "Open in new window" holds the page of the folder around its message (`api/list`'s
  `around`), so Previous, Next and the message after a delete follow the folder's order; it is titled with the
  subject, its address names the message shown, and after the folder's last message is deleted the tab closes. Esc
  leaves its message open and Back closes the tab; the link and More's entry are not offered inside it or on a
  phone. A reply, forward or Edit there opens Compose in the tab, which is then a compose window (Compose in a tab
  of its own), as Roundcube keeps `_extwin` for it.
- **Mail fits the frame.** A mail laid out wider than the frame shrinks to fit, as in Gmail: the frame cannot
  measure its content, so the server reads the widest fixed width the mail declares (`width` attributes and
  `width` or `min-width` in pixels) and adds `zoom` steps of 5% for narrower frames, down to 60%, below which
  the frame scrolls sideways.
- **Toolbar.** Roundcube's `mail-menu.html` in its order: Reply; Reply all with a menu (Reply all, Reply list),
  whose button and `R` do Reply list on list mail, as Mailu's `reply_all_mode = 1` makes Roundcube's, unless the
  setting "Default action of [Reply all] button" says reply to all; Forward with a menu (Forward inline, Forward as
  attachment, Resend (bounce)); Delete, Archive, Junk or Not junk; Mark; More (Print this message, Import,
  Download..., Edit as new, Show source, Move to..., Copy to..., Open in new window); in the message view and a
  message window, Previous and Next. Edit leads in Drafts. Reply list needs a List-Post address, which the row
  knows, as Roundcube's list does, so the context menu offers it before the body loads; replies, forwards and
  Resend are off in Drafts and Archive inside Archive, as Roundcube disables them; the keys follow the same rules.
  Forward follows Roundcube's "Forwarding mode" setting. Forward and Forward as attachment also take several
  selected messages, or Select all, as Roundcube's; they then always attach (Forward inline and Resend take one).
- **The toolbar never drops an action.** The buttons and More sit centred, as in Elastic. Buttons that do not fit
  the reader's width move into the More menu, as in Roundcube; labels stay. A split button that moves brings its
  menu's entries, Mark comes as a submenu.
- **Print and source open as pages of their own.** A browser prints a frame only as far as it is tall, so print
  opens the message as a sandboxed page, as Roundcube's print view: the subject as a heading, then Details' rows
  by Details' rules, the date `Y-m-d H:i` in the user's zone (its arrival when the mail has no Date), the
  attachments' names and sizes, and the body. Every address prints, where Roundcube prints ten and a link that
  paper cannot follow. Print marks the message read whatever the mark-read setting, as Roundcube's, but not from the
  "Open in new window" window, as not from Roundcube's full view. The page runs no script, so the browser's own
  Print (Ctrl+P) prints it, where Roundcube's opens the print dialog by itself (Andrew's choice, Oct 25, 2026).
  Source is plain text in the charset the mail's own Content-Type names, as Roundcube's, and UTF-8 where it names
  none, as every multipart, which Roundcube labels ISO-8859-1.
- **Remote images.** Blocked unless the "Allow remote resources" setting allows them: never, from trusted senders
  (the default) or always, as Roundcube's `show_images` without its contacts mode. Blocked, each keeps its size as
  a pale placeholder, so the mail's layout holds; one that declares no size, in its attributes or its style, takes
  no space, as most such are tracking pixels. One line above the mail says so, with Show, and "Always for" the
  sender in the trusted mode. A trusted sender's images load only when the From domain vouched for the mail:
  DMARC passed, or the domain signed it (DKIM), in the Authentication-Results our server stamped; else the line
  says why. Roundcube checks neither, so a forged From of a trusted sender loads its images there. They load
  through kubicle's image proxy, so the sender never sees the reader's IP address or browser. Show lasts for the
  session, as Roundcube's `safe_messages`, in print and a new window too; a change of the setting or the trusted
  senders reaches bodies already loaded.
- **Compose.** Plain text only, laid out as Roundcube's: a toolbar (Save, Attach, Discard, Close), the fields, the
  body in an editor box with Send and "Open in new window" below it, and "Options and attachments" in a column on
  the right (a drop zone with the size limit, the attached files, Return receipt, Delivery status notification, Keep
  formatting, Priority and Save sent message in). The address fields are Elastic's recipient input: each address a
  chip showing its name, the address its tooltip, with a remove button; Space, comma, semicolon or Enter after an
  address makes it a chip, Backspace in the empty input takes the last one out, a double click edits one, chips drag
  between To, Cc and Bcc, and a paste splits on new lines too; text that holds no address stays typed, so a draft
  keeps it. The browser's spelling check runs on Subject and the text, never in From or the address fields;
  Roundcube's server check (pspell) is not built. Each attached file opens Elastic's menu: Open (images, as in the
  reader), Download and Rename, whose name is the one the file goes out with. A file on its way shows Roundcube's
  progress ("37% (11.1 of 30.0 MB)", the browser's own report of the upload) and Cancel. The server reads two of a
  user's uploads and imports into memory at a time, up to 36 MiB each; the rest wait their turn, so a third file may
  show all its bytes sent while it waits to be stored. Sent text goes out as Roundcube's `format_flowed` writes it:
  lines wrapped at 72 with soft breaks (format=flowed, RFC 3676), quoted lines under their own marks, `From` and `>`
  lines space-stuffed; Keep formatting sends it as typed, for code or tables. Drafts are saved as typed. Attachments
  meet the mail server's own size limit, which its submission port names (`SIZE`, RFC 1870; Mailu's
  `MESSAGE_SIZE_LIMIT`) and the server reads at every send, and from start until front answers, after 1 s and then
  twice as long each time up to a minute, as front may start after kubicle: Compose refuses the file that would push
  the message past it, in Roundcube's words, counting base64 as sent, and the server refuses a file too large alone
  and a message too large in all, before anything goes out. Roundcube checks the same, but against its own
  `max_message_size`, which Mailu leaves at 100 MB over Postfix's 50 MB. The per-file cap Compose names is computed,
  the largest file whose base64 fits that limit (34 MB at Mailu's 50 MB), not PHP's upload limit, which Mailu sets
  to 66% of it (31 MB). Reply goes below the quote, as Roundcube's default, the caret on the line right below it;
  the quote leaves out the original's leading and trailing empty lines, and an empty quoted line is `>` alone, which
  a flowed reader cannot join to the next. A reply drops a trailing "(was: ...)" from the subject. An inline forward
  writes Roundcube's block: Subject, Date, From and To always, Cc when there is one, Reply-To when it is not From,
  then the body without its leading and trailing empty lines. "When replying" has Roundcube's fourth choice too:
  above the original, which follows unquoted below a forward's Subject, Date, From and To. Compose focuses the first
  empty field of To and Subject, else the body. Send checks as Roundcube's `check_compose_input`, in its order and
  words: a file still uploading (an alert), an empty subject (`""`; spaces count as one) asked once per Compose in a
  dialog with a field to type it in, an empty text, no recipient (an alert that gives To the focus), and more than
  five addresses in To and Cc (`max_disclosed_recipients`), asked once per Compose with "Use Bcc", which moves them
  all to Bcc; while it sends, the corner says "Sending message...", the fields are disabled, as Roundcube's
  `lock_form` leaves them, and so are the tools, which Roundcube's `set_busy` refuses without showing it; Esc and
  Back wait for the answer. Sent, Compose closes back to the view before it (on a phone the list, when no message
  was open). Cc alone goes out as To, and Bcc alone under `To: undisclosed-recipients:;`, as Roundcube writes it;
  drafts keep the fields as typed. A domain beyond ASCII goes out as punycode, as Roundcube's `idn_to_ascii`;
  letters beyond ASCII before the @ are refused (no SMTPUTF8). Without a Sent folder nothing is sent until another
  folder or Don't save is chosen; Sent set to None in Special Folders makes Don't save the default, as Roundcube
  saves no copy then; once sent, filing the copy, flagging the original and dropping the draft each run even when
  one before failed. Reply, reply all and forward with attachments: a forward or a reopened draft carries the files
  Roundcube's `write_compose_attachments` takes for its plain text editor: each file marked attachment or named and
  each forwarded message, and in a mail with HTML no inline part but a PDF; so an invite's unnamed calendar form and
  the images the HTML shows stay behind, and a forwarded message's own files travel inside it. A reply's recipients
  follow Roundcube's `compose_header_value`: To is Mail-Followup-To (Reply list and Reply all), List-Post's address
  (Reply list, RFC 2369), Mail-Reply-To, Reply-To or From, the first there is, and the original To when the mail is
  the user's own unless Reply-To chose; Reply all copies To and Cc, and From as well when Reply-To names every To
  address but not From (a list pointing replies at itself). An address goes once, the user's own stays out of Cc but
  not To, and every comparison ignores case, where Roundcube's do not all. The quote is the parts the reader shows,
  as Roundcube's `compose_body`, a forwarded message's left out: HTML as text (Reader, HTML as text), plain text
  with format=flowed unfolded. A reply or inline forward cuts the signature of a plain text part after a `-- ` line
  when fewer than 15 lines follow it, and keeps an HTML part's whole, as Roundcube's `compose_part_body` does.
  Replies and inline forwards keep the original's priority, In-Reply-To and References. The original turns read once
  Compose has it, before anything is sent, as Roundcube's, unless it travels as an attachment. Forward as attachment
  attaches each original whole as `message/rfc822`, 7bit or 8bit as RFC 2046 requires, stored as an upload, as
  Roundcube's `write_forward_attachments`: named after its subject's first 64 characters (`message_rfc822` without
  one) and `.eml`, a repeated name with `_2` and on; the subject "Fwd: " and the first subject among them;
  References with every Message-ID and In-Reply-To only for one message. Select all takes the folder in its saved
  sort, and a forward 100,000 messages at most; a message gone meanwhile is left out, as Roundcube's. Counting 10 KB
  for the text and each message whole, one that would pass the mail server's size limit is left out, and Compose
  says "Maximum size of a message (9.8 MB) exceeded. 1 message(s) have not been attached."; its size comes with its
  row, so it is never fetched. The server fetches a folder's messages up to 4 MB at a time in one engine job, a
  larger one alone, reads only each one's header and writes it into the upload in place (the column last, filled by
  incremental blob I/O), so two 30 MB messages peak at about 31 MB of server RSS above idle, and the container's
  cgroup at about 118 MB above its own, page cache included (Oct 25, 2026). Once sent, every message attached gets
  `$Forwarded` (Roundcube flags the left out ones too). Edit as new starts a draft from any message's recipients,
  subject, body, attachments, Reply-To, Followup-To, priority and receipt request, and replaces nothing; reopening a
  draft reads those back too. A new Compose ticks Return receipt and Delivery status notification when the settings
  "Always request..." say so (a reopened draft keeps its receipt request), and with "Place replies in the folder of
  the message being replied to" a reply or inline forward, and a draft of one, files its copy in the original's
  folder when that folder is subscribed, as Roundcube's `check_sent_folder`. From is editable: a list offers the
  user's identities, the account first, then the addresses on the user's domains it sent from (From in Sent, most
  used first), as Roundcube keeps a list of identities; family or colleagues on the same domains are never among
  them. Any address on those domains may be typed. A reply or forward starts from the identity the original was sent
  to (To, Cc), else for a reply the one it came from (the user's own mail), else the one in its Return-Path (a
  list's VERP address too), else the account, as Roundcube's `identity_select`; always with the display name. Each
  identity may carry Roundcube's Organization, Reply-To and Bcc (Settings, Identities): Compose adds its Reply-To
  and Bcc where they are not yet, and picking another From takes the old identity's out and puts the new one's in,
  as Roundcube's `change_identity`; Organization goes out as a header with mail from it. As Roundcube's
  `headers_input`, Return receipt asks with the From as it stands, name and address, and every message and draft
  carries `X-Sender`, the bare From address. Each file goes out with its name twice, as Roundcube's Mail_mime writes
  it: Content-Type's `name` (RFC 2047 words beyond ASCII, which older Outlook reads) and the disposition's
  `filename` (RFC 2231). Cc, Bcc, Reply-To and Followup-To appear on demand, as Roundcube's add-header buttons, each
  with a remove button; Reply-To goes out as Reply-To and Mail-Reply-To, Followup-To as Mail-Followup-To. A draft
  keeps one Message-ID through every save and the send, as Roundcube's compose does; a reopened draft keeps its own.
  Roundcube's `X-Draft-Info` carries the reply or forward and the delivery status tick; several forwarded messages
  go as Roundcube writes them, a UID set (`uid=1:3,7`), one header per folder, and a draft from Roundcube that
  forwards several flags them all once sent. A `mailto:` link fills To, Cc, Bcc, Reply-To, In-Reply-To, References,
  Subject and the text, as Roundcube's compose takes them; Settings, General, has Roundcube's link that asks the
  browser to open `mailto:` links in kubicle (`#compose=` and the link). A `mailto:` link in a mail keeps only the
  addresses kubicle can send to, as Roundcube's `washtml_link_callback`: with none it is no link, and one without an
  `@` goes to the browser as it is, as Elastic's menu lets it. Address autocomplete takes Roundcube's sources: the
  addresses written to in the Sent folder (To, Cc and Bcc, as Roundcube collects recipients) and the trusted
  senders; a match in the name or the address, from the first letter, up to 15, Up and Down stopping at the ends,
  Enter taking one, Tab taking one and moving on, Shift+Tab closing the list and moving back. The order is kubicle's
  (Andrew, Oct 25): the most used first, then the latest, a trusted sender never written to last, where Roundcube
  sorts by name. Read-receipt requests are never answered.
- **Send answers once submission took the mail.** The copy in Sent, the flag on what it answered or forwarded
  and the draft's removal follow in the engine's queue; a failure there reaches the user as a notice.
- **SMTP is kubicle's own small client.** It shares the IMAP side's TLS setup (public roots plus the fixture's
  CA), which lettre and mail-send cannot take. A From address must belong to the user: the account's domain or
  one in `KUBICLE_MAIL_DOMAINS`. Which address of those the user owns, kubicle cannot tell: Mailu's Postfix refuses
  any other envelope sender on port 10025 (`reject_authenticated_sender_login_mismatch`), kubicle sends From as the
  envelope sender, and Roundcube under Mailu (`identities_level` 0) relies on the same rule.
- **A draft exists only once it was edited,** as Roundcube's `compose_field_hash` check: opening Compose and
  leaving it untouched saves nothing in Drafts. The browser keeps an untouched copy only while Compose is open,
  so a reload returns to it.
- **Compose in a tab of its own.** "Open in new window" hands the browser copy, as it stands, to a new tab
  (`#draft=ID&window=1`), which goes on with it while this tab goes back to the mail without saving it, as
  Roundcube's `extwin` posts the form there; the setting "Compose in a new window" (off by default, as under
  Mailu) opens every Compose in a new tab, opened within the click as browsers allow only then, which fetches the
  draft there (`#compose-start=`). Neither on a phone, as Elastic. Such a tab, one a `mailto:` link opened, and a
  message window, is a compose window: once its Compose is sent (a second later, as Roundcube's), closed or
  discarded, it closes, after its last save to Drafts went out. A browser that refuses to close it leaves it on the
  mail. Every other window kubicle opens is a tab: Roundcube's "Handle popups as standard windows" is not built,
  as Elastic on a phone.
- **Drafts never lose text.** Every change is saved in the browser at once, and in the IMAP Drafts folder, where
  other mail apps see it, after about 10 s idle, on leaving the draft or on closing it. Roundcube saves to IMAP
  only every 300 s. With no Drafts folder (Special Folders' None) nothing goes to the server, as Roundcube then
  offers no Save: the status says "Saved in this browser", and leaving an edited Compose by any way but Send asks
  Roundcube's "The message has not been sent and has unsaved changes. Do you want to discard your changes?", whose
  Discard drops the browser copy. The server keeps no draft of its own: the browser holds the draft's copy, its
  folder and UID as one value, and each save replaces that copy in its folder, Drafts or the folder below it the
  draft was opened from, where Send and Discard remove it too. A Compose's saves go one at a time, each replacing
  the copy the one before left, and Send and Discard wait for a save on its way and drop the copy it made, so no
  copy stays behind. The copy a save replaces, or a send drops, leaves the list and the reader at once, and a page
  of Drafts asked while a save is on its way is not shown, as for a change; so closing or sending an edited draft
  comes back to the list with no message open, as Roundcube's Compose does. Attached files wait in the user's
  uploads file, and travel in every Drafts copy too, until the draft is sent or discarded, the file is removed, or
  Compose is left with the draft saved to Drafts, whose copy holds them (reopening it uploads them again);
  sign-out frees those of this browser's drafts, but not those of a Compose open in another tab, as Roundcube
  frees them on send, on `attachment_delete` and with its session. None goes while a send on its way reads it, and
  Sign out does nothing during a send, as Roundcube's `busy` lock; a leftover goes a month on, at the next start.
  Each Compose has an id, as Roundcube's `compose_id`: its browser copy is kept under it, so two tabs never
  overwrite each other's, and the address names it (`#draft=`), so a reload stays in Compose. An edited copy whose
  tab is gone (no tab marked it for 3 minutes) is offered at the next start: Restore, Delete or Ignore, as
  Roundcube asks. Sign-out saves edited copies to Drafts, each in turn with a save of it on its way, and clears
  them all from the browser, as Roundcube's `clear_compose_data`. Discard, after a question, deletes for good the
  copies this Compose saved; a draft opened from Drafts or below it moves to Trash instead. Roundcube has no
  Discard: its drafts leave Drafts only when deleted there.
- **Search.** Roundcube's options: the fields Subject, Sender (From, Reply-To, Followup-To), Recipient (To, Cc,
  Bcc), Body and Entire message, remembered per folder, by default every one but Entire message, as Mailu sets
  `search_mods`; Roundcube's prefixes at the start of the box (`from:`, `to:`, `reply-to:`, `cc:`, `bcc:`,
  `subject:`, `body:`, any case) search that one field instead, `reply-to:` Reply-To alone, and keep any further
  colon in the text, where Roundcube's cut it ("subject:Re: plan" looks for "Re: plan"); the type (all, unread,
  flagged, unanswered, with attachment, the five priorities); the date (younger or older than a week, a month or a
  year, by arrival, as IMAP's SINCE and BEFORE); the scope (the folder, its subscribed subfolders, all subscribed
  folders, as Roundcube's; by default the subfolders, and results show their folder only where more than one is
  searched). That default is Andrew's choice (Oct 25, 2026): Mailu sets `search_scope = 'sub'`, but Roundcube 1.6
  never reads that key and starts every search on the folder alone. Results come in pages like the list, in the
  folder's sort; threaded where the folder is threaded and it alone is searched, as Roundcube's THREAD over the
  hits; over several folders without a sort column, grouped by folder in the order of the folders, which descending
  reverses whole, as Roundcube's. A sort change re-sorts them on the same page, and mail that changed (a move, a
  mark, new mail) asks the search again on the same page, as Roundcube's list after a move keeps its page; a page
  past the end steps back to the last, and a change of the text or the options starts again from the first. A reset
  button shows while a search is pending (text, a type or a date) and, like Esc, ends it, puts its options back and
  shows page 1 of the folder, as Roundcube's `reset-search`; Chromium's own clear button is hidden, as it left the
  type and date behind. Approved exception: it searches as the user types, without Enter. Each change is a new query
  whose answers alone are drawn, the older requests aborted, and it closes the open message, as Roundcube's
  `qsearch` clears the list; the same query asked again for changed mail keeps it. Subject, From, To and Cc answer
  from Dovecot's index at once (IMAP), and only the server's answer is drawn: Mailu's Dovecot stems words, drops
  stopwords, folds accents and splits addresses, so no filter of the hits on screen could match it. The list on
  screen, the folder's or the query before's, stays until the answer comes. The rest (the body, the entire message,
  Reply-To, Followup-To, Bcc, priority) goes to IMAP once typing pauses for 250 ms, a folder at a time. A newer
  query aborts the requests of the one before, and a search whose request ended stops before its next folder. The
  server keeps no record of the latest query, as requests can arrive out of order.
- **New mail.** The list updates live. The tab title names the view, as Roundcube's does: Mail, with the
  Inbox's unread count before it, Compose or Settings. In Mail it names the folder's path, or "Search result" while
  search results show, as Roundcube's `list_pagetitle`. No pop-up notifications.
- **Keys.** Roundcube's list keys (`list.js`), which keep the focused row, where the keys act and marked at its
  left edge, apart from the selection. Up and Down open the next row as the only selection, and with nothing
  selected move the focus alone, as Roundcube's `use_arrow_key`, until Enter or Space opens the focused row; with
  Shift they select the range from the anchor, with Ctrl they move the focus alone and with Ctrl+Shift they add the
  range. Home and End do the same with the first and last row. Space opens the focused row, Ctrl+Space adds or
  removes it and Shift+Space selects the range to it; Ctrl+click adds a row, Shift+click selects a range,
  Ctrl+Shift+click adds one. A selection made with Ctrl or Shift closes the open message and opens none, back to one
  row too, as Roundcube's `multi_selecting`. In a thread Right expands it, then goes to the first reply; Left
  collapses an expanded thread, else goes to the message a reply answers, and Ctrl+Left to the thread's first. Down
  from an open reply whose thread was collapsed since goes on from its place. Then Enter, Delete, Esc, Ctrl+A,
  Page Up and Down, and the folder tree's keys (one Tab stop, Up and Down between folders, Left and Right to fold,
  with Shift the whole subtree). Plus single letters: `c` compose, `r`/`R` reply, `f` forward, `a` archive, `s`
  flag, `!` junk, `u` unread and Shift+U read, `/` search, `?` help. `g` opens Fastmail's "Go to folder" (a few
  letters, Enter) and `v` the same box to move the selection there. Where the folder column stands in the list's
  place, every list key (arrows, Home, End, Page Up and Down, Space, Enter, Delete and the letters that act on
  messages) leaves the hidden list as it is, as Roundcube's list takes keys only while it holds the focus: Up and
  Down give the open folder the focus, the tree's keys go on from there, and Enter on a folder opens it. In the
  message view (a double-click, a message window, the reader on a phone) the arrows leave Previous and Next on their
  buttons. The frame scrolls with the keys only with the focus inside it: its opaque origin keeps the app from
  scrolling it.
- **Settings.** One scrolling page of sections, listed in a column on its left: a click scrolls a section to the top
  and marks it, and the section in view is marked. On a phone the list is a row of links above. The sections follow
  Roundcube's preferences where they apply to kubicle, with Roundcube's defaults: General (name, time zone, each
  zone by its current IANA name as PHP lists Roundcube's, `Europe/Kyiv` where Chromium says `Europe/Kiev`;
  appearance, and Browser Options' link that registers kubicle for `mailto:` links), Mailbox View (mark previewed
  messages as read: never, at once or after 5 to 30 s, counted from when the body shows, so its STORE never queues
  ahead of the body; expand threads: never, all, with unread; rows per page; display next after delete or move),
  Displaying Messages (show email address with display name, display HTML, allow remote resources), Composing
  Messages (compose in a new window; always request a return receipt or a delivery status notification; place
  replies in the original's folder; reply with no quote, below or above it, or above it without indent; forward
  inline or as an attachment; Reply all's button to all or to the list), Special Folders (Drafts, Sent, Junk, Trash
  and Archive, each "As the server marks it", None or any folder but INBOX, as Roundcube's choices; applied over the
  server's SPECIAL-USE flags wherever a role is looked up; None gives the role to no folder; a chosen folder deleted
  since by another mail app gives the role back to the server's flagged folder; only a choice being made must name
  an existing folder, not INBOX, and one folder may hold several roles, as Roundcube keeps a `<role>_mbox` for each;
  then the archive plugin's "Divide archive by"), Server Settings (mark read on delete, delete Junk directly, clear
  Trash on sign-out: never, all messages or older than 30, 60 or 90 days, and as Roundcube's Trash's subfolders with
  them, each emptied as it is and removed once nothing in or below it stays; mark read on archive), then Themes,
  Folders, Identities (the account and the addresses it sent from, each with Organization, Reply-To and Bcc) and
  Trusted senders. Folders is Roundcube's folder manager: a filter box that keeps matching folders and the folders
  above them, each folder's subscription (locked for INBOX and a special folder while subscribed, so one
  unsubscribed elsewhere can come back, and for a `\Noselect` one, as Roundcube's), message count and size (the sum
  of RFC822.SIZE), Create, Rename and Delete.
- **Settings never undo another tab's.** Each save reaches every open tab of the user as an event. Settings carry
  a revision: a save from a copy older than the stored one is refused, and the tab then applies its change to
  the stored copy and saves again, so a tab that missed the event (asleep, offline) cannot revert what another
  saved. A change shows at once, and a save that fails shows the stored settings again, as Roundcube's form after a
  failed save.

## Themes

- **Tokens.** Every colour, font and radius in the interface comes from about 30 named CSS custom properties
  (surfaces, text, lines, accent, states, status, quote levels, type and shape), defined on top of a palette
  layer. A theme is a plain `.css` file that sets `color-scheme` and those tokens.
- **Built in.** Catppuccin Latte (the default light theme) and Catppuccin Frappe (the default dark theme), one
  token mapping over Catppuccin's official palette. A saved theme that no longer exists falls back to the
  default.
- **Light and dark.** The user picks a light and a dark theme; `auto` follows the system, or one theme is
  pinned. A change of the system's scheme drops the pin, as Elastic deletes its `colorMode` cookie then.
- **Custom themes.** From a themes directory the NixOS module provides, or pasted or uploaded in settings
  and stored per user. A theme can never import remote CSS: in a mail app, remote CSS can read what is on the
  screen through attribute selectors and `url()`.
- **Mail bodies are not themed.** Tokens do not reach inside the sandboxed mail frame; HTML mail always shows
  as its sender designed it, on paper.

## Speed budgets

`tests/budgets/run.sh` measures these on the development machine and fails above them. Not in a VM, where
every step ran two to five times slower and the budgets only ever failed (Oct 25, 2026).

| Action | Budget |
|---|---|
| Installed app start to list visible | < 50 ms |
| Folder switch, paging, arrow keys (client state) | < 16 ms |
| Open a prefetched message | < 16 ms |
| Open a message not prefetched, sorting (server) | < 50 ms |
| Search on 50,000 messages | < 100 ms |
| Fresh server, sign-in to the first page of 50,000 messages | < 300 ms |
| New mail visible after delivery | < 1 s |

Sorting is a server round trip: Dovecot's SORT of 50,000 messages takes 30 to 45 ms, whatever its cache
holds. A search waits for Dovecot's header search, about 35 ms in a folder of 50,000 alone (Oct 25, 2026).

## Security

Roundcube has 68 CVEs since 2020, 10 of them exploited in the wild. Most come from one design: sanitized mail
HTML inserted into the application's own page. Each rule below answers a class of them.

- **Mail HTML runs in a sandbox with its own origin.** The body renders in an `iframe` with `sandbox` and no
  `allow-same-origin` or `allow-scripts`, so it gets an opaque origin, cannot run script and cannot reach the
  app, its cookies or its storage. Its document carries a strict Content-Security-Policy: no scripts, no
  remote images, fonts or styles except through the image proxy, no forms, no frames.
- **Sandbox and CSP stop scripts; ammonia is needed for the rest.** With ammonia switched off, the hostile
  suite saw no script run, but leaks remained in 17 cases in Chromium and 10 in Firefox (of 5,299; Oct 25,
  2026): DNS lookups and TCP connections without a request from `preconnect`, `dns-prefetch` and blocked
  `<iframe src>`, and relative URLs the frame fetched from the app's own origin. Chromium also fetches a
  `<link rel=prerender>` URL (2 cases; Oct 25, 2026): NoStatePrefetch ignores CSP and the sandbox. Neither covers
  those hints, so ammonia, which drops the elements that carry them, is a required layer.
- **No `Connection-Allowlist`.** It is the one page-side gate on NoStatePrefetch (Chrome 152 on; verified in
  Chromium 155 and 156), but under it Chromium refuses every redirect a navigation from the app page meets
  (`ERR_UNSAFE_REDIRECT`): sso.php's re-sign-in, Mailu's logout and every mail link would each need a hop page.
  Andrew chose ammonia alone, as every webmail relies on (Oct 25, 2026).
- **The app's own CSP is part of that boundary.** A srcdoc frame inherits the parent page's policy on top of its
  own, and Firefox races `srcset` loads in a sandboxed srcdoc past the frame's meta policy (measured: about half
  of loads with an open app policy, none with the strict one). Ammonia drops `srcset` and `<picture>`, as
  Roundcube's washtml does, and the app header still keeps `img-src 'self' data:` and allows `style-src
  'unsafe-inline'`, which mail styles need.
- **Everything the frame loads uses signed URLs.** The frame has an opaque origin and sends no cookie, so inline
  parts, downloads and proxied images carry an HMAC over the route and every parameter before it: the session's
  public handle, the resource and an expiry. One extractor checks it and alone finds the session by that handle; the
  session cookie never appears in a URL. An inline image's URL also signs its IMAP section and decoded size, so
  its request fetches that part alone, and falls back to the whole message when the server's part numbering
  disagrees. No byte of mail is stored by the browser (`private, no-store`), as Roundcube's PHP session headers
  and its `nocacheing_headers` on every JSON answer forbid it: a route the cookie authorizes (`api/download`,
  `source`, `print`, `zip`, `api/message`) names no account, so the next account asking the same URL got the last
  one's mail, and an attachment opened from a signed URL reopened from history after Sign out. Each render signs its
  URLs anew, so a cache served them once anyway. The router gives every answer but the app's own files that header,
  `nosniff` and `default-src 'none'; sandbox`, so no route can leave one out; the files take the app's policy.
- **An admission pass gates the HTML parser.** ammonia and the HTML-to-text conversion both parse with
  `html5ever`, whose adoption agency re-opens each unclosed formatting element into every later block: a 1.6 MB
  mail of unclosed `<b>`s made it build gigabytes and could exhaust the one process. Before either runs, one
  streaming pass with `lol_html` (a tokenizer with no tree builder, so it cannot blow up itself) refuses a part
  nested past 256 elements, or whose reconstruction work would pass a bound real mail stays far under (a heavy
  Word export measured a fifth of it), or that it reads as ambiguous. `lol_html` tokenizes for real, so a `<` in
  CSS, a comment or an attribute never counts as a tag and no real mail is refused for one (`render.rs`,
  `too_complex`).
- **One hostile mail's cost is bounded.** One process serves every user, so no mail may cost it out of proportion:
  past 1,000 messages named inside it, or 50,000 part delimiter lines, a message is not parsed (422, and it still
  downloads); a part past 2 MiB, or HTML the admission pass refuses, shows a notice instead; 1,000 image sources per
  part, 16 enriched excerpts and 256 styling spans are converted, and 1,000 uuencoded files a message unpacked; a
  reply or forward quotes no HTML part past 2 MiB; tables lay out a cell per line; address lists compare as sets;
  a list row carries four recipients, the three the list shows and one for its `...`, so a mail to tens of thousands
  makes no megabyte list page (the reader fetches the full list itself);
  THREAD is read and walked without recursion; an import, a zip and a forward take 100,000 messages, a draft's files
  must fit the mail server's size limit before one is read, and a user keeps 1,000 uploads; a user's settings are 1
  MB at most; a search over several folders merges 1,000 hits of each; the image proxy fetches 8 at a time; and the
  engine keeps a trail of at most 1,000 replaced draft copies, for a concurrent save that still names one, dropped
  whole past the cap. The limits are constants in `render.rs`, `structure.rs`, `uploads.rs`, `settings.rs`,
  `sync.rs`, `proxy.rs` and `api`'s `MAX_UIDS`; each but the import's, the styling spans' and the draft trail's
  has a case in `tests/vm/cost.py` (the proxy's in
  `tests/vm/scenarios.py`, `image_proxy_bounded`), as an import small enough to finish in front's proxy timeout
  cannot tell the reads per APPEND it had from none. One user with slow image hosts can fill the proxy's 8 for
  everyone, which only delays remote images (accepted, Oct 26, 2026).
- **A crash renders once.** The reader prefetches unopened mail, so a render that kills the process would kill it
  again for every tab that reloads after the restart. Whatever parses a whole message for a page or a Compose
  (the reader, print, a reply or inline forward, an attachment's page or file) runs on the blocking pool, as many
  at once as there are CPUs, and leaves a marker in `rendering/` of the data directory while it runs; the work holds
  both, not its request, which a closed tab drops. Forward as attachment, a download and Show source read only the
  header block, on the async workers. A marker a dead process left becomes a crash record at the next start: that
  message answers 422 (it still downloads) until a new build starts, or the record is deleted.
- **No remote CSS, ever.** kubicle never fetches a stylesheet named in a mail. Roundcube's CSS proxy produced
  a run of server-side request forgery CVEs.
- **The image proxy refuses private addresses.** It resolves the host once, rejects loopback, private,
  link-local and Docker ranges, connects to that address, and accepts only image types.
- **The app sends a strict CSP and `X-DNS-Prefetch-Control: off`.** CSP does not cover DNS prefetch.
- **No `eval`, no shell, no deserialization of untrusted data.** Sessions are typed values behind an opaque
  ID. Roundcube's worst bugs came from PHP `eval`, `unserialize` and ImageMagick shell calls.
- **Protocol commands are built from types.** UIDs are integers, flags an enum, header values reject CR and
  LF, so a crafted subject or flag cannot inject an IMAP or SMTP command.
- **Every state change is a POST with a token**, checked against the session; no GET changes anything.
- **Sender authentication.** The reader trusts only the topmost Authentication-Results whose authserv-id is
  `KUBICLE_AUTHSERV_ID` (Mailu's first `HOSTNAMES` entry). The mail server adds its own above every header the mail
  came with, so one further down is the sender's whatever id it names (RFC 8601, sections 5 and 7.1), and kubicle
  does not rely on rspamd deleting such copies: rspamd 3.8.4, Mailu 2024.06's, inserts its header at the top
  (`milter_headers.lua`, order 1) on every path, received and submitted, and removes none (`remove = 0`), read Oct
  26, 2026. An import came through no server, so it renames every Authentication-Results it brings to
  `X-Imported-Authentication-Results`. The header is read by RFC 8601's grammar: a `;`, `=` or space inside a
  quoted string or a comment, where the server copies sender text such as the envelope sender's quoted local part,
  is not structure. Quoted strings follow rspamd's writer (`maybe_smtp_quote_value`), which escapes only `"`, and
  a quoted string stays whole inside a comment, as rspamd puts the sender in its SPF comment. That writer gives a
  value ending in `\` the same `\"` as an escaped quote, so a header holding `\"`, or a string never closed, cannot
  be split for sure: it shows no results, only "could not be read". A DMARC pass counts only for the `header.from`
  domain the reader shows. Details show "Signed by" (DKIM) and "Mailed by" (SPF). A warning shows when DMARC fails
  under a quarantine or reject policy, or when the From is on the user's own domains and DMARC does not vouch for
  it: unless a signed-in sender (`auth=pass`) sent it from that address, as its envelope sender (`smtp.mailfrom`)
  says, which Postfix ties to the login; another account's shows as "sent by the account". Mailu writes no
  `smtp.auth` (`add_smtp_user = false`). A DMARC failure under `policy=none` stays silent: mailing lists break
  signatures every day. Roundcube shows none of this; its `authres_status` plugin is third-party.
- **Known limits, as Roundcube's.** A deleted account's settings row and uploads stay until the address signs in
  again, and uploads go a month on only when an engine starts, as Roundcube keeps a deleted user's row. A zip
  streams to its end after Sign out, as Roundcube's request runs to its end. A Resend, reply or forward names its
  message by folder and UID without UIDVALIDITY, as Roundcube's `_uid` does, so a folder deleted and made anew while
  a tab showed it may give that UID's new message (signed part URLs do carry it).
- **Attachments download** with `Content-Disposition: attachment` and `nosniff`; raster images preview, as
  images. Text, HTML and forwarded messages open through the signed `view` route as pages of their own under the
  mail policy plus `sandbox`, as print does: text decoded to UTF-8 and served as `text/plain`, HTML through the
  sanitizer, a forwarded message drawn as the reader draws a mail. Signed part URLs name a part by a path through
  forwarded messages and unpacked files (`structure::PartPath`), so a forwarded message's own parts have URLs.
  Compose's Open and Download of the user's own uploads (`GET api/upload`) take the same headers, and only an image
  opens, as an image.

## Testing

There are no unit tests. Every test runs the real program against a real mail server, a real browser or
real hostile input, and each suite is its own flake check.

- **Hostile HTML.** Known-malicious mail goes through the real render path in a real browser with the network
  blocked and logged. A case passes if no script runs, no request leaves, nothing navigates and the app around the
  frame is untouched. It runs twice: with the sanitizer, where any DNS lookup, connection or stray request to the
  app fails a case; and without it, where a script or an HTTP request leaving fails a case and the lookups and
  connections that get through count as notes (see Security). Corpora are fetched by hash at test time and never
  committed: cure53 HTTPLeaks, emailprivacytester (ept3), DOMPurify's and html5sec's vectors, WPT parsing data,
  Roundcube's CVE payloads, PortSwigger's XSS data, Stalwart's and MimeKit's MIME tests, the classic MIME torture
  test, espoofer and the Big List of Naughty Strings.
- **How the hostile suite renders.** Most cases render many at once on a page the server serves from the app's
  origin, under the app's CSP, each in a frame with the live reader frame's attributes; a leak fails the batch,
  which then splits in fresh browsers until it names the case. The own cases and every case with a `srcset`
  open one by one in the real reader: they prove that path, and Firefox's srcset race shows only there. Each of
  those also opens as its print page, the same HTML as a page of its own under its own policy header. A quick set
  (a case per feature fingerprint) is the check while editing, and the plants run proves from one boot that each
  planted bug fails the run it must.
- **Exploration.** Bombadil drives the UI in a browser at random and checks properties in every state;
  inline-snapshot pins what it observed, so a change in behaviour shows as a diff to review. The hostile suite
  drives its browsers with a WebDriver client (Python Selenium) instead: Bombadil drives only Chromium, and it
  explores at random where a corpus needs every case opened once.
- **Mail server fixture.** A Dovecot with seeded fixture mailboxes (threads, HTML, attachments, 50,000
  messages) for listing, threading, search and sending.
- **NixOS VM tests on a copy of Mailu, the same fixture mailboxes.** The scenarios run on a simplified copy of the
  owner's Mailu setup, with front's SSO in the path, on the whole mailbox; the live checks run on it with a small
  one. A second tier on nixpkgs' Dovecot and Postfix ran the same scenarios and caught nothing this one missed.
  vm-full adds a sender's image host at a public-looking address (`tests/vm/image-server.nix`), which the image
  proxy's scenario fetches through.
- **Roundcube as the oracle for settings.** `vm-settings` runs Mailu's own Roundcube beside kubicle, each with an
  account seeded with the same mailbox, and sets pairwise combinations of the settings both have and the server
  can show (`tests/vm/differential.py` lists them and Roundcube's preference for each): in kubicle through its
  API, in Roundcube through its own `rcube_user`, as Settings' Save stores them. One script of actions runs in both
  pages (open, reply, reply all, forward, a new message, marks, archive, delete in INBOX and Junk, move, sign out)
  and after each step the two accounts' folders, flags and sent headers must match.
  `KUBICLE_SETTINGS_RANDOM=SEED:COUNT` runs random combinations instead.
- **A failed VM check keeps its evidence.** Each VM check is a run that records and always builds, and a small
  check over its verdict (`tests/record.py`, `tests/judge.nix`), so a failure needs no second run to be understood:
  the check prints the run's store path, and `.run` on the check gives it at once. A directory per machine holds
  its whole journal (kubicle's debug log, a line per request; memory and load every 2 s; the services), Chromium's
  network log with each request's net:: error, Caddy's access log and the suite's own files (failure screenshots,
  hostile reports); `nix log` on the run has every VM's console, also of one that crashed. A run caches like any
  build: `nix build --rebuild` on `.run` runs it again.
- **Hostile cost.** vm-full's `tests/vm/cost.py` holds our own cases, each a mail built to crash kubicle or make it
  spend time or memory out of proportion (Security, One hostile mail's cost is bounded). Each lands in a folder byte
  for byte through `doveadm save` and goes through the routes it attacks; it fails when kubicle's process changed, a
  route answered outside its statuses or past its seconds, an answer passed its size, or a route's peak RSS rose
  past its bound.
- **Speed budgets, the one suite that is not a flake check:** `tests/budgets/run.sh` runs on the development
  machine, alone, against the Dovecot fixture (Speed budgets).
- **Fast checks:** `tests/lint.sh` runs every formatter, linter and type checker from the small `lint` shell,
  the same on the development machine and in CI (`.github/workflows/quality.yml`). CI runs nothing else: clippy,
  the builds and the suites above compile, so they run on the development machine (`tests/suite.sh`).

## Deployment

- **Image.** Nix builds the server and the WebAssembly client into one container image.
- **A deploy reaches every browser.** The build names each client file after its content, and a theme URL
  carries its content hash as `?v=`; the server caches both for good. `index.html` and icons are never stored
  (`no-store`), as the Nix store's 1970 mtimes cannot tell versions apart; the service worker compares the page's
  text to drop the old build's files.
- **The client ships small, never at a click's expense.** JSON goes through the browser's own parser
  (serde-wasm-bindgen), and the build writes a brotli copy of each file that the server sends to browsers that
  accept it: the wasm went from 1.28 MB, about 480 KB as Caddy compressed it, to 365 KB (Oct 25, 2026). The
  Roundcube parity work of Oct 25 grew it to 1.77 MB, 488 KB in brotli (Oct 25, 2026). Two more cuts were
  measured and declined, as each costs every click for bytes a deploy sends once: Leptos's `erase_components`
  (60 KB, 2 ms a page turn and 4 ms a folder switch) and building every dependency for size (18 KB, half a
  millisecond). Whole-program LTO for the client alone took 11 KB off at no cost to a click (Oct 25, 2026). Leptos
  is the floor: it brings 170 of the client's 171 crates, and a Leptos counter alone is 24 KB in brotli.
- **A service worker starts the app.** `sw.js` answers `index.html` from its cache at once and revalidates it
  beside the start, and serves content-named files from its cache. When the session names a build other than
  the page's, the app reloads once. At a 64 ms round trip the start went from 157 to 93 ms (Oct 25, 2026): the
  page's revalidation, one round trip, had come before any request.
- **The first request leaves from `boot.js`**, while the wasm compiles: `api/session`, which holds the first page of
  the folder the URL names. The server fills that page's size and sort from the user's settings, so the client need
  not read them first, and reads the folders after it, so they come from the engine's first folder list; nothing on
  the page acts before that page is there. After the first paint, the first pages of all other folders go out
  together.
- **NixOS module.** The consuming configuration (kittyos) adds kubicle as a flake input and runs it on
  Mailu's webmail network under the `webmail` alias. It provides the mail TLS hostname, the time zone, the
  themes directory, Mailu's `WEB_ADMIN` path when it is not `/admin` (`KUBICLE_ADMIN_URL`) and where the source is
  published (`KUBICLE_SOURCE_URL`).
- **Only front reaches kubicle.** Its port is never published and it shares a network with front alone, which
  `KUBICLE_TRUSTED_PROXY` names: `sso.php` passes on any secret to a port Mailu does not rate-limit (Mailu
  integration, Rate limits), and axum sets no deadline on a request's head, so a peer that never ends one holds a
  socket; behind front's nginx no partial head reaches kubicle, as nginx reads it whole first. The container gets a
  file limit. The session cookie cannot take a `__Host-` prefix, Mailu's logout clears its name, so a sibling host
  of the mail domain could set one that shadows the user's: no sibling may serve anyone else's content (accepted,
  Oct 26, 2026).
- **Health.** `/healthz` answers 200 `ok` to the peers `KUBICLE_TRUSTED_PROXY` names and to loopback. The
  image has no shell, so its Docker healthcheck is in exec form (`docker run --health-cmd` would wrap it in
  `/bin/sh -c`): `kubicle-server --health` asks `/healthz` on the loopback address of `KUBICLE_LISTEN`'s port and
  exits 0 on a 200 within 5 s, else 1 with the reason in the container's health log. It shows that the server
  answers HTTP, not that IMAP is reachable. kubicle has no metrics endpoint; its debug log has a line per request.
- **Backup.** Only the settings database (preferences, trusted senders, custom themes, saved sort and search
  choices).
- **Replacing Roundcube.** Roundcube stays stopped until kubicle runs. Then the module takes the webmail
  container's place, and the Roundcube image pin, its database dump and its documentation go.

## Roundcube parity

kubicle copies Roundcube 1.6.18 as Mailu configures it, from both sources read side by side (Oct 25, 2026);
Features says per feature where it follows. Everything not listed here follows Roundcube. These differences
are deliberate, each with who chose it, so a later change does not take one back as a bug.

### Where kubicle differs on purpose

- **Search scope.** The folder and its subfolders by default. Mailu sets `search_scope = 'sub'`, but Roundcube
  1.6.18 never reads it, so Mailu's Roundcube searched the open folder; kept (Andrew, Oct 25).
- **Remote images.** "From trusted senders" by default, where Roundcube under Mailu never loads them, and a
  trusted sender's images load only when the From domain passed DMARC or signed the mail (DKIM), which Roundcube
  does not check (Andrew, Oct 25). A blocked image that declares no size takes no space, where Roundcube shows its
  `blocked.gif` at its own 100 by 50 px (Andrew, Oct 25).
- **Compose.** Leaving it saves to Drafts, where Roundcube asks to discard (with no Drafts folder kubicle asks
  too); Esc closes it from any field; a Discard button, which moves a draft opened from Drafts to Trash and
  deletes only copies this Compose made; drafts saved as typed, never wrapped (Andrew, Oct 25). After a send, back
  where you were, where Roundcube lists the folder (Andrew, Oct 25). A draft opened from a folder below Drafts is
  saved back there, its old copy replaced there, where Roundcube saves it to Drafts and deletes the old copy's UID
  from Drafts, which may name another draft (Andrew, Oct 25). Uploads also go on Discard and once Compose is left
  with its draft saved to Drafts, where Roundcube keeps them until the session ends (Andrew, Oct 25).
- **Identities.** The account and the addresses on the user's domains it sent from, where Roundcube keeps a list
  the user edits; Roundcube's `identity_select` picks among them (Andrew, Oct 25).
- **Sending.** A domain beyond ASCII goes as punycode, letters beyond ASCII before the @ are refused (no SMTPUTF8),
  and nothing is sent without a Sent folder, where Roundcube creates one (Andrew, Oct 25).
- **Quotes and comparisons.** An empty quoted line is `>` alone, which a flowed reader cannot join to the next,
  and the first line keeps its indentation; addresses compare ignoring case, where Roundcube compares some as raw
  text (reply rules, Details, print, the forward header).
- **Print.** No print dialog of its own: the page runs no script, and Ctrl+P prints it (Andrew, Oct 25).
- **Attachments.** A PDF downloads: no check could show a PDF opened in the browser to be safe (Oct 25).
- **Folders and list.** The "recent" accent on every folder that got new mail, where Mailu's Roundcube showed it
  on Inbox only between Refreshes; in results from several folders each row decides From or To by its own folder,
  where Roundcube decides by the folder searched; the list's name guard holds under "Show email address with
  display name" too, where Roundcube's list then shows a name holding an `@` (Andrew, Oct 25); the quota bar only
  for an account with a limit (Andrew, Oct 25); the sort remembered per folder.
- **Settings.** Saved at once, not per section; Details open or closed is account-wide, where Elastic keeps it
  per browser. New windows open as tabs: no "Handle popups as standard windows" (Andrew, Oct 25).
- **Time zone.** "Auto" is the browser's zone; Roundcube's under Mailu's sign-in is the server's, UTC on Mailu, as
  `sso.php` never sends the browser's (Andrew, Oct 25). Sent mail is dated as Roundcube's: the zone chosen, else the
  server's.
- **Dates.** kubicle's own formats (Features, List), where Roundcube writes "Today 13:45" and "2025-03-30 13:45"
  (Andrew, Oct 25).
- **Charset.** Unlabelled 8-bit text reads as UTF-8 for now: Not building has the reason and the option kept.
- **Session end.** The notice has a Sign in again button, and the sign-in comes back to the same Compose, folder
  and page, where Roundcube under Mailu opens the Inbox and a fresh Compose (Andrew, Oct 25).
- **Autocomplete order.** The most used address first, where Roundcube sorts by name (Andrew, Oct 25).
- **Archive by sender.** A From without angle brackets files under its address, where the plugin's pattern
  (`archive.php`, `sender_subfolder`) finds none in it and files the mail under `unknown`: in its `[\b<]` PHP
  reads `\b` as a backspace, not a word boundary, so only an address in angle brackets matches (Andrew, Oct 25).

## Not building

- **Several mailboxes in one session, or a unified inbox.** Needs credentials for more mailboxes than the one
  Mailu signed in; revisit once Mailu offers native OIDC (Mailu PR #4068).
- **Keycloak or other OIDC sign-in.** Mailu's sign-in decides; kubicle follows it when Mailu has OIDC.
- **A native desktop app.** GPUI can target the web, but draws on a canvas: no browser find, text selection or
  accessibility, a 2 MB+ download, and no HTML mail rendering.
- **An API for agents.**
- **HTML or rich-text compose.** The owner wrote plain text in Roundcube; may be revisited.
- **Signatures, canned responses, saved searches, contacts and CardDAV, a filter editor, PGP, password
  change, an admin page, plugins.** None was used: filters and accounts are declared in the Mailu
  configuration, and Mailu's reconcile undoes password changes made in a webmail.
- **Zip download of all attachments, an offline mode, server-side rendering, the list-above-preview layout, browser
  pop-up notifications, sending read receipts, translations.**
- **Roundcube's server spell check** (pspell, with dictionaries in the image and an editor that marks words): the
  browser's own checks Subject and the text (Andrew, Oct 25, 2026).
- **TNEF unpacking** (Oct 25, 2026). Outlook's `winmail.dat` (`application/ms-tnef`) wraps a mail's body, in HTML
  or RTF, and its files; Roundcube's `tnef_decode` unpacks them into the reader and its list. None of the 28,906
  messages in Andrew's mail carries a TNEF part, and the unpacking was 408 lines of Rust, so kubicle lists a
  `winmail.dat` as one file, which downloads and forwards as it is. The removal is kept as
  `~/.local/state/kubicle-work/patches/tnef/tnef.patch`, with a README beside it, on palanok, to bring it back.
- **Unit tests.** See Testing.
- **Mail another app flagged deleted** (Andrew, Oct 25, 2026). kubicle shows a message with `\Deleted` as an
  ordinary row: no grey row or ban mark, no undelete, no Deleted and Not deleted search types, no Compact in the
  folder menus. Mailu users delete through kubicle, which expunges at once, so the cluster served only mail
  another client marked and left; any client can expunge it. The removal is kept as a patch with a README in
  `~/.local/state/kubicle-work/patches/deleted-flag/` on palanok, to bring it back.
- **A fallback charset for unlabelled 8-bit text, for now** (Andrew, Oct 25, 2026). A part or header that names no
  charset decodes as UTF-8, a byte that is not as U+FFFD; RFC 2045's own default is US-ASCII. Roundcube reads all
  of it as its `default_charset`, ISO-8859-1, which garbles unlabelled UTF-8 and Cyrillic. The option kept for
  later: UTF-8 where the bytes are valid UTF-8, else Windows-1252 (what browsers read an ISO-8859-1 label as), and
  an unlabelled HTML part's `<meta charset>` first, as Roundcube looks for it. In `render::parse`: re-decode such
  a part from `offset_body..offset_end` with mail-parser's base64 and quoted-printable decoders; for headers,
  re-decode the raw header block before parsing it, for display only, never for a download or a forward.
  mail-parser already reads a `us-ascii` label as cp1252.
- **Special-folder roles written to the server** (IMAP METADATA `/private/specialuse`). Roundcube's
  `rcube_imap::set_special_folders` writes each role there on a Special Folders save, but only when the server
  has both SPECIAL-USE and METADATA. Mailu's Dovecot leaves `imap_metadata` off, so Roundcube under Mailu never
  writes them either, and kubicle keeps the role choices in its settings.

## To verify while building

- Front's certificate name on ports 10143 and 10025 in the owner's setup, for kubicle's TLS verification.
- That front keeps the webmail locations with kubicle in the `webmail` slot and `WEBMAIL` unchanged.
- Mailu's live IMAP capabilities (CONDSTORE, SORT, THREAD=REFS, IDLE) through front's Dovecot proxy.
