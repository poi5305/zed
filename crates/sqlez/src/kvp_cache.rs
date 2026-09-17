//! In-memory mirror of the server's `kv_store` and `scoped_kv_store` tables.
//!
//! `db::kvp::KeyValueStore::read_kvp` and `db::kvp::ScopedKeyValueStore::read` are
//! synchronous: they are called from `Dismissable::dismissed(&App) -> bool`, from
//! `Panel::load`, and from render paths, none of which can await. On wasm there is no
//! local sqlite for them to read, so they fail outright and every panel that restores
//! its state from a key-value pair comes up empty.
//!
//! The server already answers `Sql::bootstrap_kvp` with both tables in a single round
//! trip. The web entry point loads that answer into this cache before the window opens,
//! and the synchronous reads are served from it.
//!
//! A cache that has not been loaded says so instead of reporting an empty store. The two
//! are different answers and callers cannot tell them apart otherwise: `Dismissable`
//! reads a missing key as "not dismissed" and would re-show every dismissed notice.

use anyhow::{Result, bail};
use collections::HashMap;
use parking_lot::RwLock;
use std::sync::LazyLock;

#[derive(Default)]
struct CacheContents {
    unscoped: HashMap<String, String>,
    scoped: HashMap<(String, String), String>,
}

/// A loaded-once mirror of the server's key-value tables.
pub struct KeyValueCache {
    contents: RwLock<Option<CacheContents>>,
}

static GLOBAL_CACHE: LazyLock<KeyValueCache> = LazyLock::new(KeyValueCache::new);

/// The cache backing the synchronous `db::kvp` reads on wasm.
pub fn global() -> &'static KeyValueCache {
    &GLOBAL_CACHE
}

fn not_loaded<T>(operation: &str) -> Result<T> {
    bail!(
        "SQLite is not supported on wasm and the web key-value cache has not been loaded, \
         so {operation} has no answer; call db::prepare_web_key_value_cache (Sql::bootstrap_kvp) \
         during startup"
    )
}

