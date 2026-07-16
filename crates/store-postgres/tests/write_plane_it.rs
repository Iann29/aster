//! Integration proofs for the commit fence against a real Postgres —
//! the executable counterparts of the Capsule Transaction Theorem's
//! counterexamples and lemmas. Gated behind `postgres-it` like
//! `postgres_it.rs`; run with:
//!
//!     ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
//!         cargo test -p aster-store-postgres --features postgres-it \
//!         --test write_plane_it -- --test-threads=1

#![cfg(feature = "postgres-it")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use aster_capsule::{
    doc_with_i64, DocumentId, KeyInterval, ObservedWindow, RangeCertificate, ScanDirection,
    ScanStop, VersionedDocument,
};
use aster_store_postgres::{CommitOutcome, FenceInput, WritePlane, WritePlaneConfig};

const TENANT: &str = "tenant-wp";
const DEPLOYMENT: &str = "dep-wp";

fn url() -> String {
    std::env::var("ASTER_DB_URL")
        .expect("set ASTER_DB_URL to run postgres-it tests (see file header)")
}

fn plane() -> WritePlane {
    WritePlane::connect(WritePlaneConfig {
        url: url(),
        ..WritePlaneConfig::default()
    })
    .expect("connect write plane")
}

/// Drop and recreate the aster schema so every test starts deterministic.
fn fresh_plane() -> WritePlane {
    let plane = plane();
    raw_exec("DROP SCHEMA IF EXISTS aster CASCADE");
    plane.ensure_schema().expect("ensure schema");
    plane
}

fn raw_exec(sql: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
            .await
            .expect("raw connect");
        tokio::spawn(connection);
        client.batch_execute(sql).await.expect("raw exec");
    });
}

fn put(key: &str, value: i64) -> (DocumentId, Option<aster_capsule::Document>) {
    (DocumentId::new(key), Some(doc_with_i64("value", value)))
}

fn commit_blind(plane: &WritePlane, epoch: u64, snapshot: u64, key: &str, value: i64) -> CommitOutcome {
    plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot,
            read_points: &[],
            read_windows: &[],
            writes: &[put(key, value)],
        })
        .expect("commit")
}

#[test]
fn lease_epochs_strictly_increase_and_never_reuse() {
    let plane = fresh_plane();
    // Referee finding F1: failback A→B→A must never reuse an epoch.
    let a1 = plane.acquire_lease(TENANT, DEPLOYMENT, "committer-a").expect("acquire a");
    let b = plane.acquire_lease(TENANT, DEPLOYMENT, "committer-b").expect("acquire b");
    let a2 = plane.acquire_lease(TENANT, DEPLOYMENT, "committer-a").expect("acquire a again");
    assert_eq!((a1, b, a2), (1, 2, 3));
}

#[test]
fn write_skew_ce39_is_impossible_through_the_fence() {
    let plane = fresh_plane();
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    // Seed x = 0, y = 0 as two commits; snapshot after both.
    assert!(matches!(commit_blind(&plane, epoch, 0, "docs/x", 0), CommitOutcome::Committed { .. }));
    assert!(matches!(commit_blind(&plane, epoch, 1, "docs/y", 0), CommitOutcome::Committed { .. }));
    let s = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");

    // Counterexample 3.9: T1 reads y writes x, T2 reads x writes y, same
    // snapshot. Through the fence the second validator must see the first
    // writer in (s, h] and abort.
    let t1 = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s,
            read_points: &[DocumentId::new("docs/y")],
            read_windows: &[],
            writes: &[put("docs/x", 1)],
        })
        .expect("t1");
    let t2 = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s,
            read_points: &[DocumentId::new("docs/x")],
            read_windows: &[],
            writes: &[put("docs/y", 1)],
        })
        .expect("t2");
    assert!(matches!(t1, CommitOutcome::Committed { .. }), "t1: {t1:?}");
    assert_eq!(
        t2,
        CommitOutcome::Conflict {
            key: DocumentId::new("docs/x")
        }
    );
}

