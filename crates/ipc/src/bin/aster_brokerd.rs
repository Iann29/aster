use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aster_broker::{
    BrokerError, CapsuleBrokerClient, CapsuleStore, CommitFence, FenceInput, MemoryFence,
};
use aster_capsule::{
    CapsuleSealKey, DeploymentId, Document, DocumentId, MvccStore, ObservedWindow, SealContext,
    SealedCapsule, SessionBinding, TenantId, Value,
};
use aster_ipc::{
    read_frame, write_frame, InitialCapsuleGrant, IpcError, IpcRequest, IpcResponse, ModuleBundle,
    WireBrokerError, WireCommitOutcome,
};
use aster_store_postgres::{WritePlane, WritePlaneConfig};

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
    /// Memory-mode ONLY (prototype stand-in): the lease epoch this broker
    /// commits under and stamps into every minted session, default 1. In
    /// Postgres mode the epoch comes from `WritePlane::acquire_lease` at
    /// boot — the storage lease authority — and this env is ignored
    /// (C-CHANNEL obligation #2: the epoch is never self-asserted where a
    /// real authority exists).
    lease_epoch: Option<u64>,
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
        let lease_epoch = env_optional_u64("ASTER_LEASE_EPOCH")?;
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
            lease_epoch,
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
    // match. The commit fence rides the same dispatch: the write path's
    // lease authority is inseparable from where commits land.
    let configured_ts = config.snapshot_ts;
    let (store, module_source, fence, authority_epoch): (
        Arc<dyn CapsuleStore + Send + Sync>,
        Arc<dyn ModuleBundleSource + Send + Sync>,
        Arc<dyn CommitFence>,
        u64,
    ) = match config.store_kind {
        StoreKind::Memory => {
            let mvcc = Arc::new(MvccStore::new());
            for (key, document) in config.seeds {
                mvcc.seed(key, document);
            }
            // Prototype stand-in (C-CHANNEL obligation #2 applies only
            // where a real lease authority exists): memory mode has no
            // durable authority, so the epoch comes from the launch env
            // (default 1) and seeds the in-memory fence's lease directly.
            let epoch = config.lease_epoch.unwrap_or(1);
            let fence = MemoryFence::new(mvcc.clone());
            fence.seed_lease(&config.tenant.0, &config.deployment.0, epoch);
            (
                mvcc,
                Arc::new(NoModuleBundleSource {
                    reason: "module loading requires ASTER_STORE=postgres",
                }),
                Arc::new(fence),
                epoch,
            )
        }
        StoreKind::Postgres => {
            let url = config
                .db_url
                .clone()
                .expect("postgres url present by from_env");
            let pg_cfg = aster_store_postgres::PostgresConfig {
                url: url.clone(),
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
            // Lease authority (C-CHANNEL obligation #2): acquire the
            // single-writer lease at boot. The resulting epoch is BOTH
            // this broker's committer epoch and the lease_epoch stamped
            // into every minted session — the env variable is not
            // consulted. Sessions minted before a failover die naturally:
            // once a new holder acquires, the fence's V2 check rejects
            // their context epoch.
            let plane = Arc::new(
                WritePlane::connect(WritePlaneConfig {
                    url,
                    ..WritePlaneConfig::default()
                })
                .map_err(|err| format!("write plane connect: {err}"))?,
            );
            plane
                .ensure_schema()
                .map_err(|err| format!("write plane schema: {err}"))?;
            let holder = format!("aster_brokerd:{}", std::process::id());
            let epoch =
                WritePlane::acquire_lease(&plane, &config.tenant.0, &config.deployment.0, &holder)
                    .map_err(|err| format!("acquire lease: {err}"))?;
            if config.lease_epoch.is_some() {
                eprintln!(
                    "aster_brokerd: ASTER_LEASE_EPOCH is ignored with ASTER_STORE=postgres — \
                     the storage lease authority owns the epoch"
                );
            }
            (
                store.clone() as Arc<dyn CapsuleStore + Send + Sync>,
                store as Arc<dyn ModuleBundleSource + Send + Sync>,
                plane,
                epoch,
            )
        }
    };
    // Harnesses parse this line to launch cells with the matching epoch
    // (see docker/smoke-postgres.sh) — keep the `lease epoch=` shape.
    eprintln!(
        "aster_brokerd: lease epoch={} source={}",
        authority_epoch,
        match config.store_kind {
            StoreKind::Memory => "env-standin",
            StoreKind::Postgres => "lease-authority",
        }
    );
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
        fence,
        authority_epoch,
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
    //
    // Per-connection I/O failures stay per-connection: a `?` on the response
    // write let ONE bad client (a peer that hung up before reading — EPIPE,
    // Rust ignores SIGPIPE — or a response that outgrew the frame cap) kill
    // the broker and with it the in-process session table every other live
    // cell depends on. Only accept errors remain fatal.
    for stream in listener.incoming() {
        let mut stream = stream?;
        let request = read_frame::<IpcRequest>(&mut stream);
        let should_shutdown = match request {
            Ok(request) => {
                let (response, should_shutdown) = handle_request(&broker, request);
                send_response(&mut stream, &response);
                should_shutdown
            }
            Err(error) => {
                let response =
                    IpcResponse::Error(WireBrokerError::new("bad_request", error.to_string()));
                send_response(&mut stream, &response);
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

/// Write one response, containing every failure to that connection.
///
/// `write_frame` serializes fully before writing, so a `FrameTooLarge`
/// response has put zero bytes on the socket — the stream is still clean
/// and the peer gets a small structured `response_too_large` error it can
/// act on (e.g. re-issue the hydrate with a smaller limit) instead of a
/// dead connection. No broker-side cap on HydratePrefix limits backs this
/// up: response size, not the requested limit, is the scarce resource (a
/// huge limit over a small prefix is harmless, and store allocation is
/// bounded by what actually matches), and this serialize-then-check gate
/// measures the real thing exactly.
///
/// Any other write failure — EPIPE from a peer that closed before reading,
/// a short write — is logged and dropped; the accept loop moves on.
fn send_response(stream: &mut UnixStream, response: &IpcResponse) {
    match write_frame(stream, response) {
        Ok(()) => {}
        Err(IpcError::FrameTooLarge { len, max }) => {
            let fallback = IpcResponse::Error(WireBrokerError::new(
                "response_too_large",
                format!("response frame would be {len} bytes; the IPC cap is {max}"),
            ));
            if let Err(error) = write_frame(stream, &fallback) {
                eprintln!("aster_brokerd: connection write error: {error}");
            }
        }
        Err(error) => {
            eprintln!("aster_brokerd: connection write error: {error}");
        }
    }
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
            // legitimate. The request's cell_id stands in for trusted
            // launch metadata in this prototype (a production broker reads
            // it from the launch channel, not the payload — the remaining
            // C-CHANNEL obligation #1).
            if context.session.is_some() {
                return (
                    IpcResponse::InitialCapsule(Err(WireBrokerError::new(
                        "initial_context_bound",
                        "InitialCapsule requires an unbound context; brokerd mints the session",
                    ))),
                    false,
                );
            }
            // The lease epoch, by contrast, is authority-derived since S9a
            // (C-CHANNEL obligation #2): every minted session carries the
            // broker's own epoch. A context claiming any other value is a
            // stale pre-failover launch config or hostile — refuse at mint
            // so the cell fails fast instead of dying on its first hydrate.
            if context.lease_epoch != broker.authority_epoch {
                return (
                    IpcResponse::InitialCapsule(Err(WireBrokerError::new(
                        "stale_lease_epoch",
                        format!(
                            "context lease epoch {} does not match the lease authority epoch {}",
                            context.lease_epoch, broker.authority_epoch
                        ),
                    ))),
                    false,
                );
            }
            let session = broker
                .sessions
                .mint(&context.cell_id, broker.authority_epoch);
            let bound = SealContext::bound(context.cell_id, broker.authority_epoch, session);
            let result =
                match broker.initial_capsule(&bound, tenant, deployment, snapshot_ts, prewarm) {
                    Ok(capsule) => Ok(InitialCapsuleGrant { capsule, session }),
                    Err(error) => {
                        // A failed grant returned no session id to anyone —
                        // drop the reservation so hostile cells can't bloat the
                        // table with failing requests.
                        let _ = broker.sessions.remove(&session);
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
        IpcRequest::Commit {
            session,
            capsule,
            declared_reads,
            writes,
        } => {
            // Commit carries no separate context claim — the capsule's own
            // seal fields ARE the claimed context for the session
            // chokepoint. Equality against the trusted table entry happens
            // in resolve_bound_context; the seal MAC against the rebuilt
            // bound context (inside `commit`) is the enforcement.
            let claimed = SealContext {
                cell_id: capsule.seal().cell_id.clone(),
                lease_epoch: capsule.seal().lease_epoch,
                session: capsule.seal().session,
            };
            let result = broker
                .resolve_bound_context(session, &claimed)
                .and_then(|bound| {
                    let outcome = broker.commit(&bound, capsule, &declared_reads, &writes);
                    // Session end-of-life (the S4 deferred obligation): one
                    // session = one transaction attempt. EVERY answer past
                    // the session gate — Committed, Conflict, or a
                    // structured rejection — closes the session; only
                    // transport failures (which never reach this point)
                    // leave it open. Replay of the ISSUED capsule bytes is
                    // inevitable under bearer semantics (theorem CE2.1);
                    // closing the session here is what bounds it.
                    if let Some(session) = session {
                        let _ = broker.sessions.remove(&session);
                    }
                    outcome
                });
            (IpcResponse::Commit(result), false)
        }
        // Abort is the no-commit end-of-life for a session: same closure
        // rule as Commit, no capsule and no data authority involved. The
        // session gate still applies in table form — unknown ids are
        // rejected, not silently ignored, so double-closes surface.
        IpcRequest::Abort { session } => {
            let result = match broker.sessions.remove(&session) {
                Some(_) => Ok(()),
                None => Err(WireBrokerError::new(
                    "unknown_session",
                    "session id is not registered with this broker",
                )),
            };
            (IpcResponse::Abort(result), false)
        }
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
/// End-of-life (S9a): one session = one transaction attempt — Commit (any
/// structured answer) and Abort both remove the entry, and subsequent
/// verbs on the id get `unknown_session`. What remains unbounded is the
/// MINT side: a hostile cell can still grow the table by hammering
/// InitialCapsule and never finishing — accepted for the prototype,
/// alongside launch-token minting (C-CHANNEL obligation #1).
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

    /// Remove a session, returning its entry so callers can distinguish
    /// a real close from a no-op (Abort's unknown_session answer rides
    /// on this — one atomic map op, no lookup-then-remove race).
    fn remove(&self, session: &SessionBinding) -> Option<SessionEntry> {
        self.sessions
            .lock()
            .expect("session table lock")
            .remove(session)
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
    /// The commit fence commits land through: the Postgres `WritePlane`
    /// in postgres mode, `MemoryFence` over the read store otherwise.
    fence: Arc<dyn CommitFence>,
    /// The lease epoch this broker holds — acquired from the storage
    /// lease authority at boot in postgres mode, env stand-in (default 1)
    /// in memory mode. Committer epoch for every fence call AND the
    /// lease_epoch stamped into every minted session.
    authority_epoch: u64,
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

    /// The theorem's fence admission path (§1.8, Variante B): verify the
    /// capsule seal against the session-bound context, validate the
    /// declared subset against the sealed observation set, reduce
    /// everything to a `FenceInput`, and let the commit fence decide.
    fn commit(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        declared_reads: &[DocumentId],
        writes: &[(DocumentId, Option<Document>)],
    ) -> Result<WireCommitOutcome, WireBrokerError> {
        // (a) Seal verification against the table-rebuilt bound context —
        // the same trust chokepoint as every hydrate — plus the broker's
        // tenant/deployment/snapshot pins.
        let capsule = capsule
            .into_capsule(&self.seal_key, context)
            .map_err(|error| WireBrokerError::from(BrokerError::from(error)))?;
        if capsule.tenant != self.tenant {
            return Err(WireBrokerError::from(BrokerError::TenantMismatch));
        }
        if capsule.deployment != self.deployment {
            return Err(WireBrokerError::from(BrokerError::DeploymentMismatch));
        }
        if capsule.ts != self.snapshot_ts {
            return Err(WireBrokerError::from(BrokerError::Remote(format!(
                "capsule snapshot_ts {} is not broker snapshot {}",
                capsule.ts, self.snapshot_ts
            ))));
        }
        // (b) Variante B declaration check (Repair B-SUBSET): every
        // declared point must reference an observation atom the sealed
        // capsule actually carries, without duplicates. The INVERSE is
        // legal by design: a capsule key left undeclared demotes that
        // dependency to an authorized blind write — T2 still orders it;
        // omission costs the cell its own conflict protection, never
        // anyone else's safety.
        let mut declared = BTreeSet::new();
        for key in declared_reads {
            if !capsule.docs.contains_key(key) {
                return Err(WireBrokerError::new(
                    "declared_read_not_in_capsule",
                    format!(
                        "declared read {:?} is not an observation this capsule carries",
                        key.0
                    ),
                ));
            }
            if !declared.insert(key) {
                return Err(WireBrokerError::new(
                    "duplicate_declared_read",
                    format!("declared read {:?} appears more than once", key.0),
                ));
            }
        }
        // (c) Build the fence input. Point observations are the declared
        // keys — the versioned state they were observed at is pinned by
        // the sealed capsule at `capsule.ts` (S-SNAPSHOT), and the fence's
        // `Changed({k}; (s, h])` predicate needs only the key. Conflict
        // windows are derived by THIS committer from ALL sealed range
        // certificates (theorem A6 — windows are never a cell claim);
        // conservative inclusion of every certificate is the honest
        // default: a scan the declaration forgot still keeps its phantom
        // protection, at the price of spurious aborts, never a missed
        // conflict.
        let windows: Vec<ObservedWindow> = capsule
            .ranges
            .iter()
            .map(|certificate| certificate.window())
            .collect();
        let input = FenceInput {
            tenant: &self.tenant.0,
            deployment: &self.deployment.0,
            committer_epoch: self.authority_epoch,
            context_epoch: context.lease_epoch,
            snapshot: capsule.ts,
            read_points: declared_reads,
            read_windows: &windows,
            writes,
        };
        self.fence
            .commit(&input)
            .map(WireCommitOutcome::from)
            .map_err(|error| WireBrokerError::from(BrokerError::from(error)))
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
    use aster_capsule::{doc_with_i64, SnapshotCapsule, VersionedDocument};
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
        initial_grant_with_prewarm(broker, context, Vec::new())
    }

    fn initial_grant_with_prewarm(
        broker: &ProcessBroker,
        context: &SealContext,
        prewarm: Vec<DocumentId>,
    ) -> InitialCapsuleGrant {
        match handle_request(
            broker,
            IpcRequest::InitialCapsule {
                context: context.clone(),
                tenant: TenantId::new("tenant-test"),
                deployment: DeploymentId::new("dep-test"),
                snapshot_ts: 1,
                prewarm,
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

    /// Wire twin of the capsule-level rewritten-session test: grants A and
    /// B share cell_id/epoch, so rewriting A's (attacker-mutable)
    /// `seal.session` claim to B and presenting on B's session passes both
    /// the session table AND the seal's friendly ct_eq pre-check — only the
    /// session bytes inside the MAC reject the transplant. Pins the wire
    /// code to the seal MacMismatch mapping, NOT unknown_session /
    /// session_context_mismatch / WrongSession; a mutant that drops the
    /// session bytes from `seal_mac` hydrates successfully here.
    #[test]
    fn rewritten_session_claim_dies_at_the_seal_mac() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let context = SealContext::new("cell-a", 1);
        let grant_a = initial_grant(&broker, &context);
        let grant_b = initial_grant(&broker, &context);
        assert_ne!(grant_a.session, grant_b.session, "sessions must be unique");

        let mut capsule = grant_a.capsule;
        capsule.seal_mut_for_test().session = Some(grant_b.session);

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: Some(grant_b.session),
                capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "seal_MacMismatch", "got {error:?}");
            }
            other => panic!("rewritten session claim should fail, got {other:?}"),
        }
    }

    /// Wire mirror of the broker-level snapshot pinning tests: a hydrate
    /// through `handle_request` must scan at the capsule's ts even after
    /// the store head advanced past the broker's pinned snapshot. A mutant
    /// that scans at `store.snapshot_ts()` leaks docs/b into the
    /// certificate and the capsule.
    #[test]
    fn wire_hydrate_prefix_scans_at_capsule_ts_not_store_head() {
        let store = Arc::new(MvccStore::new());
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        let snapshot_ts = store.snapshot_ts().expect("snapshot ts");
        // initial_grant pins snapshot_ts=1 in its request; the single seed
        // above puts the store head exactly there.
        assert_eq!(snapshot_ts, 1);
        let broker = ProcessBroker {
            store: store.clone(),
            module_source: Arc::new(FakeModuleSource::new(None)),
            seal_key: CapsuleSealKey::derive_for_tests(b"test-seed"),
            tenant: TenantId::new("tenant-test"),
            deployment: DeploymentId::new("dep-test"),
            snapshot_ts,
            sessions: SessionTable::default(),
            fence: Arc::new(MemoryFence::new(store.clone())),
            authority_epoch: 1,
        };
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        // Store head advances past the broker's pinned snapshot.
        store.seed(DocumentId::new("docs/b"), doc_with_i64("value", 2));

        let response = handle_request(
            &broker,
            IpcRequest::HydratePrefix {
                context,
                session: Some(grant.session),
                capsule: grant.capsule,
                prefix: "docs/".into(),
                limit: 10,
            },
        )
        .0;
        let sealed = match response {
            IpcResponse::HydratePrefix(Ok(sealed)) => sealed,
            other => panic!("prefix hydrate should succeed, got {other:?}"),
        };
        let capsule = sealed.capsule();
        assert_eq!(capsule.ranges.len(), 1);
        assert_eq!(capsule.ranges[0].keys, vec![DocumentId::new("docs/a")]);
        assert!(
            capsule.get(&DocumentId::new("docs/b")).is_none(),
            "post-snapshot key must not hydrate into the capsule"
        );
    }

    /// C4 containment at the frame layer: a response that outgrows the
    /// frame cap never touches the socket (write_frame serializes first)
    /// and the peer gets the small structured error instead. The
    /// process_boundary E2E drives the same path over a real brokerd.
    #[test]
    fn oversized_response_maps_to_structured_error() {
        let (mut server, mut peer) = UnixStream::pair().expect("socketpair");
        let mut document = Document::new();
        document.insert(
            "blob".to_string(),
            Value::Text("x".repeat(2 * aster_ipc::MAX_FRAME_BYTES)),
        );
        let mut capsule = SnapshotCapsule::empty(
            TenantId::new("tenant-test"),
            DeploymentId::new("dep-test"),
            1,
        );
        capsule.hydrate_point(
            DocumentId::new("docs/big"),
            VersionedDocument {
                version: Some(1),
                document: Some(document),
            },
        );
        let sealed = SealedCapsule::new(
            capsule,
            &CapsuleSealKey::derive_for_tests(b"test-seed"),
            &SealContext::new("cell-a", 1),
        );

        send_response(&mut server, &IpcResponse::HydratePrefix(Ok(sealed)));

        let received: IpcResponse = read_frame(&mut peer).expect("fallback frame");
        match received {
            IpcResponse::Error(error) => {
                assert_eq!(error.code, "response_too_large", "got {error:?}");
            }
            other => panic!("expected structured error, got {other:?}"),
        }
    }

    /// C4 containment for a peer that hung up before reading: the write
    /// hits EPIPE (Rust ignores SIGPIPE) and must be swallowed, never
    /// propagated up to kill the accept loop.
    #[test]
    fn send_response_survives_peer_hangup() {
        let (mut server, peer) = UnixStream::pair().expect("socketpair");
        drop(peer);
        send_response(&mut server, &IpcResponse::ShutdownAck);
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

    /// Drive the Commit verb through the full wire path.
    fn commit_request(
        broker: &ProcessBroker,
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        declared_reads: Vec<DocumentId>,
        writes: Vec<(DocumentId, Option<Document>)>,
    ) -> Result<WireCommitOutcome, WireBrokerError> {
        match handle_request(
            broker,
            IpcRequest::Commit {
                session,
                capsule,
                declared_reads,
                writes,
            },
        )
        .0
        {
            IpcResponse::Commit(result) => result,
            other => panic!("expected a Commit response, got {other:?}"),
        }
    }

    fn put(key: &str, value: i64) -> (DocumentId, Option<Document>) {
        (DocumentId::new(key), Some(doc_with_i64("value", value)))
    }

    /// InitialCapsule mints sessions with the broker's OWN epoch; a
    /// context claiming any other epoch is refused at mint (C-CHANNEL
    /// obligation #2 — the epoch is authority-derived, never accepted
    /// from the payload) and leaves no session behind.
    #[test]
    fn initial_capsule_rejects_stale_lease_epoch() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let response = handle_request(
            &broker,
            IpcRequest::InitialCapsule {
                context: SealContext::new("cell-a", 9),
                tenant: TenantId::new("tenant-test"),
                deployment: DeploymentId::new("dep-test"),
                snapshot_ts: 1,
                prewarm: Vec::new(),
            },
        )
        .0;
        match response {
            IpcResponse::InitialCapsule(Err(error)) => {
                assert_eq!(error.code, "stale_lease_epoch", "got {error:?}");
            }
            other => panic!("stale-epoch mint should fail, got {other:?}"),
        }
        assert!(
            broker
                .sessions
                .sessions
                .lock()
                .expect("session table lock")
                .is_empty(),
            "refused mint must not leak a table entry"
        );
    }

    /// Happy path: declared subset + fence Committed, and the write is
    /// visible through the shared store the fence appended into.
    #[test]
    fn commit_happy_path_appends_through_the_fence() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7)); // head = broker snapshot = 1
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        let outcome = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            vec![DocumentId::new("docs/1")],
            vec![put("docs/2", 42)],
        )
        .expect("commit should pass the fence");
        assert_eq!(outcome, WireCommitOutcome::Committed { ts: 2 });
        let written = store.read_at(&DocumentId::new("docs/2"), 2);
        assert_eq!(written.version, Some(2));
        assert!(written.document.is_some(), "fence write must be visible");
    }

    /// Variante B omission (T2): a capsule key left UNdeclared is a legal
    /// blind write — the fence must not conflict on interference with an
    /// observation the cell chose not to declare.
    #[test]
    fn undeclared_capsule_key_is_a_legal_blind_write() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        // Interfering write to the OBSERVED-but-undeclared key.
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 8));

        let outcome = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            Vec::new(),
            vec![put("docs/other", 1)],
        )
        .expect("blind write should commit");
        assert!(
            matches!(outcome, WireCommitOutcome::Committed { .. }),
            "omission demotes to an authorized blind write, got {outcome:?}"
        );
    }

    /// B-SUBSET: declaring a key the capsule never carried is a structured
    /// error — and the session still closes (the request spent its one
    /// transaction attempt).
    #[test]
    fn commit_rejects_declared_read_not_in_capsule() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let error = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule.clone(),
            vec![DocumentId::new("docs/ghost")],
            vec![put("docs/2", 1)],
        )
        .expect_err("undeclarable read must be rejected");
        assert_eq!(error.code, "declared_read_not_in_capsule", "got {error:?}");
        assert!(
            broker.sessions.lookup(&grant.session).is_none(),
            "a structured commit answer past the session gate closes the session"
        );
    }

    /// B-SUBSET also rejects duplicate declarations.
    #[test]
    fn commit_rejects_duplicate_declared_read() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        let error = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            vec![DocumentId::new("docs/1"), DocumentId::new("docs/1")],
            vec![put("docs/2", 1)],
        )
        .expect_err("duplicate declaration must be rejected");
        assert_eq!(error.code, "duplicate_declared_read", "got {error:?}");
    }

    /// The commit verb goes through the same session chokepoint as every
    /// capsule verb: no session and unknown sessions are structured
    /// rejections (and neither touches the fence).
    #[test]
    fn commit_requires_a_live_session() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let error = commit_request(
            &broker,
            None,
            grant.capsule.clone(),
            Vec::new(),
            vec![put("docs/2", 1)],
        )
        .expect_err("unbound commit must fail");
        assert_eq!(error.code, "session_required", "got {error:?}");

        let error = commit_request(
            &broker,
            Some(SessionBinding::from_bytes([0x77; 32])),
            grant.capsule,
            Vec::new(),
            vec![put("docs/2", 1)],
        )
        .expect_err("unknown-session commit must fail");
        assert_eq!(error.code, "unknown_session", "got {error:?}");
    }

    /// Session end-of-life: a commit (any outcome) closes the session, so
    /// the next verb on it is unknown.
    #[test]
    fn commit_closes_the_session() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        let outcome = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule.clone(),
            vec![DocumentId::new("docs/1")],
            vec![put("docs/2", 1)],
        )
        .expect("commit");
        assert!(matches!(outcome, WireCommitOutcome::Committed { .. }));

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: Some(grant.session),
                capsule: grant.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "unknown_session", "got {error:?}");
            }
            other => panic!("post-commit verb should fail, got {other:?}"),
        }
    }

    /// Abort closes the session without committing; a second abort (or any
    /// later verb) sees unknown_session.
    #[test]
    fn abort_closes_the_session() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant(&broker, &context);

        let response = handle_request(
            &broker,
            IpcRequest::Abort {
                session: grant.session,
            },
        )
        .0;
        assert!(
            matches!(response, IpcResponse::Abort(Ok(()))),
            "first abort should close cleanly, got {response:?}"
        );

        let response = handle_request(
            &broker,
            IpcRequest::Abort {
                session: grant.session,
            },
        )
        .0;
        match response {
            IpcResponse::Abort(Err(error)) => {
                assert_eq!(error.code, "unknown_session", "got {error:?}");
            }
            other => panic!("double abort should fail, got {other:?}"),
        }

        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context,
                session: Some(grant.session),
                capsule: grant.capsule,
                key: DocumentId::new("docs/1"),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "unknown_session", "got {error:?}");
            }
            other => panic!("post-abort verb should fail, got {other:?}"),
        }
    }

    /// A session minted before a failover dies at the FENCE, not at the
    /// session table: after a new holder acquires the lease, the old
    /// context epoch is rejected by the V2 check as a structured
    /// StaleEpoch outcome.
    #[test]
    fn commit_stale_epoch_context_rejected_via_fence_outcome() {
        let (broker, store, fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        // Failover: a new holder acquires; the lease moves to epoch 2
        // while this broker (and the minted session) stay at 1.
        let new_epoch = fence
            .acquire_lease("tenant-test", "dep-test", "committer-b")
            .expect("failover acquire");
        assert_eq!(new_epoch, 2);

        let outcome = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            vec![DocumentId::new("docs/1")],
            vec![put("docs/2", 1)],
        )
        .expect("fence answers, structured");
        assert_eq!(outcome, WireCommitOutcome::StaleEpoch { lease_epoch: 2 });
    }

    /// Conflict surfaces on the wire: a write lands between capsule
    /// issuance and commit, touching a DECLARED read.
    #[test]
    fn commit_conflict_when_declared_read_changed_after_issuance() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let context = SealContext::new("cell-a", 1);
        let grant = initial_grant_with_prewarm(&broker, &context, vec![DocumentId::new("docs/1")]);

        // Interfering write after issuance (a foreign writer, via the
        // store seam the fence's admission window must catch).
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 8));

        let outcome = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            vec![DocumentId::new("docs/1")],
            vec![put("docs/2", 1)],
        )
        .expect("commit answers, structured");
        assert_eq!(
            outcome,
            WireCommitOutcome::Conflict {
                key: DocumentId::new("docs/1")
            }
        );
    }

    fn test_broker(module_source: Arc<dyn ModuleBundleSource + Send + Sync>) -> ProcessBroker {
        let (broker, _store, _fence) = test_broker_parts(module_source);
        broker
    }

    /// Like `test_broker`, but hands back the concrete store + fence so
    /// commit tests can seed interfering writes and move the lease.
    /// The fence rides the SAME `MvccStore` the broker reads from —
    /// mirroring run_broker's memory arm — and its lease is seeded to
    /// the broker's authority epoch (1), like the env stand-in.
    fn test_broker_parts(
        module_source: Arc<dyn ModuleBundleSource + Send + Sync>,
    ) -> (ProcessBroker, Arc<MvccStore>, Arc<MemoryFence>) {
        let store = Arc::new(MvccStore::new());
        let fence = Arc::new(MemoryFence::new(store.clone()));
        fence.seed_lease("tenant-test", "dep-test", 1);
        let broker = ProcessBroker {
            store: store.clone(),
            module_source,
            seal_key: CapsuleSealKey::derive_for_tests(b"test-seed"),
            tenant: TenantId::new("tenant-test"),
            deployment: DeploymentId::new("dep-test"),
            snapshot_ts: 1,
            sessions: SessionTable::default(),
            fence: fence.clone(),
            authority_epoch: 1,
        };
        (broker, store, fence)
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

/// Postgres-gated commit e2e: the SAME `handle_request` fence admission
/// path the default suite exercises over `MemoryFence`, against the REAL
/// `WritePlane` — boot-acquired lease epoch, capsules sealed with it,
/// point/window conflicts judged by the Postgres fence, and session
/// end-of-life bounding capsule replay. The read side stays the in-memory
/// store (the Convex read adapter is not what is under test here); every
/// conflict is seeded through the write plane itself, so the admission
/// decision is 100% Postgres.
///
/// Run with:
///     ASTER_DB_URL=postgres://aster:aster@127.0.0.1:5433/aster \
///         cargo test -p aster-ipc --features postgres-it \
///         --bin aster_brokerd -- --test-threads=1
#[cfg(all(test, feature = "postgres-it"))]
mod pg_commit_e2e {
    use super::*;
    use aster_broker::CommitOutcome;
    use aster_capsule::doc_with_i64;
    use std::time::{SystemTime, UNIX_EPOCH};

    const TENANT: &str = "tenant-ipc-it";

    fn url() -> String {
        std::env::var("ASTER_DB_URL").expect("set ASTER_DB_URL to run postgres-it tests")
    }

    /// Unique deployment per test run: the `aster` schema is shared with
    /// the write_plane_it suite, so isolation comes from the log's
    /// (tenant, deployment) keying, never from schema drops.
    fn fresh_deployment(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        format!("dep-ipc-{label}-{nanos}")
    }

    /// brokerd's postgres boot sequence in miniature (run_broker's
    /// Postgres arm): connect the write plane, ensure the schema, acquire
    /// the lease — the epoch it returns is the broker's authority_epoch
    /// AND the epoch stamped into every session. Tests drive
    /// `handle_request` directly because a binary's internals are not
    /// linkable from `tests/`.
    ///
    /// One blind write seeds `docs/a` on BOTH sides (write-plane log and
    /// the in-memory read store), so the broker's pinned snapshot (1) is
    /// a committed prefix of the real log (S-SNAPSHOT).
    fn pg_broker(deployment: &str) -> (ProcessBroker, Arc<WritePlane>, u64) {
        let plane = Arc::new(
            WritePlane::connect(WritePlaneConfig {
                url: url(),
                ..WritePlaneConfig::default()
            })
            .expect("connect write plane"),
        );
        plane.ensure_schema().expect("ensure schema");
        let epoch = WritePlane::acquire_lease(&plane, TENANT, deployment, "aster_brokerd-it")
            .expect("boot lease acquire");
        assert_eq!(epoch, 1, "fresh (tenant, deployment) must start at epoch 1");
        let seeded = WritePlane::commit(
            &plane,
            &FenceInput {
                tenant: TENANT,
                deployment,
                committer_epoch: epoch,
                context_epoch: epoch,
                snapshot: 0,
                read_points: &[],
                read_windows: &[],
                writes: &[(DocumentId::new("docs/a"), Some(doc_with_i64("value", 1)))],
            },
        )
        .expect("seed write");
        assert_eq!(seeded, CommitOutcome::Committed { ts: 1 });

        let store = Arc::new(MvccStore::new());
        store.seed(DocumentId::new("docs/a"), doc_with_i64("value", 1));
        let broker = ProcessBroker {
            store,
            module_source: Arc::new(NoModuleBundleSource {
                reason: "no module loading in the commit e2e",
            }),
            seal_key: CapsuleSealKey::derive_for_tests(b"pg-commit-e2e-seed"),
            tenant: TenantId::new(TENANT),
            deployment: DeploymentId::new(deployment.to_string()),
            snapshot_ts: 1,
            sessions: SessionTable::default(),
            fence: plane.clone(),
            authority_epoch: epoch,
        };
        (broker, plane, epoch)
    }

    fn grant(broker: &ProcessBroker, deployment: &str, epoch: u64) -> InitialCapsuleGrant {
        match handle_request(
            broker,
            IpcRequest::InitialCapsule {
                context: SealContext::new("cell-pg", epoch),
                tenant: TenantId::new(TENANT),
                deployment: DeploymentId::new(deployment.to_string()),
                snapshot_ts: 1,
                prewarm: vec![DocumentId::new("docs/a")],
            },
        )
        .0
        {
            IpcResponse::InitialCapsule(Ok(grant)) => grant,
            other => panic!("initial capsule should succeed, got {other:?}"),
        }
    }

    fn commit_request(
        broker: &ProcessBroker,
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        declared_reads: Vec<DocumentId>,
        writes: Vec<(DocumentId, Option<Document>)>,
    ) -> Result<WireCommitOutcome, WireBrokerError> {
        match handle_request(
            broker,
            IpcRequest::Commit {
                session,
                capsule,
                declared_reads,
                writes,
            },
        )
        .0
        {
            IpcResponse::Commit(result) => result,
            other => panic!("expected a Commit response, got {other:?}"),
        }
    }

    fn blind_write(plane: &WritePlane, deployment: &str, epoch: u64, snapshot: u64, key: &str) {
        let outcome = WritePlane::commit(
            plane,
            &FenceInput {
                tenant: TENANT,
                deployment,
                committer_epoch: epoch,
                context_epoch: epoch,
                snapshot,
                read_points: &[],
                read_windows: &[],
                writes: &[(DocumentId::new(key), Some(doc_with_i64("value", 9)))],
            },
        )
        .expect("interfering write");
        assert!(
            matches!(outcome, CommitOutcome::Committed { .. }),
            "interfering write must land: {outcome:?}"
        );
    }

    /// Happy path against the real fence, then replay-after-close.
    ///
    /// CE2.1 note: replay of the ISSUED capsule bytes is inevitable —
    /// capsules are stateless bearer evidence and the broker stores no
    /// "latest" digest. Session end-of-life is what bounds it: the commit
    /// closed the session, so the replayed bytes die at the session gate
    /// (`unknown_session`) before any fence work.
    #[test]
    fn pg_commit_through_boot_epoch_and_replay_after_close_rejected() {
        let deployment = fresh_deployment("happy");
        let (broker, plane, epoch) = pg_broker(&deployment);
        let g = grant(&broker, &deployment, epoch);

        let outcome = commit_request(
            &broker,
            Some(g.session),
            g.capsule.clone(),
            vec![DocumentId::new("docs/a")],
            vec![(DocumentId::new("docs/out"), Some(doc_with_i64("value", 2)))],
        )
        .expect("commit through the real fence");
        assert_eq!(outcome, WireCommitOutcome::Committed { ts: 2 });
        // The append is in the real log.
        let written = plane
            .read_point(TENANT, &deployment, &DocumentId::new("docs/out"), 2)
            .expect("read back");
        assert_eq!(written.version, Some(2));

        // Replay the identical request — same session id, same capsule.
        let error = commit_request(
            &broker,
            Some(g.session),
            g.capsule,
            vec![DocumentId::new("docs/a")],
            vec![(DocumentId::new("docs/out"), Some(doc_with_i64("value", 3)))],
        )
        .expect_err("replay after close must be rejected");
        assert_eq!(error.code, "unknown_session", "got {error:?}");
        // And nothing landed for it.
        let after = plane
            .snapshot_ts(TENANT, &deployment)
            .expect("tip after replay");
        assert_eq!(after, 2, "rejected replay must not append");
    }

    /// A write-plane commit between issuance and commit conflicts a
    /// declared point read.
    #[test]
    fn pg_point_conflict_between_issuance_and_commit() {
        let deployment = fresh_deployment("point");
        let (broker, plane, epoch) = pg_broker(&deployment);
        let g = grant(&broker, &deployment, epoch);

        blind_write(&plane, &deployment, epoch, 1, "docs/a");

        let outcome = commit_request(
            &broker,
            Some(g.session),
            g.capsule,
            vec![DocumentId::new("docs/a")],
            vec![(DocumentId::new("docs/out"), Some(doc_with_i64("value", 2)))],
        )
        .expect("fence answers, structured");
        assert_eq!(
            outcome,
            WireCommitOutcome::Conflict {
                key: DocumentId::new("docs/a")
            }
        );
    }

    /// Range windows ride every commit (conservative inclusion of ALL
    /// sealed certificates), so a phantom insert into a scanned prefix
    /// conflicts even with an EMPTY declared point set.
    #[test]
    fn pg_window_conflict_from_sealed_certificate() {
        let deployment = fresh_deployment("window");
        let (broker, plane, epoch) = pg_broker(&deployment);
        let g = grant(&broker, &deployment, epoch);

        // Hydrate a prefix scan into the capsule (read side, at s=1):
        // docs/a is the only live key, so the certificate is Exhausted
        // and its window covers the whole docs/ prefix.
        let context = SealContext::new("cell-pg", epoch);
        let sealed = match handle_request(
            &broker,
            IpcRequest::HydratePrefix {
                context,
                session: Some(g.session),
                capsule: g.capsule,
                prefix: "docs/".into(),
                limit: 10,
            },
        )
        .0
        {
            IpcResponse::HydratePrefix(Ok(sealed)) => sealed,
            other => panic!("prefix hydrate should succeed, got {other:?}"),
        };
        assert_eq!(sealed.capsule().ranges.len(), 1);

        // Phantom insert into the scanned prefix, through the write plane.
        blind_write(&plane, &deployment, epoch, 1, "docs/zz");

        let outcome = commit_request(
            &broker,
            Some(g.session),
            sealed,
            Vec::new(),
            vec![(DocumentId::new("docs/out"), Some(doc_with_i64("value", 2)))],
        )
        .expect("fence answers, structured");
        assert_eq!(
            outcome,
            WireCommitOutcome::Conflict {
                key: DocumentId::new("docs/zz")
            }
        );
    }

    /// Failover: a new holder acquires the lease AFTER this broker booted
    /// and minted a session. The session dies naturally at the fence's V2
    /// epoch check — a structured StaleEpoch, not a session-table error.
    #[test]
    fn pg_stale_epoch_after_failover_rejected_by_fence() {
        let deployment = fresh_deployment("failover");
        let (broker, plane, epoch) = pg_broker(&deployment);
        let g = grant(&broker, &deployment, epoch);

        let new_epoch = WritePlane::acquire_lease(&plane, TENANT, &deployment, "committer-b")
            .expect("failover acquire");
        assert_eq!(new_epoch, epoch + 1);

        let outcome = commit_request(
            &broker,
            Some(g.session),
            g.capsule,
            vec![DocumentId::new("docs/a")],
            vec![(DocumentId::new("docs/out"), Some(doc_with_i64("value", 2)))],
        )
        .expect("fence answers, structured");
        assert_eq!(
            outcome,
            WireCommitOutcome::StaleEpoch {
                lease_epoch: new_epoch
            }
        );
    }
}
