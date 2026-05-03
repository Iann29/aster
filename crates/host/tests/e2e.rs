use aster_capsule::{doc_with_i64, DeploymentId, DocumentId, TenantId, Value};
use aster_host::AsterHost;
use aster_runner::{FunctionKind, Invocation, Program, SandboxCell};

#[test]
fn query_mutation_and_action_traverse_capsule_pipeline() {
    let tenant = TenantId::new("tenant-red");
    let deployment = DeploymentId::new("deploy-prod");
    let host = AsterHost::new(tenant.clone(), deployment.clone(), 4, 16);
    host.seed_i64("counters/main", "value", 41);
    host.seed_i64("counters/other", "value", 1);

    let query = Invocation {
        tenant: tenant.clone(),
        deployment: deployment.clone(),
        snapshot_ts: host.snapshot_ts(),
        kind: FunctionKind::Query,
        program: Program::Sum {
            keys: vec![
                DocumentId::new("counters/main"),
                DocumentId::new("counters/other"),
            ],
            field: "value".to_string(),
        },
        prewarm: vec![DocumentId::new("counters/main")],
    };
    let query_outcome = host.execute(query).expect("query should complete");
    assert_eq!(query_outcome.execution.output, Value::Int(42));
    assert_eq!(
        query_outcome.execution.traps, 1,
        "the second key was hydrated as a read trap"
    );
    assert_eq!(query_outcome.commit_ts, None);

    let mutation = Invocation {
        tenant: tenant.clone(),
        deployment: deployment.clone(),
        snapshot_ts: host.snapshot_ts(),
        kind: FunctionKind::Mutation,
        program: Program::Increment {
            key: DocumentId::new("counters/main"),
            field: "value".to_string(),
            by: 1,
        },
        prewarm: vec![],
    };
    let mutation_outcome = host.execute(mutation).expect("mutation should commit");
    assert_eq!(mutation_outcome.execution.output, Value::Int(42));
    assert!(mutation_outcome.commit_ts.is_some());

    let action = Invocation {
        tenant,
        deployment,
        snapshot_ts: host.snapshot_ts(),
        kind: FunctionKind::Action,
        program: Program::EffectAfterRead {
            key: DocumentId::new("counters/main"),
            field: "value".to_string(),
            effect: "send-webhook".to_string(),
            idempotency_key: "tenant-red/deploy-prod/webhook/42".to_string(),
        },
        prewarm: vec![],
    };
    let action_outcome = host
        .execute(action)
        .expect("action should create an effect fence");
    assert_eq!(
        action_outcome.execution.output,
        Value::Text("effect:send-webhook".to_string())
    );
    let effects = host.effect_log();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].observed, Some(42));
}

#[test]
fn two_mutation_results_over_same_snapshot_cannot_both_commit() {
    let tenant = TenantId::new("tenant-blue");
    let deployment = DeploymentId::new("deploy-prod");
    let host = AsterHost::new(tenant.clone(), deployment.clone(), 1, 16);
    host.store()
        .seed(DocumentId::new("counters/main"), doc_with_i64("value", 1));
    let base_ts = host.snapshot_ts();

    let cell = SandboxCell::new("cell-conflict", tenant.clone(), deployment.clone(), 8);
    let invocation = Invocation {
        tenant,
        deployment,
        snapshot_ts: base_ts,
        kind: FunctionKind::Mutation,
        program: Program::Increment {
            key: DocumentId::new("counters/main"),
            field: "value".to_string(),
            by: 1,
        },
        prewarm: vec![DocumentId::new("counters/main")],
    };

    let first = cell
        .execute(host.store(), invocation.clone())
        .expect("first execution");
    let second = cell
        .execute(host.store(), invocation)
        .expect("second execution");

    host.store()
        .commit(base_ts, &first.read_set, &first.write_set)
        .expect("first commit wins");
    let loser = host
        .store()
        .commit(base_ts, &second.read_set, &second.write_set);
    assert!(loser.is_err(), "OCC must reject the stale mutation result");
}
