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

/// Build a document the way the Postgres adapter does in production —
/// with a `_raw` field carrying the upstream Convex JSON envelope. The
/// cell's `1.0/get` handler reads `_raw` verbatim
/// (see `aster_v8cell::doc_raw_as_json`) and hands it back to JS as the
/// `Convex.asyncSyscall` resolved string.
fn doc_with_raw_json(raw_json: &str) -> Document {
    let mut doc = Document::new();
    doc.insert("_raw".to_string(), Value::Text(raw_json.to_string()));
    doc
}

#[test]
fn module_get_by_id_through_fake_broker_returns_doc() {
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
        .execute_module_query_with_broker(
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
}

#[test]
fn module_rejects_seed_ian_as_mutation() {
    // Same bundle, but ask for the mutation. Aster v0.5 cells don't
    // have DB write capability so we reject up front rather than
    // half-execute through `invokeMutation` and surface a downstream
    // syscall failure. See /tmp/aster-research-convex-udf-runner.md §8 D6.
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

#[test]
fn module_missing_export_lists_available() {
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
