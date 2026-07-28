//! End-to-end proof that an Aster v8cell can run a real `npx convex deploy`
//! bundle and answer one of its queries against a fake broker.
//!
//! The fixture at `tests/fixtures/messages.bundled.js` is the byte-for-byte
//! output of `npx convex deploy --debug-bundle-path` against the
//! `aster-e2e-fixture` Convex app (research walkthrough +
//! reproduction commands at `/tmp/aster-research-bundle-ground-truth.md`).
//! It is a fully flattened ESM module — every `convex/server`,
//! `convex/values`, and `_generated/*` import has been inlined by esbuild,
//! so the cell-side ESM loader never has to resolve a cross-module import.
//!
//! The hard proof in this file is `module_get_by_id_through_fake_broker_returns_doc`:
//! given that bundle, the cell compiles + evaluates it as a V8 ES module,
//! looks up the `getById` query export, calls `invokeQuery(args)`, and the
//! handler's `await ctx.db.get(id)` lands at the broker via
//! `Convex.asyncSyscall("1.0/get", ...)` — the existing trap path the
//! cell already implemented in `feat/v8cell-convex-async-syscall` (#8).
//! The output is the resolved JSON string verbatim from `invokeQuery`.

use aster_broker::LocalCapsuleBroker;
use aster_capsule::{
    CapsuleSealKey, DeploymentId, Document, DocumentId, MvccStore, TenantId, Value,
};
use aster_v8cell::{V8CellError, V8SandboxCell};

const BUNDLE: &str = include_str!("fixtures/messages.bundled.js");

