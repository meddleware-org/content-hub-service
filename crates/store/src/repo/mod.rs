//! Every SQL statement of the hub. Queries are SQLx macros, checked at compile time against the migrated schema
//! (offline metadata in `.sqlx/`). Writes take a transaction so the change and its audit-log row commit together.
//!
//! Lists use keyset pagination on `id` (newest first): the cursor is the last id of the previous page, encoded
//! opaquely (`encode_cursor`), and a page fetches `limit + 1` rows to know whether another page exists.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

pub mod audit;
pub mod budgets;
pub mod delivery;
pub mod instances;
pub mod items;
pub mod metrics;
pub mod phases;
pub mod platforms;
pub mod posts;
pub mod public;
pub mod seed;
pub mod settings;
pub mod snapshots;
pub mod spend;
pub mod stats;
pub mod targets;
pub mod tasks;
pub mod users;

/// The largest page an API caller may ask for.
pub const MAX_LIMIT: i64 = 100;
/// The page size when the caller gives none.
pub const DEFAULT_LIMIT: i64 = 50;

/// One page of a list.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Pass back as `cursor` for the next page; absent on the last page.
    pub next_cursor: Option<String>,
}

/// A cursor and a limit, validated.
#[derive(Debug, Clone, Copy)]
pub struct PageRequest {
    /// Return rows with an id below this one.
    pub before: Option<i64>,
    pub limit: i64,
}

impl PageRequest {
    pub fn new(cursor: Option<&str>, limit: Option<i64>) -> Result<Self, InvalidCursor> {
        let before = cursor.map(decode_cursor).transpose()?;
        Ok(PageRequest { before, limit: limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) })
    }

    /// The `before` bound for SQL (`i64::MAX` for the first page).
    pub fn bound(&self) -> i64 {
        self.before.unwrap_or(i64::MAX)
    }

    /// Rows to fetch (one more than the page, to know whether another page follows).
    pub fn fetch(&self) -> i64 {
        self.limit + 1
    }

    /// Turn `limit + 1` fetched rows into a page.
    pub fn page<T>(&self, mut rows: Vec<T>, id: impl Fn(&T) -> i64) -> Page<T> {
        let more = rows.len() as i64 > self.limit;
        rows.truncate(self.limit as usize);
        let next_cursor = if more { rows.last().map(|r| encode_cursor(id(r))) } else { None };
        Page { items: rows, next_cursor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the cursor is not valid")]
pub struct InvalidCursor;

pub fn encode_cursor(id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("v1:{id}"))
}

pub fn decode_cursor(s: &str) -> Result<i64, InvalidCursor> {
    let raw = URL_SAFE_NO_PAD.decode(s).map_err(|_| InvalidCursor)?;
    let text = std::str::from_utf8(&raw).map_err(|_| InvalidCursor)?;
    text.strip_prefix("v1:").and_then(|n| n.parse::<i64>().ok()).filter(|n| *n > 0).ok_or(InvalidCursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_garbage_is_refused() {
        assert_eq!(decode_cursor(&encode_cursor(42)), Ok(42));
        for bad in ["", "!!", "djE6", "djE6LTE", "eDo0Mg"] {
            assert_eq!(decode_cursor(bad), Err(InvalidCursor), "{bad}");
        }
    }

    #[test]
    fn limits_are_clamped_and_pages_know_when_to_stop() {
        let p = PageRequest::new(None, Some(1000)).expect("ok");
        assert_eq!((p.limit, p.bound()), (MAX_LIMIT, i64::MAX));
        let p = PageRequest::new(None, Some(2)).expect("ok");
        let page = p.page(vec![9_i64, 8, 7], |x| *x);
        assert_eq!(page.items, vec![9, 8]);
        assert_eq!(page.next_cursor.as_deref().map(decode_cursor), Some(Ok(8)));
        let last = p.page(vec![3_i64], |x| *x);
        assert!(last.next_cursor.is_none());
    }
}