#[test]
fn concurrent_write_skew_pair_commits_exactly_once() {
    let plane = Arc::new(fresh_plane());
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    assert!(matches!(commit_blind(&plane, epoch, 0, "docs/x", 0), CommitOutcome::Committed { .. }));
    assert!(matches!(commit_blind(&plane, epoch, 1, "docs/y", 0), CommitOutcome::Committed { .. }));
    let s = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");

    // Both fences race on real connections; the lease row lock must
    // serialize them so exactly one commits and one aborts.
    let barrier = Arc::new(Barrier::new(2));
    let spawn = |reads: &'static str, writes: &'static str| {
        let plane = Arc::clone(&plane);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            plane
                .commit(&FenceInput {
                    tenant: TENANT,
                    deployment: DEPLOYMENT,
                    committer_epoch: epoch,
                    context_epoch: epoch,
                    snapshot: s,
                    read_points: &[DocumentId::new(reads)],
                    read_windows: &[],
                    writes: &[put(writes, 1)],
                })
                .expect("commit")
        })
    };
    let t1 = spawn("docs/y", "docs/x");
    let t2 = spawn("docs/x", "docs/y");
    let outcomes = [t1.join().expect("join t1"), t2.join().expect("join t2")];
    let committed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CommitOutcome::Committed { .. }))
        .count();
    let conflicted = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, CommitOutcome::Conflict { .. }))
        .count();
    assert_eq!(
        (committed, conflicted),
        (1, 1),
        "expected exactly one commit and one conflict, got {outcomes:?}"
    );
}

#[test]
fn stale_epoch_cannot_append_after_failover() {
    let plane = fresh_plane();
    let epoch_a = plane.acquire_lease(TENANT, DEPLOYMENT, "committer-a").expect("acquire a");
    assert!(matches!(
        commit_blind(&plane, epoch_a, 0, "docs/a", 1),
        CommitOutcome::Committed { .. }
    ));

    let epoch_b = plane.acquire_lease(TENANT, DEPLOYMENT, "committer-b").expect("acquire b");
    // Old-epoch committer AND old-epoch capsule context are both fenced.
    let stale = commit_blind(&plane, epoch_a, 1, "docs/a", 2);
    assert_eq!(stale, CommitOutcome::StaleEpoch { lease_epoch: epoch_b });
    let mixed = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch_b,
            context_epoch: epoch_a,
            snapshot: 1,
            read_points: &[],
            read_windows: &[],
            writes: &[put("docs/a", 2)],
        })
        .expect("mixed epoch commit");
    assert_eq!(mixed, CommitOutcome::StaleEpoch { lease_epoch: epoch_b });

    assert!(matches!(
        commit_blind(&plane, epoch_b, 1, "docs/a", 3),
        CommitOutcome::Committed { .. }
    ));
    // Lemma 3.11 (epoch block order): no old-epoch row may carry a
    // timestamp above any new-epoch row.
    let value = plane
        .read_point(TENANT, DEPLOYMENT, &DocumentId::new("docs/a"), u64::MAX >> 1)
        .expect("read");
    assert_eq!(
        value.document.and_then(|doc| doc.get("value").cloned()),
        Some(aster_capsule::Value::Int(3))
    );
}

