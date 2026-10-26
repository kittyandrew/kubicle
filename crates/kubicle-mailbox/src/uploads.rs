//! A user's uploads until Compose no longer needs them (docs/design.md, Drafts never lose text): one SQLite file per
//! user, so a request can reach no other user's files. The draft's copy in Drafts has them too.

use std::{path::Path, time::Duration};

use kubicle_api::DraftAttachment;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{Error, db::Db};

const MAX_UPLOADS: i64 = 200 << 20; // a user's unsent uploads together, so one user cannot fill the disk
/// A user's unsent uploads in number: empty files never reach `MAX_UPLOADS`, and each add sums every row.
const MAX_FILES: i64 = 1000;

pub struct Uploads(Db);

impl Uploads {
    /// Opens the user's file, `uploads-` and the address as sessions and settings key it, every byte outside
    /// `[a-z0-9@._+-]` percent-encoded, `%` too, so two addresses never share a file; and drops uploads a month old.
    pub fn open(data_dir: &Path, user: &str) -> Result<Self, Error> {
        let plain = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@._+-".contains(&b);
        let name: String = user.bytes().map(|b| if plain(b) { char::from(b).to_string() } else { format!("%{b:02x}") }).collect();
        let db = Connection::open(data_dir.join(format!("uploads-{name}.sqlite")))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS uploads (
                id TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                content_type TEXT NOT NULL,
                created INTEGER NOT NULL DEFAULT (unixepoch()),
                data BLOB NOT NULL -- last, or add's zeroblob turns into real zeros when SQLite builds the row
            );
            DELETE FROM uploads WHERE created < unixepoch() - 30 * 86400;",
        )?;
        Ok(Self(Db::new(db)))
    }

    /// Stores an upload; refused when the user's uploads would grow past `MAX_UPLOADS` or `MAX_FILES`.
    pub fn add(&self, attachment: &DraftAttachment, data: &[u8]) -> Result<bool, Error> {
        let size = i64::try_from(data.len()).unwrap_or(i64::MAX);
        self.0.call(|db| -> rusqlite::Result<bool> {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (used, files): (i64, i64) =
                tx.query_row("SELECT coalesce(sum(length(data)), 0), count(*) FROM uploads", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if used.saturating_add(size) > MAX_UPLOADS || files >= MAX_FILES {
                return Ok(false);
            }
            // Zeros first, the data then written into them in place: bound as a value, a 30 MB upload costs two more
            // copies in SQLite's memory (the value, then the row's record), and `INSERT ... SELECT` here a third.
            let sql = "INSERT INTO uploads (id, filename, content_type, data) VALUES (?1, ?2, ?3, zeroblob(?4))";
            tx.execute(sql, params![attachment.id, attachment.filename, attachment.content_type, size])?;
            tx.blob_open("main", "uploads", "data", tx.last_insert_rowid(), false)?.write_at(data, 0)?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// What each of `ids` adds to a message as sent (`encoded_size`), read without its data, in no order; an id not
    /// stored is left out.
    pub fn sizes(&self, ids: &[&str]) -> Result<Vec<(String, u64)>, Error> {
        let ids = serde_json::to_string(ids).expect("ids serialize");
        let sql = "SELECT id, content_type, length(data) FROM uploads WHERE id IN (SELECT value FROM json_each(?1))";
        self.0.call(|db| {
            let mut rows = db.prepare(sql)?;
            let sized = rows.query_map([ids], |r| {
                let (id, content_type, size): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
                Ok((id, kubicle_api::encoded_size(u64::try_from(size).unwrap_or_default(), &content_type)))
            })?;
            sized.collect()
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<(DraftAttachment, Vec<u8>)>, Error> {
        let sql = "SELECT filename, content_type, data FROM uploads WHERE id = ?1";
        self.0.call(|db| {
            db.query_row(sql, [id], |r| {
                let data: Vec<u8> = r.get(2)?;
                Ok((DraftAttachment { id: id.to_owned(), filename: r.get(0)?, content_type: r.get(1)?, size: data.len() as u64 }, data))
            })
            .optional()
        })
    }

    /// Drops every upload, for an account made anew under the address.
    pub fn clear(&self) -> Result<(), Error> { self.0.call(|db| db.execute("DELETE FROM uploads", []).map(drop)) }

    pub fn remove(&self, ids: &[&str]) -> Result<(), Error> {
        let ids = serde_json::to_string(ids).expect("ids serialize");
        self.0.call(|db| db.execute("DELETE FROM uploads WHERE id IN (SELECT value FROM json_each(?1))", [ids]).map(drop))
    }
}
