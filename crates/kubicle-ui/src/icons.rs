//! Line icons on a 24-unit grid, drawn with the current text colour (`.icon` in base.css sets the stroke).

use kubicle_api::{Folder, FolderRole};
use leptos::prelude::*;

pub const INBOX: &str = "M3 13h5l1.5 3h5L16 13h5M3 13l2.5-8h13L21 13v6H3z";
pub const DRAFTS: &str = "M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4";
pub const SENT: &str = "M21 3L10 14M21 3l-7 18-4-7-7-4z";
pub const JUNK: &str = "M12 3l8 3v6c0 4.5-3.4 8-8 9-4.6-1-8-4.5-8-9V6zM9.5 9.5l5 5M14.5 9.5l-5 5";
pub const TRASH: &str = "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6";
pub const TRASH_EMPTY: &str = "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13";
pub const ARCHIVE: &str = "M3 4h18v4H3zM5 8v12h14V8M10 12h4";
pub const FOLDER: &str = "M3 6h6l2 2h10v11H3z";
pub const REPLY: &str = "M9 7l-5 5 5 5M4 12h11a5 5 0 0 1 5 5v2";
pub const REPLY_ALL: &str = "M12 7l-5 5 5 5M7 7l-5 5 5 5M7 12h8a5 5 0 0 1 5 5v2";
pub const FORWARD: &str = "M15 7l5 5-5 5M20 12H9a5 5 0 0 0-5 5v2";
pub const FLAG: &str = "M5 21V4M5 4h12l-2.5 4 2.5 4H5";
pub const MAIL: &str = "M3 6h18v12H3zM3 6l9 7 9-7";
pub const MAIL_OPEN: &str = "M3 10l9-6 9 6v10H3zM3 10l9 6 9-6";
pub const UNFLAG: &str = "M5 21V4M5 4h12l-2.5 4 2.5 4H5M3 3l18 18";
pub const TAG: &str = "M3 4h8l10 10-7 7L4 11zM7.5 7.5h.01";
pub const UPLOAD: &str = "M12 16V5M7 10l5-5 5 5M5 20h14";
pub const CARET: &str = "M8 10l4 4 4-4";
pub const MOVE: &str = "M3 6h6l2 2h10v11H3zM9 13.5h7M13.5 11l2.5 2.5-2.5 2.5";
pub const PRINT: &str = "M7 9V3h10v6M7 17H4V9h16v8h-3M7 14h10v7H7z";
pub const SOURCE: &str = "M8 8l-4 4 4 4M16 8l4 4-4 4M13.5 5l-3 14";
pub const MORE: &str = "M6.2 12a1.2 1.2 0 1 1-2.4 0 1.2 1.2 0 1 1 2.4 0M13.2 12a1.2 1.2 0 1 1-2.4 0 1.2 1.2 0 1 1 2.4 0M20.2 12a1.2 1.2 0 1 1-2.4 0 1.2 1.2 0 1 1 2.4 0";
pub const COMPOSE: &str = "M12 4H5v15h15v-7M18 3l3 3-8 8h-3v-3z";
pub const SEARCH: &str = "M10.5 4a6.5 6.5 0 1 0 0 13 6.5 6.5 0 1 0 0-13M15.5 15.5L20 20";
pub const SETTINGS: &str = "M4 7h9M17 7h3M15 5v4M4 17h3M11 17h9M9 15v4";
pub const LOGOUT: &str = "M10 4H5v16h5M14 8l4 4-4 4M18 12H9";
pub const INFO: &str = "M12 3a9 9 0 1 0 0 18 9 9 0 1 0 0-18M12 11v5M12 8h.01";
pub const HELP: &str = "M12 3a9 9 0 1 0 0 18 9 9 0 1 0 0-18M9.5 9.5a2.5 2.5 0 1 1 3.5 2.3c-.6.3-1 .9-1 1.6v.6M12 17h.01";
pub const BACK: &str = "M15 5l-7 7 7 7";
pub const MENU: &str = "M4 7h16M4 12h16M4 17h16";
pub const CLOSE: &str = "M6 6l12 12M18 6L6 18";
pub const ATTACH: &str = "M20 11.5l-8 8a5 5 0 0 1-7-7l8.5-8.5a3.3 3.3 0 0 1 4.7 4.7L9.7 17a1.7 1.7 0 0 1-2.4-2.4L15 7";
pub const CHEVRON: &str = "M9 6l6 6-6 6";
pub const CHEVRONS: &str = "M6 6l6 6-6 6M12 6l6 6-6 6";
pub const FILTER: &str = "M4 5h16l-6 7.5V18l-4 2v-7.5z";
pub const OPTIONS: &str = "M4 6h16M7 12h10M10 18h4";
pub const IMAGE: &str = "M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4M15 8.5h.01";
pub const DOWNLOAD: &str = "M12 4v11M7 10l5 5 5-5M5 20h14";
pub const PLUS: &str = "M12 5v14M5 12h14";
pub const MORE_VERTICAL: &str = "M12 3.8a1.2 1.2 0 1 1 0 2.4 1.2 1.2 0 1 1 0-2.4M12 10.8a1.2 1.2 0 1 1 0 2.4 1.2 1.2 0 1 1 0-2.4M12 17.8a1.2 1.2 0 1 1 0 2.4 1.2 1.2 0 1 1 0-2.4";
pub const REPORT: &str = "M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 16h6";
pub const LOCK: &str = "M6 11h12v9H6zM8.5 11V8a3.5 3.5 0 0 1 7 0v3";
pub const MOON: &str = "M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z";
pub const SUN: &str =
    "M12 8a4 4 0 1 0 0 8 4 4 0 1 0 0-8M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4";
pub const CHECK: &str = "M5 12.5l4.5 4.5L19 7.5";
pub const OPEN: &str = "M14 4h6v6M20 4l-9 9M18 14v6H4V6h6";
pub const COPY: &str = "M9 9h11v11H9zM5 15H4V4h11v1";
pub const SELECT: &str = "M4 4h16v16H4zM8 12l3 3 5-6";
pub const THREADS: &str = "M4 6h16M10 12h10M10 18h10M6 6v12h2M6 12h2";
pub const ASTERISK: &str = "M12 4v16M5 8l14 8M19 8L5 16";
pub const DISK: &str = "M4 13h16v6H4zM4 13l3-8h10l3 8M8 16h.01";

#[must_use]
pub fn icon(d: &'static str) -> impl IntoView {
    view! { <svg class="icon" viewBox="0 0 24 24" aria-hidden="true"><path d=d /></svg> }
}

/// A folder's icon by Elastic's rules: Archive's for any folder holding that role, as its rule comes last, else the
/// icon of the role it is named by.
#[must_use]
pub fn folder(f: &Folder) -> &'static str { role(if f.roles.contains(&FolderRole::Archive) { FolderRole::Archive } else { f.role() }) }

fn role(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => INBOX,
        FolderRole::Drafts => DRAFTS,
        FolderRole::Sent => SENT,
        FolderRole::Junk => JUNK,
        FolderRole::Trash => TRASH,
        FolderRole::Archive => ARCHIVE,
        FolderRole::Other => FOLDER,
    }
}