#[test]
fn gc_blocks_on_inflight_fence_and_enforces_coverage() {
    let plane = Arc::new(fresh_plane());
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    for (i, value) in [1, 2, 3].iter().enumerate() {
        assert!(matches!(
            commit_blind(&plane, epoch, i as u64, "docs/k", *value),
            CommitOutcome::Committed { .. }
        ));
    }

    // Simulate a fence mid-validation by holding the retention row lock in
    // a raw transaction, then prove the sweeper blocks until it releases —
    // the check-then-use race from Lemma R's counterexample cannot happen.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let done = Arc::new(AtomicBool::new(false));
    runtime.block_on(async {
        let (mut client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
            .await
            .expect("raw connect");
        tokio::spawn(connection);
        let tx = client.transaction().await.expect("begin");
        tx.query_one(
            "SELECT low_watermark FROM aster.retention
             WHERE tenant = $1 AND deployment = $2 FOR UPDATE",
            &[&TENANT, &DEPLOYMENT],
        )
        .await
        .expect("hold retention pin");

        let sweeper = {
            let plane = Arc::clone(&plane);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let watermark = plane
                    .advance_retention(TENANT, DEPLOYMENT, 2)
                    .expect("advance retention");
                done.store(true, Ordering::SeqCst);
                watermark
            })
        };
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !done.load(Ordering::SeqCst),
            "sweeper advanced the watermark while the fence held the pin"
        );
        tx.commit().await.expect("release pin");
        assert_eq!(sweeper.join().expect("join sweeper"), 2);
    });

    // Coverage rule g <= s: a snapshot below the watermark must be refused.
    let refused = commit_blind(&plane, epoch, 1, "docs/other", 9);
    assert_eq!(refused, CommitOutcome::RetentionViolated { low_watermark: 2 });
    // A covered snapshot still commits.
    let s = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");
    assert!(matches!(
        commit_blind(&plane, epoch, s, "docs/other", 9),
        CommitOutcome::Committed { .. }
    ));
}

#[test]
fn replay_commits_as_a_second_transaction() {
    let plane = fresh_plane();
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    // Attack 13: a replayed blind-write request is revalidated and becomes
    // another legal serial transaction — no nonce, no at-most-once claim.
    let first = commit_blind(&plane, epoch, 0, "docs/r", 1);
    let second = commit_blind(&plane, epoch, 0, "docs/r", 1);
    match (first, second) {
        (CommitOutcome::Committed { ts: t1 }, CommitOutcome::Committed { ts: t2 }) => {
            assert!(t2 > t1)
        }
        other => panic!("expected two commits, got {other:?}"),
    }
}

#[test]
fn phantom_insert_conflicts_with_exhausted_window_but_not_past_boundary() {
    let plane = fresh_plane();
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    for (i, key) in ["docs/a", "docs/b", "docs/c"].iter().enumerate() {
        assert!(matches!(
            commit_blind(&plane, epoch, i as u64, key, 1),
            CommitOutcome::Committed { .. }
        ));
    }
    let s = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");

    // Interfering insert lands after the snapshot.
    assert!(matches!(
        commit_blind(&plane, epoch, s, "docs/zz", 7),
        CommitOutcome::Committed { .. }
    ));

    // An EXHAUSTED scan observed the whole prefix — the insert is a
    // phantom in its exhaustion gap and must conflict (attack 5).
    let exhausted = RangeCertificate {
        interval: KeyInterval::Prefix("docs/".into()),
        direction: ScanDirection::Ascending,
        limit: 10,
        keys: vec![
            DocumentId::new("docs/a"),
            DocumentId::new("docs/b"),
            DocumentId::new("docs/c"),
        ],
        stop: ScanStop::Exhausted,
    };
    let outcome = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s,
            read_points: &[],
            read_windows: &[exhausted.window()],
            writes: &[put("docs/out", 1)],
        })
        .expect("exhausted-window commit");
    assert_eq!(
        outcome,
        CommitOutcome::Conflict {
            key: DocumentId::new("docs/zz")
        }
    );

    // A BOUNDARY scan (limit hit at docs/c) observed only the prefix
    // through docs/c — the same insert is past the boundary and must NOT
    // abort this commit (referee finding F2, end-to-end).
    let boundary = RangeCertificate {
        interval: KeyInterval::Prefix("docs/".into()),
        direction: ScanDirection::Ascending,
        limit: 3,
        keys: vec![
            DocumentId::new("docs/a"),
            DocumentId::new("docs/b"),
            DocumentId::new("docs/c"),
        ],
        stop: ScanStop::Boundary,
    };
    let outcome = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s,
            read_points: &[],
            read_windows: &[boundary.window()],
            writes: &[put("docs/out2", 1)],
        })
        .expect("boundary-window commit");
    assert!(
        matches!(outcome, CommitOutcome::Committed { .. }),
        "write past the boundary must not conflict: {outcome:?}"
    );
}

