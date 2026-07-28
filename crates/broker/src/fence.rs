//! The commit-fence seam: the Capsule Transaction Theorem's
//! `CommitFence(ctx, Cap, R, W)` (§1.8) as a storage capability the
//! brokerd binary can dispatch on, mirroring how `CapsuleStore` abstracts
//! the read side.
//!
//! Two implementations exist:
//!
//! - `WritePlane` in `aster-store-postgres` — the real thing: one Postgres
//!   transaction over the lease row, the retention pin, and the commit log.
//! - [`MemoryFence`] below — the same admission semantics over the
//!   prototype `MvccStore`, so the default (database-free) test suite can
//!   exercise the full cell → broker → fence loop.
//!
//! A gated parity test pins one identical scenario to identical outcomes
//! across the two impls (`write_plane_it.rs::
//! memory_fence_matches_write_plane_outcomes`) so the in-memory semantics
//! cannot drift off the real fence.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aster_capsule::{
    CommitError, Document, DocumentId, MvccStore, ObservedWindow, ReadSet, Timestamp, WriteSet,
};

use crate::StoreError;

/// One point-in-time commit request, already reduced by the committer to
/// the theorem's validated ingredients: the declared authenticated
/// observations (points + range windows) and the policy-authorized write
/// set. Capsule verification happens in the committer, not here.
pub struct FenceInput<'a> {
    pub tenant: &'a str,
    pub deployment: &'a str,
    /// Epoch this committer acquired from the lease authority (`e_now`).
    pub committer_epoch: u64,
    /// Epoch sealed in the capsule context (`ctx.e`). V2 requires it to
    /// equal the live lease epoch — passing it separately lets the fence
    /// reject a stale-epoch capsule even when the committer itself is
    /// current.
    pub context_epoch: u64,
    /// The capsule snapshot `s`.
    pub snapshot: Timestamp,
    /// Declared point observations (present, absent, and tombstone reads
    /// all conflict on any write event touching the key — the theorem's
    /// `Changed({k}; (s,h])` predicate).
    pub read_points: &'a [DocumentId],
    /// Declared range observations, already reduced to conflict windows.
    pub read_windows: &'a [ObservedWindow],
    /// The write set: `Some(document)` puts, `None` deletes.
    pub writes: &'a [(DocumentId, Option<Document>)],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitOutcome {
    Committed {
        ts: Timestamp,
    },
    /// A committed write in `(s, h]` intersects a declared observation.
    Conflict {
        key: DocumentId,
    },
    /// Lease epoch mismatch — the committer or the capsule context is
    /// stale relative to the storage lease authority.
    StaleEpoch {
        lease_epoch: u64,
    },
    /// The capsule claims a snapshot the log has never committed
    /// (S-SNAPSHOT violation — honest brokers cannot produce this).
    SnapshotBeyondHorizon {
        horizon: Timestamp,
    },
    /// Validation history below the snapshot is no longer retained
    /// (Lemma R: coverage `g <= s` failed). Retry from a fresh snapshot —
    /// which in the v0.7 prototype means a fresh broker launch: brokerd
    /// pins its snapshot at boot, and the demo read plane never observes
    /// `aster.log` commits (the F9 integration decision the paper scopes
    /// out). The retry contract is the fence's; the prototype's snapshot
    /// plumbing is what limits how a caller can act on it.
    RetentionViolated {
        low_watermark: Timestamp,
    },
    /// Mutations-only path: an empty write set never enters the log.
    EmptyWriteSet,
}

/// Storage half of the write path: lease authority + atomic commit fence.
pub trait CommitFence: Send + Sync {
    /// Acquire (or take over) the single-writer lease for a deployment.
    /// Epochs are strictly increasing and never reused, even across
    /// failback A→B→A (theorem A7 / referee F1).
    fn acquire_lease(
        &self,
        tenant: &str,
        deployment: &str,
        holder: &str,
    ) -> Result<u64, StoreError>;

