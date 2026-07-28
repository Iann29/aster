use std::time::{Duration, Instant};

use aster_capsule::{DeploymentId, MvccStore, TenantId};
use aster_v8cell::{V8CellError, V8SandboxCell};

#[test]
fn infinite_javascript_is_terminated_by_internal_watchdog() {
    let tenant = TenantId::new("tenant-timeout");
    let deployment = DeploymentId::new("deployment-timeout");
    let store = MvccStore::new();
    let cell = V8SandboxCell::with_resource_limits(
        tenant.clone(),
        deployment.clone(),
        8,
        128 * 1024 * 1024,
        Duration::from_millis(50),
    );
    let started = Instant::now();
    let error = cell
        .execute_async_main(
            &store,
            tenant,
            deployment,
            store.snapshot_ts(),
            Vec::new(),
            "async function main() { while (true) {} }",
        )
        .expect_err("infinite loop must be terminated");
    assert_eq!(error, V8CellError::ExecutionTimedOut { limit_ms: 50 });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "watchdog did not terminate promptly: {:?}",
        started.elapsed()
    );
}
