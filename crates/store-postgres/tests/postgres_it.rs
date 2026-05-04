//! Integration tests against a real Postgres database. Gated behind the
//! `postgres-it` feature so default `cargo test --workspace` doesn't
//! require a live DB.
//!
//! Run locally:
//!     docker run -d --rm --name aster-pg-dev -p 5433:5432 \
//!         -e POSTGRES_USER=aster -e POSTGRES_PASSWORD=aster \
//!         -e POSTGRES_DB=aster postgres:16
//!     ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
//!         cargo test -p aster-store-postgres --features postgres-it -- --test-threads=1
//!
//! In CI, the dedicated `postgres-it` lane spins up `postgres:16` as a
//! GitHub Actions service container and runs this file. See
//! `.github/workflows/ci.yml`.
//!
//! `--test-threads=1` because the tests share schema state — each test
//! starts by re-applying the fixture so the seed is deterministic.

#![cfg(feature = "postgres-it")]

use std::path::Path;

use aster_broker::{CapsuleStore, StoreError};
use aster_capsule::{DocumentId, Value};
use aster_convex_codec::DocumentIdV6;
use aster_store_postgres::{PostgresCapsuleStore, PostgresConfig};
use tokio::runtime::Builder;
use tokio_postgres::NoTls;

const TEST_SCHEMA: &str = "convex_dev";
const TEST_TABLE_ID_HEX: &str = "0123456789abcdef0123456789abcdef";
const ID_IAN_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ID_CAUE_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// `seed.sql` registers the `messages` tablet under table_number 10001.
/// Must match the `_tables` row body. Hardcoded here so the IDv6 test
/// can encode the same number the resolver expects to find.
const TEST_TABLE_NUMBER: u32 = 10001;

fn url() -> String {
    std::env::var("ASTER_DB_URL").expect(
        "ASTER_DB_URL must be set for postgres-it tests \
         (e.g. postgres://aster:aster@127.0.0.1:5433/aster)",
    )
}

/// Apply schema + seed fresh on every test so they don't see each
/// other's leftover state. The schema is dropped + recreated rather
/// than truncated because the fixtures change between commits.
///
/// Sync wrapper: we deliberately can't use `#[tokio::test]` because
/// `PostgresCapsuleStore` owns its own runtime and `block_on`s into
/// it; nesting runtimes panics. So the reset spins up a tiny ad-hoc
/// runtime, runs the SQL, and drops it before the store is created.
fn reset_fixture(url: &str) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("ad-hoc runtime for fixture reset");
    rt.block_on(async {
        let (client, conn) = tokio_postgres::connect(url, NoTls)
            .await
            .expect("connect for fixture reset");
        let handle = tokio::spawn(async move {
            let _ = conn.await;
        });
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {TEST_SCHEMA} CASCADE"))
            .await
            .expect("drop schema");

        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let schema_sql = std::fs::read_to_string(here.join("tests/fixtures/schema.sql"))
            .expect("read schema.sql");
        let seed_sql =
            std::fs::read_to_string(here.join("tests/fixtures/seed.sql")).expect("read seed.sql");

        client
            .batch_execute(&schema_sql)
            .await
            .expect("apply schema");
        client.batch_execute(&seed_sql).await.expect("apply seed");
        drop(client);
        handle.abort();
    });
}

fn make_store() -> PostgresCapsuleStore {
    let cfg = PostgresConfig {
        url: url(),
        schema: TEST_SCHEMA.into(),
        ..PostgresConfig::default()
    };
    PostgresCapsuleStore::connect(cfg).expect("connect store")
}

#[test]
fn snapshot_ts_returns_max_of_documents_and_fence() {
    reset_fixture(&url());
    let store = make_store();
    let ts = store.snapshot_ts().expect("snapshot_ts");
    assert_eq!(ts, 200, "expected snapshot_ts to track the latest commit");
}

#[test]
fn read_point_returns_latest_revision_at_or_before_ts() {
    reset_fixture(&url());
    let store = make_store();
    let key = DocumentId::new(format!("{TEST_TABLE_ID_HEX}/{ID_IAN_HEX}"));

    let value = store.read_point(&key, 200).expect("read_point");
    assert_eq!(value.version, Some(100), "ian was inserted at ts=100");
    let raw = value.document.as_ref().and_then(|d| d.get("_raw")).cloned();
    match raw {
        Some(Value::Text(s)) => assert!(
            s.contains("\"name\":\"ian\""),
            "raw bytes should round-trip the inserted JSON, got {s:?}"
        ),
        other => panic!("expected _raw text, got {other:?}"),
    }
}

#[test]
fn read_point_returns_missing_for_ts_before_insert() {
    reset_fixture(&url());
    let store = make_store();
    let key = DocumentId::new(format!("{TEST_TABLE_ID_HEX}/{ID_IAN_HEX}"));
    let value = store.read_point(&key, 50).expect("read_point");
    assert!(value.version.is_none(), "expected missing, got {value:?}");
    assert!(value.document.is_none());
}