    /// The commit fence: run V2–V5 against the committed suffix `(s, h]`
    /// and append the write set atomically at a fresh `c > h`.
    fn commit(&self, input: &FenceInput<'_>) -> Result<CommitOutcome, StoreError>;
}

/// In-memory `CommitFence` over the prototype `MvccStore`, honoring the
/// same admission semantics as the Postgres `WritePlane`: epoch equality
/// against a live lease, a stable horizon, point conflicts by version
/// equality across `(s, h]`, window conflicts via
/// `ObservedWindow::contains` over the keys changed after the snapshot,
/// and a monotone fresh commit timestamp.
///
/// The lease mutex is the in-memory analog of the fence's lease-row
/// `FOR UPDATE`: every commit enters through it, so validation and append
/// share one horizon as long as all writes flow through the fence. A
/// direct `MvccStore::seed` bypasses it — tests use that deliberately to
/// fake a foreign writer; the final `MvccStore::commit` still re-validates
/// the declared points under the store's own lock, so even that race
/// cannot commit over a changed declared read.
pub struct MemoryFence {
    store: Arc<MvccStore>,
    /// (tenant, deployment) → current lease epoch.
    leases: Mutex<HashMap<(String, String), u64>>,
}

impl MemoryFence {
    pub fn new(store: Arc<MvccStore>) -> Self {
        Self {
            store,
            leases: Mutex::new(HashMap::new()),
        }
    }

    /// Pin the lease epoch directly — the `ASTER_LEASE_EPOCH` prototype
    /// stand-in for memory-mode brokerds. A real authority only ever
    /// moves epochs through [`CommitFence::acquire_lease`]; this exists
    /// because the in-memory world has no durable authority to ask.
    pub fn seed_lease(&self, tenant: &str, deployment: &str, epoch: u64) {
        self.leases
            .lock()
            .expect("memory fence lease lock")
            .insert((tenant.to_string(), deployment.to_string()), epoch);
    }
}

impl CommitFence for MemoryFence {
    /// `holder` is accepted for signature parity but not recorded — the
    /// in-memory fence keeps no ownership ledger, only the epoch counter
    /// whose strict growth is what the fence checks depend on.
    fn acquire_lease(
        &self,
        tenant: &str,
        deployment: &str,
        _holder: &str,
    ) -> Result<u64, StoreError> {
        let mut leases = self.leases.lock().expect("memory fence lease lock");
        let epoch = leases
            .entry((tenant.to_string(), deployment.to_string()))
            .and_modify(|epoch| *epoch += 1)
            .or_insert(1);
        Ok(*epoch)
    }

