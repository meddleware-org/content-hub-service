//! Every SQL statement of the hub. Queries are SQLx macros, checked at compile time against the migrated schema
//! (offline metadata in `.sqlx/`). Writes take a transaction so the change and its audit-log row commit together.
//!
//! Lists use keyset pagination on `id` (newest first): the cursor is the last id of the previous page, encoded
//! opaquely (`encode_cursor`), and a page fetches `limit + 1` rows to know whether another page exists.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

/// The append-only audit log.
pub mod audit;
/// Spend caps and usage.
pub mod budgets;
/// The worker's claim and finish queries.
pub mod delivery;
/// Platform instances and their attempts.
pub mod instances;
/// Roadmap items.
pub mod items;
/// Counts for the metrics endpoint.
pub mod metrics;
/// Roadmap phases.
pub mod phases;
/// Platform definitions.
pub mod platforms;
/// Posts and their revisions.
pub mod posts;
/// The public views.
pub mod public;
/// Seed import.
pub mod seed;
/// Admin sign-in state: logins in progress and sessions.
pub mod sessions;
/// Runtime settings.
pub mod settings;
/// Stored roadmap snapshots.
pub mod snapshots;
/// The spend ledger.
pub mod spend;
/// Dashboard counts.
pub mod stats;
/// Platform targets.
pub mod targets;
/// Tasks, dependencies, platforms and links.
pub mod tasks;
/// People who have signed in.
pub mod users;

/// The largest page an API caller may ask for.
pub const MAX_LIMIT: i64 = 100;
/// The page size when the caller gives none.
pub const DEFAULT_LIMIT: i64 = 50;

/// One page of a list.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    /// The rows on this page.
    pub items: Vec<T>,
    /// Pass back as `cursor` for the next page; absent on the last page.
    pub next_cursor: Option<String>,
}

/// A cursor and a limit, validated.
#[derive(Debug, Clone, Copy)]
pub struct PageRequest {
    /// Return rows with an id below this one.
    pub before: Option<i64>,
    /// How many rows a page holds.
    pub limit: i64,
}

impl PageRequest {
    /// Validate a cursor and a limit; a missing limit is the default and a limit outside 1 to 100 is clamped.
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

/// The cursor is not one this service issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the cursor is not valid")]
pub struct InvalidCursor;

/// The cursor for a page that ends before row `id`.
pub fn encode_cursor(id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("v1:{id}"))
}

/// The row id a cursor stands for.
pub fn decode_cursor(s: &str) -> Result<i64, InvalidCursor> {
    let raw = URL_SAFE_NO_PAD.decode(s).map_err(|_| InvalidCursor)?;
    let text = std::str::from_utf8(&raw).map_err(|_| InvalidCursor)?;
    text.strip_prefix("v1:").and_then(|n| n.parse::<i64>().ok()).filter(|n| *n > 0).ok_or(InvalidCursor)
}

/// Unit tests for this module.
#[cfg(test)]
mod tests {
    use super::*;

    /// Cursors round trip and garbage is refused.
    #[test]
    fn cursors_round_trip_and_garbage_is_refused() {
        assert_eq!(decode_cursor(&encode_cursor(42)), Ok(42));
        for bad in ["", "!!", "djE6", "djE6LTE", "eDo0Mg"] {
            assert_eq!(decode_cursor(bad), Err(InvalidCursor), "{bad}");
        }
    }

    /// Limits are clamped and pages know when to stop.
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
