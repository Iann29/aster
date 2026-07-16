use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aster_broker::{BrokerError, CapsuleBrokerClient, CapsuleStore};
use aster_capsule::{
    CapsuleSealKey, DeploymentId, Document, DocumentId, MvccStore, SealContext, SealedCapsule,
    SessionBinding, TenantId, Value,
};
use aster_ipc::{
    read_frame, write_frame, InitialCapsuleGrant, IpcRequest, IpcResponse, ModuleBundle,
    WireBrokerError,
};

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
    store_kind: StoreKind,
    /// Postgres connection URL when `store_kind == Postgres`. None for memory.
    db_url: Option<String>,
    /// Postgres schema where Convex tables live. Convex calls this `@db_name`;
    /// defaults to `public` when ASTER_DB_SCHEMA is unset, which matches a
    /// vanilla self-hosted Convex install.
    db_schema: String,
    /// Local-FS modules directory mounted into brokerd. Only meaningful for
    /// `ASTER_STORE=postgres`; memory-store brokerds report module loading as
    /// unavailable even when this is set.
    modules_dir: Option<PathBuf>,
}

impl BrokerConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path = env_path("ASTER_BROKER_SOCK")?;
        let tenant = TenantId::new(env_string("ASTER_TENANT")?);
        let deployment = DeploymentId::new(env_string("ASTER_DEPLOYMENT")?);
        let snapshot_ts = env_optional_u64("ASTER_SNAPSHOT_TS")?.unwrap_or(0);
        let seeds = parse_seeds(&env_optional_string("ASTER_SEED_I64")?.unwrap_or_default())?;
        let seal_key = CapsuleSealKey::derive_for_tests(env_string("ASTER_SEAL_SEED")?.as_bytes());
        let store_kind =
            StoreKind::from_env_value(&env_optional_string("ASTER_STORE")?.unwrap_or_default())?;
        let db_url = match store_kind {
            StoreKind::Memory => None,
            StoreKind::Postgres => Some(resolve_db_url()?),
        };
        let db_schema =
            env_optional_string("ASTER_DB_SCHEMA")?.unwrap_or_else(|| "public".to_string());
        let modules_dir = env_optional_string("ASTER_MODULES_DIR")?.map(PathBuf::from);
        Ok(Self {
            socket_path,
            tenant,
            deployment,
            snapshot_ts,
            seeds,
            seal_key,
            store_kind,
            db_url,
            db_schema,
            modules_dir,
        })
    }
}

trait ModuleBundleSource: Send + Sync {
    fn load_module_bundle(&self, path: &str) -> Result<Option<Vec<u8>>, BrokerError>;
}

struct NoModuleBundleSource {
    reason: &'static str,
}

impl ModuleBundleSource for NoModuleBundleSource {
    fn load_module_bundle(&self, _path: &str) -> Result<Option<Vec<u8>>, BrokerError> {
        Err(BrokerError::Remote(self.reason.to_string()))
    }
}

