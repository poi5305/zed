use std::{cell::RefCell, path::Path};

use anyhow::{Result, bail};

pub struct Connection {
    persistent: bool,
    pub(crate) write: RefCell<bool>,
}

unsafe impl Send for Connection {}

fn sqlite_unsupported<T>() -> Result<T> {
    bail!("SQLite is not supported on wasm")
}

impl Connection {
    pub(crate) fn open(_uri: &str, persistent: bool) -> Result<Self> {
        Ok(Self {
            persistent,
            write: RefCell::new(true),
        })
    }

    /// Handle only. There is no local sqlite; SQL operations fail with
    /// `SQLite is not supported on wasm`.
    pub fn open_file(uri: &str) -> Self {
        Self::open(uri, true).expect("wasm Connection handle")
    }

    /// Handle only. There is no local sqlite; SQL operations fail with
    /// `SQLite is not supported on wasm`.
    pub fn open_memory(uri: Option<&str>) -> Self {
        Self::open(uri.unwrap_or(":memory:"), false).expect("wasm Connection handle")
    }

    pub fn persistent(&self) -> bool {
        self.persistent
    }

    pub fn can_write(&self) -> bool {
        *self.write.borrow()
    }

    pub fn backup_main(&self, _destination: &Connection) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn backup_main_to(&self, _destination: impl AsRef<Path>) -> Result<()> {
        sqlite_unsupported()
    }

    pub fn sql_has_syntax_error(&self, _sql: &str) -> Option<(String, usize)> {
        Some(("SQLite is not supported on wasm".into(), 0))
    }

    pub(crate) fn last_error(&self) -> Result<()> {
        sqlite_unsupported()
    }

    pub(crate) fn with_write<T>(&self, callback: impl FnOnce(&Connection) -> T) -> T {
        *self.write.borrow_mut() = true;
        let result = callback(self);
        *self.write.borrow_mut() = false;
        result
    }
}