    fn commit(&self, input: &FenceInput<'_>) -> Result<CommitOutcome, StoreError> {
        if input.writes.is_empty() {
            return Ok(CommitOutcome::EmptyWriteSet);
        }
        // Fence entry: the lease lock serializes every commit (see the
        // struct docs for the seed caveat).
        let leases = self.leases.lock().expect("memory fence lease lock");
        let lease_epoch = *leases
            .get(&(input.tenant.to_string(), input.deployment.to_string()))
            .ok_or_else(|| StoreError::Backend("no lease acquired for this deployment".into()))?;
        // V2: both the committer and the capsule context must match the
        // live lease epoch.
        if lease_epoch != input.committer_epoch || input.context_epoch != lease_epoch {
            return Ok(CommitOutcome::StaleEpoch { lease_epoch });
        }

        // Stable horizon h.
        let h = self.store.snapshot_ts();
        if input.snapshot > h {
            return Ok(CommitOutcome::SnapshotBeyondHorizon { horizon: h });
        }
        // Retention (Lemma R): the in-memory store never GCs, so the
        // low-watermark is pinned at 0 and coverage `g <= s` always holds
        // — `RetentionViolated` is unreachable here by construction, not
        // by a skipped check.

        // V4, points: a write event in (s, h] is exactly a version change
        // between the two reads (revisions strictly increase), which
        // covers present, absent, and tombstone observations alike.
        for key in input.read_points {
            if self.store.read_at(key, input.snapshot).version != self.store.read_at(key, h).version
            {
                return Ok(CommitOutcome::Conflict { key: key.clone() });
            }
        }
        // V4, windows: every key with a write event in (s, h] is tested
        // against the declared conflict windows.
        if !input.read_windows.is_empty() {
            for key in self.store.changed_keys_in(input.snapshot, h) {
                if input
                    .read_windows
                    .iter()
                    .any(|window| window.contains(&key))
                {
                    return Ok(CommitOutcome::Conflict { key });
                }
            }
        }

        // Atomic append at fresh c = h + 1. `MvccStore::commit` re-checks
        // the declared point versions against the live head under the
        // store's own lock, so the append cannot land over a declared
        // read that changed since the checks above.
        let mut read_set = ReadSet::default();
        for key in input.read_points {
            read_set.observe(key.clone(), self.store.read_at(key, input.snapshot).version);
        }
        let mut write_set = WriteSet::default();
        for (key, document) in input.writes {
            match document {
                Some(document) => write_set.put(key.clone(), document.clone()),
                None => write_set.delete(key.clone()),
            }
        }
        match self.store.commit(input.snapshot, &read_set, &write_set) {
            Ok(ts) => Ok(CommitOutcome::Committed { ts }),
            Err(CommitError::Conflict { key, .. }) => Ok(CommitOutcome::Conflict { key }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::{doc_with_i64, KeyInterval, RangeCertificate, ScanDirection, ScanStop};

    const TENANT: &str = "tenant-fence";
    const DEPLOYMENT: &str = "dep-fence";

    fn fence() -> (MemoryFence, Arc<MvccStore>) {
        let store = Arc::new(MvccStore::new());
        (MemoryFence::new(store.clone()), store)
    }

    fn put(key: &str, value: i64) -> (DocumentId, Option<Document>) {
        (DocumentId::new(key), Some(doc_with_i64("value", value)))
    }

    fn commit_blind(
        fence: &MemoryFence,
        epoch: u64,
        snapshot: u64,
        key: &str,
        value: i64,
    ) -> CommitOutcome {
        fence
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
        let (fence, _store) = fence();
        let a1 = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire a");
        let b = fence
            .acquire_lease(TENANT, DEPLOYMENT, "b")
            .expect("acquire b");
        let a2 = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire a again");
        assert_eq!((a1, b, a2), (1, 2, 3));
    }

    #[test]
    fn empty_write_set_never_enters_the_log() {
        let (fence, store) = fence();
        let outcome = fence
            .commit(&FenceInput {
                tenant: TENANT,
                deployment: DEPLOYMENT,
                committer_epoch: 1,
                context_epoch: 1,
                snapshot: 0,
                read_points: &[],
                read_windows: &[],
                writes: &[],
            })
            .expect("commit");
        assert_eq!(outcome, CommitOutcome::EmptyWriteSet);
        assert_eq!(store.snapshot_ts(), 0, "nothing may be appended");
    }

    #[test]
    fn commit_without_lease_is_a_backend_error() {
        let (fence, _store) = fence();
        let error = commit_no_lease(&fence);
        assert!(matches!(error, StoreError::Backend(_)), "got {error:?}");
    }

    fn commit_no_lease(fence: &MemoryFence) -> StoreError {
        fence
            .commit(&FenceInput {
                tenant: TENANT,
                deployment: DEPLOYMENT,
                committer_epoch: 1,
                context_epoch: 1,
                snapshot: 0,
                read_points: &[],
                read_windows: &[],
                writes: &[put("docs/x", 1)],
            })
            .expect_err("no lease acquired must error")
    }

    #[test]
    fn stale_committer_or_context_epoch_is_fenced() {
        let (fence, _store) = fence();
        let epoch = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire");
        assert!(matches!(
            commit_blind(&fence, epoch, 0, "docs/x", 1),
            CommitOutcome::Committed { .. }
        ));
        let epoch_b = fence
            .acquire_lease(TENANT, DEPLOYMENT, "b")
            .expect("acquire b");
        // Old committer + old context.
        assert_eq!(
            commit_blind(&fence, epoch, 1, "docs/x", 2),
            CommitOutcome::StaleEpoch {
                lease_epoch: epoch_b
            }
        );
        // Current committer, stale capsule context.
        let mixed = fence
            .commit(&FenceInput {
                tenant: TENANT,
                deployment: DEPLOYMENT,
                committer_epoch: epoch_b,
                context_epoch: epoch,
                snapshot: 1,
                read_points: &[],
                read_windows: &[],
                writes: &[put("docs/x", 2)],
            })
            .expect("mixed epoch commit");
        assert_eq!(
            mixed,
            CommitOutcome::StaleEpoch {
                lease_epoch: epoch_b
            }
        );
    }

    #[test]
    fn snapshot_beyond_horizon_is_rejected() {
        let (fence, _store) = fence();
        let epoch = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire");
        assert_eq!(
            commit_blind(&fence, epoch, 5, "docs/x", 1),
            CommitOutcome::SnapshotBeyondHorizon { horizon: 0 }
        );
    }

    #[test]
    fn point_conflicts_on_any_write_event_including_absence_and_tombstone() {
        let (fence, store) = fence();
        let epoch = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire");
        assert!(matches!(
            commit_blind(&fence, epoch, 0, "docs/seed", 1),
            CommitOutcome::Committed { .. }
        ));
        let s = store.snapshot_ts();

        // Absence flip: reader observed docs/ghost absent at s; a foreign
        // writer seeds it after issuance.
        store.seed(DocumentId::new("docs/ghost"), doc_with_i64("value", 5));
        let outcome = fence
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

        // Tombstone write (R5): a DELETE of a declared read is a write
        // event and must conflict too.
        let s = store.snapshot_ts();
        let deleted = fence
            .commit(&FenceInput {
                tenant: TENANT,
                deployment: DEPLOYMENT,
                committer_epoch: epoch,
                context_epoch: epoch,
                snapshot: s,
                read_points: &[],
                read_windows: &[],
                writes: &[(DocumentId::new("docs/seed"), None)],
            })
            .expect("delete");
        assert!(matches!(deleted, CommitOutcome::Committed { .. }));
        let outcome = fence
            .commit(&FenceInput {
                tenant: TENANT,
                deployment: DEPLOYMENT,
                committer_epoch: epoch,
                context_epoch: epoch,
                snapshot: s,
                read_points: &[DocumentId::new("docs/seed")],
                read_windows: &[],
                writes: &[put("docs/dependent", 1)],
            })
            .expect("reader fence");
        assert_eq!(
            outcome,
            CommitOutcome::Conflict {
                key: DocumentId::new("docs/seed")
            }
        );
    }

    #[test]
    fn window_conflicts_mirror_certificate_semantics() {
        let (fence, store) = fence();
        let epoch = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire");
        for (i, key) in ["docs/a", "docs/b", "docs/c"].iter().enumerate() {
            assert!(matches!(
                commit_blind(&fence, epoch, i as u64, key, 1),
                CommitOutcome::Committed { .. }
            ));
        }
        let s = store.snapshot_ts();
        // Interfering insert after the snapshot.
        assert!(matches!(
            commit_blind(&fence, epoch, s, "docs/zz", 7),
            CommitOutcome::Committed { .. }
        ));

        // An EXHAUSTED scan observed the whole prefix: the insert is a
        // phantom in its exhaustion gap and must conflict.
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
        let outcome = fence
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

        // A BOUNDARY scan observed only through docs/c — the same insert
        // is past the boundary and must NOT abort this commit (F2).
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
        let outcome = fence
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
    fn commit_timestamps_are_monotone_and_replay_is_a_second_transaction() {
        let (fence, store) = fence();
        let epoch = fence
            .acquire_lease(TENANT, DEPLOYMENT, "a")
            .expect("acquire");
        let first = commit_blind(&fence, epoch, 0, "docs/r", 1);
        let second = commit_blind(&fence, epoch, 0, "docs/r", 1);
        match (first, second) {
            (CommitOutcome::Committed { ts: t1 }, CommitOutcome::Committed { ts: t2 }) => {
                assert!(t2 > t1)
            }
            other => panic!("expected two commits, got {other:?}"),
        }
        // The committed writes are visible through the shared store.
        let tip = store.snapshot_ts();
        assert!(store
            .read_at(&DocumentId::new("docs/r"), tip)
            .document
            .is_some());
    }
}