impl ModuleBundleSource for aster_store_postgres::PostgresCapsuleStore {
    fn load_module_bundle(&self, path: &str) -> Result<Option<Vec<u8>>, BrokerError> {
        aster_store_postgres::PostgresCapsuleStore::load_module_bundle(self, path)
            .map_err(BrokerError::from)
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
    let (store, module_source): (
        Arc<dyn CapsuleStore + Send + Sync>,
        Arc<dyn ModuleBundleSource + Send + Sync>,
    ) = match config.store_kind {
        StoreKind::Memory => {
            let mvcc = MvccStore::new();
            for (key, document) in config.seeds {
                mvcc.seed(key, document);
            }
            (
                Arc::new(mvcc),
                Arc::new(NoModuleBundleSource {
                    reason: "module loading requires ASTER_STORE=postgres",
                }),
            )
        }
        StoreKind::Postgres => {
            let url = config
                .db_url
                .clone()
                .expect("postgres url present by from_env");
            let pg_cfg = aster_store_postgres::PostgresConfig {
                url,
                schema: config.db_schema.clone(),
                modules_dir: config.modules_dir.clone(),
                ..aster_store_postgres::PostgresConfig::default()
            };
            // Connect is lazy — `connect()` builds the runtime + pool but
            // does NOT open a TCP connection. First snapshot_ts call
            // below is the one that actually checks if Postgres is up.
            // Failure here is a config error (bad URL, missing host),
            // worth dying at startup.
            let store = Arc::new(
                aster_store_postgres::PostgresCapsuleStore::connect(pg_cfg)
                    .map_err(|err| format!("postgres connect: {err}"))?,
            );
            (
                store.clone() as Arc<dyn CapsuleStore + Send + Sync>,
                store as Arc<dyn ModuleBundleSource + Send + Sync>,
            )
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
        module_source,
        seal_key: config.seal_key,
        tenant: config.tenant,
        deployment: config.deployment,
        snapshot_ts,
        sessions: SessionTable::default(),
    };

    let listener = UnixListener::bind(&config.socket_path)?;
    eprintln!(
        "aster_brokerd: ready socket={} snapshot_ts={}",
        config.socket_path.display(),
        snapshot_ts
    );

    // One connection per request, served serially until a Shutdown verb.
    // v0.6 capped total connections served since boot (ASTER_MAX_CONNECTIONS)
    // and exited past the cap — with one connection per read trap, a busy
    // broker killed itself mid-workload. The write path multiplies traps, so
    // the lifetime budget is gone; concurrency control belongs at the
    // accept/queue layer if the broker ever goes parallel.
    for stream in listener.incoming() {
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
        } => {
            // Only brokerd mints session bindings — a pre-bound context in
            // an InitialCapsule request is confused or hostile, never
            // legitimate. The request's cell_id/lease_epoch stand in for
            // trusted launch metadata in this prototype (a production
            // broker reads them from the launch channel, not the payload).
            if context.session.is_some() {
                return (
                    IpcResponse::InitialCapsule(Err(WireBrokerError::new(
                        "initial_context_bound",
                        "InitialCapsule requires an unbound context; brokerd mints the session",
                    ))),
                    false,
                );
            }
            let session = broker.sessions.mint(&context.cell_id, context.lease_epoch);
            let bound = SealContext::bound(context.cell_id, context.lease_epoch, session);
            let result =
                match broker.initial_capsule(&bound, tenant, deployment, snapshot_ts, prewarm) {
                    Ok(capsule) => Ok(InitialCapsuleGrant { capsule, session }),
                    Err(error) => {
                        // A failed grant returned no session id to anyone —
                        // drop the reservation so hostile cells can't bloat the
                        // table with failing requests.
                        broker.sessions.remove(&session);
                        Err(WireBrokerError::from(error))
                    }
                };
            (IpcResponse::InitialCapsule(result), false)
        }
        IpcRequest::HydratePoint {
            context,
            session,
            capsule,
            key,
        } => (
            IpcResponse::HydratePoint(broker.resolve_bound_context(session, &context).and_then(
                |bound| {
                    broker
                        .hydrate_point(&bound, capsule, key)
                        .map_err(WireBrokerError::from)
                },
            )),
            false,
        ),
        IpcRequest::HydratePrefix {
            context,
            session,
            capsule,
            prefix,
            limit,
        } => (
            IpcResponse::HydratePrefix(broker.resolve_bound_context(session, &context).and_then(
                |bound| {
                    broker
                        .hydrate_prefix(&bound, capsule, prefix, limit)
                        .map_err(WireBrokerError::from)
                },
            )),
            false,
        ),
        IpcRequest::LoadModuleBundle {
            context,
            session,
            capsule,
            path,
        } => (
            IpcResponse::LoadModuleBundle(
                broker
                    .resolve_bound_context(session, &context)
                    .and_then(|bound| {
                        broker
                            .load_module_bundle(&bound, capsule, path)
                            .map(|bundle| {
                                bundle.map(|(path, bytes)| ModuleBundle::from_bytes(path, &bytes))
                            })
                            .map_err(WireBrokerError::from)
                    }),
            ),
            false,
        ),
        // Shutdown is process-lifecycle scaffolding for the prototype
        // harnesses, not a capsule verb — it carries no capsule and grants
        // no data authority, so it stays outside the session gate.
        IpcRequest::Shutdown => (IpcResponse::ShutdownAck, true),
    }
}

/// Broker-side registry of live sessions: session id → the immutable
/// context the id was minted for. This is the C-CHANNEL repair's trusted
/// table: capsule verbs present a session id, and the broker rebuilds the
/// expected bound `SealContext` from THIS table — the request's serialized
/// context is only checked for equality against the record and then
/// discarded, never used as authority.
///
/// Unbounded for now: sessions get no end-of-life until the S9 commit verb
/// lands (commit/abort closes a session; lease-epoch fencing sweeps the
/// rest). Until then a hostile cell can grow this map by hammering
/// InitialCapsule — accepted for the prototype, tracked for S9.
#[derive(Default)]
struct SessionTable {
    sessions: Mutex<HashMap<SessionBinding, SessionEntry>>,
}

#[derive(Clone)]
struct SessionEntry {
    cell_id: String,
    lease_epoch: u64,
}

impl SessionTable {
    /// Mint a fresh unguessable session id and register it. OS entropy via
    /// `getrandom` — session ids gate whose seals verify on this channel,
    /// so anything predictable (time, counters, constant seeds) would let
    /// one cell impersonate another's channel. If the OS RNG fails the
    /// broker cannot operate securely; dying is the only safe behavior.
    fn mint(&self, cell_id: &str, lease_epoch: u64) -> SessionBinding {
        let mut sessions = self.sessions.lock().expect("session table lock");
        loop {
            let mut id = [0_u8; 32];
            getrandom::fill(&mut id).expect("OS entropy for session id");
            // 256-bit collision is astronomically unlikely; the loop is for
            // totality, not an expected path.
            if let Entry::Vacant(vacant) = sessions.entry(SessionBinding::from_bytes(id)) {
                let session = *vacant.key();
                vacant.insert(SessionEntry {
                    cell_id: cell_id.to_string(),
                    lease_epoch,
                });
                return session;
            }
        }
    }

