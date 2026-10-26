//! One SQLite connection behind a mutex, shared by the two stores kubicle keeps on disk (`settings.rs` and
//! `uploads.rs`). Here, not in either store, so the next reader finds the shared handle by its own name.

use std::{sync::Mutex, sync::PoisonError};

use rusqlite::Connection;

use crate::Error;

/// One SQLite connection. Calls run on the calling worker thread inside `block_in_place`, which lets the runtime
/// move other tasks off it meanwhile; the wait for the lock too, else enough waiting calls held every worker.
pub(crate) struct Db(Mutex<Connection>);

impl Db {
    pub(crate) fn new(db: Connection) -> Self { Self(Mutex::new(db)) }

    pub(crate) fn call<T, E>(&self, call: impl FnOnce(&mut Connection) -> Result<T, E>) -> Result<T, Error>
    where Error: From<E> {
        // A panicked call leaves no partial state: its transaction rolled back as it unwound.
        Ok(tokio::task::block_in_place(|| call(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner)))?)
    }
}
