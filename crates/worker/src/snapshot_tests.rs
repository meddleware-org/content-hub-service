//! The export against a real PostgreSQL (as the writer role) and a fake blob store.

use std::sync::Mutex;

use content_hub_core::snapshot::{Signer, verify_personal_message};
use content_hub_publish::blob::{BlobError, StoredBlob};
use content_hub_store::Access;
use content_hub_store::testdb::TestDb;
use sqlx::{PgPool, Row};

use super::*;

struct FakeStore {
    puts: Mutex<Vec<(Vec<u8>, u32)>>,
    fail: bool,
}

impl FakeStore {
    fn new(fail: bool) -> FakeStore {
        FakeStore { puts: Mutex::new(Vec::new()), fail }
    }
    fn count(&self) -> usize {
        self.puts.lock().expect("puts").len()
    }
}

impl BlobStore for FakeStore {
    async fn put(&self, bytes: &[u8], epochs: u32) -> Result<StoredBlob, BlobError> {
        if self.fail {
            return Err(BlobError::Unreachable("down".into()));
        }
        self.puts.lock().expect("puts").push((bytes.to_vec(), epochs));
        Ok(StoredBlob {
            blob_id: "Qc9aP0V6x3mYk2w1TnqL8sRZb4dEoFhJiUyXvCgA7MM".into(),
            object_id: Some(format!("0x{}", "ab".repeat(32))),
            end_epoch: Some(99),
            already_certified: false,
        })
    }
}

struct Env {
    _db: TestDb,
    pool: PgPool,
}

async fn env() -> Option<Env> {
    let db = TestDb::migrated().await?;
    let pool = db.pool(&db.writer, Access::ReadWrite);
    sqlx::raw_sql(
        "INSERT INTO hub.roadmap_phases (key, sort_order, internal_name, internal_description, public_name, public_description, visible_human)
           VALUES ('p1', 1, 'SECRET-INTERNAL-NAME', 'SECRET-INTERNAL-PLAN', 'Launch', 'The first phase', true);
         INSERT INTO hub.roadmap_items (phase_id, key, item_type, internal_title, internal_description, public_title, public_description, visible_human, status)
           SELECT id, 'mint', 'content', 'SECRET-INTERNAL-TITLE', 'SECRET-INTERNAL-DETAIL', 'Mint', 'Opens soon', true, 'planned' FROM hub.roadmap_phases",
    )
    .execute(&pool)
    .await
    .expect("seed");
    Some(Env { _db: db, pool })
}

macro_rules! need {
    () => {
        match env().await {
            Some(e) => e,
            None => return,
        }
    };
}

fn signer() -> Signer {
    Signer::from_seed(&[7u8; 32]).expect("seed")
}

fn cfg() -> ExportConfig {
    ExportConfig::for_network("testnet").expect("cfg")
}

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 11, day, hour, 0, 0).single().expect("time")
}

async fn rows(pool: &PgPool) -> i64 {
    sqlx::query("SELECT count(*) FROM hub.snapshots").fetch_one(pool).await.expect("count").get(0)
}

#[tokio::test]
async fn the_first_export_signs_stores_and_records_the_exact_bytes() {
    let e = need!();
    let store = FakeStore::new(false);
    let s = signer();
    let out = export(&e.pool, &s, Some(&store), &cfg(), at(1, 12)).await.expect("export");
    let Outcome::Stored { sha256, signer: who, blob_id, expires_at, renewed } = out else { panic!("{out:?}") };
    assert!(!renewed);
    assert_eq!(who, s.address());
    assert_eq!(expires_at, at(1, 12) + chrono::Duration::days(29), "30 testnet epochs of one day, minus one epoch");

    // The blob that went to Walrus is the canonical document, and the recorded hash and signature are over exactly it.
    let puts = store.puts.lock().expect("puts").clone();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].1, 30);
    assert_eq!(sha256_hex(&puts[0].0), sha256);
    let row =
        snapshots::latest(&mut e.pool.acquire().await.expect("c"), "testnet").await.expect("latest").expect("row");
    assert_eq!(
        (row.sha256.as_str(), row.blob_id.as_deref(), row.epochs),
        (sha256.as_str(), Some(blob_id.as_str()), Some(30))
    );
    assert_eq!(verify_personal_message(&puts[0].0, &row.signature).as_deref(), Ok(s.address().as_str()));
    // It is canonical JSON of the two public roadmaps.
    let doc: serde_json::Value = serde_json::from_slice(&puts[0].0).expect("json");
    assert_eq!(doc["schema"], SCHEMA);
    assert_eq!(doc["human"]["phases"][0]["items"][0]["title"], "Mint");
    assert_eq!(doc["bot"]["phases"], json!([]));
    assert_eq!(canonical(&doc).expect("canonical"), puts[0].0, "re-canonicalising changes nothing");
    let audited: i64 = sqlx::query("SELECT count(*) FROM hub.audit_log WHERE action = 'snapshot.export'")
        .fetch_one(&e.pool)
        .await
        .expect("a")
        .get(0);
    assert_eq!(audited, 1);
}

#[tokio::test]
async fn internal_wording_never_reaches_the_signed_document() {
    let e = need!();
    let store = FakeStore::new(false);
    export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await.expect("export");
    let text = String::from_utf8(store.puts.lock().expect("puts")[0].0.clone()).expect("utf8");
    for secret in ["SECRET-INTERNAL", "p1", "\"key\"", "internal_name", "sort_order", "visible_human"] {
        assert!(!text.contains(secret), "the snapshot leaks {secret}: {text}");
    }
    assert!(text.contains("Opens soon"));
}

