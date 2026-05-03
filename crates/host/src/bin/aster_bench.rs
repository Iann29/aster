use std::env;
use std::time::{Duration, Instant};

use aster_capsule::{DeploymentId, DocumentId, TenantId};
use aster_host::AsterHost;
use aster_runner::{FunctionKind, Invocation, Program};

fn main() {
    let iterations = env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(10_000);
    let keys = env::args()
        .nth(2)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(32);

    let tenant = TenantId::new("bench-tenant");
    let deployment = DeploymentId::new("bench-deployment");
    let host = AsterHost::new(tenant.clone(), deployment.clone(), 8, keys + 8);
    for idx in 0..keys {
        host.seed_i64(format!("items/{idx:04}"), "value", 1);
    }
    host.seed_i64("counter/main", "value", 0);

    let key_vec: Vec<DocumentId> = (0..keys)
        .map(|idx| DocumentId::new(format!("items/{idx:04}")))
        .collect();

    let warm_query = bench_loop(iterations, || {
        let invocation = Invocation {
            tenant: tenant.clone(),
            deployment: deployment.clone(),
            snapshot_ts: host.snapshot_ts(),
            kind: FunctionKind::Query,
            program: Program::Sum {
                keys: key_vec.clone(),
                field: "value".to_string(),
            },
            prewarm: key_vec.clone(),
        };
        let outcome = host.execute(invocation).expect("warm query");
        assert_eq!(outcome.execution.traps, 0);
    });

    let cold_trap_query = bench_loop(iterations, || {
        let invocation = Invocation {
            tenant: tenant.clone(),
            deployment: deployment.clone(),
            snapshot_ts: host.snapshot_ts(),
            kind: FunctionKind::Query,
            program: Program::Sum {
                keys: key_vec.clone(),
                field: "value".to_string(),
            },
            prewarm: vec![],
        };
        let outcome = host.execute(invocation).expect("cold query");
        assert_eq!(outcome.execution.traps, keys);
    });

    let mutation = bench_loop(iterations, || {
        let invocation = Invocation {
            tenant: tenant.clone(),
            deployment: deployment.clone(),
            snapshot_ts: host.snapshot_ts(),
            kind: FunctionKind::Mutation,
            program: Program::Increment {
                key: DocumentId::new("counter/main"),
                field: "value".to_string(),
                by: 1,
            },
            prewarm: vec![],
        };
        let outcome = host.execute(invocation).expect("mutation");
        assert!(outcome.commit_ts.is_some());
    });

    println!(
        "{{\"iterations\":{iterations},\"keys\":{keys},\"warm_query_avg_ns\":{},\"cold_trap_query_avg_ns\":{},\"mutation_avg_ns\":{}}}",
        avg_ns(warm_query, iterations),
        avg_ns(cold_trap_query, iterations),
        avg_ns(mutation, iterations),
    );
}

fn bench_loop(iterations: usize, mut op: impl FnMut()) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        op();
    }
    started.elapsed()
}

fn avg_ns(duration: Duration, iterations: usize) -> u128 {
    duration.as_nanos() / iterations.max(1) as u128
}
