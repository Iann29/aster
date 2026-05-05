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

    let source = match &config.source {
        SourceLocation::Path(p) => fs::read_to_string(p)?,
        SourceLocation::Inline(s) => s.clone(),
        SourceLocation::Bundle(module_path) => load_bundle_source(&broker, &config, module_path)?,
    };

    let cell = V8SandboxCell::new(
        config.tenant.clone(),
        config.deployment.clone(),
        config.max_traps,
    );
    let result = cell.execute_async_main_with_broker(
        &broker,
        config.cell_id,
        config.lease_epoch,
        config.tenant,
        config.deployment,
        config.snapshot_ts,
        config.prewarm,
        &source,
    )?;

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
fn load_bundle_source(
    broker: &UdsCapsuleBrokerClient,
    config: &CellConfig,
    module_path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let context = SealContext::new(config.cell_id.clone(), config.lease_epoch);
    let capsule = broker.initial_capsule(
        &context,
        config.tenant.clone(),
        config.deployment.clone(),
        config.snapshot_ts,
        config.prewarm.clone(),
    )?;
    let bytes = broker
        .load_module_bundle(&context, capsule, module_path)?
        .ok_or_else(|| format!("module {module_path:?} not present in broker's source packages"))?;
    let source = bundle::extract_module_source(&bytes, module_path)?;
    Ok(source)
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
///
/// Exactly one must be set. Setting more than one rejects so callers
/// don't silently pick the wrong source.
#[derive(Debug)]
enum SourceLocation {
    Path(PathBuf),
    Inline(String),
    Bundle(String),
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
        let inline = std::env::var("ASTER_JS_INLINE")
            .ok()
            .filter(|s| !s.is_empty());
        let path = std::env::var("ASTER_JS").ok().filter(|s| !s.is_empty());
        let module = std::env::var("ASTER_MODULE_PATH")
            .ok()
            .filter(|s| !s.is_empty());

        // At most one of the three may be set. Counting set-to-Some
        // flags is the cleanest way to reject "any two" combinations
        // without a 2x2x2 truth table.
        let set = [inline.is_some(), path.is_some(), module.is_some()]
            .into_iter()
            .filter(|b| *b)
            .count();
        let source = match set {
            0 => {
                return Err(
                    "missing required env: set one of ASTER_JS, ASTER_JS_INLINE, ASTER_MODULE_PATH"
                        .into(),
                );
            }
            1 => match (inline, path, module) {
                (Some(s), None, None) => SourceLocation::Inline(s),
                (None, Some(p), None) => SourceLocation::Path(PathBuf::from(p)),
                (None, None, Some(m)) => SourceLocation::Bundle(m),
                _ => unreachable!("set==1 picks exactly one"),
            },
            _ => {
                return Err(
                    "set exactly one of ASTER_JS, ASTER_JS_INLINE, ASTER_MODULE_PATH — they're \
                     mutually exclusive"
                        .into(),
                );
            }
        };
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
}