    fn remove(&self, session: &SessionBinding) {
        self.sessions
            .lock()
            .expect("session table lock")
            .remove(session);
    }

    fn lookup(&self, session: &SessionBinding) -> Option<SessionEntry> {
        self.sessions
            .lock()
            .expect("session table lock")
            .get(session)
            .cloned()
    }
}

struct ProcessBroker {
    store: Arc<dyn CapsuleStore + Send + Sync>,
    module_source: Arc<dyn ModuleBundleSource + Send + Sync>,
    seal_key: CapsuleSealKey,
    tenant: TenantId,
    deployment: DeploymentId,
    snapshot_ts: u64,
    sessions: SessionTable,
}

impl ProcessBroker {
    /// Resolve the presented session id into the bound `SealContext` every
    /// capsule verb verifies and reseals with. The claimed request context
    /// must equal the trusted record (its own binding may be omitted or
    /// must match) — theorem: "a serialized context in a request is either
    /// omitted or required to equal ctx_c". The returned context is built
    /// exclusively from the broker's own table entry.
    fn resolve_bound_context(
        &self,
        session: Option<SessionBinding>,
        claimed: &SealContext,
    ) -> Result<SealContext, WireBrokerError> {
        let Some(session) = session else {
            return Err(WireBrokerError::new(
                "session_required",
                "capsule verbs must present the session id minted at InitialCapsule",
            ));
        };
        let Some(entry) = self.sessions.lookup(&session) else {
            return Err(WireBrokerError::new(
                "unknown_session",
                "session id is not registered with this broker",
            ));
        };
        let claimed_binding_ok = match claimed.session {
            None => true,
            Some(claimed_session) => claimed_session == session,
        };
        if claimed.cell_id != entry.cell_id
            || claimed.lease_epoch != entry.lease_epoch
            || !claimed_binding_ok
        {
            return Err(WireBrokerError::new(
                "session_context_mismatch",
                "request context does not match the session's registered context",
            ));
        }
        Ok(SealContext::bound(
            entry.cell_id,
            entry.lease_epoch,
            session,
        ))
    }
}

impl ProcessBroker {
    fn verify_capsule(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
    ) -> Result<(), BrokerError> {
        let capsule = capsule.into_capsule(&self.seal_key, context)?;
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
        Ok(())
    }