#[test]
fn absence_and_tombstone_reads_conflict_on_later_writes() {
    let plane = fresh_plane();
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    assert!(matches!(commit_blind(&plane, epoch, 0, "docs/seed", 1), CommitOutcome::Committed { .. }));
    let s = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");

    // Another transaction inserts the key this reader observed as absent.
    assert!(matches!(
        commit_blind(&plane, epoch, s, "docs/ghost", 5),
        CommitOutcome::Committed { .. }
    ));
    // Attack 6 (absence flip): the declared absent-read must conflict.
    let outcome = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s,
            read_points: &[DocumentId::new("docs/ghost")],
            read_windows: &[],
            writes: &[put("docs/dependent", 1)],
        })
        .expect("absent-read commit");
    assert_eq!(
        outcome,
        CommitOutcome::Conflict {
            key: DocumentId::new("docs/ghost")
        }
    );
}

#[test]
fn read_point_and_live_prefix_scan_follow_mvcc_semantics() {
    let plane = fresh_plane();
    let epoch = plane.acquire_lease(TENANT, DEPLOYMENT, "committer").expect("acquire");
    assert!(matches!(commit_blind(&plane, epoch, 0, "docs/a", 1), CommitOutcome::Committed { .. }));
    let s_before_delete = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");
    // Delete docs/a (tombstone), then add docs/b and docs/c.
    let outcome = plane
        .commit(&FenceInput {
            tenant: TENANT,
            deployment: DEPLOYMENT,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot: s_before_delete,
            read_points: &[],
            read_windows: &[],
            writes: &[(DocumentId::new("docs/a"), None)],
        })
        .expect("delete");
    assert!(matches!(outcome, CommitOutcome::Committed { .. }));
    assert!(matches!(commit_blind(&plane, epoch, 2, "docs/b", 2), CommitOutcome::Committed { .. }));
    assert!(matches!(commit_blind(&plane, epoch, 3, "docs/c", 3), CommitOutcome::Committed { .. }));

    // Point read at the old snapshot still sees the pre-delete document.
    let old = plane
        .read_point(TENANT, DEPLOYMENT, &DocumentId::new("docs/a"), s_before_delete)
        .expect("old read");
    assert!(old.document.is_some());
    // At the tip it is an explicit tombstone: versioned absence.
    let tip = plane.snapshot_ts(TENANT, DEPLOYMENT).expect("tip");
    let now = plane
        .read_point(TENANT, DEPLOYMENT, &DocumentId::new("docs/a"), tip)
        .expect("tip read");
    assert_eq!(now.document, None);
    assert!(now.version.is_some(), "tombstone must keep its version");
    // Never-written keys are (None, None).
    let missing = plane
        .read_point(TENANT, DEPLOYMENT, &DocumentId::new("docs/never"), tip)
        .expect("missing read");
    assert_eq!(missing, VersionedDocument::missing());

    // Live prefix scan at the tip: the tombstoned docs/a must not consume
    // a limit slot (the S3b bug this store never inherits).
    let rows = plane
        .read_prefix_live(TENANT, DEPLOYMENT, "docs/", 2, tip)
        .expect("live scan");
    let keys: Vec<&str> = rows.iter().map(|(key, _)| key.0.as_str()).collect();
    assert_eq!(keys, vec!["docs/b", "docs/c"]);

    // Prefix wildcards are matched literally.
    let rows = plane
        .read_prefix_live(TENANT, DEPLOYMENT, "docs/%", 10, tip)
        .expect("wildcard scan");
    assert!(rows.is_empty(), "LIKE wildcard must be escaped: {rows:?}");
}

#[test]
fn observed_window_types_are_reusable_across_threads() {
    // Compile-time style check that ObservedWindow (built from a
    // certificate) crosses the FenceInput boundary naturally.
    let cert = RangeCertificate {
        interval: KeyInterval::Prefix("docs/".into()),
        direction: ScanDirection::Ascending,
        limit: 1,
        keys: vec![DocumentId::new("docs/a")],
        stop: ScanStop::Boundary,
    };
    let window: ObservedWindow = cert.window();
    assert!(window.contains(&DocumentId::new("docs/a")));
    assert!(!window.contains(&DocumentId::new("docs/b")));
}
