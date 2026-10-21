//! Settings: the app's configuration, as one JSON blob.
//!
//! Settings are small, read as a whole, and written as a whole — so they
//! live in a single JSON column rather than a table of key/value rows.
//! Unknown keys are preserved on write (a newer build's setting survives
//! an older build's round trip), which is what makes the blob safe to
//! extend without a migration.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value};

/// The settings blob, over a connection.
pub struct Settings<'a> {
    conn: &'a Connection,
}

impl<'a> Settings<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// The settings as a JSON object. Empty when nothing was ever
    /// written — the app then runs on its defaults.
    pub fn read(&self) -> Result<Map<String, Value>> {
        let row: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key = 'app'", [], |row| {
                row.get(0)
            })
            .optional()
            .context("couldn't read the settings")?;
        let Some(json) = row else {
            return Ok(Map::new());
        };
        // A blob that isn't an object (or isn't JSON at all) reads as
        // "no settings" rather than failing the app's startup.
        Ok(serde_json::from_str::<Value>(&json)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default())
    }

    /// Replace the settings with `settings`, keeping any key this build
    /// doesn't know about.
    ///
    /// A key this build *does* know about is replaced outright — including
    /// by a null, which is how a setting is turned off. A key this build
    /// doesn't know about is left alone, so an older build never silently
    /// drops a newer build's setting.
    pub fn write(&self, settings: &Map<String, Value>) -> Result<()> {
        let mut merged = self.read()?;
        for (key, value) in settings {
            merged.insert(key.clone(), value.clone());
        }
        self.write_all(&merged)
    }

    /// Write the settings blob exactly as given — no merge, no defaults.
    fn write_all(&self, settings: &Map<String, Value>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES ('app', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = ?1",
                [serde_json::to_string(settings).context("couldn't encode the settings")?],
            )
            .context("couldn't write the settings")?;
        Ok(())
    }

    /// One setting's value, when it is set.
    pub fn get(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.read()?.get(key).cloned())
    }

    /// Set one setting's value.
    pub fn set(&self, key: &str, value: Value) -> Result<()> {
        let mut settings = self.read()?;
        settings.insert(key.to_string(), value);
        self.write_all(&settings)
    }

    /// Forget one setting.
    pub fn unset(&self, key: &str) -> Result<()> {
        let mut settings = self.read()?;
        settings.remove(key);
        self.write_all(&settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings round-trip: what was written reads back unchanged.
    #[test]
    fn settings_round_trip() {
        let store = crate::Store::open_in_memory().unwrap();
        let settings = store.settings();
        assert!(settings.read().unwrap().is_empty());
        settings
            .set("sort", Value::String("oldest".into()))
            .unwrap();
        settings.set("wrap", Value::Bool(true)).unwrap();
        let read = settings.read().unwrap();
        assert_eq!(read.get("sort"), Some(&Value::String("oldest".into())));
        assert_eq!(read.get("wrap"), Some(&Value::Bool(true)));
    }

    /// Writing a key replaces it; other keys stay.
    #[test]
    fn writing_a_key_leaves_the_others_alone() {
        let store = crate::Store::open_in_memory().unwrap();
        let settings = store.settings();
        settings
            .set("sort", Value::String("oldest".into()))
            .unwrap();
        settings
            .set("sort", Value::String("recent".into()))
            .unwrap();
        settings.set("wrap", Value::Bool(true)).unwrap();
        let read = settings.read().unwrap();
        assert_eq!(read.get("sort"), Some(&Value::String("recent".into())));
        assert_eq!(read.get("wrap"), Some(&Value::Bool(true)));
    }

    /// A key this build doesn't know about survives a round trip — an
    /// older build never silently drops a newer build's setting.
    #[test]
    fn unknown_keys_survive_a_write() {
        let store = crate::Store::open_in_memory().unwrap();
        let settings = store.settings();
        settings
            .set("from_the_future", Value::String("keep me".into()))
            .unwrap();
        settings
            .set("sort", Value::String("oldest".into()))
            .unwrap();
        let read = settings.read().unwrap();
        assert_eq!(
            read.get("from_the_future"),
            Some(&Value::String("keep me".into()))
        );
    }

    /// Unsetting a key removes it, and leaves the rest.
    #[test]
    fn unsetting_removes_one_key() {
        let store = crate::Store::open_in_memory().unwrap();
        let settings = store.settings();
        settings
            .set("sort", Value::String("oldest".into()))
            .unwrap();
        settings.set("wrap", Value::Bool(true)).unwrap();
        settings.unset("sort").unwrap();
        let read = settings.read().unwrap();
        assert_eq!(read.get("sort"), None);
        assert_eq!(read.get("wrap"), Some(&Value::Bool(true)));
    }

    /// A settings blob that isn't an object reads as "no settings" — a
    /// corrupt value must never keep the app from starting.
    #[test]
    fn a_corrupt_settings_blob_degrades_to_empty() {
        let store = crate::Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO settings (key, value) VALUES ('app', 'not json')",
                [],
            )
            .unwrap();
        assert!(store.settings().read().unwrap().is_empty());
    }
}