#[tokio::test]
async fn unchanged_data_gives_the_same_bytes_and_is_not_stored_again() {
    let e = need!();
    let store = FakeStore::new(false);
    let first = export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await.expect("1");
    let Outcome::Stored { sha256, .. } = &first else { panic!("{first:?}") };
    let second = export(&e.pool, &signer(), Some(&store), &cfg(), at(3, 12)).await.expect("2");
    assert!(matches!(&second, Outcome::Unchanged { sha256: s, .. } if s == sha256), "{second:?}");
    assert_eq!((store.count(), rows(&e.pool).await), (1, 1));
}

#[tokio::test]
async fn a_change_waits_for_the_minimum_interval() {
    let e = need!();
    let store = FakeStore::new(false);
    export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await.expect("1");
    sqlx::query("UPDATE hub.roadmap_items SET public_title = 'Mint (updated)'").execute(&e.pool).await.expect("edit");
    let soon = export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 20)).await.expect("2");
    assert!(matches!(soon, Outcome::TooSoon { next_after, .. } if next_after == at(2, 12)), "{soon:?}");
    assert_eq!((store.count(), rows(&e.pool).await), (1, 1));
    let later = export(&e.pool, &signer(), Some(&store), &cfg(), at(2, 13)).await.expect("3");
    assert!(matches!(later, Outcome::Stored { renewed: false, .. }), "{later:?}");
    assert_eq!((store.count(), rows(&e.pool).await), (2, 2));
}

#[tokio::test]
async fn an_unchanged_snapshot_is_stored_again_before_its_storage_ends() {
    let e = need!();
    let store = FakeStore::new(false);
    export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await.expect("1");
    // 25 days later the estimate (day 30) is within the 7-day margin: renew the same row.
    let renewed =
        export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12) + chrono::Duration::days(25)).await.expect("renew");
    assert!(matches!(renewed, Outcome::Stored { renewed: true, .. }), "{renewed:?}");
    assert_eq!((store.count(), rows(&e.pool).await), (2, 1), "the same row, new storage");
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM hub.audit_log WHERE entity_type = 'snapshot' ORDER BY id")
            .fetch_all(&e.pool)
            .await
            .expect("audit");
    assert_eq!(actions, ["snapshot.export", "snapshot.renew"]);
}

#[tokio::test]
async fn a_failed_store_records_nothing() {
    let e = need!();
    let broken = FakeStore::new(true);
    let r = export(&e.pool, &signer(), Some(&broken), &cfg(), at(1, 12)).await;
    assert!(matches!(r, Err(ExportError::Blob(BlobError::Unreachable(_)))), "{r:?}");
    assert_eq!(rows(&e.pool).await, 0);
    // The next run, with the store back, succeeds.
    let ok = FakeStore::new(false);
    assert!(matches!(export(&e.pool, &signer(), Some(&ok), &cfg(), at(1, 13)).await, Ok(Outcome::Stored { .. })));
}

#[tokio::test]
async fn without_a_store_the_snapshot_is_signed_and_recorded_unstored() {
    let e = need!();
    let none: Option<&FakeStore> = None;
    let r = export(&e.pool, &signer(), none, &cfg(), at(1, 12)).await.expect("export");
    assert!(matches!(r, Outcome::Signed { .. }), "{r:?}");
    let row = snapshots::latest(&mut e.pool.acquire().await.expect("c"), "testnet").await.expect("l").expect("row");
    assert_eq!((row.blob_id, row.expires_at, row.epochs), (None, None, None));
    assert!(matches!(export(&e.pool, &signer(), none, &cfg(), at(2, 12)).await, Ok(Outcome::Unchanged { .. })));
    // When a store appears, the unstored snapshot is stored (it is not "healthy").
    let store = FakeStore::new(false);
    assert!(matches!(
        export(&e.pool, &signer(), Some(&store), &cfg(), at(2, 13)).await,
        Ok(Outcome::Stored { renewed: true, .. })
    ));
}

#[tokio::test]
async fn two_exports_do_not_run_at_once() {
    let e = need!();
    let mut holder = e.pool.acquire().await.expect("holder");
    assert!(snapshots::try_lock(&mut holder).await.expect("lock"));
    let store = FakeStore::new(false);
    assert_eq!(export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await.expect("busy"), Outcome::Busy);
    assert_eq!(store.count(), 0);
    snapshots::unlock(&mut holder).await.expect("unlock");
    assert!(matches!(export(&e.pool, &signer(), Some(&store), &cfg(), at(1, 12)).await, Ok(Outcome::Stored { .. })));
}

#[test]
fn defaults_per_network() {
    let t = ExportConfig::for_network("testnet").expect("t");
    assert_eq!((t.epochs, t.epoch_len.as_secs()), (30, 86_400));
    let m = ExportConfig::for_network("mainnet").expect("m");
    assert_eq!((m.epochs, m.epoch_len.as_secs()), (2, 14 * 86_400));
    assert_eq!(m.estimated_expiry(at(1, 0)), at(1, 0) + chrono::Duration::days(14));
    assert!(ExportConfig::for_network("devnet").is_err());
}
