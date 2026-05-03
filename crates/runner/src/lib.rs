//! Runner cells for Aster's capsule execution model.
//!
//! A cell is the unit that would become a jailed OS process in production. The
//! prototype keeps it in-process so the tests are deterministic and easy to run.
//! The invariants are still explicit: a cell is pinned to one tenant/deployment,
//! it starts with an immutable snapshot capsule, and it can only ask the broker
//! to hydrate data via `ReadTrap` values scoped to the same capsule.

use aster_capsule::{
    doc_with_i64, DeploymentId, DocumentId, MvccStore, ReadSet, ReadTrap, SnapshotCapsule,
    TenantId, Value, WriteSet,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FunctionKind {
    Query,
    Mutation,
    Action,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Program {
    /// Read a set of documents and sum the integer field.
    Sum {
        keys: Vec<DocumentId>,
        field: String,
    },
    /// Read one counter document and write back an incremented value.
    Increment {
        key: DocumentId,
        field: String,
        by: i64,
    },
    /// Produce an idempotent side-effect description after reading a key.
    ///
    /// Convex actions cannot be automatically retried once they do external IO.
    /// Aster therefore returns an effect fence: the host may perform the effect
    /// only after checking that the capsule is fresh enough for the deployment's
    /// action policy. This prototype records the fence and does not perform IO.
    EffectAfterRead {
        key: DocumentId,
        field: String,
        effect: String,
        idempotency_key: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectFence {
    pub effect: String,
    pub idempotency_key: String,
    pub snapshot_hash: u64,
    pub observed: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub kind: FunctionKind,
    pub output: Value,
    pub read_set: ReadSet,
    pub write_set: WriteSet,
    pub effect: Option<EffectFence>,
    pub capsule_hash: u64,
    pub traps: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    Complete(ExecutionResult),
    Trap(ReadTrap),
}

impl Program {
    pub fn evaluate(&self, kind: FunctionKind, capsule: &SnapshotCapsule) -> StepOutcome {
        match self {
            Program::Sum { keys, field } => evaluate_sum(kind, capsule, keys, field),
            Program::Increment { key, field, by } => {
                evaluate_increment(kind, capsule, key, field, *by)
            }
            Program::EffectAfterRead {
                key,
                field,
                effect,
                idempotency_key,
            } => evaluate_effect(kind, capsule, key, field, effect, idempotency_key),
        }
    }
}

fn evaluate_sum(
    kind: FunctionKind,
    capsule: &SnapshotCapsule,
    keys: &[DocumentId],
    field: &str,
) -> StepOutcome {
    let mut read_set = ReadSet::default();
    let mut total = 0_i64;
    for key in keys {
        let Some(value) = capsule.get(key) else {
            return StepOutcome::Trap(ReadTrap::Point(key.clone()));
        };
        read_set.observe(key.clone(), value.version);
        if let Some(document) = &value.document {
            if let Some(number) = document.get(field).and_then(Value::as_i64) {
                total += number;
            }
        }
    }
    StepOutcome::Complete(ExecutionResult {
        kind,
        output: Value::Int(total),
        read_set,
        write_set: WriteSet::default(),
        effect: None,
        capsule_hash: capsule.root_hash,
        traps: 0,
    })
}

fn evaluate_increment(
    kind: FunctionKind,
    capsule: &SnapshotCapsule,
    key: &DocumentId,
    field: &str,
    by: i64,
) -> StepOutcome {
    let Some(value) = capsule.get(key) else {
        return StepOutcome::Trap(ReadTrap::Point(key.clone()));
    };
    let mut read_set = ReadSet::default();
    read_set.observe(key.clone(), value.version);
    let current = value
        .document
        .as_ref()
        .and_then(|document| document.get(field))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let next = current + by;
    let mut write_set = WriteSet::default();
    write_set.put(key.clone(), doc_with_i64(field, next));
    StepOutcome::Complete(ExecutionResult {
        kind,
        output: Value::Int(next),
        read_set,
        write_set,
        effect: None,
        capsule_hash: capsule.root_hash,
        traps: 0,
    })
}

fn evaluate_effect(
    kind: FunctionKind,
    capsule: &SnapshotCapsule,
    key: &DocumentId,
    field: &str,
    effect: &str,
    idempotency_key: &str,
) -> StepOutcome {
    let Some(value) = capsule.get(key) else {
        return StepOutcome::Trap(ReadTrap::Point(key.clone()));
    };
    let mut read_set = ReadSet::default();
    read_set.observe(key.clone(), value.version);
    let observed = value
        .document
        .as_ref()
        .and_then(|document| document.get(field))
        .and_then(Value::as_i64);
    StepOutcome::Complete(ExecutionResult {
        kind,
        output: Value::Text(format!("effect:{effect}")),
        read_set,
        write_set: WriteSet::default(),
        effect: Some(EffectFence {
            effect: effect.to_string(),
            idempotency_key: idempotency_key.to_string(),
            snapshot_hash: capsule.root_hash,
            observed,
        }),
        capsule_hash: capsule.root_hash,
        traps: 0,
    })
}

#[derive(Debug)]
pub struct CapsuleBroker<'a> {
    store: &'a MvccStore,
}

impl<'a> CapsuleBroker<'a> {
    pub fn new(store: &'a MvccStore) -> Self {
        Self { store }
    }

    pub fn hydrate(&self, capsule: &mut SnapshotCapsule, trap: ReadTrap) {
        match trap {
            ReadTrap::Point(key) => {
                let value = self.store.read_at(&key, capsule.ts);
                capsule.hydrate_point(key, value);
            }
            ReadTrap::Prefix { prefix, limit } => {
                for (key, value) in self.store.prefix_at(&prefix, limit, capsule.ts) {
                    capsule.hydrate_point(key, value);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub tenant: TenantId,
    pub deployment: DeploymentId,
    pub snapshot_ts: u64,
    pub kind: FunctionKind,
    pub program: Program,
    pub prewarm: Vec<DocumentId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CellError {
    WrongTenant,
    WrongDeployment,
    TooManyTraps { limit: usize },
    KindProgramMismatch,
}

#[derive(Debug)]
pub struct SandboxCell {
    pub cell_id: String,
    tenant: TenantId,
    deployment: DeploymentId,
    max_traps: usize,
}

impl SandboxCell {
    pub fn new(
        cell_id: impl Into<String>,
        tenant: TenantId,
        deployment: DeploymentId,
        max_traps: usize,
    ) -> Self {
        Self {
            cell_id: cell_id.into(),
            tenant,
            deployment,
            max_traps,
        }
    }

    pub fn execute(
        &self,
        store: &MvccStore,
        invocation: Invocation,
    ) -> Result<ExecutionResult, CellError> {
        if invocation.tenant != self.tenant {
            return Err(CellError::WrongTenant);
        }
        if invocation.deployment != self.deployment {
            return Err(CellError::WrongDeployment);
        }
        if !kind_matches(&invocation.kind, &invocation.program) {
            return Err(CellError::KindProgramMismatch);
        }

        let mut capsule = store.build_capsule(
            invocation.tenant.clone(),
            invocation.deployment.clone(),
            invocation.snapshot_ts,
            invocation.prewarm.clone(),
        );
        let broker = CapsuleBroker::new(store);
        let mut traps = 0_usize;
        loop {
            match invocation.program.evaluate(invocation.kind, &capsule) {
                StepOutcome::Complete(mut result) => {
                    result.traps = traps;
                    result.capsule_hash = capsule.root_hash;
                    if let Some(effect) = &mut result.effect {
                        effect.snapshot_hash = capsule.root_hash;
                    }
                    return Ok(result);
                }
                StepOutcome::Trap(trap) => {
                    if traps >= self.max_traps {
                        return Err(CellError::TooManyTraps {
                            limit: self.max_traps,
                        });
                    }
                    traps += 1;
                    broker.hydrate(&mut capsule, trap);
                }
            }
        }
    }
}

fn kind_matches(kind: &FunctionKind, program: &Program) -> bool {
    matches!(
        (kind, program),
        (FunctionKind::Query, Program::Sum { .. })
            | (FunctionKind::Mutation, Program::Increment { .. })
            | (FunctionKind::Action, Program::EffectAfterRead { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::doc_with_i64;

    #[test]
    fn query_hydrates_missing_key_with_read_trap() {
        let store = MvccStore::new();
        let tenant = TenantId::new("tenant-a");
        let deployment = DeploymentId::new("dep-a");
        store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 7));
        let ts = store.snapshot_ts();
        let cell = SandboxCell::new("cell-1", tenant.clone(), deployment.clone(), 8);
        let result = cell
            .execute(
                &store,
                Invocation {
                    tenant,
                    deployment,
                    snapshot_ts: ts,
                    kind: FunctionKind::Query,
                    program: Program::Sum {
                        keys: vec![DocumentId::new("counters/a")],
                        field: "value".to_string(),
                    },
                    prewarm: vec![],
                },
            )
            .expect("query should complete");
        assert_eq!(result.output, Value::Int(7));
        assert_eq!(result.traps, 1);
    }
}
