use anyhow::{Result, bail};

use crate::connection::Connection;

impl Connection {
    pub fn migrate(
        &self,
        _domain: &'static str,
        _migrations: &[&'static str],
        _should_allow_migration_change: &mut dyn FnMut(usize, &str, &str) -> bool,
    ) -> Result<()> {
        bail!("SQLite is not supported on wasm")
    }
}
