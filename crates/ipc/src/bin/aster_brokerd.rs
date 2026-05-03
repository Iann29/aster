use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use aster_broker::{BrokerError, CapsuleBrokerClient};
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

#[derive(Debug)]
struct BrokerConfig {
    socket_path: PathBuf,
    tenant: TenantId,
    deployment: DeploymentId,
    snapshot_ts: u64,
    seeds: Vec<(DocumentId, Document)>,
    seal_key: CapsuleSealKey,
    max_connections: usize,
}

impl BrokerConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path = env_path("ASTER_BROKER_SOCK")?;
        let tenant = TenantId::new(env_string("ASTER_TENANT")?);
        let deployment = DeploymentId::new(env_string("ASTER_DEPLOYMENT")?);
        let snapshot_ts = env_optional_u64("ASTER_SNAPSHOT_TS")?.unwrap_or(0);
        let seeds = parse_seeds(&env_string("ASTER_SEED_I64")?)?;
        let seal_key = CapsuleSealKey::derive_for_tests(env_string("ASTER_SEAL_SEED")?.as_bytes());
        let max_connections = env_optional_usize("ASTER_MAX_CONNECTIONS")?.unwrap_or(1024);
        Ok(Self {
            socket_path,
            tenant,
            deployment,
            snapshot_ts,
            seeds,
            seal_key,
            max_connections,
        })
    }
}

fn run_broker(config: BrokerConfig) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if config.socket_path.exists() {
        fs::remove_file(&config.socket_path)?;
    }

    let store = MvccStore::new();
    for (key, document) in config.seeds {
        store.seed(key, document);
    }
    let snapshot_ts = if config.snapshot_ts == 0 {
        store.snapshot_ts()
    } else {
        config.snapshot_ts
    };
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

#[derive(Debug)]
struct ProcessBroker {
    store: MvccStore,
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
            .build_capsule(tenant, deployment, snapshot_ts, prewarm);
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
        let value = self.store.read_at(&key, capsule.ts);
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
