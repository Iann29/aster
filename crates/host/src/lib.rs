//! Aster host facade.
//!
//! The production adapter sits between an unmodified `convex-backend` remote
//! runner client and Aster's capsule fabric. This crate demonstrates the same
//! responsibility in-process: choose a tenant-pinned cell, execute a query,
//! mutation, or action against an immutable capsule, and send mutation results
//! through a single OCC committer.

use std::sync::Mutex;

use aster_capsule::{CommitError, DeploymentId, DocumentId, MvccStore, TenantId, Timestamp};
use aster_runner::{
    CellError, EffectFence, ExecutionResult, FunctionKind, Invocation, SandboxCell,
};

#[derive(Debug)]
pub struct AsterHost {
    tenant: TenantId,
    deployment: DeploymentId,
    store: MvccStore,
    cells: Vec<SandboxCell>,
    next_cell: Mutex<usize>,
    effect_log: Mutex<Vec<EffectFence>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOutcome {
    pub execution: ExecutionResult,
    pub commit_ts: Option<Timestamp>,
}

#[derive(Debug)]
pub enum HostError {
    NoCells,
    Cell(CellError),
    Commit(CommitError),
}

impl From<CellError> for HostError {
    fn from(value: CellError) -> Self {
        Self::Cell(value)
    }
}

impl From<CommitError> for HostError {
    fn from(value: CommitError) -> Self {
        Self::Commit(value)
    }
}

impl AsterHost {
    pub fn new(
        tenant: TenantId,
        deployment: DeploymentId,
        cell_count: usize,
        max_traps_per_invocation: usize,
    ) -> Self {
        let cells = (0..cell_count)
            .map(|idx| {
                SandboxCell::new(
                    format!("cell-{idx}"),
                    tenant.clone(),
                    deployment.clone(),
                    max_traps_per_invocation,
                )
            })
            .collect();
        Self {
            tenant,
            deployment,
            store: MvccStore::new(),
            cells,
            next_cell: Mutex::new(0),
            effect_log: Mutex::new(Vec::new()),
        }
    }

    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    pub fn deployment(&self) -> &DeploymentId {
        &self.deployment
    }

    pub fn store(&self) -> &MvccStore {
        &self.store
    }

    pub fn seed_i64(&self, key: impl Into<String>, field: &str, value: i64) -> Timestamp {
        self.store.seed(
            DocumentId::new(key.into()),
            aster_capsule::doc_with_i64(field, value),
        )
    }

    pub fn snapshot_ts(&self) -> Timestamp {
        self.store.snapshot_ts()
    }

    pub fn execute(&self, invocation: Invocation) -> Result<HostOutcome, HostError> {
        let cell = self.pick_cell()?;
        let result = cell.execute(&self.store, invocation.clone())?;
        let commit_ts = match result.kind {
            FunctionKind::Mutation => Some(self.store.commit(
                invocation.snapshot_ts,
                &result.read_set,
                &result.write_set,
            )?),
            FunctionKind::Query => None,
            FunctionKind::Action => {
                if let Some(effect) = &result.effect {
                    self.effect_log
                        .lock()
                        .expect("effect log mutex poisoned")
                        .push(effect.clone());
                }
                None
            }
        };
        Ok(HostOutcome {
            execution: result,
            commit_ts,
        })
    }

    pub fn effect_log(&self) -> Vec<EffectFence> {
        self.effect_log
            .lock()
            .expect("effect log mutex poisoned")
            .clone()
    }

    fn pick_cell(&self) -> Result<&SandboxCell, HostError> {
        if self.cells.is_empty() {
            return Err(HostError::NoCells);
        }
        let mut next = self.next_cell.lock().expect("cell cursor mutex poisoned");
        let idx = *next % self.cells.len();
        *next = next.wrapping_add(1);
        Ok(&self.cells[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster_capsule::Value;
    use aster_runner::Program;

    #[test]
    fn host_commits_mutation_and_records_action_fence() {
        let tenant = TenantId::new("tenant-a");
        let deployment = DeploymentId::new("dep-a");
        let host = AsterHost::new(tenant.clone(), deployment.clone(), 2, 8);
        host.seed_i64("counters/main", "value", 10);

        let mutation = Invocation {
            tenant: tenant.clone(),
            deployment: deployment.clone(),
            snapshot_ts: host.snapshot_ts(),
            kind: FunctionKind::Mutation,
            program: Program::Increment {
                key: DocumentId::new("counters/main"),
                field: "value".to_string(),
                by: 5,
            },
            prewarm: vec![],
        };
        let mutation_outcome = host.execute(mutation).expect("mutation should commit");
        assert_eq!(mutation_outcome.execution.output, Value::Int(15));
        assert!(mutation_outcome.commit_ts.is_some());

        let action = Invocation {
            tenant,
            deployment,
            snapshot_ts: host.snapshot_ts(),
            kind: FunctionKind::Action,
            program: Program::EffectAfterRead {
                key: DocumentId::new("counters/main"),
                field: "value".to_string(),
                effect: "notify:webhook".to_string(),
                idempotency_key: "tenant-a/dep-a/test/1".to_string(),
            },
            prewarm: vec![],
        };
        let action_outcome = host.execute(action).expect("action should fence");
        assert_eq!(
            action_outcome.execution.output,
            Value::Text("effect:notify:webhook".to_string())
        );
        assert_eq!(host.effect_log().len(), 1);
    }
}