    fn load_module_bundle(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        path: String,
    ) -> Result<Option<(String, Vec<u8>)>, BrokerError> {
        if path.trim().is_empty() {
            return Err(BrokerError::Remote("module path is required".into()));
        }
        self.verify_capsule(context, capsule)?;
        self.module_source
            .load_module_bundle(&path)
            .map(|bundle| bundle.map(|bytes| (path, bytes)))
    }
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

    fn hydrate_prefix(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        prefix: String,
        limit: usize,
    ) -> Result<SealedCapsule, BrokerError> {
        if limit == 0 {
            return Err(BrokerError::ZeroScanLimit);
        }
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
        // Certificates are evidence about the capsule snapshot: scan at
        // capsule.ts (== broker snapshot after the check above), never at
        // whatever the store head has advanced to.
        let (certificate, entries) = self.store.scan_prefix(&prefix, limit, capsule.ts)?;
        capsule.hydrate_range(certificate, entries);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn parses_seed_documents() {
        let seeds = parse_seeds("items/a:value:20,items/b:value:22").expect("parse");
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].0, DocumentId::new("items/a"));
        assert_eq!(seeds[1].1.get("value"), Some(&Value::Int(22)));
    }

    /// Drive the full wire path for InitialCapsule: mint + grant. Session
    /// tests must go through `handle_request` — calling the trait method
    /// directly would skip the layer under test.
    fn initial_grant(broker: &ProcessBroker, context: &SealContext) -> InitialCapsuleGrant {
        match handle_request(
            broker,
            IpcRequest::InitialCapsule {
                context: context.clone(),
                tenant: TenantId::new("tenant-test"),
                deployment: DeploymentId::new("dep-test"),
                snapshot_ts: 1,
                prewarm: Vec::new(),
            },
        )
        .0
        {
            IpcResponse::InitialCapsule(Ok(grant)) => grant,
            other => panic!("initial capsule should succeed, got {other:?}"),
        }
    }

    #[test]
    fn initial_capsule_mints_session_and_seals_bound() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        // The registered entry is what future hydrates resolve against.
        let entry = broker
            .sessions
            .lookup(&grant.session)
            .expect("session registered");
        assert_eq!(entry.cell_id, "cell-a");
        assert_eq!(entry.lease_epoch, 1);

