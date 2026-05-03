use aster_capsule::{
    doc_with_i64, CapsuleSealKey, DeploymentId, DocumentId, MvccStore, SealContext, SealError,
    SealedCapsule, TenantId, Value,
};
use aster_v8cell::V8SandboxCell;

#[test]
fn sealed_capsule_is_cell_bound_and_tamper_evident() {
    let store = MvccStore::new();
    let tenant = TenantId::new("tenant-sealed");
    let deployment = DeploymentId::new("dep-sealed");
    let key = DocumentId::new("docs/alpha");
    store.seed(key.clone(), doc_with_i64("value", 11));
    let capsule = store.build_capsule(tenant, deployment, store.snapshot_ts(), vec![key.clone()]);

    let seal_key = CapsuleSealKey::derive_for_tests(b"host-e2e-seal-key");
    let cell_a = SealContext::new("cell-a", 7);
    let sealed = SealedCapsule::new(capsule, &seal_key, &cell_a);
    assert!(sealed.verify(&seal_key, &cell_a).is_ok());

    let cell_b = SealContext::new("cell-b", 7);
    assert_eq!(sealed.verify(&seal_key, &cell_b), Err(SealError::WrongCell));

    let mut tampered = sealed.clone();
    tampered.capsule_mut_for_test().hydrate_point(
        key,
        aster_capsule::VersionedDocument {
            version: Some(99),
            document: Some(doc_with_i64("value", 12)),
        },
    );
    assert_eq!(
        tampered.verify(&seal_key, &cell_a),
        Err(SealError::DigestMismatch)
    );
}

#[test]
fn v8_read_trap_continuation_executes_against_hydrated_capsule() {
    let tenant = TenantId::new("tenant-v8-e2e");
    let deployment = DeploymentId::new("dep-v8-e2e");
    let store = MvccStore::new();
    store.seed(DocumentId::new("counters/a"), doc_with_i64("value", 40));
    store.seed(DocumentId::new("counters/b"), doc_with_i64("value", 2));
    let ts = store.snapshot_ts();

    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);
    let source = r#"
        async function main() {
          const a = await Aster.read("counters/a", "value");
          const b = await Aster.read("counters/b", "value");
          return a + b;
        }
    "#;
    let result = cell
        .execute_async_main(
            &store,
            tenant,
            deployment,
            ts,
            vec![DocumentId::new("counters/a")],
            source,
        )
        .expect("V8 read trap continuation should finish");

    assert_eq!(result.output, Value::Int(42));
    assert_eq!(result.traps, 1);
    assert_ne!(result.capsule_hash, 0);
}
