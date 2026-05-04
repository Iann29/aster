use std::fs;
use std::path::PathBuf;

use aster_capsule::{DeploymentId, DocumentId, TenantId};
use aster_ipc::UdsCapsuleBrokerClient;
use aster_v8cell::V8SandboxCell;

fn main() {
    if let Err(error) = run() {
        eprintln!("aster_v8cell: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = CellConfig::from_env()?;
    let source = match &config.source {
        SourceLocation::Path(p) => fs::read_to_string(p)?,
        SourceLocation::Inline(s) => s.clone(),
    };
    let broker = UdsCapsuleBrokerClient::new(config.socket_path);
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

/// Where the cell loads its JS source from. `ASTER_JS` is the file
/// path used by the Docker smoke harness (`/tenant/main.js` mounted
/// from the host). `ASTER_JS_INLINE` skips the filesystem entirely
/// and runs the literal env value as the script — used by the
/// Synapse cell-on-demand spawn path, where ferrying a file across
/// the docker-out-of-docker boundary is more friction than just
/// stuffing the source into an env var on `docker run`.
///
/// Exactly one must be set. Setting both rejects so callers don't
/// silently pick the wrong one.
#[derive(Debug)]
enum SourceLocation {
    Path(PathBuf),
    Inline(String),
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
        let source = match (inline, path) {
            (Some(s), None) => SourceLocation::Inline(s),
            (None, Some(p)) => SourceLocation::Path(PathBuf::from(p)),
            (Some(_), Some(_)) => {
                return Err(
                    "set ASTER_JS or ASTER_JS_INLINE, not both — they're mutually exclusive".into(),
                );
            }
            (None, None) => return Err("missing required env ASTER_JS or ASTER_JS_INLINE".into()),
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
