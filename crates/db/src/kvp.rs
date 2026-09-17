use anyhow::Context as _;
use gpui::App;
use sqlez_macros::sql;
use util::ResultExt as _;

use crate::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    write_and_log,
};

pub struct KeyValueStore(crate::sqlez::thread_safe_connection::ThreadSafeConnection);

impl KeyValueStore {
    pub fn from_app_db(db: &crate::AppDatabase) -> Self {
        Self(db.0.clone())
    }
}

impl Domain for KeyValueStore {
    const NAME: &str = stringify!(KeyValueStore);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS kv_store(
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            ) STRICT;
        ),
        sql!(
            CREATE TABLE IF NOT EXISTS scoped_kv_store(
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY(namespace, key)
            ) STRICT;
        ),
    ];
}

crate::static_connection!(KeyValueStore, []);

pub trait Dismissable {
    const KEY: &'static str;

    fn dismissed(cx: &App) -> bool {
        KeyValueStore::global(cx)
            .read_kvp(Self::KEY)
            .log_err()
            .is_some_and(|s| s.is_some())
    }

    fn set_dismissed(is_dismissed: bool, cx: &mut App) {
        let db = KeyValueStore::global(cx);
        write_and_log(cx, move || async move {
            if is_dismissed {
                db.write_kvp(Self::KEY.into(), "1".into()).await
            } else {
                db.delete_kvp(Self::KEY.into()).await
            }
        })
    }
}

impl KeyValueStore {
    #[cfg(not(target_family = "wasm"))]
    query! {
        pub fn read_kvp(key: &str) -> Result<Option<String>> {
            SELECT value FROM kv_store WHERE key = (?)
        }
    }

    /// Served from `sqlez::kvp_cache`, which `db::prepare_web_key_value_cache` fills from
    /// the server before the window opens. This call cannot await and there is no local
    /// sqlite behind it; an unloaded cache reports that rather than an empty store.
    ///
    /// On wasm every `query!` routes to the one server database, so this store and
    /// `GlobalKeyValueStore` already share a `kv_store` table, and therefore a cache.
    #[cfg(target_family = "wasm")]
    pub fn read_kvp(&self, key: &str) -> anyhow::Result<Option<String>> {
        crate::sqlez::kvp_cache::global().read(key)
    }

    pub async fn write_kvp(&self, key: String, value: String) -> anyhow::Result<()> {
        log::debug!("Writing key-value pair for key {key}");

        #[cfg(target_family = "wasm")]
        {
            self.write_kvp_inner(key.clone(), value.clone()).await?;
            return crate::sqlez::kvp_cache::global().write(key, value);
        }

        #[cfg(not(target_family = "wasm"))]
        self.write_kvp_inner(key, value).await
    }

    query! {
        async fn write_kvp_inner(key: String, value: String) -> Result<()> {
            INSERT OR REPLACE INTO kv_store(key, value) VALUES ((?), (?))
        }
    }

    pub async fn delete_kvp(&self, key: String) -> anyhow::Result<()> {
        #[cfg(target_family = "wasm")]
        {
            self.delete_kvp_inner(key.clone()).await?;
            return crate::sqlez::kvp_cache::global().delete(&key);
        }

        #[cfg(not(target_family = "wasm"))]
        self.delete_kvp_inner(key).await
    }

    query! {
        async fn delete_kvp_inner(key: String) -> Result<()> {
            DELETE FROM kv_store WHERE key = (?)
        }
    }

    pub fn scoped<'a>(&'a self, namespace: &'a str) -> ScopedKeyValueStore<'a> {
        ScopedKeyValueStore {
            store: self,
            namespace,
        }
    }
}

pub struct ScopedKeyValueStore<'a> {
    // Unread on wasm, where every method goes to the server instead of a local connection.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    store: &'a KeyValueStore,
    namespace: &'a str,
}

#[cfg(not(target_family = "wasm"))]
const SCOPED_READ_SQL: &str =
    "SELECT value FROM scoped_kv_store WHERE namespace = (?) AND key = (?)";
const SCOPED_WRITE_SQL: &str =
    "INSERT OR REPLACE INTO scoped_kv_store(namespace, key, value) VALUES ((?), (?), (?))";
const SCOPED_DELETE_SQL: &str = "DELETE FROM scoped_kv_store WHERE namespace = (?) AND key = (?)";
const SCOPED_DELETE_ALL_SQL: &str = "DELETE FROM scoped_kv_store WHERE namespace = (?)";

