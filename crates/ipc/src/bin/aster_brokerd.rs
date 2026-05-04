use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;

use aster_broker::{BrokerError, CapsuleBrokerClient, CapsuleStore};
use aster_capsule::{
    CapsuleSealKey, DeploymentId, Document, DocumentId, MvccStore, SealContext, SealedCapsule,
    TenantId, Value,
};
use aster_ipc::{read_frame, write_frame, IpcRequest, IpcResponse, WireBrokerError};

fn main() {
    if let Err(error) = run() {
        eprintln!("aster_brokerd: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = BrokerConfig::from_env()?;
    run_broker(config)?;
    Ok(())
}

/// Which `CapsuleStore` impl the brokerd should construct.
///
/// `memory` (default) keeps the in-memory `MvccStore` the v0.3 prototype
/// shipped with — useful for compose smoke tests and the
/// `process_boundary` E2E. `postgres` switches to `PostgresCapsuleStore`
/// reading from the same Convex database the upstream backend writes to;
/// requires `ASTER_DB_URL_FILE` or `ASTER_DB_URL`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum StoreKind {
    Memory,
    Postgres,
}

impl StoreKind {
    fn from_env_value(s: &str) -> Result<Self, String> {
        match s {
            "memory" | "" => Ok(Self::Memory),
            "postgres" => Ok(Self::Postgres),
            other => Err(format!(
                "ASTER_STORE={other:?} is not recognised — use 'memory' or 'postgres'"
            )),
        }
    }
}

#[derive(Debug)]
struct BrokerConfig {
    socket_path: PathBuf,
    tenant: TenantId,
    deployment: DeploymentId,
    snapshot_ts: u64,
    seeds: Vec<(DocumentId, Document)>,
    seal_key: CapsuleSealKey,
    max_connections: usize,
    store_kind: StoreKind,
    /// Postgres connection URL when `store_kind == Postgres`. None for memory.
    db_url: Option<String>,
    /// Postgres schema where Convex tables live. Convex calls this `@db_name`;
    /// defaults to `public` when ASTER_DB_SCHEMA is unset, which matches a
    /// vanilla self-hosted Convex install.
    db_schema: String,
}

impl BrokerConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path = env_path("ASTER_BROKER_SOCK")?;
        let tenant = TenantId::new(env_string("ASTER_TENANT")?);
        let deployment = DeploymentId::new(env_string("ASTER_DEPLOYMENT")?);
        let snapshot_ts = env_optional_u64("ASTER_SNAPSHOT_TS")?.unwrap_or(0);
        let seeds = parse_seeds(&env_optional_string("ASTER_SEED_I64")?.unwrap_or_default())?;
        let seal_key = CapsuleSealKey::derive_for_tests(env_string("ASTER_SEAL_SEED")?.as_bytes());
        let max_connections = env_optional_usize("ASTER_MAX_CONNECTIONS")?.unwrap_or(1024);
        let store_kind =
            StoreKind::from_env_value(&env_optional_string("ASTER_STORE")?.unwrap_or_default())?;
        let db_url = match store_kind {
            StoreKind::Memory => None,
            StoreKind::Postgres => Some(resolve_db_url()?),
        };
        let db_schema =
            env_optional_string("ASTER_DB_SCHEMA")?.unwrap_or_else(|| "public".to_string());
        Ok(Self {
            socket_path,
            tenant,
            deployment,
            snapshot_ts,
            seeds,
            seal_key,
            max_connections,
            store_kind,
            db_url,
            db_schema,
        })
    }
}

/// Discover the Postgres URL. File-mount form wins so the URL never
/// appears in `ps` / a container's env-var dump. Operators put their
/// secret at a path readable only by the brokerd UID.
fn resolve_db_url() -> Result<String, Box<dyn std::error::Error>> {
    if let Some(path) = env_optional_string("ASTER_DB_URL_FILE")? {
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("read ASTER_DB_URL_FILE={path}: {err}"))?;
        return Ok(raw.trim().to_string());
    }
    if let Some(url) = env_optional_string("ASTER_DB_URL")? {
        return Ok(url);
    }
    Err("ASTER_STORE=postgres requires ASTER_DB_URL_FILE or ASTER_DB_URL".into())
}