        // The capsule is sealed for the BOUND context — an unbound verify
        // (the pre-S4 shape) must fail.
        let bound = SealContext::bound("cell-a", 1, grant.session);
        grant
            .capsule
            .verify(&broker.seal_key, &bound)
            .expect("bound verify");
        assert!(grant.capsule.verify(&broker.seal_key, &context).is_err());
    }

    #[test]
    fn initial_capsule_rejects_pre_bound_context() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::bound("cell-a", 1, SessionBinding::from_bytes([0x11; 32]));
        let response = handle_request(
            &broker,
            IpcRequest::InitialCapsule {
                context,
                tenant: TenantId::new("tenant-test"),
                deployment: DeploymentId::new("dep-test"),
                snapshot_ts: 1,
                prewarm: Vec::new(),
            },
        )
        .0;
        match response {
            IpcResponse::InitialCapsule(Err(error)) => {
                assert_eq!(error.code, "initial_context_bound", "got {error:?}");
            }
            other => panic!("pre-bound initial should fail, got {other:?}"),
        }
    }

    #[test]
    fn failed_initial_capsule_leaves_no_session_behind() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let response = handle_request(
            &broker,
            IpcRequest::InitialCapsule {
                context: SealContext::new("cell-a", 1),
                tenant: TenantId::new("tenant-other"),
                deployment: DeploymentId::new("dep-test"),
                snapshot_ts: 1,
                prewarm: Vec::new(),
            },
        )
        .0;
        assert!(
            matches!(response, IpcResponse::InitialCapsule(Err(_))),
            "tenant mismatch must fail the grant"
        );
        assert!(
            broker
                .sessions
                .sessions
                .lock()
                .expect("session table lock")
                .is_empty(),
            "failed grant must not leak a table entry"
        );
    }

    #[test]
    fn hydrate_requires_a_session() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: None,
                capsule: grant.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "session_required", "got {error:?}");
            }
            other => panic!("unbound hydrate should fail, got {other:?}"),
        }
    }

    #[test]
    fn hydrate_rejects_unknown_session() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: Some(SessionBinding::from_bytes([0x99; 32])),
                capsule: grant.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "unknown_session", "got {error:?}");
            }
            other => panic!("unknown session should fail, got {other:?}"),
        }
    }

    #[test]
    fn hydrate_rejects_claimed_context_that_mismatches_session() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let grant = initial_grant(&broker, &SealContext::new("cell-a", 1));

        // Claimed cell-b on cell-a's session: C-CHANNEL relabeling attempt.
        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context: SealContext::new("cell-b", 1),
                session: Some(grant.session),
                capsule: grant.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "session_context_mismatch", "got {error:?}");
            }
            other => panic!("relabelled hydrate should fail, got {other:?}"),
        }
    }

    /// The theorem's re-spawned-cell scenario: two sessions minted for the
    /// SAME cell_id and epoch. A capsule sealed under session A presented
    /// on session B passes the table checks (identical public context) and
    /// must die in seal verification — the session binding in the MAC is
    /// what carries the rejection.
    #[test]
    fn capsule_from_another_session_fails_seal_verification() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::new("cell-a", 1);
        let grant_a = initial_grant(&broker, &context);
        let grant_b = initial_grant(&broker, &context);
        assert_ne!(grant_a.session, grant_b.session, "sessions must be unique");

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: Some(grant_b.session),
                capsule: grant_a.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert!(
                    error.code.contains("WrongSession"),
                    "expected seal-level wrong-session rejection, got {error:?}"
                );
            }
            other => panic!("cross-session capsule should fail, got {other:?}"),
        }
    }

    #[test]
    fn load_module_bundle_requires_matching_capsule_context() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(Some(b"zip".to_vec()))));
        let grant = initial_grant(&broker, &SealContext::new("cell-a", 1));

        // Pre-S4 this died in seal verification (WrongCell); the session
        // table now rejects the relabelled context before the seal is
        // even consulted.
        let response = handle_request(
            &broker,
            IpcRequest::LoadModuleBundle {
                context: SealContext::new("cell-b", 1),
                session: Some(grant.session),
                capsule: grant.capsule,
                path: "messages.js".into(),
            },
        )
        .0;

        match response {
            IpcResponse::LoadModuleBundle(Err(error)) => {
                assert_eq!(error.code, "session_context_mismatch", "got {error:?}");
            }
            other => panic!("wrong-cell module load should fail, got {other:?}"),
        }
    }

    #[test]
    fn load_module_bundle_returns_base64_payload() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(Some(b"zip".to_vec()))));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let response = handle_request(
            &broker,
            IpcRequest::LoadModuleBundle {
                context,
                session: Some(grant.session),
                capsule: grant.capsule,
                path: "messages.js".into(),
            },
        )
        .0;

        match response {
            IpcResponse::LoadModuleBundle(Ok(Some(bundle))) => {
                assert_eq!(bundle.path, "messages.js");
                assert_eq!(bundle.decode_bytes().expect("decode"), b"zip");
            }
            other => panic!("module load should return bytes, got {other:?}"),
        }
    }

    fn test_broker(module_source: Arc<dyn ModuleBundleSource + Send + Sync>) -> ProcessBroker {
        ProcessBroker {
            store: Arc::new(MvccStore::new()),
            module_source,
            seal_key: CapsuleSealKey::derive_for_tests(b"test-seed"),
            tenant: TenantId::new("tenant-test"),
            deployment: DeploymentId::new("dep-test"),
            snapshot_ts: 1,
            sessions: SessionTable::default(),
        }
    }

    struct FakeModuleSource {
        bytes: Mutex<Option<Vec<u8>>>,
    }

    impl FakeModuleSource {
        fn new(bytes: Option<Vec<u8>>) -> Self {
            Self {
                bytes: Mutex::new(bytes),
            }
        }
    }

    impl ModuleBundleSource for FakeModuleSource {
        fn load_module_bundle(&self, _path: &str) -> Result<Option<Vec<u8>>, BrokerError> {
            Ok(self.bytes.lock().expect("module source mutex").clone())
        }
    }
}
