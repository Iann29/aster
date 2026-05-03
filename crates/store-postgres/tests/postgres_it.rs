//! Integration tests against a real Postgres database. Gated behind the
//! `postgres-it` feature so default `cargo test --workspace` doesn't
//! require a live DB.
//!
//! Run locally:
//!     docker run -d --rm --name aster-pg -p 5432:5432 \
//!         -e POSTGRES_PASSWORD=postgres postgres:16
//!     ASTER_DB_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
//!         cargo test -p aster-store-postgres --features postgres-it
//!
//! In CI, the dedicated `postgres-it` lane spins up the same image as a
//! GitHub Actions service container. See commit 5/5.
//!
//! These tests intentionally talk to a *blank* Postgres until commit 4
//! lands — they prove the connection + pool + runtime path works, not
//! the SQL. Real SQL tests arrive when the SQL does.

#![cfg(feature = "postgres-it")]

use aster_store_postgres::{PostgresCapsuleStore, PostgresConfig};

fn url() -> String {
    std::env::var("ASTER_DB_URL").expect(
        "ASTER_DB_URL must be set for postgres-it tests \
         (e.g. postgres://postgres:postgres@127.0.0.1:5432/postgres)",
    )
}

#[test]
fn connect_against_real_postgres_succeeds() {
    let cfg = PostgresConfig {
        url: url(),
        ..PostgresConfig::default()
    };
    let _store = PostgresCapsuleStore::connect(cfg).expect("connect parses + builds pool");
    // Lazy — first checkout would happen here. Stub returns Backend error
    // before doing real work, so this test only proves no panic on
    // construction. Commit 4 adds a SELECT 1 round-trip test.
}
