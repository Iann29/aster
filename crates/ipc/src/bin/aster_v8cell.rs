use std::fs;
use std::path::PathBuf;

use aster_broker::CapsuleBrokerClient;
use aster_capsule::{DeploymentId, DocumentId, SealContext, TenantId};
use aster_ipc::{bundle, UdsCapsuleBrokerClient};
use aster_v8cell::V8SandboxCell;

fn main() {
    if let Err(error) = run() {
        eprintln!("aster_v8cell: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = CellConfig::from_env()?;
    let broker = UdsCapsuleBrokerClient::new(&config.socket_path);

    let cell = V8SandboxCell::new(
        config.tenant.clone(),
        config.deployment.clone(),
        config.max_traps,
    );
    let result = match &config.source {
        SourceLocation::Path(p) => {
            let source = fs::read_to_string(p)?;
            cell.execute_async_main_with_broker(
                &broker,
                config.cell_id,
                config.lease_epoch,
                config.tenant,
                config.deployment,
                config.snapshot_ts,
                config.prewarm,
                &source,
            )?
        }
        SourceLocation::Inline(s) => cell.execute_async_main_with_broker(
            &broker,
            config.cell_id,
            config.lease_epoch,
            config.tenant,
            config.deployment,
            config.snapshot_ts,
            config.prewarm,
            s,
        )?,
        SourceLocation::Bundle {
            module_path,
            invoke,
        } => {
            let source = load_bundle_source(
                &broker,
                &config.cell_id,
                config.lease_epoch,
                &config.tenant,
                &config.deployment,
                config.snapshot_ts,
                &config.prewarm,
                module_path,
            )?;
            match invoke {
                None => cell.execute_async_main_with_broker(
                    &broker,
                    config.cell_id,
                    config.lease_epoch,
                    config.tenant,
                    config.deployment,
                    config.snapshot_ts,
                    config.prewarm,
                    &source,
                )?,
                Some(BundleInvocation {
                    function_name,
                    args_json,
                }) => cell.execute_module_query_with_broker(
                    &broker,
                    config.cell_id,
                    config.lease_epoch,
                    config.tenant,
                    config.deployment,
                    config.snapshot_ts,
                    config.prewarm,
                    &source,
                    function_name,
                    args_json,
                )?,
            }
        }
    };

    // Serialise the Value via serde_json so strings, bools and null
    // round-trip — the original i64-only formatter silently dropped
    // anything that wasn't an integer (Text → 0, masking real
    // results the JS function actually returned).
    let output_json = match &result.output {
        aster_capsule::Value::Int(n) => serde_json::Value::from(*n),
        aster_capsule::Value::Text(s) => serde_json::Value::from(s.as_str()),
        aster_capsule::Value::Bool(b) => serde_json::Value::from(*b),
        aster_capsule::Value::Null => serde_json::Value::Null,
    };
    let envelope = serde_json::json!({
        "output": output_json,
        "traps": result.traps,
        "capsule_hash": result.capsule_hash,
    });
    println!("{}", serde_json::to_string(&envelope).unwrap());
    Ok(())
}

/// Fetch a Convex bundle ZIP for `module_path` from the broker, unpack
/// the matching entry, and hand back the JS source string.
///
/// The broker requires a sealed capsule for any `LoadModuleBundle` —
/// that's the security gate added by Iann29/aster#19. So we bootstrap
/// our own capsule through the same `InitialCapsule` IPC the regular
/// execute path uses internally; the broker's `initial_capsule` is
/// idempotent, so the inner cell call later builds another capsule
/// without conflict.
///
/// Empty / `Some(None)` from the broker — module path resolved cleanly
/// but the bundle row isn't present — surfaces as a typed startup
/// error here. The cell never attempts to V8-execute "no source".
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
    let bytes = broker
        .load_module_bundle(&context, capsule, module_path)?
        .ok_or_else(|| format!("module {module_path:?} not present in broker's source packages"))?;
    let source = bundle::extract_module_source(&bytes, module_path)?;
    Ok(source)
}

/// What to do once the bundle source is loaded.
///
/// `None` → run the bundle's source as an `async function main()`,
/// matching the legacy `ASTER_JS` / `ASTER_JS_INLINE` shape. Used by
/// PR #20-vintage smoke harnesses.
///
/// `Some(_)` → invoke a named export with caller-supplied args via the
/// new `execute_module_query_with_broker` entry point. The export
/// must be a Convex `query()`; `mutation` / `action` are rejected.
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
///   When paired with `ASTER_FUNCTION_NAME` + `ASTER_ARGS_JSON`, the
///   cell switches to the module-query path: compile bundle as ES
///   module, look up the named export, call `invokeQuery(args)`.
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
    lease_epoch: u64,
    prewarm: Vec<DocumentId>,
    source: SourceLocation,
    max_traps: usize,
}

impl CellConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let source = SourceLocation::from_env_map(EnvMap::from_process())?;
        Ok(Self {
            socket_path: PathBuf::from(env_string("ASTER_BROKER_SOCK")?),
            tenant: TenantId::new(env_string("ASTER_TENANT")?),
            deployment: DeploymentId::new(env_string("ASTER_DEPLOYMENT")?),
            snapshot_ts: env_string("ASTER_SNAPSHOT_TS")?.parse()?,
            cell_id: env_string("ASTER_CELL_ID")?,
            lease_epoch: env_string("ASTER_LEASE_EPOCH")?.parse()?,
            prewarm: parse_prewarm(&std::env::var("ASTER_PREWARM").unwrap_or_default()),
            source,
            max_traps: std::env::var("ASTER_MAX_TRAPS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(64),
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