impl ScopedKeyValueStore<'_> {
    /// See `KeyValueStore::read_kvp`: synchronous, so on wasm it reads the cache that
    /// `db::prepare_web_key_value_cache` filled from `scoped_kv_store`.
    pub fn read(&self, key: &str) -> anyhow::Result<Option<String>> {
        #[cfg(target_family = "wasm")]
        {
            return crate::sqlez::kvp_cache::global().read_scoped(self.namespace, key);
        }

        #[cfg(not(target_family = "wasm"))]
        {
            self.store
                .select_row_bound::<(&str, &str), String>(SCOPED_READ_SQL)?((
                self.namespace,
                key,
            ))
            .context("Failed to read from scoped_kv_store")
        }
    }

    pub async fn write(&self, key: String, value: String) -> anyhow::Result<()> {
        let namespace = self.namespace.to_owned();

        #[cfg(target_family = "wasm")]
        {
            crate::sqlez::remote_sql::exec_bound(
                SCOPED_WRITE_SQL,
                (namespace.as_str(), key.as_str(), value.as_str()),
            )
            .await
            .context("Failed to write to scoped_kv_store")?;
            return crate::sqlez::kvp_cache::global().write_scoped(namespace, key, value);
        }

        #[cfg(not(target_family = "wasm"))]
        self.store
            .write(move |connection| {
                connection.exec_bound::<(&str, &str, &str)>(SCOPED_WRITE_SQL)?((
                    &namespace, &key, &value,
                ))
                .context("Failed to write to scoped_kv_store")
            })
            .await
    }

    pub async fn delete(&self, key: String) -> anyhow::Result<()> {
        let namespace = self.namespace.to_owned();

        #[cfg(target_family = "wasm")]
        {
            crate::sqlez::remote_sql::exec_bound(
                SCOPED_DELETE_SQL,
                (namespace.as_str(), key.as_str()),
            )
            .await
            .context("Failed to delete from scoped_kv_store")?;
            return crate::sqlez::kvp_cache::global().delete_scoped(&namespace, &key);
        }

        #[cfg(not(target_family = "wasm"))]
        self.store
            .write(move |connection| {
                connection.exec_bound::<(&str, &str)>(SCOPED_DELETE_SQL)?((&namespace, &key))
                    .context("Failed to delete from scoped_kv_store")
            })
            .await
    }

    pub async fn delete_all(&self) -> anyhow::Result<()> {
        let namespace = self.namespace.to_owned();

        #[cfg(target_family = "wasm")]
        {
            crate::sqlez::remote_sql::exec_bound(SCOPED_DELETE_ALL_SQL, namespace.as_str())
                .await
                .context("Failed to delete_all from scoped_kv_store")?;
            return crate::sqlez::kvp_cache::global().delete_scoped_namespace(&namespace);
        }

        #[cfg(not(target_family = "wasm"))]
        self.store
            .write(move |connection| {
                connection.exec_bound::<&str>(SCOPED_DELETE_ALL_SQL)?(&namespace)
                    .context("Failed to delete_all from scoped_kv_store")
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::kvp::KeyValueStore;

    #[gpui::test]
    async fn test_kvp() {
        let db = KeyValueStore::open_test_db("test_kvp").await;

        assert_eq!(db.read_kvp("key-1").unwrap(), None);

        db.write_kvp("key-1".to_string(), "one".to_string())
            .await
            .unwrap();
        assert_eq!(db.read_kvp("key-1").unwrap(), Some("one".to_string()));

        db.write_kvp("key-1".to_string(), "one-2".to_string())
            .await
            .unwrap();
        assert_eq!(db.read_kvp("key-1").unwrap(), Some("one-2".to_string()));

        db.write_kvp("key-2".to_string(), "two".to_string())
            .await
            .unwrap();
        assert_eq!(db.read_kvp("key-2").unwrap(), Some("two".to_string()));

        db.delete_kvp("key-1".to_string()).await.unwrap();
        assert_eq!(db.read_kvp("key-1").unwrap(), None);
    }

    #[gpui::test]
    async fn test_scoped_kvp() {
        let db = KeyValueStore::open_test_db("test_scoped_kvp").await;

        let scope_a = db.scoped("namespace-a");
        let scope_b = db.scoped("namespace-b");

        // Reading a missing key returns None
        assert_eq!(scope_a.read("key-1").unwrap(), None);

        // Writing and reading back a key works
        scope_a
            .write("key-1".to_string(), "value-a1".to_string())
            .await
            .unwrap();
        assert_eq!(scope_a.read("key-1").unwrap(), Some("value-a1".to_string()));

        // Two namespaces with the same key don't collide
        scope_b
            .write("key-1".to_string(), "value-b1".to_string())
            .await
            .unwrap();
        assert_eq!(scope_a.read("key-1").unwrap(), Some("value-a1".to_string()));
        assert_eq!(scope_b.read("key-1").unwrap(), Some("value-b1".to_string()));

        // delete removes a single key without affecting others in the namespace
        scope_a
            .write("key-2".to_string(), "value-a2".to_string())
            .await
            .unwrap();
        scope_a.delete("key-1".to_string()).await.unwrap();
        assert_eq!(scope_a.read("key-1").unwrap(), None);
        assert_eq!(scope_a.read("key-2").unwrap(), Some("value-a2".to_string()));
        assert_eq!(scope_b.read("key-1").unwrap(), Some("value-b1".to_string()));

        // delete_all removes all keys in a namespace without affecting other namespaces
        scope_a
            .write("key-3".to_string(), "value-a3".to_string())
            .await
            .unwrap();
        scope_a.delete_all().await.unwrap();
        assert_eq!(scope_a.read("key-2").unwrap(), None);
        assert_eq!(scope_a.read("key-3").unwrap(), None);
        assert_eq!(scope_b.read("key-1").unwrap(), Some("value-b1".to_string()));
    }
}

pub struct GlobalKeyValueStore(ThreadSafeConnection);

impl Domain for GlobalKeyValueStore {
    const NAME: &str = stringify!(GlobalKeyValueStore);
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS kv_store(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;
    )];
}

impl std::ops::Deref for GlobalKeyValueStore {
    type Target = ThreadSafeConnection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(not(target_family = "wasm"))]
static GLOBAL_KEY_VALUE_STORE: std::sync::LazyLock<GlobalKeyValueStore> =
    std::sync::LazyLock::new(|| {
        let db_dir = crate::database_dir();
        GlobalKeyValueStore(gpui::block_on(crate::open_db::<GlobalKeyValueStore>(
            db_dir,
            crate::GlobalDbScope,
        )))
    });

/// The wasm store, built without blocking.
///
/// Opening a database on wasm does not touch a local one: `ThreadSafeConnection::build`
/// returns immediately there, because the schema lives on the server and
/// `db::prepare_web_database` applies it through `Sql::migrate`. So the future completes
/// on its first poll and `now_or_never` is sound here for the same reason it is in
/// `fuzzy::match_strings_blocking` -- the wasm path cannot suspend.
///
/// Before this, `global()` panicked on wasm telling the caller to build a store from
/// `AppDatabase` instead. Nothing did, and the panic fired during startup, taking the
/// window with it -- the first thing a browser actually running this build hit.
#[cfg(target_family = "wasm")]
static GLOBAL_KEY_VALUE_STORE: std::sync::LazyLock<GlobalKeyValueStore> =
    std::sync::LazyLock::new(|| {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        // Polled by hand rather than with `futures`, which this crate does not depend on,
        // and in the same shape `fuzzy::match_strings_blocking` uses for the same reason.
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut future = std::pin::pin!(crate::open_in_memory_db::<GlobalKeyValueStore>(
            crate::FALLBACK_DB_NAME
        ));
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(connection) => GlobalKeyValueStore(connection),
            Poll::Pending => {
                unreachable!("the wasm open path returns without awaiting and cannot suspend")
            }
        }
    });

