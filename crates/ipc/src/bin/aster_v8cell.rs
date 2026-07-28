use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use aster_broker::CapsuleBrokerClient;
use aster_capsule::{DeploymentId, DocumentId, SealContext, TenantId};
use aster_ipc::{bundle, UdsCapsuleBrokerClient, WireCommitOutcome};
use aster_v8cell::{V8ExecutionResult, V8SandboxCell};

fn main() {
    if let Err(error) = run() {
        eprintln!("aster_v8cell: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = CellConfig::from_env()?;
    let broker = match config.launch_token.as_deref() {
        Some(token) => UdsCapsuleBrokerClient::with_launch_token(&config.socket_path, token),
        None => UdsCapsuleBrokerClient::new(&config.socket_path),
    };
    let cell = V8SandboxCell::with_resource_limits(
        config.tenant.clone(),
        config.deployment.clone(),
        config.max_traps,
        config.max_heap_bytes,
        config.execution_timeout,
    );
    let executable = prepare_source(&config, &broker)?;
    let max_attempts = config.max_retries.saturating_add(1);

    for attempt in 1..=max_attempts {
        let mut result = match execute_prepared(&cell, &broker, &config, &executable) {
            Ok(result) => result,
            Err(error) => {
                if broker.session().is_some() {
                    if let Err(abort_error) = broker.abort() {
                        eprintln!(
                            "aster_v8cell: failed to abort session after execution error: \
                             {abort_error}"
                        );
                    }
                }
                return Err(error.into());
            }
        };

        if result.write_set.is_empty() {
            broker.abort()?;
            print_execution_envelope(&result, None, attempt)?;
            return Ok(());
        }

        let sealed = result
            .sealed_capsule
            .take()
            .ok_or("broker-backed execution returned no sealed capsule")?;
        let outcome = broker.commit(
            sealed,
            result.consumed_reads.clone(),
            result.write_set.clone(),
        )?;
        let retryable = matches!(
            outcome,
            WireCommitOutcome::Conflict { .. } | WireCommitOutcome::RetentionViolated { .. }
        ) && config.snapshot_ts == 0
            && attempt < max_attempts;
        if retryable {
            continue;
        }

        print_execution_envelope(&result, Some(&outcome), attempt)?;
        return Ok(());
    }

    unreachable!("attempt loop always returns on its final iteration")
}

enum PreparedSource {
    Raw(String),
    Module {
        source: String,
        function_name: String,
        args_json: String,
    },
}

fn prepare_source(
    config: &CellConfig,
    broker: &UdsCapsuleBrokerClient,
) -> Result<PreparedSource, Box<dyn std::error::Error>> {
    match &config.source {
        SourceLocation::Path(path) => Ok(PreparedSource::Raw(fs::read_to_string(path)?)),
        SourceLocation::Inline(source) => Ok(PreparedSource::Raw(source.clone())),
        SourceLocation::Bundle {
            module_path,
            invoke,
        } => {
            let source = load_bundle_source(
                broker,
                &config.cell_id,
                config.lease_epoch,
                &config.tenant,
                &config.deployment,
                config.snapshot_ts,
                &config.prewarm,
                module_path,
            )?;
            match invoke {
                None => Ok(PreparedSource::Raw(source)),
                Some(invoke) => Ok(PreparedSource::Module {
                    source,
                    function_name: invoke.function_name.clone(),
                    args_json: invoke.args_json.clone(),
                }),
            }
        }
    }
}

fn execute_prepared(
    cell: &V8SandboxCell,
    broker: &UdsCapsuleBrokerClient,
    config: &CellConfig,
    executable: &PreparedSource,
) -> Result<V8ExecutionResult, aster_v8cell::V8CellError> {
    match executable {
        PreparedSource::Raw(source) => cell.execute_async_main_with_broker(
            broker,
            config.cell_id.clone(),
            config.lease_epoch,
            config.tenant.clone(),
            config.deployment.clone(),
            config.snapshot_ts,
            config.prewarm.clone(),
            source,
        ),
        PreparedSource::Module {
            source,
            function_name,
            args_json,
        } => cell.execute_module_function_with_broker(
            broker,
            config.cell_id.clone(),
            config.lease_epoch,
            config.tenant.clone(),
            config.deployment.clone(),
            config.snapshot_ts,
            config.prewarm.clone(),
            source,
            function_name,
            args_json,
        ),
    }
}

fn print_execution_envelope(
    result: &V8ExecutionResult,
    commit: Option<&WireCommitOutcome>,
    attempts: usize,
) -> Result<(), serde_json::Error> {
    let output = match &result.output {
        aster_capsule::Value::Int(value) => serde_json::Value::from(*value),
        aster_capsule::Value::Text(value) => serde_json::Value::from(value.as_str()),
        aster_capsule::Value::Bool(value) => serde_json::Value::from(*value),
        aster_capsule::Value::Null => serde_json::Value::Null,
    };
    let transaction_status = match commit {
        None | Some(WireCommitOutcome::EmptyWriteSet) => "read_only",
        Some(WireCommitOutcome::Committed { .. }) => "committed",
        Some(WireCommitOutcome::Conflict { .. }) => "conflict",
        Some(WireCommitOutcome::StaleEpoch { .. }) => "stale_epoch",
        Some(WireCommitOutcome::SnapshotBeyondHorizon { .. }) => "invalid_snapshot",
        Some(WireCommitOutcome::RetentionViolated { .. }) => "retention_retry_exhausted",
    };
    let envelope = serde_json::json!({
        "output": output,
        "traps": result.traps,
        "capsule_hash": result.capsule_hash,
        "consumed_reads": result.consumed_reads,
        "write_set": result.write_set,
        "commit": commit,
        "transaction_status": transaction_status,
        "attempts": attempts,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

/// Fetch a Convex bundle ZIP for `module_path` from the broker, unpack
/// the matching entry, and hand back the JS source string.
///
/// The broker requires a sealed capsule for `LoadModuleBundle`, so source
/// acquisition uses a short-lived session. That session is always aborted
/// after the load attempt; execution starts a separate transaction session
/// at the latest authoritative snapshot.
#[allow(clippy::too_many_arguments)]
fn load_bundle_source(
    broker: &UdsCapsuleBrokerClient,
    cell_id: &str,
    lease_epoch: u64,
    tenant: &TenantId,
    deployment: &DeploymentId,
    snapshot_ts: u64,
    prewarm: &[DocumentId],
    module_path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let context = SealContext::new(cell_id.to_string(), lease_epoch);
    let capsule = broker.initial_capsule(
        &context,
        tenant.clone(),
        deployment.clone(),
        snapshot_ts,
        prewarm.to_vec(),
    )?;
    let loaded = broker.load_module_bundle(&context, capsule, module_path);
    let aborted = broker.abort();
    let bytes = loaded?
        .ok_or_else(|| format!("module {module_path:?} not present in broker's source packages"))?;
    aborted?;
    Ok(bundle::extract_module_source(&bytes, module_path)?)
}

/// What to do once the bundle source is loaded.
///
/// `None` → run the bundle's source as an `async function main()`,
/// matching the legacy `ASTER_JS` / `ASTER_JS_INLINE` shape. Used by
/// PR #20-vintage smoke harnesses.
///
/// `Some(_)` → inspect the named Convex export and invoke it as a query or
/// mutation according to its bundle marker. Actions remain rejected.
#[derive(Debug)]
struct BundleInvocation {
    function_name: String,
    args_json: String,
}

/// Where the cell loads its JS source from.
///
/// - `ASTER_JS=<path>` — file path on a mount the cell container
///   already has. The Docker smoke harness uses this with
///   `/tenant/main.js` mounted from the host.
/// - `ASTER_JS_INLINE=<source>` — literal source on an env var.
///   Synapse's `aster/invoke` endpoint uses this so a one-shot cell
///   doesn't need a sibling volume just to ferry a single string.
/// - `ASTER_MODULE_PATH=<path>` — pulls the bundle ZIP for `<path>`
///   from the broker over `LoadModuleBundle`, unzips, picks the
///   matching entry. The path matches the way the user named the
///   module (e.g. `messages` or `convex/messages.js`); the bundle
///   adapter on the broker side has already hash-verified the bytes.
///   cell compiles the bundle as ESM, inspects the named export, and calls
///   `invokeQuery(args)` or `invokeMutation(args)` accordingly.
///
/// Exactly one of {Path, Inline, Bundle} must be set. Setting more
/// than one rejects so callers don't silently pick the wrong source.
#[derive(Debug)]
enum SourceLocation {
    Path(PathBuf),
    Inline(String),
    Bundle {
        module_path: String,
        invoke: Option<BundleInvocation>,
    },
}

#[derive(Debug)]
struct CellConfig {
    socket_path: PathBuf,
    tenant: TenantId,
    deployment: DeploymentId,
    snapshot_ts: u64,
    cell_id: String,
    launch_token: Option<String>,
    lease_epoch: u64,
    prewarm: Vec<DocumentId>,
    source: SourceLocation,
    max_traps: usize,
    max_heap_bytes: usize,
    execution_timeout: Duration,
    max_retries: usize,
}

impl CellConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let source = SourceLocation::from_env_map(EnvMap::from_process())?;
        let snapshot_ts = match std::env::var("ASTER_SNAPSHOT_TS") {
            Ok(value) if !value.is_empty() => value.parse()?,
            Ok(_) | Err(std::env::VarError::NotPresent) => 0,
            Err(error) => return Err(error.into()),
        };
        let max_traps = match std::env::var("ASTER_MAX_TRAPS") {
            Ok(value) => value.parse()?,
            Err(std::env::VarError::NotPresent) => 64,
            Err(error) => return Err(error.into()),
        };
        let max_heap_bytes = match std::env::var("ASTER_MAX_HEAP_BYTES") {
            Ok(value) => value.parse()?,
            Err(std::env::VarError::NotPresent) => 128 * 1024 * 1024,
            Err(error) => return Err(error.into()),
        };
        if !(16 * 1024 * 1024..=1024 * 1024 * 1024).contains(&max_heap_bytes) {
            return Err("ASTER_MAX_HEAP_BYTES must be between 16MiB and 1GiB".into());
        }
        let execution_timeout_ms = match std::env::var("ASTER_EXECUTION_TIMEOUT_MS") {
            Ok(value) => value.parse()?,
            Err(std::env::VarError::NotPresent) => 30_000_u64,
            Err(error) => return Err(error.into()),
        };
        if !(10..=300_000).contains(&execution_timeout_ms) {
            return Err("ASTER_EXECUTION_TIMEOUT_MS must be in 10..=300000".into());
        }
        let max_retries = match std::env::var("ASTER_MAX_RETRIES") {
            Ok(value) => value.parse()?,
            Err(std::env::VarError::NotPresent) => 3,
            Err(error) => return Err(error.into()),
        };
        let launch_token = match std::env::var("ASTER_LAUNCH_TOKEN") {
            Ok(value) if !value.is_empty() => Some(value),
            Ok(_) | Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            socket_path: PathBuf::from(env_string("ASTER_BROKER_SOCK")?),
            tenant: TenantId::new(env_string("ASTER_TENANT")?),
            deployment: DeploymentId::new(env_string("ASTER_DEPLOYMENT")?),
            snapshot_ts,
            cell_id: env_string("ASTER_CELL_ID")?,
            launch_token,
            lease_epoch: env_string("ASTER_LEASE_EPOCH")?.parse()?,
            prewarm: parse_prewarm(&std::env::var("ASTER_PREWARM").unwrap_or_default()),
            source,
            max_traps,
            max_heap_bytes,
            execution_timeout: Duration::from_millis(execution_timeout_ms),
            max_retries,
        })
    }
}

/// Helper that collapses "var present and non-empty" lookups for the
/// source-selection envs. Keeping the env reads behind one struct
/// makes the unit tests below trivial: they pass a synthetic map and
/// don't fight with `std::env`'s process-global state.
#[derive(Debug, Default)]
struct EnvMap {
    inline: Option<String>,
    path: Option<String>,
    module: Option<String>,
    function_name: Option<String>,
    args_json: Option<String>,
}

impl EnvMap {
    fn from_process() -> Self {
        Self {
            inline: std::env::var("ASTER_JS_INLINE")
                .ok()
                .filter(|s| !s.is_empty()),
            path: std::env::var("ASTER_JS").ok().filter(|s| !s.is_empty()),
            module: std::env::var("ASTER_MODULE_PATH")
                .ok()
                .filter(|s| !s.is_empty()),
            function_name: std::env::var("ASTER_FUNCTION_NAME")
                .ok()
                .filter(|s| !s.is_empty()),
            args_json: std::env::var("ASTER_ARGS_JSON")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

impl SourceLocation {
    /// Decide which JS source the cell will run from a parsed env map.
    ///
    /// Rules, in priority order:
    ///
    /// 1. `ASTER_FUNCTION_NAME` / `ASTER_ARGS_JSON` only make sense
    ///    paired with `ASTER_MODULE_PATH`. They are also redundant
    ///    with `ASTER_JS` / `ASTER_JS_INLINE` (those run a free-form
    ///    `async function main()` script — there's no named export to
    ///    invoke). Both error cases reject up front so the operator
    ///    sees a clear "you mixed two modes" message instead of a
    ///    silent fallthrough.
    /// 2. Exactly one of `ASTER_JS`, `ASTER_JS_INLINE`,
    ///    `ASTER_MODULE_PATH` must be set.
    /// 3. When `ASTER_MODULE_PATH` is set, either BOTH companion envs
    ///    or NEITHER. Half-configured (one but not both) is a typed
    ///    error: it almost certainly means the caller forgot one.
    fn from_env_map(env: EnvMap) -> Result<Self, Box<dyn std::error::Error>> {
        // Cross-mode guard #1: function-name combined with the legacy
        // free-form scripts. The free-form path runs `async main()`;
        // there's no named export to dispatch into. Check the
        // mutual-exclusion BEFORE the "missing module" check so the
        // operator gets the more specific "you mixed two modes"
        // message instead of "add ASTER_MODULE_PATH" — that hint
        // would be misleading when ASTER_JS is what's actually wrong.
        if env.function_name.is_some() && (env.path.is_some() || env.inline.is_some()) {
            return Err("ASTER_FUNCTION_NAME is mutually exclusive with \
                        ASTER_JS / ASTER_JS_INLINE — the named-export path \
                        only works with ASTER_MODULE_PATH"
                .into());
        }
        // Cross-mode guard #2: function-name without a module path
        // and no legacy script either. Operator probably meant to set
        // ASTER_MODULE_PATH.
        if env.function_name.is_some() && env.module.is_none() {
            return Err("ASTER_FUNCTION_NAME set without ASTER_MODULE_PATH — \
                        the named-export path requires a module bundle"
                .into());
        }
        // Mirror guards for ASTER_ARGS_JSON. Same reasoning, same shape.
        if env.args_json.is_some() && (env.path.is_some() || env.inline.is_some()) {
            return Err("ASTER_ARGS_JSON is mutually exclusive with \
                        ASTER_JS / ASTER_JS_INLINE"
                .into());
        }
        if env.args_json.is_some() && env.module.is_none() {
            return Err(
                "ASTER_ARGS_JSON set without ASTER_MODULE_PATH — args only apply \
                 to the named-export path"
                    .into(),
            );
        }

        // Counting set-to-Some flags is the cleanest way to reject "any
        // two" combinations of the source envs without a 2x2x2 truth
        // table.
        let set = [
            env.inline.is_some(),
            env.path.is_some(),
            env.module.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        match set {
            0 => Err(
                "missing required env: set one of ASTER_JS, ASTER_JS_INLINE, ASTER_MODULE_PATH"
                    .into(),
            ),
            1 => match (env.inline, env.path, env.module) {
                (Some(s), None, None) => Ok(SourceLocation::Inline(s)),
                (None, Some(p), None) => Ok(SourceLocation::Path(PathBuf::from(p))),
                (None, None, Some(m)) => {
                    let invoke = match (env.function_name, env.args_json) {
                        (None, None) => None,
                        (Some(fn_name), Some(args_json)) => Some(BundleInvocation {
                            function_name: fn_name,
                            args_json,
                        }),
                        // Half-configured. Tell the operator which side
                        // is missing; "both or neither" is the real rule
                        // but the friendlier error names the absent one.
                        (Some(_), None) => {
                            return Err("ASTER_FUNCTION_NAME set but ASTER_ARGS_JSON \
                                        missing — the module-query path needs both \
                                        (use ASTER_ARGS_JSON='[]' for zero-arg queries)"
                                .into())
                        }
                        (None, Some(_)) => {
                            return Err("ASTER_ARGS_JSON set but ASTER_FUNCTION_NAME \
                                        missing — name the export to invoke"
                                .into())
                        }
                    };
                    Ok(SourceLocation::Bundle {
                        module_path: m,
                        invoke,
                    })
                }
                _ => unreachable!("set==1 picks exactly one"),
            },
            _ => Err(
                "set exactly one of ASTER_JS, ASTER_JS_INLINE, ASTER_MODULE_PATH — \
                     they're mutually exclusive"
                    .into(),
            ),
        }
    }
}

fn parse_prewarm(raw: &str) -> Vec<DocumentId> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(DocumentId::new)
        .collect()
}

fn env_string(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("missing required env {name}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prewarm_keys() {
        assert_eq!(
            parse_prewarm("items/a, items/b"),
            vec![DocumentId::new("items/a"), DocumentId::new("items/b")]
        );
    }

    /// Plain `ASTER_JS_INLINE=<src>` resolves to `Inline`. The legacy
    /// shape, locked in.
    #[test]
    fn env_map_inline_only_picks_inline() {
        let env = EnvMap {
            inline: Some("globalThis.main = async () => 1;".into()),
            ..Default::default()
        };
        match SourceLocation::from_env_map(env).expect("inline-only is valid") {
            SourceLocation::Inline(s) => assert!(s.contains("globalThis.main")),
            other => panic!("expected Inline, got {other:?}"),
        }
    }

    /// Plain `ASTER_JS=<path>` resolves to `Path`.
    #[test]
    fn env_map_path_only_picks_path() {
        let env = EnvMap {
            path: Some("/tenant/main.js".into()),
            ..Default::default()
        };
        match SourceLocation::from_env_map(env).expect("path-only is valid") {
            SourceLocation::Path(p) => assert_eq!(p, PathBuf::from("/tenant/main.js")),
            other => panic!("expected Path, got {other:?}"),
        }
    }

    /// `ASTER_MODULE_PATH` alone keeps PR #20's "load bundle, run as
    /// async main" behaviour. The new fields stay `None`.
    #[test]
    fn env_map_module_only_picks_bundle_without_invoke() {
        let env = EnvMap {
            module: Some("messages".into()),
            ..Default::default()
        };
        match SourceLocation::from_env_map(env).expect("module-only is valid") {
            SourceLocation::Bundle {
                module_path,
                invoke,
            } => {
                assert_eq!(module_path, "messages");
                assert!(invoke.is_none(), "no companion envs → no invocation");
            }
            other => panic!("expected Bundle, got {other:?}"),
        }
    }

    /// Module + function + args triggers the new module-query path.
    #[test]
    fn env_map_module_with_function_and_args_picks_invoke() {
        let env = EnvMap {
            module: Some("messages".into()),
            function_name: Some("getById".into()),
            args_json: Some(r#"[{"id":"k01_messages_e2e"}]"#.into()),
            ..Default::default()
        };
        match SourceLocation::from_env_map(env).expect("module-query is valid") {
            SourceLocation::Bundle {
                module_path,
                invoke,
            } => {
                assert_eq!(module_path, "messages");
                let invoke = invoke.expect("companion envs → Some");
                assert_eq!(invoke.function_name, "getById");
                assert_eq!(invoke.args_json, r#"[{"id":"k01_messages_e2e"}]"#);
            }
            other => panic!("expected Bundle, got {other:?}"),
        }
    }

    /// Naming a function but not a module is never what the operator
    /// wanted — the named-export path requires a bundle.
    #[test]
    fn env_map_function_without_module_rejects() {
        let env = EnvMap {
            function_name: Some("getById".into()),
            inline: Some("globalThis.main = async () => 1;".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("function with inline must reject")
            .to_string();
        assert!(
            err.contains("ASTER_FUNCTION_NAME") && err.contains("ASTER_JS_INLINE"),
            "expected guard message, got {err:?}"
        );

        let env = EnvMap {
            function_name: Some("getById".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("function alone must reject")
            .to_string();
        assert!(
            err.contains("ASTER_FUNCTION_NAME") && err.contains("ASTER_MODULE_PATH"),
            "expected guard message, got {err:?}"
        );
    }

    /// Mixing function-name + ASTER_JS path rejects with a typed
    /// error (the legacy path runs a free-form `main()`, not a
    /// named export).
    #[test]
    fn env_map_function_with_path_rejects() {
        let env = EnvMap {
            function_name: Some("getById".into()),
            path: Some("/tenant/main.js".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("function with path must reject")
            .to_string();
        assert!(
            err.contains("ASTER_FUNCTION_NAME") && err.contains("ASTER_JS"),
            "expected guard message, got {err:?}"
        );
    }

    /// Args without a module are nonsense — error names the rule.
    #[test]
    fn env_map_args_without_module_rejects() {
        let env = EnvMap {
            args_json: Some("[]".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("args alone must reject")
            .to_string();
        assert!(
            err.contains("ASTER_ARGS_JSON") && err.contains("ASTER_MODULE_PATH"),
            "expected guard message, got {err:?}"
        );
    }

    /// Module with function but no args, or args but no function, is
    /// almost certainly a forgotten env var. Reject with a directive
    /// message — including the "use [] for zero-arg" hint that
    /// keeps the operator from second-guessing the args shape.
    #[test]
    fn env_map_module_with_half_configured_invoke_rejects() {
        let env = EnvMap {
            module: Some("messages".into()),
            function_name: Some("getById".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("function without args must reject")
            .to_string();
        assert!(
            err.contains("ASTER_ARGS_JSON") && err.contains("missing"),
            "expected hint about ARGS_JSON, got {err:?}"
        );
        assert!(err.contains("[]"), "expected zero-arg hint, got {err:?}");

        let env = EnvMap {
            module: Some("messages".into()),
            args_json: Some("[]".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("args without function must reject")
            .to_string();
        assert!(
            err.contains("ASTER_FUNCTION_NAME") && err.contains("missing"),
            "expected hint about FUNCTION_NAME, got {err:?}"
        );
    }

    /// Setting two of the three source envs rejects, mirroring the
    /// pre-PR behaviour. Locks in the existing test coverage in this
    /// new shape.
    #[test]
    fn env_map_two_sources_rejects() {
        let env = EnvMap {
            inline: Some("x".into()),
            path: Some("/p".into()),
            ..Default::default()
        };
        let err = SourceLocation::from_env_map(env)
            .expect_err("two source envs must reject")
            .to_string();
        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got {err:?}"
        );
    }

    /// No source env at all — caller forgot to wire any of the three.
    #[test]
    fn env_map_zero_sources_rejects() {
        let env = EnvMap::default();
        let err = SourceLocation::from_env_map(env)
            .expect_err("zero source envs must reject")
            .to_string();
        assert!(
            err.contains("missing required env"),
            "expected missing-env error, got {err:?}"
        );
    }
}