impl Default for KeyValueCache {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyValueCache {
    pub fn new() -> Self {
        Self {
            contents: RwLock::new(None),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.contents.read().is_some()
    }

    /// Replaces the cache with the rows the server just handed over.
    pub fn load(&self, unscoped: Vec<(String, String)>, scoped: Vec<(String, String, String)>) {
        *self.contents.write() = Some(CacheContents {
            unscoped: unscoped.into_iter().collect(),
            scoped: scoped
                .into_iter()
                .map(|(namespace, key, value)| ((namespace, key), value))
                .collect(),
        });
    }

    pub fn read(&self, key: &str) -> Result<Option<String>> {
        let contents = self.contents.read();
        let Some(contents) = contents.as_ref() else {
            return not_loaded(&format!("read_kvp({key:?})"));
        };
        Ok(contents.unscoped.get(key).cloned())
    }

    pub fn write(&self, key: String, value: String) -> Result<()> {
        let mut contents = self.contents.write();
        let Some(contents) = contents.as_mut() else {
            return not_loaded(&format!("write_kvp({key:?})"));
        };
        contents.unscoped.insert(key, value);
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        let mut contents = self.contents.write();
        let Some(contents) = contents.as_mut() else {
            return not_loaded(&format!("delete_kvp({key:?})"));
        };
        contents.unscoped.remove(key);
        Ok(())
    }

    pub fn read_scoped(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        let contents = self.contents.read();
        let Some(contents) = contents.as_ref() else {
            return not_loaded(&format!("scoped({namespace:?}).read({key:?})"));
        };
        Ok(contents
            .scoped
            .get(&(namespace.to_string(), key.to_string()))
            .cloned())
    }

    pub fn write_scoped(&self, namespace: String, key: String, value: String) -> Result<()> {
        let mut contents = self.contents.write();
        let Some(contents) = contents.as_mut() else {
            return not_loaded(&format!("scoped({namespace:?}).write({key:?})"));
        };
        contents.scoped.insert((namespace, key), value);
        Ok(())
    }

    pub fn delete_scoped(&self, namespace: &str, key: &str) -> Result<()> {
        let mut contents = self.contents.write();
        let Some(contents) = contents.as_mut() else {
            return not_loaded(&format!("scoped({namespace:?}).delete({key:?})"));
        };
        contents
            .scoped
            .remove(&(namespace.to_string(), key.to_string()));
        Ok(())
    }

    pub fn delete_scoped_namespace(&self, namespace: &str) -> Result<()> {
        let mut contents = self.contents.write();
        let Some(contents) = contents.as_mut() else {
            return not_loaded(&format!("scoped({namespace:?}).delete_all()"));
        };
        contents
            .scoped
            .retain(|(entry_namespace, _), _| entry_namespace != namespace);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::KeyValueCache;

    fn loaded_cache() -> KeyValueCache {
        let cache = KeyValueCache::new();
        cache.load(
            vec![
                ("theme".to_string(), "One Dark".to_string()),
                ("session_id".to_string(), "abc123".to_string()),
                ("empty".to_string(), String::new()),
            ],
            vec![
                (
                    "dock_panel_size".to_string(),
                    "ProjectPanel".to_string(),
                    "{\"size\":240.0}".to_string(),
                ),
                (
                    "dock_panel_size".to_string(),
                    "TerminalPanel".to_string(),
                    "{\"size\":320.0}".to_string(),
                ),
                (
                    "pickers".to_string(),
                    "ProjectPanel".to_string(),
                    "picker-state".to_string(),
                ),
                (
                    "theme".to_string(),
                    "theme".to_string(),
                    "scoped-theme".to_string(),
                ),
            ],
        );
        cache
    }

    /// F1/F2: a synchronous `read_kvp` must return what the server holds.
    #[test]
    fn read_serves_the_value_loaded_from_the_server() {
        let cache = loaded_cache();
        assert!(cache.is_loaded(), "load() must mark the cache as loaded");
        assert_eq!(
            cache.read("theme").map_err(|error| error.to_string()),
            Ok(Some("One Dark".to_string())),
            "read_kvp(\"theme\") after loading the server's kv_store"
        );
        assert_eq!(
            cache.read("empty").map_err(|error| error.to_string()),
            Ok(Some(String::new())),
            "a stored empty string is a value, not a missing key"
        );
        assert_eq!(
            cache
                .read("never_written")
                .map_err(|error| error.to_string()),
            Ok(None),
            "a key the server does not hold reads as absent, not as an error"
        );
    }

    /// F3: the scoped read that `workspace::dock` uses once per panel.
    #[test]
    fn read_scoped_serves_the_namespaced_value() {
        let cache = loaded_cache();
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(Some("{\"size\":240.0}".to_string())),
            "scoped(\"dock_panel_size\").read(\"ProjectPanel\")"
        );
        assert_eq!(
            cache
                .read_scoped("pickers", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(Some("picker-state".to_string())),
            "the same key in a second namespace must not collide"
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "GitPanel")
                .map_err(|error| error.to_string()),
            Ok(None),
            "a panel with no stored size reads as absent"
        );
    }

    /// The new boundary must not swallow a legal input: a scoped entry and an
    /// unscoped entry that happen to share a name are separate rows on the server
    /// and must stay separate here.
    #[test]
    fn scoped_and_unscoped_namespaces_do_not_collide() {
        let cache = loaded_cache();
        assert_eq!(
            cache.read("theme").map_err(|error| error.to_string()),
            Ok(Some("One Dark".to_string())),
            "unscoped kv_store entry named \"theme\""
        );
        assert_eq!(
            cache
                .read_scoped("theme", "theme")
                .map_err(|error| error.to_string()),
            Ok(Some("scoped-theme".to_string())),
            "scoped_kv_store entry with namespace \"theme\" and key \"theme\""
        );
    }

    /// F4: a write must be visible to the very next synchronous read, otherwise a
    /// panel that persists and then re-reads its state sees the pre-write value.
    #[test]
    fn write_is_visible_to_the_next_read() {
        let cache = loaded_cache();
        assert_eq!(
            cache
                .write("theme".to_string(), "One Light".to_string())
                .map_err(|error| error.to_string()),
            Ok(())
        );
        assert_eq!(
            cache.read("theme").map_err(|error| error.to_string()),
            Ok(Some("One Light".to_string())),
            "read_kvp after write_kvp"
        );

        assert_eq!(
            cache
                .write_scoped(
                    "dock_panel_size".to_string(),
                    "ProjectPanel".to_string(),
                    "{\"size\":480.0}".to_string(),
                )
                .map_err(|error| error.to_string()),
            Ok(())
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(Some("{\"size\":480.0}".to_string())),
            "scoped read after scoped write"
        );
    }

    /// F5: a delete must be visible to the very next synchronous read.
    #[test]
    fn delete_removes_the_key_from_the_next_read() {
        let cache = loaded_cache();
        assert_eq!(
            cache.delete("theme").map_err(|error| error.to_string()),
            Ok(())
        );
        assert_eq!(
            cache.read("theme").map_err(|error| error.to_string()),
            Ok(None),
            "read_kvp after delete_kvp"
        );
        assert_eq!(
            cache.read("session_id").map_err(|error| error.to_string()),
            Ok(Some("abc123".to_string())),
            "deleting one key must leave the others alone"
        );

        assert_eq!(
            cache
                .delete_scoped("dock_panel_size", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(())
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(None),
            "scoped read after scoped delete"
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "TerminalPanel")
                .map_err(|error| error.to_string()),
            Ok(Some("{\"size\":320.0}".to_string())),
            "deleting one scoped key must leave its namespace siblings alone"
        );
    }

    /// F6: `delete_all` clears exactly one namespace.
    #[test]
    fn delete_namespace_clears_only_that_namespace() {
        let cache = loaded_cache();
        assert_eq!(
            cache
                .delete_scoped_namespace("dock_panel_size")
                .map_err(|error| error.to_string()),
            Ok(())
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(None)
        );
        assert_eq!(
            cache
                .read_scoped("dock_panel_size", "TerminalPanel")
                .map_err(|error| error.to_string()),
            Ok(None)
        );
        assert_eq!(
            cache
                .read_scoped("pickers", "ProjectPanel")
                .map_err(|error| error.to_string()),
            Ok(Some("picker-state".to_string())),
            "delete_all on one namespace must leave other namespaces intact"
        );
        assert_eq!(
            cache.read("theme").map_err(|error| error.to_string()),
            Ok(Some("One Dark".to_string())),
            "delete_all on a namespace must not touch the unscoped store"
        );
    }

    /// The honesty guard: before the cache is loaded, a read must fail and name the
    /// loader, never report an empty store. Green before and after the fix.
    #[test]
    fn reads_before_load_refuse_instead_of_reporting_an_empty_store() {
        let cache = KeyValueCache::new();
        assert!(!cache.is_loaded());

        let read = cache.read("theme");
        let message = read
            .as_ref()
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| format!("unexpectedly succeeded with {:?}", read.as_ref().ok()));
        assert!(
            message.contains("prepare_web_key_value_cache"),
            "read before load must name the loader, got: {message}"
        );

        let scoped = cache.read_scoped("dock_panel_size", "ProjectPanel");
        let scoped_message = scoped
            .as_ref()
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| format!("unexpectedly succeeded with {:?}", scoped.as_ref().ok()));
        assert!(
            scoped_message.contains("prepare_web_key_value_cache"),
            "scoped read before load must name the loader, got: {scoped_message}"
        );

        assert!(
            cache
                .write("theme".to_string(), "One Light".to_string())
                .is_err(),
            "a write before load must be refused; accepting it would be silently dropped by the \
             load that follows"
        );
    }
}