impl GlobalKeyValueStore {
    pub fn global() -> &'static Self {
        &GLOBAL_KEY_VALUE_STORE
    }

    #[cfg(not(target_family = "wasm"))]
    query! {
        pub fn read_kvp(key: &str) -> Result<Option<String>> {
            SELECT value FROM kv_store WHERE key = (?)
        }
    }

    /// See `KeyValueStore::read_kvp`, whose cache this shares: on wasm both stores
    /// resolve to the same server `kv_store` table.
    #[cfg(target_family = "wasm")]
    pub fn read_kvp(&self, key: &str) -> anyhow::Result<Option<String>> {
        crate::sqlez::kvp_cache::global().read(key)
    }

    pub async fn write_kvp(&self, key: String, value: String) -> anyhow::Result<()> {
        log::debug!("Writing global key-value pair for key {key}");

        #[cfg(target_family = "wasm")]
        {
            self.write_kvp_inner(key.clone(), value.clone()).await?;
            return crate::sqlez::kvp_cache::global().write(key, value);
        }

        #[cfg(not(target_family = "wasm"))]
        self.write_kvp_inner(key, value).await
    }

    query! {
        async fn write_kvp_inner(key: String, value: String) -> Result<()> {
            INSERT OR REPLACE INTO kv_store(key, value) VALUES ((?), (?))
        }
    }

    pub async fn delete_kvp(&self, key: String) -> anyhow::Result<()> {
        #[cfg(target_family = "wasm")]
        {
            self.delete_kvp_inner(key.clone()).await?;
            return crate::sqlez::kvp_cache::global().delete(&key);
        }

        #[cfg(not(target_family = "wasm"))]
        self.delete_kvp_inner(key).await
    }

    query! {
        async fn delete_kvp_inner(key: String) -> Result<()> {
            DELETE FROM kv_store WHERE key = (?)
        }
    }
}
