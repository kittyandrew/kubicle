//! The settings database: per-user preferences and user themes. The only state worth a backup
//! (docs/design.md, Deployment).

use std::path::Path;

use axum::http::StatusCode;
use kubicle_api::Settings;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, db::Db};

pub struct Store(Db);

/// The largest settings row stored, as JSON: every request that reads settings parses the row under the one lock.
/// Themes take most of it, 64 KiB each at most (`themes.rs`).
pub const MAX_SETTINGS: usize = 1 << 20;

const VALIDITY: &str = "SELECT inbox_validity FROM accounts WHERE user = ?1";

impl Store {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (user TEXT PRIMARY KEY, json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts (user TEXT PRIMARY KEY, inbox_validity INTEGER NOT NULL);",
        )?;
        Ok(Self(Db::new(db)))
    }

    /// A user's settings, or the defaults for someone who never changed any.
    pub fn settings(&self, user: &str) -> Result<Settings, Error> { self.0.call(|db| read(db, user)) }

    /// Ties the user's state to the account whose INBOX has this UIDVALIDITY (`Client::inbox_validity`): the first
    /// sign-in records it, and another one, an account made anew under the address, drops the settings stored before.
    /// The caller drops the rest of that account's state first.
    pub fn incarnation(&self, user: &str, validity: u32) -> Result<(), Error> {
        self.0.call(|db| {
            let tx = db.transaction()?;
            let stored: Option<u32> = tx.query_row(VALIDITY, [user], |r| r.get(0)).optional()?;
            let sql = "INSERT INTO accounts VALUES (?1, ?2) ON CONFLICT (user) DO UPDATE SET inbox_validity = excluded.inbox_validity";
            tx.execute(sql, params![user, validity])?;
            if stored.is_some_and(|v| v != validity) {
                tx.execute("DELETE FROM settings WHERE user = ?1", [user])?;
            }
            tx.commit()
        })
    }

    /// The INBOX UIDVALIDITY `incarnation` recorded for the user, 0 before the first sign-in.
    pub fn validity(&self, user: &str) -> Result<u32, Error> {
        self.0.call(|db| db.query_row(VALIDITY, [user], |r| r.get(0)).optional().map(Option::unwrap_or_default))
    }

    /// Stores `settings` at the next revision and returns them as stored. A copy read before the last save is
    /// refused (409): it would undo that save.
    pub fn save(&self, user: &str, settings: &Settings) -> Result<Settings, Error> {
        self.edit(user, |stored| {
            if stored.revision != settings.revision {
                return Err(Error::status(StatusCode::CONFLICT, "the settings were changed in another tab or device"));
            }
            *stored = settings.clone();
            Ok(true)
        })
        .map(|(settings, _)| settings)
    }

    /// Moves the preferences of a renamed folder and of every folder below it to the new names, as the IMAP
    /// RENAME moved the folders. `None` when no preference named one.
    pub fn rename_folder(&self, user: &str, from: &str, to: &str, delimiter: Option<char>) -> Result<Option<Settings>, Error> {
        let renamed = |name: &str| kubicle_api::within(name, from, delimiter).then(|| format!("{to}{}", &name[from.len()..]));
        let (settings, moved) = self.edit(user, |s| {
            let names: Vec<String> = s.folders.keys().filter(|n| renamed(n).is_some()).cloned().collect();
            let mut moved = !names.is_empty();
            for name in names {
                let prefs = s.folders.remove(&name).expect("listed just above");
                s.folders.insert(renamed(&name).expect("filtered above"), prefs);
            }
            for name in s.special_folders.values_mut().flatten() {
                if let Some(new) = renamed(name) {
                    *name = new;
                    moved = true;
                }
            }
            Ok(moved)
        })?;
        Ok(moved.then_some(settings))
    }

    /// Reads, changes and writes a user's settings under the one lock, so no other save lands in between; the
    /// revision goes up by one. A `change` that answers false changed nothing, and nothing is written.
    fn edit(&self, user: &str, change: impl FnOnce(&mut Settings) -> Result<bool, Error>) -> Result<(Settings, bool), Error> {
        self.0.call(|db| {
            let tx = db.transaction()?;
            let mut settings = read(&tx, user)?;
            let revision = settings.revision;
            if !change(&mut settings)? {
                return Ok((settings, false));
            }
            settings.revision = revision + 1;
            let json = serde_json::to_string(&settings).expect("Settings always serializes");
            if json.len() > MAX_SETTINGS {
                return Err(Error::status(StatusCode::PAYLOAD_TOO_LARGE, "the settings are over 1 MB: delete a theme"));
            }
            let sql = "INSERT INTO settings (user, json) VALUES (?1, ?2) ON CONFLICT (user) DO UPDATE SET json = excluded.json";
            tx.execute(sql, params![user, json])?;
            tx.commit()?;
            Ok((settings, true))
        })
    }
}

/// A user's stored settings, or the defaults; an error too for a stored copy that no longer parses.
fn read(db: &Connection, user: &str) -> Result<Settings, Error> {
    let json: Option<String> = db.query_row("SELECT json FROM settings WHERE user = ?1", [user], |row| row.get(0)).optional()?;
    let parsed = json.map_or_else(|| Ok(Settings::default()), |json| serde_json::from_str(&json));
    parsed.map_err(|e| Error::status(StatusCode::INTERNAL_SERVER_ERROR, format!("stored settings: {e}")))
}
