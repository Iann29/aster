use aster_broker::{CapsuleBrokerClient, LocalCapsuleBroker};
use aster_capsule::{
    doc_with_i64, CapsuleSealKey, DeploymentId, DocumentId, MvccStore, SealContext, TenantId, Value,
};
use aster_v8cell::V8SandboxCell;

#[test]
fn v8_cell_uses_broker_capability_instead_of_store_handle() {
    let tenant = TenantId::new("tenant-boundary");
    let deployment = DeploymentId::new("dep-boundary");
    let store = MvccStore::new();
    store.seed(DocumentId::new("items/a"), doc_with_i64("value", 19));
    store.seed(DocumentId::new("items/b"), doc_with_i64("value", 23));
    let ts = store.snapshot_ts();

    // The broker owns the read-capable store and seal key. The V8 cell below
    // receives only `&impl CapsuleBrokerClient`, which is the code-level
    // boundary v0.1 lacked.
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"host-boundary-test"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
    let source = r#"
        async function main() {
          const a = await Aster.read("items/a", "value");
          const b = await Aster.read("items/b", "value");
          return a + b;
        }
    "#;

    let result = cell
        .execute_async_main_with_broker(
            &broker,
            "cell-boundary-1",
            1,
            tenant,
            deployment,
            ts,
            vec![DocumentId::new("items/a")],
            source,
        )
        .expect("V8 broker-backed execution should complete");

    assert_eq!(result.output, Value::Int(42));
    assert_eq!(result.traps, 1);
}

#[test]
fn broker_hydrate_rejects_wrong_cell_context() {
    let tenant = TenantId::new("tenant-boundary");
    let deployment = DeploymentId::new("dep-boundary");
    let store = MvccStore::new();
    let key = DocumentId::new("items/a");
    store.seed(key.clone(), doc_with_i64("value", 42));
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"host-boundary-test"),
    );
    let cell_a = SealContext::new("cell-a", 1);
    let cell_b = SealContext::new("cell-b", 1);
    let sealed = broker
        .initial_capsule(&cell_a, tenant, deployment, store.snapshot_ts(), vec![])
        .expect("initial capsule");

    let rejected = broker.hydrate_point(&cell_b, sealed, key);
    assert!(rejected.is_err(), "wrong-cell replay must be rejected");
}