/// Production uses one isolate per cell process. rusty_v8 teardown is not
/// reliable across several fixture-heavy isolates in one Rust test process,
/// so each contract test mirrors production and executes in a fresh child.
fn run_v8_test_in_subprocess(name: &str) -> bool {
    if std::env::var("ASTER_V8_TEST_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .env("ASTER_V8_TEST_CHILD", name)
        .args(["--exact", name, "--nocapture"])
        .output()
        .expect("spawn isolated V8 test");
    assert!(
        output.status.success(),
        "isolated V8 test {name} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// Build a document the way the Postgres adapter does in production —
/// with a `_raw` field carrying the upstream Convex JSON envelope. The
/// cell's `1.0/get` handler reads `_raw` verbatim
/// (see `V8CellState::consume_get_raw_json`) and hands it back to JS as the
/// `Convex.asyncSyscall` resolved string.
fn doc_with_raw_json(raw_json: &str) -> Document {
    let mut doc = Document::new();
    doc.insert("_raw".to_string(), Value::Text(raw_json.to_string()));
    doc
}

#[test]
fn module_get_by_id_through_fake_broker_returns_doc() {
    if run_v8_test_in_subprocess("module_get_by_id_through_fake_broker_returns_doc") {
        return;
    }
    let tenant = TenantId::new("tenant-bundle-e2e");
    let deployment = DeploymentId::new("dep-bundle-e2e");

    // Aster wire-id form (`<table_hex>/<id_hex>`). The bundle's
    // `db.get(id)` doesn't care about the format on the way out — it
    // hands the string straight to `Convex.asyncSyscall("1.0/get", {id, ...})`
    // and our cell broker keys directly off the id string.
    let id_str = "k01_messages_e2e";
    let store = MvccStore::new();
    let raw_json = r#"{"_id":"k01_messages_e2e","_creationTime":1700000000000.0,"name":"ian","body":"hello from aster"}"#;
    store.seed(DocumentId::new(id_str), doc_with_raw_json(raw_json));
    let ts = store.snapshot_ts();

    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-bundle-e2e"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    // Args wire shape: JSON array of arg-objects. Upstream Convex
    // serialises a `ConvexArray` into JSON and JS-side
    // `invokeFunction(handler, ctx, args)` does `handler(ctx, ...args)`
    // — so `args` must be iterable, i.e. an array. See
    // `/tmp/convex-backend/crates/value/src/array.rs:46-51` for the
    // upstream encoder and the `nt` function at line 1735 of the
    // bundle for the spread.
    let args_json = format!(r#"[{{"id":"{id_str}"}}]"#);

    let result = cell
        .execute_module_function_with_broker(
            &broker,
            "cell-bundle-e2e",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            BUNDLE,
            "getById",
            &args_json,
        )
        .expect("module-loader path should run getById end to end");

    // The cell returns invokeQuery's resolved string verbatim
    // (`Value::Text` of `JSON.stringify(convexToJson(result))`). For
    // this fixture's `getById` handler — `async (ctx, {id}) => ctx.db.get(id)`
    // — the result is the document we seeded, so the final string
    // should contain the document's user fields.
    let output_json = match result.output {
        Value::Text(s) => s,
        other => panic!("expected Value::Text from invokeQuery, got {other:?}"),
    };
    assert!(
        output_json.contains(r#""name":"ian""#),
        "expected name field in result, got: {output_json}"
    );
    assert!(
        output_json.contains(r#""body":"hello from aster""#),
        "expected body field in result, got: {output_json}"
    );
    assert!(
        output_json.contains(r#""_id":"k01_messages_e2e""#),
        "expected _id field in result, got: {output_json}"
    );

    // Exactly one trap drained — the `1.0/get` syscall the bundle's
    // `db.get(id)` issues. If this drifts upward, something else inside
    // the bundle started firing async syscalls (validators? meta?), and
    // the v0.5 syscall stub list needs widening.
    assert_eq!(
        result.traps, 1,
        "exactly one Convex.asyncSyscall trap expected for db.get"
    );

    // Variante B consumption ledger (referee F7): the module path records
    // the same observation the commit verb will declare.
    assert_eq!(
        result.consumed_reads,
        vec![DocumentId::new(id_str)],
        "db.get(id) must land in the consumption ledger"
    );
}

#[test]
fn module_rejects_seed_ian_as_mutation() {
    if run_v8_test_in_subprocess("module_rejects_seed_ian_as_mutation") {
        return;
    }
    // Same bundle, but ask for the mutation THROUGH THE QUERY ENTRY
    // POINT. Since S9b mutations run via their own entry point
    // (`execute_module_mutation_with_broker`, write-set semantics); the
    // query gate still rejects them up front rather than half-execute
    // through `invokeMutation`. See /tmp/aster-research-convex-udf-runner.md §8 D6.
    let tenant = TenantId::new("tenant-bundle-mut");
    let deployment = DeploymentId::new("dep-bundle-mut");
    let store = MvccStore::new();
    let ts = store.snapshot_ts();
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-bundle-e2e"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    let err = cell
        .execute_module_query_with_broker(
            &broker,
            "cell-bundle-mut",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            BUNDLE,
            "seedIan",
            "[]",
        )
        .expect_err("mutation must be rejected by the v0.5 cell");

    match err {
        V8CellError::Run(msg) => {
            assert!(
                msg.contains("mutation"),
                "expected mutation rejection message, got {msg:?}"
            );
            assert!(
                msg.contains("seedIan"),
                "expected the export name in the rejection, got {msg:?}"
            );
        }
        other => panic!("expected Run rejection, got {other:?}"),
    }
}

/// Hand-written stand-in for an `npx convex deploy` bundle whose mutation
/// exercises the S9b write-set verbs. It honors the exact contract the
/// cell depends on from the real bundle (registration_impl.js):
/// the export carries `isMutation: true` + `invokeMutation(argsJson)`
/// returning `Promise<string>` of the JSON-encoded result, and db verbs go
/// through `Convex.asyncSyscall` with the upstream names. Hand-written
/// because the real fixture's `seedIan` inserts WITHOUT an `_id` — the
/// shape that needs server-side id minting (future work); this module
/// supplies explicit ids, the prototype's supported form.
const HANDWRITTEN_MUTATION_MODULE: &str = r#"
const syscall = async (name, args) =>
  JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));

export const bumpOrSeed = {
  isMutation: true,
  isPublic: true,
  invokeMutation: async (argsJson) => {
    const [{ id, table }] = JSON.parse(argsJson);
    const existing = await syscall("1.0/get", { id });
    if (existing === null) {
      const minted = await syscall("1.0/insert", {
        table,
        value: { _id: id, n: 1 },
      });
      return JSON.stringify({ seeded: minted._id, n: 1 });
    }
    await syscall("1.0/shallowMerge", { id, value: { n: existing.n + 1 } });
    return JSON.stringify({ seeded: null, n: existing.n + 1 });
  },
};
"#;

/// S9b: a Convex-shaped mutation runs through the module entry point and
/// births the write set in the cell — `invokeMutation` drives `1.0/get`
/// (hydrated + consumed) then `1.0/shallowMerge` (accumulated, never sent
/// to the broker), and the result carries (consumed_reads, write_set,
/// sealed_capsule): the whole transaction bundle the Commit verb takes.
#[test]
fn module_mutation_handwritten_births_write_set() {
    if run_v8_test_in_subprocess("module_mutation_handwritten_births_write_set") {
        return;
    }
    let tenant = TenantId::new("tenant-mut-ws");
    let deployment = DeploymentId::new("dep-mut-ws");
    let store = MvccStore::new();
    let key = DocumentId::new("docs/seed");
    store.seed(
        key.clone(),
        doc_with_raw_json(r#"{"_id":"docs/seed","n":1}"#),
    );
    let ts = store.snapshot_ts();
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-mutation-ws"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    let result = cell
        .execute_module_function_with_broker(
            &broker,
            "cell-mut-ws",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            HANDWRITTEN_MUTATION_MODULE,
            "bumpOrSeed",
            r#"[{"id":"docs/seed","table":"docs"}]"#,
        )
        .expect("mutation module should run end to end");

    match &result.output {
        Value::Text(s) => assert_eq!(
            serde_json::from_str::<serde_json::Value>(s).expect("output is JSON"),
            serde_json::json!({ "seeded": null, "n": 2 })
        ),
        other => panic!("expected Text output from invokeMutation, got {other:?}"),
    }
    assert_eq!(
        result.traps, 1,
        "one hydration for the get; the patch is local"
    );
    assert_eq!(
        result.consumed_reads,
        vec![key.clone()],
        "the read the mutation branched on is the declared dependency"
    );
    assert_eq!(result.write_set.len(), 1);
    let (write_key, write_doc) = &result.write_set[0];
    assert_eq!(write_key, &key);
    let raw = match write_doc.as_ref().expect("patch writes a put").get("_raw") {
        Some(Value::Text(raw)) => raw.clone(),
        other => panic!("write doc lacks _raw, got {other:?}"),
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).expect("_raw is JSON"),
        serde_json::json!({ "_id": "docs/seed", "n": 2 })
    );
    assert!(
        result.sealed_capsule.is_some(),
        "broker path must hand back the final sealed capsule for the commit half"
    );
}

/// The inverse of `module_rejects_seed_ian_as_mutation`: a query export
/// cannot ride the mutation entry point.
#[test]
fn module_mutation_path_rejects_query_export() {
    if run_v8_test_in_subprocess("module_mutation_path_rejects_query_export") {
        return;
    }
    let tenant = TenantId::new("tenant-mut-gate");
    let deployment = DeploymentId::new("dep-mut-gate");
    let store = MvccStore::new();
    let ts = store.snapshot_ts();
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-bundle-e2e"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    let err = cell
        .execute_module_mutation_with_broker(
            &broker,
            "cell-mut-gate",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            BUNDLE,
            "getById",
            "[]",
        )
        .expect_err("query export must be rejected by the mutation entry point");
    match err {
        V8CellError::Run(msg) => {
            assert!(
                msg.contains("is a query") && msg.contains("getById"),
                "expected query-shape rejection, got {msg:?}"
            );
        }
        other => panic!("expected Run rejection, got {other:?}"),
    }
}

/// A real bundle insert carries no `_id`: the broker mints one, the cell
/// returns it to JS, and the pending write carries Convex system fields.
#[test]
fn module_real_bundle_seed_ian_mints_id_and_builds_write_set() {
    if run_v8_test_in_subprocess("module_real_bundle_seed_ian_mints_id_and_builds_write_set") {
        return;
    }
    let tenant = TenantId::new("tenant-mut-real");
    let deployment = DeploymentId::new("dep-mut-real");
    let store = MvccStore::new();
    let ts = store.snapshot_ts();
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-bundle-e2e"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    let result = cell
        .execute_module_mutation_with_broker(
            &broker,
            "cell-mut-real",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            BUNDLE,
            "seedIan",
            "[{}]",
        )
        .expect("real bundle insert");
    assert_eq!(result.traps, 1, "id allocation is one broker round trip");
    assert!(result.consumed_reads.is_empty());
    assert_eq!(result.write_set.len(), 1);
    let (id, document) = &result.write_set[0];
    assert!(id.0.starts_with("messages/"), "minted id: {}", id.0);
    let document = document.as_ref().expect("insert is a put");
    let Value::Text(raw) = document.get("_raw").expect("raw Convex document") else {
        panic!("inserted document must carry _raw JSON");
    };
    let value: serde_json::Value = serde_json::from_str(raw).expect("inserted JSON");
    assert_eq!(value["_id"], id.0);
    assert_eq!(value["name"], "ian");
    assert_eq!(value["body"], "hello");
    assert!(value.get("_creationTime").is_some());

    let Value::Text(output) = result.output else {
        panic!("invokeMutation output must be encoded text");
    };
    let returned: String = serde_json::from_str(&output).expect("returned id");
    assert_eq!(returned, id.0);
}

#[test]
fn module_missing_export_lists_available() {
    if run_v8_test_in_subprocess("module_missing_export_lists_available") {
        return;
    }
    // Typo'd export name should produce a typed error that surfaces
    // the actually-available exports, so an operator can spot the
    // mismatch without round-tripping the cell.
    let tenant = TenantId::new("tenant-bundle-miss");
    let deployment = DeploymentId::new("dep-bundle-miss");
    let store = MvccStore::new();
    let ts = store.snapshot_ts();
    let broker = LocalCapsuleBroker::new(
        &store,
        CapsuleSealKey::derive_for_tests(b"v8cell-bundle-e2e"),
    );
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 8);

    let err = cell
        .execute_module_query_with_broker(
            &broker,
            "cell-bundle-miss",
            1,
            tenant,
            deployment,
            ts,
            vec![],
            BUNDLE,
            "ghost",
            "[]",
        )
        .expect_err("missing export must error");

    match err {
        V8CellError::Run(msg) => {
            assert!(
                msg.contains("ghost"),
                "expected the typo'd name in the error, got {msg:?}"
            );
            assert!(
                msg.contains("getById"),
                "expected getById in the available list, got {msg:?}"
            );
            assert!(
                msg.contains("seedIan"),
                "expected seedIan in the available list, got {msg:?}"
            );
        }
        other => panic!("expected Run error, got {other:?}"),
    }
}
