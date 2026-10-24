//! Review locations are independent of fetched PR snapshots and draft reviews.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// A viewport in one changed file. Line numbers survive display-row rebuilds;
/// `row` is the fallback for headers/binary files. `row_delta` preserves a
/// location inside a comment following the anchored code line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewPosition {
    pub path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub row: usize,
    pub row_delta: usize,
    /// Fraction of the top row above the viewport (independent of font size).
    pub fraction: f32,
    pub horizontal_offset: f32,
    pub split: bool,
}

pub struct PositionStore<'a> {
    conn: &'a Connection,
}

impl<'a> PositionStore<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn read(&self, repository: &str, number: u64) -> Result<Option<ReviewPosition>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT position FROM review_positions WHERE repository = ?1 AND number = ?2",
                params![repository, number],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("couldn't decode review position"))
            .transpose()
    }

    pub fn write(&self, repository: &str, number: u64, position: &ReviewPosition) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO review_positions (repository, number, position) VALUES (?1, ?2, ?3)
             ON CONFLICT(repository, number) DO UPDATE SET position = excluded.position",
                params![repository, number, serde_json::to_string(position)?],
            )
            .context("couldn't save review position")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn positions_are_independent_per_repository_and_pr() {
        let store = Store::open_in_memory().unwrap();
        let positions = store.positions();
        assert_eq!(positions.read("acme/widgets", 7).unwrap(), None);
        let mut position = ReviewPosition {
            path: "src/lib.rs".into(),
            new_line: Some(52),
            fraction: 0.25,
            ..Default::default()
        };
        positions.write("acme/widgets", 7, &position).unwrap();
        assert_eq!(
            positions.read("acme/widgets", 7).unwrap(),
            Some(position.clone())
        );
        assert_eq!(positions.read("other/widgets", 7).unwrap(), None);
        assert_eq!(positions.read("acme/widgets", 8).unwrap(), None);
        position.new_line = Some(99);
        positions.write("acme/widgets", 7, &position).unwrap();
        assert_eq!(positions.read("acme/widgets", 7).unwrap(), Some(position));
    }

    #[test]
    fn malformed_positions_report_errors_without_affecting_other_prs() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO review_positions VALUES ('acme/widgets', 7, 'not json')",
                [],
            )
            .unwrap();
        assert!(store.positions().read("acme/widgets", 7).is_err());
        assert_eq!(store.positions().read("acme/widgets", 8).unwrap(), None);
    }
}