fn run_broker(config: BrokerConfig) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if config.socket_path.exists() {
        fs::remove_file(&config.socket_path)?;
    }

    // Pick the storage backend at runtime based on ASTER_STORE. The
    // Arc<dyn ...> shape lets the request loop stay backend-agnostic;
    // this is the single dispatch point and adding more backends
    // (e.g. ASTER_STORE=mock for fuzz harnesses) only touches this
    // match.
    let configured_ts = config.snapshot_ts;
    let store: Arc<dyn CapsuleStore + Send + Sync> = match config.store_kind {
        StoreKind::Memory => {
            let mvcc = MvccStore::new();
            for (key, document) in config.seeds {
                mvcc.seed(key, document);
            }
            Arc::new(mvcc)
        }
        StoreKind::Postgres => {
            let url = config
                .db_url
                .clone()
                .expect("postgres url present by from_env");
            let pg_cfg = aster_store_postgres::PostgresConfig {
                url,
                schema: config.db_schema.clone(),
                ..aster_store_postgres::PostgresConfig::default()
            };
            // Connect is lazy — `connect()` builds the runtime + pool but
            // does NOT open a TCP connection. First snapshot_ts call
            // below is the one that actually checks if Postgres is up.
            // Failure here is a config error (bad URL, missing host),
            // worth dying at startup.
            let store = aster_store_postgres::PostgresCapsuleStore::connect(pg_cfg)
                .map_err(|err| format!("postgres connect: {err}"))?;
            Arc::new(store)
        }
    };
    let snapshot_ts = if configured_ts == 0 {
        store
            .snapshot_ts()
            .map_err(|err| format!("snapshot_ts: {err}"))?
    } else {
        configured_ts
    };
    eprintln!(
        "aster_brokerd: store={} snapshot_ts={}",
        match config.store_kind {
            StoreKind::Memory => "memory",
            StoreKind::Postgres => "postgres",
        },
        snapshot_ts
    );
    let broker = ProcessBroker {
        store,
        seal_key: config.seal_key,
        tenant: config.tenant,
        deployment: config.deployment,
        snapshot_ts,
    };

    let listener = UnixListener::bind(&config.socket_path)?;
    eprintln!(
        "aster_brokerd: ready socket={} snapshot_ts={}",
        config.socket_path.display(),
        snapshot_ts
    );

    for (count, stream) in listener.incoming().enumerate() {
        if count >= config.max_connections {
            eprintln!("aster_brokerd: max connections reached");
            break;
        }
        let mut stream = stream?;
        let request = read_frame::<IpcRequest>(&mut stream);
        let should_shutdown = match request {
            Ok(request) => {
                let (response, should_shutdown) = handle_request(&broker, request);
                write_frame(&mut stream, &response)?;
                should_shutdown
            }
            Err(error) => {
                let response =
                    IpcResponse::Error(WireBrokerError::new("bad_request", error.to_string()));
                write_frame(&mut stream, &response)?;
                false
            }
        };
        if should_shutdown {
            eprintln!("aster_brokerd: shutdown requested");
            break;
        }
    }

    let _ = fs::remove_file(&config.socket_path);
    Ok(())
}

fn handle_request(broker: &ProcessBroker, request: IpcRequest) -> (IpcResponse, bool) {
    match request {
        IpcRequest::InitialCapsule {
            context,
            tenant,
            deployment,
            snapshot_ts,
            prewarm,
        } => (
            IpcResponse::InitialCapsule(
                broker
                    .initial_capsule(&context, tenant, deployment, snapshot_ts, prewarm)
                    .map_err(WireBrokerError::from),
            ),
            false,
        ),
        IpcRequest::HydratePoint {
            context,
            capsule,
            key,
        } => (
            IpcResponse::HydratePoint(
                broker
                    .hydrate_point(&context, capsule, key)
                    .map_err(WireBrokerError::from),
            ),
            false,
        ),
        IpcRequest::Shutdown => (IpcResponse::ShutdownAck, true),
    }
}

struct ProcessBroker {
    store: Arc<dyn CapsuleStore + Send + Sync>,
    seal_key: CapsuleSealKey,
    tenant: TenantId,
    deployment: DeploymentId,
    snapshot_ts: u64,
}

impl CapsuleBrokerClient for ProcessBroker {
    fn initial_capsule(
        &self,
        context: &SealContext,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
    ) -> Result<SealedCapsule, BrokerError> {
        if tenant != self.tenant {
            return Err(BrokerError::TenantMismatch);
        }
        if deployment != self.deployment {
            return Err(BrokerError::DeploymentMismatch);
        }
        if snapshot_ts != self.snapshot_ts {
            return Err(BrokerError::Remote(format!(
                "snapshot_ts {snapshot_ts} is not broker snapshot {}",
                self.snapshot_ts
            )));
        }
        let capsule = self
            .store
            .build_capsule(tenant, deployment, snapshot_ts, prewarm)?;
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }

    fn hydrate_point(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        key: DocumentId,
    ) -> Result<SealedCapsule, BrokerError> {
        let mut capsule = capsule.into_capsule(&self.seal_key, context)?;
        if capsule.tenant != self.tenant {
            return Err(BrokerError::TenantMismatch);
        }
        if capsule.deployment != self.deployment {
            return Err(BrokerError::DeploymentMismatch);
        }
        if capsule.ts != self.snapshot_ts {
            return Err(BrokerError::Remote(format!(
                "capsule snapshot_ts {} is not broker snapshot {}",
                capsule.ts, self.snapshot_ts
            )));
        }
        let value = self.store.read_point(&key, capsule.ts)?;
        capsule.hydrate_point(key, value);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }
}

fn parse_seeds(raw: &str) -> Result<Vec<(DocumentId, Document)>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|entry| {
            let mut parts = entry.split(':');
            let key = parts
                .next()
                .filter(|part| !part.is_empty())
                .ok_or_else(|| format!("bad seed entry {entry:?}: missing key"))?;
            let field = parts
                .next()
                .filter(|part| !part.is_empty())
                .ok_or_else(|| format!("bad seed entry {entry:?}: missing field"))?;
            let value = parts
                .next()
                .ok_or_else(|| format!("bad seed entry {entry:?}: missing value"))?
                .parse::<i64>()
                .map_err(|error| format!("bad seed entry {entry:?}: {error}"))?;
            if parts.next().is_some() {
                return Err(format!("bad seed entry {entry:?}: too many ':' parts"));
            }
            let mut document = Document::new();
            document.insert(field.to_string(), Value::Int(value));
            Ok((DocumentId::new(key), document))
        })
        .collect()
}

fn env_optional_string(name: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn env_string(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("missing required env {name}").into())
}

fn env_path(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(PathBuf::from(env_string(name)?))
}

fn env_optional_u64(name: &str) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value.parse()?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn env_optional_usize(name: &str) -> Result<Option<usize>, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value.parse()?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seed_documents() {
        let seeds = parse_seeds("items/a:value:20,items/b:value:22").expect("parse");
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].0, DocumentId::new("items/a"));
        assert_eq!(seeds[1].1.get("value"), Some(&Value::Int(22)));
    }
}