#[test]
fn read_point_for_unknown_id_is_missing_not_error() {
    reset_fixture(&url());
    let store = make_store();
    let key = DocumentId::new(format!(
        "{TEST_TABLE_ID_HEX}/cccccccccccccccccccccccccccccccc"
    ));
    let value = store.read_point(&key, 200).expect("read_point");
    assert!(value.version.is_none());
    assert!(value.document.is_none());
}

#[test]
fn read_prefix_returns_every_doc_in_table() {
    reset_fixture(&url());
    let store = make_store();
    let prefix = format!("{TEST_TABLE_ID_HEX}/");

    let rows = store.read_prefix(&prefix, 100, 200).expect("read_prefix");
    assert_eq!(rows.len(), 2, "expected both seeded docs, got {rows:?}");

    for (id, value) in &rows {
        assert!(id.0.starts_with(&format!("{TEST_TABLE_ID_HEX}/")));
        assert!(value.version.is_some());
        assert!(value.document.is_some());
    }

    let ids: Vec<&str> = rows.iter().map(|(d, _)| d.0.as_str()).collect();
    assert!(ids.iter().any(|id| id.contains(ID_IAN_HEX)));
    assert!(ids.iter().any(|id| id.contains(ID_CAUE_HEX)));
}

#[test]
fn read_prefix_honours_limit() {
    reset_fixture(&url());
    let store = make_store();
    let prefix = format!("{TEST_TABLE_ID_HEX}/");
    let rows = store.read_prefix(&prefix, 1, 200).expect("read_prefix");
    assert_eq!(rows.len(), 1, "limit=1 should clip to a single row");
}

#[test]
fn read_prefix_at_old_ts_only_sees_first_insert() {
    reset_fixture(&url());
    let store = make_store();
    let prefix = format!("{TEST_TABLE_ID_HEX}/");
    let rows = store.read_prefix(&prefix, 100, 150).expect("read_prefix");
    assert_eq!(rows.len(), 1, "ts=150 should only see the first insert");
    assert!(rows[0].0 .0.contains(ID_IAN_HEX));
}

#[test]
fn malformed_document_id_is_backend_error_not_panic() {
    let store = make_store();
    let key = DocumentId::new("not-a-valid-encoded-id");
    match store.read_point(&key, 200) {
        Err(StoreError::Backend(_)) => {}
        other => panic!("expected Backend(_), got {other:?}"),
    }
}

/// IDv6 → tablet UUID resolution: the cell hands the broker an IDv6
/// string a Convex JS bundle would produce. The mapping cache reads
/// `_tables`, finds the user table by `number=10001`, and the same
/// SQL path returns the same row the wire-form test got.
#[test]
fn read_point_resolves_idv6_via_table_mapping_cache() {
    reset_fixture(&url());
    let store = make_store();

    let mut internal = [0u8; 16];
    for (i, byte) in internal.iter_mut().enumerate() {
        *byte = 0xAA;
        let _ = i;
    }
    let id = DocumentIdV6::new(TEST_TABLE_NUMBER, internal);
    let key = DocumentId::new(id.encode());

    let value = store.read_point(&key, 200).expect("read_point via IDv6");
    assert_eq!(value.version, Some(100), "ian was inserted at ts=100");
    let raw = value.document.as_ref().and_then(|d| d.get("_raw")).cloned();
    match raw {
        Some(Value::Text(s)) => assert!(
            s.contains("\"name\":\"ian\""),
            "raw bytes should round-trip, got {s:?}"
        ),
        other => panic!("expected _raw text, got {other:?}"),
    }
}

/// `_tables` rows in `state="deleting"` must NOT be exposed by the
/// cache — using a stale tablet UUID would silently return the wrong
/// document if `number` was reused. This test asks for an IDv6
/// pointing at the `deleting` row's number; resolution must fail
/// rather than return the user document.
#[test]
fn read_point_skips_deleting_tables_in_mapping_cache() {
    reset_fixture(&url());
    let store = make_store();

    // 9999 is the `deleting` row from seed.sql — must be invisible.
    let id = DocumentIdV6::new(9999, [0xAA; 16]);
    let key = DocumentId::new(id.encode());
    match store.read_point(&key, 200) {
        Err(StoreError::Backend(msg)) => assert!(
            msg.contains("9999"),
            "expected error to name the missing table_number, got {msg:?}"
        ),
        other => panic!("expected Backend(_) for deleting table, got {other:?}"),
    }
}

/// Cache miss followed by hit: the second IDv6 read for the same
/// table_number must NOT re-run the refresh. There is no clean way
/// to count Postgres roundtrips from a black-box test, so we settle
/// for "the second call returns the same data and stays fast".
/// (Hot-path assertions live in the unit test
/// `tests::dispatch_uses_cache_when_table_number_known`.)
#[test]
fn read_point_idv6_second_call_is_cache_hit() {
    reset_fixture(&url());
    let store = make_store();
    let id = DocumentIdV6::new(TEST_TABLE_NUMBER, [0xAA; 16]);
    let key = DocumentId::new(id.encode());

    let first = store.read_point(&key, 200).expect("first");
    let second = store.read_point(&key, 200).expect("second");
    assert_eq!(first.version, second.version);
    assert_eq!(
        first.document.as_ref().and_then(|d| d.get("_raw")),
        second.document.as_ref().and_then(|d| d.get("_raw"))
    );
}
