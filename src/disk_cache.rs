use rusqlite::{params, Connection};
use serde::de::DeserializeOwned;
use std::{
    fs,
    sync::{Arc, Mutex},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiskCacheError {
    #[error("Sqlite error {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Bincode error {0}")]
    Bincode(#[from] bincode::Error),
    #[error("IO error {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct DiskCache<K, V> {
    conn: Arc<Mutex<Connection>>,
    _k: std::marker::PhantomData<K>,
    _v: std::marker::PhantomData<V>,
}

impl<K: serde::ser::Serialize, V: serde::ser::Serialize + DeserializeOwned> DiskCache<K, V> {
    pub fn create(name: &str) -> Result<Self, DiskCacheError> {
        let cache_dir = dirs::cache_dir().unwrap_or("./cache".into());
        let dir = cache_dir.join(env!("CARGO_PKG_NAME"));
        fs::create_dir_all(dir.clone())?;
        let db_path = dir.join(format!("{}.db", name));
        println!("Using cache at {:?}", db_path);

        let conn = Connection::open(db_path)?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS cache (
                key TEXT PRIMARY KEY,
                value BLOB NOT NULL,
                expiry INTEGER NOT NULL
            )",
            [],
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            _k: std::marker::PhantomData,
            _v: std::marker::PhantomData,
        })
    }

    pub fn set(&self, key: K, value: V, expires_in_seconds: u64) -> Result<(), DiskCacheError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cache (key, value, expiry) VALUES (?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value, expiry = excluded.expiry",
            params![
                bincode::serialize(&key)?,
                bincode::serialize(&value)?,
                now + expires_in_seconds,
            ],
        )?;

        Ok(())
    }

    pub fn get(&self, key: &K) -> Result<Option<V>, DiskCacheError> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn.prepare("SELECT value, expiry FROM cache WHERE key = ?")?;
        let mut rows = stmt.query(params![bincode::serialize(&key)?])?;

        if let Some(row) = rows.next().unwrap() {
            let value: Vec<u8> = row.get(0)?;
            let expiry: u64 = row.get(1)?;

            if expiry
                > std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            {
                return Ok(Some(bincode::deserialize(&value[..])?));
            } else {
                conn.execute(
                    "DELETE FROM cache WHERE key = ?",
                    params![bincode::serialize(&key)?],
                )?;
            }
        }

        Ok(None)
    }
}
