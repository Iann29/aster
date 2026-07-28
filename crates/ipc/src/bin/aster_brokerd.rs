use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aster_broker::{
    BrokerError, CapsuleBrokerClient, CapsuleStore, CommitFence, FenceInput, MemoryFence,
};
use aster_capsule::{
    CapsuleSealKey, DeploymentId, Document, DocumentId, MvccStore, ObservedWindow, SealContext,
    SealedCapsule, SessionBinding, TenantId, Timestamp, Value,
};
use aster_ipc::{
    launch::{LaunchAuthorizer, LaunchTokenKey},
    policy::DeploymentPolicy,
    read_frame, write_frame, InitialCapsuleGrant, IpcError, IpcRequest, IpcResponse, ModuleBundle,
    WireBrokerError, WireCommitOutcome,
};
use aster_store_postgres::{AuthoritativeCapsuleStore, WritePlane, WritePlaneConfig};
use base64::Engine as _;

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
    launch_key: Option<LaunchTokenKey>,
    store_kind: StoreKind,
    policy: DeploymentPolicy,
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
    /// Only this effective UID may speak the UDS protocol. Defaults to the
    /// broker's own euid, so an omitted setting stays fail-closed.
    allowed_peer_uid: u32,
}

struct BrokerAuthority {
    store: Arc<dyn CapsuleStore + Send + Sync>,
    module_source: Arc<dyn ModuleBundleSource + Send + Sync>,
    fence: Arc<dyn CommitFence>,
    epoch: u64,
}

impl BrokerConfig {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path = env_path("ASTER_BROKER_SOCK")?;
        let tenant = TenantId::new(env_string("ASTER_TENANT")?);
        let deployment = DeploymentId::new(env_string("ASTER_DEPLOYMENT")?);
        let snapshot_ts = env_optional_u64("ASTER_SNAPSHOT_TS")?.unwrap_or(0);
        let seeds = parse_seeds(&env_optional_string("ASTER_SEED_I64")?.unwrap_or_default())?;
        let store_kind =
            StoreKind::from_env_value(&env_optional_string("ASTER_STORE")?.unwrap_or_default())?;
        let seal_key = resolve_seal_key(store_kind)?;
        let launch_key = resolve_launch_key(store_kind)?;
        let policy = match env_optional_string("ASTER_POLICY_FILE")? {
            Some(path) => DeploymentPolicy::from_path(&path)?,
            None if store_kind == StoreKind::Memory => DeploymentPolicy::allow_all_for_tests(),
            None => {
                return Err(
                    "ASTER_STORE=postgres requires ASTER_POLICY_FILE with explicit authority"
                        .into(),
                )
            }
        };
        let db_url = match store_kind {
            StoreKind::Memory => None,
            StoreKind::Postgres => Some(resolve_db_url()?),
        };
        let db_schema =
            env_optional_string("ASTER_DB_SCHEMA")?.unwrap_or_else(|| "public".to_string());
        let modules_dir = env_optional_string("ASTER_MODULES_DIR")?.map(PathBuf::from);
        let lease_epoch = env_optional_u64("ASTER_LEASE_EPOCH")?;
        let allowed_peer_uid = resolve_allowed_peer_uid()?;
        Ok(Self {
            socket_path,
            tenant,
            deployment,
            snapshot_ts,
            seeds,
            seal_key,
            launch_key,
            store_kind,
            policy,
            db_url,
            db_schema,
            modules_dir,
            lease_epoch,
            allowed_peer_uid,
        })
    }
}

fn resolve_allowed_peer_uid() -> Result<u32, Box<dyn std::error::Error>> {
    if let Some(uid) = env_optional_u64("ASTER_ALLOWED_PEER_UID")? {
        return u32::try_from(uid)
            .map_err(|_| format!("ASTER_ALLOWED_PEER_UID={uid} exceeds u32").into());
    }

    // SAFETY: geteuid has no preconditions and cannot fail.
    Ok(unsafe { libc::geteuid() })
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
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("stat ASTER_DB_URL_FILE={path}: {error}"))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(
                format!("ASTER_DB_URL_FILE={path} has mode {mode:04o}; use 0400 or 0600").into(),
            );
        }
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("read ASTER_DB_URL_FILE={path}: {err}"))?;
        return Ok(raw.trim().to_string());
    }
    if let Some(url) = env_optional_string("ASTER_DB_URL")? {
        return Ok(url);
    }
    Err("ASTER_STORE=postgres requires ASTER_DB_URL_FILE or ASTER_DB_URL".into())
}

fn resolve_seal_key(store_kind: StoreKind) -> Result<CapsuleSealKey, Box<dyn std::error::Error>> {
    if let Some(key) = read_secret_key("ASTER_SEAL_KEY_FILE")? {
        return Ok(CapsuleSealKey::from_bytes(key));
    }
    match store_kind {
        StoreKind::Memory => Ok(CapsuleSealKey::derive_for_tests(
            env_string("ASTER_SEAL_SEED")?.as_bytes(),
        )),
        StoreKind::Postgres => Err(
            "ASTER_STORE=postgres requires ASTER_SEAL_KEY_FILE; deterministic \
             ASTER_SEAL_SEED is restricted to memory-mode tests"
                .into(),
        ),
    }
}

fn resolve_launch_key(
    store_kind: StoreKind,
) -> Result<Option<LaunchTokenKey>, Box<dyn std::error::Error>> {
    if let Some(key) = read_secret_key("ASTER_LAUNCH_KEY_FILE")? {
        return Ok(Some(LaunchTokenKey::from_bytes(key)));
    }
    match store_kind {
        StoreKind::Memory => Ok(None),
        StoreKind::Postgres => Err("ASTER_STORE=postgres requires ASTER_LAUNCH_KEY_FILE".into()),
    }
}

fn read_secret_key(name: &str) -> Result<Option<[u8; 32]>, Box<dyn std::error::Error>> {
    let Some(path) = env_optional_string(name)? else {
        return Ok(None);
    };
    let metadata = fs::metadata(&path).map_err(|error| format!("stat {name}={path}: {error}"))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{name}={path} has mode {mode:04o}; group/other permissions must be zero \
             (use 0400 or 0600)"
        )
        .into());
    }
    let raw = fs::read(&path).map_err(|error| format!("read {name}={path}: {error}"))?;
    decode_secret_key(&raw, name).map(Some).map_err(Into::into)
}

fn decode_secret_key(raw: &[u8], name: &str) -> Result<[u8; 32], String> {
    if raw.len() == 32 {
        return raw
            .try_into()
            .map_err(|_| format!("{name} must contain exactly 32 bytes"));
    }

    let encoded = std::str::from_utf8(raw)
        .map(str::trim)
        .map_err(|error| format!("{name} is neither 32 raw bytes nor UTF-8 text: {error}"))?;
    if encoded.len() == 64 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut key = [0_u8; 32];
        for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
            let pair = std::str::from_utf8(pair).expect("hex pair is ASCII");
            key[index] = u8::from_str_radix(pair, 16)
                .map_err(|error| format!("invalid hex {name}: {error}"))?;
        }
        return Ok(key);
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|error| format!("{name} is not valid hex/base64: {error}"))?;
    decoded
        .as_slice()
        .try_into()
        .map_err(|_| format!("decoded {name} is {} bytes, expected 32", decoded.len()))
}

fn publish_authority_epoch(epoch: u64) -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = env_optional_string("ASTER_AUTHORITY_EPOCH_FILE")? else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    writeln!(file, "{epoch}")?;
    file.sync_all()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o444))?;
    fs::rename(&temporary, &path)?;
    Ok(())
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
    let BrokerAuthority {
        store,
        module_source,
        fence,
        epoch: authority_epoch,
    } = match config.store_kind {
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
            BrokerAuthority {
                store: mvcc,
                module_source: Arc::new(NoModuleBundleSource {
                    reason: "module loading requires ASTER_STORE=postgres",
                }),
                fence: Arc::new(fence),
                epoch,
            }
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
            // Module bundles remain deployment inputs sourced from Convex's
            // module tables and hash-verified local storage. Transaction
            // documents do not: reads and commits below share aster.log.
            let module_source = Arc::new(
                aster_store_postgres::PostgresCapsuleStore::connect(pg_cfg)
                    .map_err(|err| format!("module store connect: {err}"))?,
            );
            // One WritePlane instance owns the lease, snapshots, document
            // reads, retention, conflict validation, and append. This is the
            // shared-history premise required by T2; the v0.7 split between
            // Convex reads and aster.log writes no longer exists here.
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
            let store = Arc::new(AuthoritativeCapsuleStore::with_id_allocator(
                plane.clone(),
                config.tenant.clone(),
                config.deployment.clone(),
                module_source.clone(),
            ));
            BrokerAuthority {
                store: store as Arc<dyn CapsuleStore + Send + Sync>,
                module_source: module_source as Arc<dyn ModuleBundleSource + Send + Sync>,
                fence: plane,
                epoch,
            }
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
    let head = store
        .snapshot_ts()
        .map_err(|err| format!("snapshot_ts: {err}"))?;
    let fixed_snapshot_ts = (configured_ts != 0).then_some(configured_ts);
    if let Some(fixed) = fixed_snapshot_ts {
        if fixed > head {
            return Err(format!(
                "ASTER_SNAPSHOT_TS={fixed} is beyond the authoritative store head {head}"
            )
            .into());
        }
        eprintln!(
            "aster_brokerd: store={} snapshot_mode=fixed snapshot_ts={} head={}",
            match config.store_kind {
                StoreKind::Memory => "memory",
                StoreKind::Postgres => "postgres",
            },
            fixed,
            head
        );
    } else {
        eprintln!(
            "aster_brokerd: store={} snapshot_mode=latest head={}",
            match config.store_kind {
                StoreKind::Memory => "memory",
                StoreKind::Postgres => "postgres",
            },
            head
        );
    }
    let broker = ProcessBroker {
        store,
        module_source,
        seal_key: config.seal_key,
        launch_authorizer: config.launch_key.map(LaunchAuthorizer::new),
        policy: config.policy,
        allow_shutdown: config.store_kind == StoreKind::Memory,
        tenant: config.tenant,
        deployment: config.deployment,
        fixed_snapshot_ts,
        sessions: SessionTable::default(),
        fence,
        authority_epoch,
    };

    let max_connections = broker.policy.max_concurrent_sessions;
    let wake_socket = broker
        .allow_shutdown
        .then(|| Arc::new(config.socket_path.clone()));
    let broker = Arc::new(broker);
    let listener = UnixListener::bind(&config.socket_path)?;
    fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))?;
    publish_authority_epoch(authority_epoch)?;
    eprintln!(
        "aster_brokerd: ready socket={} snapshot_mode={} peer_uid={} max_connections={}",
        config.socket_path.display(),
        if fixed_snapshot_ts.is_some() {
            "fixed"
        } else {
            "latest"
        },
        config.allowed_peer_uid,
        max_connections
    );

    // One request per connection. Requests run concurrently up to the policy's
    // active-session bound; a silent peer can consume one slot until its I/O
    // deadline, but cannot head-of-line block every other cell.
    let live_connections = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    while !shutdown.load(Ordering::Acquire) {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let deadline = Some(Duration::from_secs(10));
        let _ = stream.set_read_timeout(deadline);
        let _ = stream.set_write_timeout(deadline);

        match peer_uid(&stream) {
            Ok(uid) if uid == config.allowed_peer_uid => {}
            Ok(uid) => {
                send_response(
                    &mut stream,
                    &IpcResponse::Error(WireBrokerError::new(
                        "peer_uid_denied",
                        format!("peer uid {uid} is not authorized"),
                    )),
                );
                continue;
            }
            Err(error) => {
                send_response(
                    &mut stream,
                    &IpcResponse::Error(WireBrokerError::new(
                        "peer_credential_error",
                        error.to_string(),
                    )),
                );
                continue;
            }
        }

        if live_connections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < max_connections).then_some(current + 1)
            })
            .is_err()
        {
            send_response(
                &mut stream,
                &IpcResponse::Error(WireBrokerError::new(
                    "broker_busy",
                    format!("active connection limit {max_connections} reached"),
                )),
            );
            continue;
        }

        let worker_broker = Arc::clone(&broker);
        let worker_live = Arc::clone(&live_connections);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_wake = wake_socket.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("aster-broker-connection".into())
            .spawn(move || {
                let _active = ActiveConnection(worker_live);
                if serve_connection(&worker_broker, &mut stream) {
                    worker_shutdown.store(true, Ordering::Release);
                    if let Some(socket) = worker_wake {
                        let _ = UnixStream::connect(socket.as_ref());
                    }
                }
            })
        {
            live_connections.fetch_sub(1, Ordering::AcqRel);
            eprintln!("aster_brokerd: connection worker spawn failed: {error}");
        }
    }

    while live_connections.load(Ordering::Acquire) != 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!("aster_brokerd: shutdown requested");
    let _ = fs::remove_file(&config.socket_path);
    Ok(())
}

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_connection(broker: &ProcessBroker, stream: &mut UnixStream) -> bool {
    match read_frame::<IpcRequest>(stream) {
        Ok(request) => {
            let (response, should_shutdown) = handle_request(broker, request);
            send_response(stream, &response);
            should_shutdown
        }
        Err(error) => {
            let response =
                IpcResponse::Error(WireBrokerError::new("bad_request", error.to_string()));
            send_response(stream, &response);
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` points to writable storage of `length` bytes,
    // `length` itself is valid, and `stream` owns a live socket descriptor.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    // SAFETY: getsockopt succeeded and initialized the complete ucred value.
    Ok(unsafe { credentials.assume_init() }.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credential authentication requires Linux SO_PEERCRED",
    ))
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

fn unix_time_s() -> Result<u64, WireBrokerError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| WireBrokerError::new("clock_error", error.to_string()))
}

fn handle_request(broker: &ProcessBroker, request: IpcRequest) -> (IpcResponse, bool) {
    match request {
        IpcRequest::InitialCapsule {
            launch_token,
            context,
            tenant,
            deployment,
            snapshot_ts,
            prewarm,
        } => {
            // Only brokerd mints session bindings. In production, the
            // one-time launch token below authenticates the cell id and
            // binds it to this tenant, deployment, and lease epoch.
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
            if tenant != broker.tenant {
                return (
                    IpcResponse::InitialCapsule(Err(WireBrokerError::from(
                        BrokerError::TenantMismatch,
                    ))),
                    false,
                );
            }
            if deployment != broker.deployment {
                return (
                    IpcResponse::InitialCapsule(Err(WireBrokerError::from(
                        BrokerError::DeploymentMismatch,
                    ))),
                    false,
                );
            }
            for key in &prewarm {
                if let Err(error) = reject_noncanonical_point_id(key) {
                    return (IpcResponse::InitialCapsule(Err(error)), false);
                }
            }
            if let Err(error) = broker.authorize_initial(&prewarm) {
                return (IpcResponse::InitialCapsule(Err(error)), false);
            }
            if let Some(authorizer) = &broker.launch_authorizer {
                let Some(token) = launch_token.as_deref() else {
                    return (
                        IpcResponse::InitialCapsule(Err(WireBrokerError::new(
                            "launch_token_required",
                            "postgres broker requires a one-time launch token",
                        ))),
                        false,
                    );
                };
                let now = match unix_time_s() {
                    Ok(now) => now,
                    Err(error) => return (IpcResponse::InitialCapsule(Err(error)), false),
                };
                if let Err(error) = authorizer.verify(
                    token,
                    &context.cell_id,
                    &broker.tenant.0,
                    &broker.deployment.0,
                    broker.authority_epoch,
                    now,
                ) {
                    return (
                        IpcResponse::InitialCapsule(Err(WireBrokerError::new(
                            "launch_token_rejected",
                            error.to_string(),
                        ))),
                        false,
                    );
                }
            }
            let session = match broker.sessions.mint(
                &context.cell_id,
                broker.authority_epoch,
                broker.policy.max_concurrent_sessions,
                broker.policy.session_ttl_seconds,
            ) {
                Ok(session) => session,
                Err(error) => return (IpcResponse::InitialCapsule(Err(error)), false),
            };
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
                    reject_noncanonical_point_id(&key)?;
                    broker.authorize_point_hydrate(&capsule, &key)?;
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
                    broker.authorize_scan(&capsule, &prefix, limit)?;
                    broker
                        .hydrate_prefix(&bound, capsule, prefix, limit)
                        .map_err(WireBrokerError::from)
                },
            )),
            false,
        ),
        IpcRequest::MintDocumentId {
            context,
            session,
            table,
        } => (
            IpcResponse::MintDocumentId(broker.resolve_bound_context(session, &context).and_then(
                |bound| {
                    broker.authorize_insert(&table)?;
                    broker
                        .mint_document_id(&bound, &table)
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
                        broker.authorize_module(&path)?;
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
            let resolved = broker.consume_bound_context(session, &claimed);
            // One session = one commit ATTEMPT, and the attempt SPENDS the
            // session BEFORE the fence runs (review C6 + re-referee F4):
            // consumption is keyed to presenting a registered id, not to
            // the outcome — gate rejections close it too, and a parallel
            // broker could never double-spend one session into two fence
            // executions. The presenter holds the bearer id (they could
            // Abort it anyway), and the UDS client drops its held id on
            // every structured Commit answer. Replay of the ISSUED capsule
            // bytes is inevitable under bearer semantics (theorem CE2.1);
            // spending the session here is what bounds it.
            let result = resolved.and_then(|bound| {
                let outcome = broker.commit(&bound, capsule, &declared_reads, &writes);
                // Fail-closed but never silent (review B1): StaleEpoch
                // means the lease moved and every commit this broker
                // relays is dead — say so on stderr so the operator
                // learns before the cells do.
                if let Ok(WireCommitOutcome::StaleEpoch { lease_epoch }) = &outcome {
                    eprintln!(
                        "aster-brokerd: fence refused commit — stale lease epoch \
                             (authority is at epoch {lease_epoch}, this broker holds {}); \
                             the lease moved, relaunch brokerd to re-acquire",
                        broker.authority_epoch
                    );
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
        // Production lifecycle is controlled by the container supervisor.
        // Keeping an unauthenticated shutdown verb on the cell socket would
        // turn every tenant into a deployment-wide DoS principal.
        IpcRequest::Shutdown if broker.allow_shutdown => (IpcResponse::ShutdownAck, true),
        IpcRequest::Shutdown => (
            IpcResponse::Error(WireBrokerError::new(
                "shutdown_disabled",
                "shutdown over the cell socket is disabled for this broker",
            )),
            false,
        ),
    }
}

/// Broker-side registry of live sessions: session id → immutable context.
/// Capsule verbs present a session id; the broker rebuilds the bound
/// `SealContext` from this trusted table and treats serialized context only
/// as a consistency check. Commit/Abort consumes one attempt, while a policy
/// cap plus monotonic TTL bounds abandoned grants and disconnected clients.
#[derive(Default)]
struct SessionTable {
    sessions: Mutex<HashMap<SessionBinding, SessionEntry>>,
}

#[derive(Clone)]
struct SessionEntry {
    cell_id: String,
    lease_epoch: u64,
    expires_at: Instant,
}

impl SessionTable {
    /// Mint a fresh unguessable session id and register it. OS entropy via
    /// `getrandom` — session ids gate whose seals verify on this channel,
    /// so anything predictable (time, counters, constant seeds) would let
    /// one cell impersonate another's channel. If the OS RNG fails the
    /// broker cannot operate securely; dying is the only safe behavior.
    fn mint(
        &self,
        cell_id: &str,
        lease_epoch: u64,
        max_sessions: usize,
        ttl_seconds: u64,
    ) -> Result<SessionBinding, WireBrokerError> {
        let now = Instant::now();
        let expires_at = now
            .checked_add(Duration::from_secs(ttl_seconds))
            .ok_or_else(|| WireBrokerError::new("session_ttl_invalid", "session TTL overflow"))?;
        let mut sessions = self.sessions.lock().expect("session table lock");
        sessions.retain(|_, entry| entry.expires_at > now);
        if sessions.len() >= max_sessions {
            return Err(WireBrokerError::new(
                "session_capacity_exceeded",
                format!("deployment already has {max_sessions} live sessions"),
            ));
        }
        loop {
            let mut id = [0_u8; 32];
            getrandom::fill(&mut id)
                .map_err(|error| WireBrokerError::new("entropy_unavailable", error.to_string()))?;
            if let Entry::Vacant(vacant) = sessions.entry(SessionBinding::from_bytes(id)) {
                let session = *vacant.key();
                vacant.insert(SessionEntry {
                    cell_id: cell_id.to_string(),
                    lease_epoch,
                    expires_at,
                });
                return Ok(session);
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
            .filter(|entry| entry.expires_at > Instant::now())
    }

    fn lookup(&self, session: &SessionBinding) -> Option<SessionEntry> {
        let now = Instant::now();
        let mut sessions = self.sessions.lock().expect("session table lock");
        match sessions.get(session) {
            Some(entry) if entry.expires_at > now => Some(entry.clone()),
            Some(_) => {
                sessions.remove(session);
                None
            }
            None => None,
        }
    }
}

struct ProcessBroker {
    store: Arc<dyn CapsuleStore + Send + Sync>,
    module_source: Arc<dyn ModuleBundleSource + Send + Sync>,
    seal_key: CapsuleSealKey,
    launch_authorizer: Option<LaunchAuthorizer>,
    policy: DeploymentPolicy,
    allow_shutdown: bool,
    tenant: TenantId,
    deployment: DeploymentId,
    fixed_snapshot_ts: Option<u64>,
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
        let session = session.ok_or_else(|| {
            WireBrokerError::new(
                "session_required",
                "capsule verbs must present the session id minted at InitialCapsule",
            )
        })?;
        let entry = self.sessions.lookup(&session).ok_or_else(|| {
            WireBrokerError::new(
                "unknown_session",
                "session id is not registered with this broker",
            )
        })?;
        Self::validate_session_context(session, claimed, entry)
    }

    /// Atomically spend a session before a commit reaches the fence.
    ///
    /// Lookup-then-remove is insufficient once the UDS accept loop is
    /// concurrent: two requests can both resolve the same bearer session.
    /// Taking the entry in one locked map operation gives exactly one request
    /// the authority to attempt a commit; every racer gets unknown_session.
    fn consume_bound_context(
        &self,
        session: Option<SessionBinding>,
        claimed: &SealContext,
    ) -> Result<SealContext, WireBrokerError> {
        let session = session.ok_or_else(|| {
            WireBrokerError::new(
                "session_required",
                "commit must present the session id minted at InitialCapsule",
            )
        })?;
        let entry = self.sessions.remove(&session).ok_or_else(|| {
            WireBrokerError::new(
                "unknown_session",
                "session id is not registered with this broker",
            )
        })?;
        Self::validate_session_context(session, claimed, entry)
    }

    fn validate_session_context(
        session: SessionBinding,
        claimed: &SealContext,
        entry: SessionEntry,
    ) -> Result<SealContext, WireBrokerError> {
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

    fn authorize_initial(&self, prewarm: &[DocumentId]) -> Result<(), WireBrokerError> {
        if prewarm.len() > self.policy.max_reads_per_transaction {
            return Err(self.policy_limit(
                "policy_read_limit",
                "prewarm reads",
                prewarm.len(),
                self.policy.max_reads_per_transaction,
            ));
        }
        for key in prewarm {
            self.authorize_read(key)?;
        }
        Ok(())
    }

    fn authorize_read(&self, key: &DocumentId) -> Result<(), WireBrokerError> {
        if self.policy.allows_read(&key.0) {
            Ok(())
        } else {
            Err(self.policy_denied("policy_read_denied", "read", &key.0))
        }
    }

    fn authorize_point_hydrate(
        &self,
        capsule: &SealedCapsule,
        key: &DocumentId,
    ) -> Result<(), WireBrokerError> {
        self.authorize_read(key)?;
        let current = capsule.capsule().docs.len();
        if !capsule.capsule().docs.contains_key(key)
            && current >= self.policy.max_reads_per_transaction
        {
            return Err(self.policy_limit(
                "policy_read_limit",
                "observed point reads",
                current.saturating_add(1),
                self.policy.max_reads_per_transaction,
            ));
        }
        Ok(())
    }

    fn authorize_scan(
        &self,
        capsule: &SealedCapsule,
        prefix: &str,
        limit: usize,
    ) -> Result<(), WireBrokerError> {
        if !self.policy.allows_scan(prefix) {
            return Err(self.policy_denied("policy_read_denied", "scan", prefix));
        }
        if limit > self.policy.max_scan_limit {
            return Err(self.policy_limit(
                "policy_scan_limit",
                "scan limit",
                limit,
                self.policy.max_scan_limit,
            ));
        }
        let requested_total = capsule.capsule().docs.len().saturating_add(limit);
        if requested_total > self.policy.max_reads_per_transaction {
            return Err(self.policy_limit(
                "policy_read_limit",
                "maximum observed reads after scan",
                requested_total,
                self.policy.max_reads_per_transaction,
            ));
        }
        Ok(())
    }

    fn authorize_module(&self, path: &str) -> Result<(), WireBrokerError> {
        if self.policy.allows_module(path) {
            Ok(())
        } else {
            Err(self.policy_denied("policy_module_denied", "module load", path))
        }
    }

    fn authorize_insert(&self, table: &str) -> Result<(), WireBrokerError> {
        if self.policy.allows_insert(table) {
            Ok(())
        } else {
            Err(self.policy_denied("policy_insert_denied", "insert", table))
        }
    }

    fn policy_denied(&self, code: &str, operation: &str, target: &str) -> WireBrokerError {
        WireBrokerError::new(
            code,
            format!(
                "{operation} of {target:?} is outside deployment policy version {}",
                self.policy.version
            ),
        )
    }

    fn policy_limit(
        &self,
        code: &str,
        subject: &str,
        actual: usize,
        maximum: usize,
    ) -> WireBrokerError {
        WireBrokerError::new(
            code,
            format!(
                "{subject}={actual} exceeds policy maximum {maximum} (version {})",
                self.policy.version
            ),
        )
    }
}

impl ProcessBroker {
    /// Resolve the snapshot for a new session. Production clients request
    /// `0`, meaning the current authoritative head. An explicit broker pin
    /// remains available for deterministic replay and benchmark harnesses.
    fn issue_snapshot(&self, requested: Timestamp) -> Result<Timestamp, BrokerError> {
        match self.fixed_snapshot_ts {
            Some(fixed) if requested == 0 || requested == fixed => Ok(fixed),
            Some(fixed) => Err(BrokerError::Remote(format!(
                "snapshot_ts {requested} is not broker fixed snapshot {fixed}"
            ))),
            None => {
                let head = self.store.snapshot_ts().map_err(BrokerError::from)?;
                if requested == 0 {
                    return Ok(head);
                }
                if requested > head {
                    return Err(BrokerError::Remote(format!(
                        "snapshot_ts {requested} is beyond authoritative head {head}"
                    )));
                }
                Ok(requested)
            }
        }
    }

    fn enforce_snapshot_mode(&self, snapshot: Timestamp) -> Result<(), BrokerError> {
        if let Some(fixed) = self.fixed_snapshot_ts {
            if snapshot != fixed {
                return Err(BrokerError::Remote(format!(
                    "capsule snapshot_ts {snapshot} is not broker fixed snapshot {fixed}"
                )));
            }
        }
        Ok(())
    }

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
        self.enforce_snapshot_mode(capsule.ts)?;
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
        self.enforce_snapshot_mode(capsule.ts)
            .map_err(WireBrokerError::from)?;
        // (b) Variante B declaration check (Repair B-SUBSET): every
        // declared point must reference an observation atom the sealed
        // capsule actually carries, without duplicates. The INVERSE is
        // legal by design: a capsule key left undeclared demotes that
        // dependency to an authorized blind write — T2 still orders it;
        // omission costs the cell its own conflict protection, never
        // anyone else's safety.
        if capsule.docs.len() > self.policy.max_reads_per_transaction
            || declared_reads.len() > self.policy.max_reads_per_transaction
        {
            return Err(self.policy_limit(
                "policy_read_limit",
                "authenticated or declared reads",
                capsule.docs.len().max(declared_reads.len()),
                self.policy.max_reads_per_transaction,
            ));
        }
        let mut declared = BTreeSet::new();
        for key in declared_reads {
            reject_noncanonical_point_id(key)?;
            self.authorize_read(key)?;
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
        // Writes get the same shape discipline: a duplicate write key would
        // surface as an aster.log primary-key violation deep inside the
        // fence transaction (an opaque backend error) instead of a
        // structured rejection here. The cell runtime's write set is a
        // BTreeMap so it can't produce one — this guards hand-rolled
        // clients.
        if writes.len() > self.policy.max_writes_per_transaction {
            return Err(self.policy_limit(
                "policy_write_limit",
                "writes",
                writes.len(),
                self.policy.max_writes_per_transaction,
            ));
        }
        let mut write_keys = BTreeSet::new();
        for (key, _) in writes {
            reject_noncanonical_point_id(key)?;
            if !self.policy.allows_write(&key.0) {
                return Err(self.policy_denied("policy_write_denied", "write", &key.0));
            }
            if !write_keys.insert(key) {
                return Err(WireBrokerError::new(
                    "duplicate_write_key",
                    format!("write key {:?} appears more than once", key.0),
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

/// Re-referee F7: the postgres store resolves BOTH the Convex IDv6
/// spelling and the raw wire form `<table_hex>/<id_hex>` to the same row,
/// while every layer above — capsule keys, consumption ledger, write set,
/// conflict scan — keys by the raw string. Two spellings of one document
/// would evade read-your-own-writes and pairwise conflict detection, and
/// the threat model's executor speaks this wire protocol directly (a
/// Byzantine cell IS a hand-rolled native caller), so the broker seam
/// rejects the alias outright: IDv6 is the only point-document spelling a
/// cell may use. Table prefixes (`<table_hex>/`) are unaffected — they
/// name tables, not documents, and have no alias.
fn reject_noncanonical_point_id(key: &DocumentId) -> Result<(), WireBrokerError> {
    let raw = key.0.as_str();
    let is_raw_wire_form = raw.len() == 65
        && raw.as_bytes()[32] == b'/'
        && raw
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 32 || b.is_ascii_hexdigit());
    if is_raw_wire_form {
        return Err(WireBrokerError::new(
            "noncanonical_document_id",
            format!(
                "point id {raw:?} uses the raw wire spelling; document ids must be \
                 the canonical Convex IDv6 form"
            ),
        ));
    }
    Ok(())
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
        let snapshot_ts = self.issue_snapshot(snapshot_ts)?;
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
        self.enforce_snapshot_mode(capsule.ts)?;
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
        self.enforce_snapshot_mode(capsule.ts)?;
        // Certificates are evidence about the capsule snapshot: always scan
        // at capsule.ts, never at the store head that may advance later.
        let (certificate, entries) = self.store.scan_prefix(&prefix, limit, capsule.ts)?;
        capsule.hydrate_range(certificate, entries);
        Ok(SealedCapsule::new(capsule, &self.seal_key, context))
    }
    fn mint_document_id(
        &self,
        _context: &SealContext,
        table: &str,
    ) -> Result<DocumentId, BrokerError> {
        self.store
            .mint_document_id(table)
            .map_err(BrokerError::from)
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

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_uid_reads_kernel_credentials() {
        let (stream, _peer) = UnixStream::pair().expect("socket pair");
        // SAFETY: geteuid has no preconditions and cannot fail.
        let expected = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&stream).expect("SO_PEERCRED"), expected);
    }

    #[test]
    fn parses_seed_documents() {
        let seeds = parse_seeds("items/a:value:20,items/b:value:22").expect("parse");
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].0, DocumentId::new("items/a"));
        assert_eq!(seeds[1].1.get("value"), Some(&Value::Int(22)));
    }

    #[test]
    fn session_table_enforces_capacity_and_reclaims_expired_entries() {
        let table = SessionTable::default();
        let first = table
            .mint("cell-a", 1, 1, 60)
            .expect("first session within capacity");
        let error = table
            .mint("cell-b", 1, 1, 60)
            .expect_err("second live session must exceed capacity");
        assert_eq!(error.code, "session_capacity_exceeded");

        table
            .sessions
            .lock()
            .expect("session table lock")
            .get_mut(&first)
            .expect("first entry")
            .expires_at = Instant::now();
        assert!(
            table.lookup(&first).is_none(),
            "expired session is rejected"
        );
        table
            .mint("cell-b", 1, 1, 60)
            .expect("expired slot is reclaimed");
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
                launch_token: None,
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
                launch_token: None,
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
                launch_token: None,
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
            launch_authorizer: None,
            policy: DeploymentPolicy::allow_all_for_tests(),
            allow_shutdown: true,
            tenant: TenantId::new("tenant-test"),
            deployment: DeploymentId::new("dep-test"),
            fixed_snapshot_ts: Some(snapshot_ts),
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

    /// Adversarial-review C6: a Commit whose claimed context fails the
    /// session gate still CLOSES the presented (table-registered) session —
    /// the UDS client drops its held id on every structured answer, so an
    /// entry left alive would be orphaned in the table forever.
    #[test]
    fn commit_context_mismatch_closes_the_presented_session() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let grant_a = initial_grant(&broker, &SealContext::new("cell-a", 1));
        let grant_b = initial_grant(&broker, &SealContext::new("cell-a", 1));

        // Capsule B's seal names session B — presented on session A's id,
        // the gate must refuse (claimed binding != presented id).
        let response = handle_request(
            &broker,
            IpcRequest::Commit {
                session: Some(grant_a.session),
                capsule: grant_b.capsule,
                declared_reads: vec![],
                writes: vec![],
            },
        )
        .0;
        match response {
            IpcResponse::Commit(Err(error)) => {
                assert_eq!(error.code, "session_context_mismatch", "got {error:?}");
            }
            other => panic!("expected gate rejection, got {other:?}"),
        }

        // The presented session died with the attempt...
        let closed = handle_request(
            &broker,
            IpcRequest::Abort {
                session: grant_a.session,
            },
        )
        .0;
        match closed {
            IpcResponse::Abort(Err(error)) => {
                assert_eq!(error.code, "unknown_session", "got {error:?}");
            }
            other => panic!("session A should be closed, got {other:?}"),
        }
        // ...and only that one: the session the capsule named is untouched.
        let alive = handle_request(
            &broker,
            IpcRequest::Abort {
                session: grant_b.session,
            },
        )
        .0;
        assert!(
            matches!(alive, IpcResponse::Abort(Ok(()))),
            "session B must survive"
        );
    }

    /// Adversarial-review C4 sibling: duplicate write keys are refused as a
    /// structured error at the broker — not an opaque aster.log primary-key
    /// violation (Postgres fence) or a silent last-wins dedup (memory fence).
    #[test]
    fn commit_rejects_duplicate_write_keys() {
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));
        let grant = initial_grant(&broker, &SealContext::new("cell-a", 1));
        let key = DocumentId::new("docs/dup");
        let response = handle_request(
            &broker,
            IpcRequest::Commit {
                session: Some(grant.session),
                capsule: grant.capsule,
                declared_reads: vec![],
                writes: vec![(key.clone(), None), (key, None)],
            },
        )
        .0;
        match response {
            IpcResponse::Commit(Err(error)) => {
                assert_eq!(error.code, "duplicate_write_key", "got {error:?}");
            }
            other => panic!("expected duplicate_write_key, got {other:?}"),
        }
    }

    /// Re-referee F7: the raw wire spelling `<table_hex>/<id_hex>` aliases
    /// the IDv6 form at the store — the broker seam must refuse it for
    /// point documents so one logical document has exactly one protocol
    /// spelling.
    #[test]
    fn raw_wire_form_point_ids_are_rejected_at_the_broker_seam() {
        let raw = format!("{}/{}", "a".repeat(32), "b".repeat(32));
        let broker = test_broker(Arc::new(FakeModuleSource::new(None)));

        // Hydrate refuses it...
        let grant_h = initial_grant(&broker, &SealContext::new("cell-a", 1));
        let response = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context: SealContext::new("cell-a", 1),
                session: Some(grant_h.session),
                capsule: grant_h.capsule,
                key: DocumentId::new(raw.clone()),
            },
        )
        .0;
        match response {
            IpcResponse::HydratePoint(Err(error)) => {
                assert_eq!(error.code, "noncanonical_document_id", "got {error:?}");
            }
            other => panic!("expected noncanonical rejection, got {other:?}"),
        }

        // ...and so does a Commit write key.
        let grant_c = initial_grant(&broker, &SealContext::new("cell-a", 1));
        let response = handle_request(
            &broker,
            IpcRequest::Commit {
                session: Some(grant_c.session),
                capsule: grant_c.capsule,
                declared_reads: vec![],
                writes: vec![(DocumentId::new(raw), None)],
            },
        )
        .0;
        match response {
            IpcResponse::Commit(Err(error)) => {
                assert_eq!(error.code, "noncanonical_document_id", "got {error:?}");
            }
            other => panic!("expected noncanonical rejection, got {other:?}"),
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
                launch_token: None,
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

    #[test]
    fn concurrent_commits_cannot_double_spend_one_session() {
        let (broker, store, _fence) = test_broker_parts(Arc::new(FakeModuleSource::new(None)));
        store.seed(DocumentId::new("docs/1"), doc_with_i64("value", 7));
        let broker = Arc::new(broker);
        let grant = initial_grant(&broker, &SealContext::new("cell-a", 1));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let broker = Arc::clone(&broker);
            let barrier = Arc::clone(&barrier);
            let capsule = grant.capsule.clone();
            let session = grant.session;
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                commit_request(
                    &broker,
                    Some(session),
                    capsule,
                    Vec::new(),
                    vec![put("docs/2", 1)],
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("commit worker"))
            .collect();

        let committed = results
            .iter()
            .filter(|result| matches!(result, Ok(WireCommitOutcome::Committed { .. })))
            .count();
        let rejected = results
            .iter()
            .filter(|result| matches!(result, Err(error) if error.code == "unknown_session"))
            .count();
        assert_eq!((committed, rejected), (1, 1), "results: {results:?}");
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

    #[test]
    fn deployment_policy_denies_every_unauthorized_authority_surface() {
        let mut broker = test_broker(Arc::new(FakeModuleSource::new(Some(b"zip".to_vec()))));
        broker.policy = DeploymentPolicy {
            version: 9,
            read_prefixes: vec!["docs/public/".into()],
            write_prefixes: vec!["docs/public/".into()],
            module_prefixes: vec!["functions/".into()],
            insert_tables: vec!["public_docs".into()],
            max_reads_per_transaction: 8,
            max_writes_per_transaction: 4,
            max_scan_limit: 4,
            max_concurrent_sessions: 8,
            session_ttl_seconds: 60,
        };
        let context = SealContext::new("cell-policy", 1);
        let grant = initial_grant(&broker, &context);

        let point = handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context: context.clone(),
                session: Some(grant.session),
                capsule: grant.capsule.clone(),
                key: DocumentId::new("docs/private/x"),
            },
        )
        .0;
        assert!(matches!(
            &point,
            IpcResponse::HydratePoint(Err(WireBrokerError { code, .. }))
                if code == "policy_read_denied"
        ));

        let scan = handle_request(
            &broker,
            IpcRequest::HydratePrefix {
                context: context.clone(),
                session: Some(grant.session),
                capsule: grant.capsule.clone(),
                prefix: "docs/".into(),
                limit: 4,
            },
        )
        .0;
        assert!(matches!(
            &scan,
            IpcResponse::HydratePrefix(Err(WireBrokerError { code, .. }))
                if code == "policy_read_denied"
        ));

        let denied_insert = handle_request(
            &broker,
            IpcRequest::MintDocumentId {
                context: context.clone(),
                session: Some(grant.session),
                table: "private_docs".into(),
            },
        )
        .0;
        assert!(matches!(
            &denied_insert,
            IpcResponse::MintDocumentId(Err(WireBrokerError { code, .. }))
                if code == "policy_insert_denied"
        ));
        let allowed_insert = handle_request(
            &broker,
            IpcRequest::MintDocumentId {
                context: context.clone(),
                session: Some(grant.session),
                table: "public_docs".into(),
            },
        )
        .0;
        let IpcResponse::MintDocumentId(Ok(minted)) = allowed_insert else {
            panic!("authorized table should mint an id");
        };
        assert!(minted.0.starts_with("public_docs/"));

        let module = handle_request(
            &broker,
            IpcRequest::LoadModuleBundle {
                context,
                session: Some(grant.session),
                capsule: grant.capsule.clone(),
                path: "admin/root.js".into(),
            },
        )
        .0;
        assert!(matches!(
            &module,
            IpcResponse::LoadModuleBundle(Err(WireBrokerError { code, .. }))
                if code == "policy_module_denied"
        ));

        let write = commit_request(
            &broker,
            Some(grant.session),
            grant.capsule,
            Vec::new(),
            vec![put("docs/private/x", 1)],
        )
        .expect_err("private write must be rejected before the fence");
        assert_eq!(write.code, "policy_write_denied");
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
            launch_authorizer: None,
            policy: DeploymentPolicy::allow_all_for_tests(),
            allow_shutdown: true,
            tenant: TenantId::new("tenant-test"),
            deployment: DeploymentId::new("dep-test"),
            fixed_snapshot_ts: Some(1),
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

/// Postgres-gated transaction e2e over one authoritative history. The SAME
/// `WritePlane` owns capsule reads, retention, conflict validation, and append;
/// `handle_request` adds the session-bound seal and declaration gate. This is
/// the executable T2 composition proof: a committed write is visible to the
/// next freshly issued capsule, and an interposed write is visible to the
/// fence's `(s,h]` conflict scan.
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
    /// One blind write seeds `docs/a` in the sole authoritative history.
    /// The broker issues each unpinned session at the current WritePlane tip.
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

        let tenant = TenantId::new(TENANT);
        let deployment_id = DeploymentId::new(deployment.to_string());
        let store = Arc::new(AuthoritativeCapsuleStore::new(
            plane.clone(),
            tenant.clone(),
            deployment_id.clone(),
        ));
        let broker = ProcessBroker {
            store,
            module_source: Arc::new(NoModuleBundleSource {
                reason: "no module loading in the commit e2e",
            }),
            seal_key: CapsuleSealKey::derive_for_tests(b"pg-commit-e2e-seed"),
            policy: DeploymentPolicy::allow_all_for_tests(),
            launch_authorizer: None,
            allow_shutdown: false,
            tenant,
            deployment: deployment_id,
            fixed_snapshot_ts: None,
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
                launch_token: None,
                context: SealContext::new("cell-pg", epoch),
                tenant: TenantId::new(TENANT),
                deployment: DeploymentId::new(deployment.to_string()),
                snapshot_ts: 0,
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

        // A fresh session is issued at the new authoritative tip and reads
        // the committed value through the SAME store the fence appended to.
        let fresh = grant(&broker, &deployment, epoch);
        assert_eq!(fresh.capsule.capsule().ts, 2);
        let visible = match handle_request(
            &broker,
            IpcRequest::HydratePoint {
                context: SealContext::new("cell-pg", epoch),
                session: Some(fresh.session),
                capsule: fresh.capsule,
                key: DocumentId::new("docs/out"),
            },
        )
        .0
        {
            IpcResponse::HydratePoint(Ok(capsule)) => capsule,
            other => panic!("fresh authoritative read should succeed, got {other:?}"),
        };
        assert_eq!(
            visible
                .capsule()
                .docs
                .get(&DocumentId::new("docs/out"))
                .expect("committed key in fresh capsule")
                .version,
            Some(2)
        );
        assert!(matches!(
            handle_request(
                &broker,
                IpcRequest::Abort {
                    session: fresh.session,
                },
            )
            .0,
            IpcResponse::Abort(Ok(()))
        ));
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

    /// Session-aware in-process client — the UDS client's session
    /// discipline in miniature, over `handle_request`: InitialCapsule
    /// mints and holds the broker session, hydrates present it. Lets a V8
    /// cell run against the wire dispatch (so its capsules are sealed
    /// session-BOUND, committable) without a socket.
    struct SessionWireClient<'a> {
        broker: &'a ProcessBroker,
        session: Mutex<Option<SessionBinding>>,
    }

    impl<'a> SessionWireClient<'a> {
        fn new(broker: &'a ProcessBroker) -> Self {
            Self {
                broker,
                session: Mutex::new(None),
            }
        }

        fn session(&self) -> Option<SessionBinding> {
            *self.session.lock().expect("session slot")
        }
    }

    impl CapsuleBrokerClient for SessionWireClient<'_> {
        fn initial_capsule(
            &self,
            context: &SealContext,
            tenant: TenantId,
            deployment: DeploymentId,
            snapshot_ts: u64,
            prewarm: Vec<DocumentId>,
        ) -> Result<SealedCapsule, BrokerError> {
            match handle_request(
                self.broker,
                IpcRequest::InitialCapsule {
                    launch_token: None,
                    context: context.clone(),
                    tenant,
                    deployment,
                    snapshot_ts,
                    prewarm,
                },
            )
            .0
            {
                IpcResponse::InitialCapsule(Ok(grant)) => {
                    *self.session.lock().expect("session slot") = Some(grant.session);
                    Ok(grant.capsule)
                }
                IpcResponse::InitialCapsule(Err(error)) => Err(BrokerError::Remote(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                other => Err(BrokerError::Remote(format!(
                    "unexpected response {other:?}"
                ))),
            }
        }

        fn hydrate_point(
            &self,
            context: &SealContext,
            capsule: SealedCapsule,
            key: DocumentId,
        ) -> Result<SealedCapsule, BrokerError> {
            match handle_request(
                self.broker,
                IpcRequest::HydratePoint {
                    context: context.clone(),
                    session: self.session(),
                    capsule,
                    key,
                },
            )
            .0
            {
                IpcResponse::HydratePoint(Ok(sealed)) => Ok(sealed),
                IpcResponse::HydratePoint(Err(error)) => Err(BrokerError::Remote(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                other => Err(BrokerError::Remote(format!(
                    "unexpected response {other:?}"
                ))),
            }
        }

        fn mint_document_id(
            &self,
            context: &SealContext,
            table: &str,
        ) -> Result<DocumentId, BrokerError> {
            match handle_request(
                self.broker,
                IpcRequest::MintDocumentId {
                    context: context.clone(),
                    session: self.session(),
                    table: table.to_string(),
                },
            )
            .0
            {
                IpcResponse::MintDocumentId(Ok(id)) => Ok(id),
                IpcResponse::MintDocumentId(Err(error)) => Err(BrokerError::Remote(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                other => Err(BrokerError::Remote(format!(
                    "unexpected response {other:?}"
                ))),
            }
        }

        fn hydrate_prefix(
            &self,
            context: &SealContext,
            capsule: SealedCapsule,
            prefix: String,
            limit: usize,
        ) -> Result<SealedCapsule, BrokerError> {
            match handle_request(
                self.broker,
                IpcRequest::HydratePrefix {
                    context: context.clone(),
                    session: self.session(),
                    capsule,
                    prefix,
                    limit,
                },
            )
            .0
            {
                IpcResponse::HydratePrefix(Ok(sealed)) => Ok(sealed),
                IpcResponse::HydratePrefix(Err(error)) => Err(BrokerError::Remote(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                other => Err(BrokerError::Remote(format!(
                    "unexpected response {other:?}"
                ))),
            }
        }
    }

    /// S9b twin against the REAL fence: the write set born in a V8 cell —
    /// JS reads `docs/spec` (absent at the snapshot, consumed) and inserts
    /// through the syscall shim — commits through `handle_request` into
    /// Postgres via `WritePlane`; a second interleaved cell at the same
    /// snapshot, whose declared absence read was flipped by that commit,
    /// gets Conflict from the real fence. Both executions finish BEFORE
    /// either commit, on independent sessions.
    #[test]
    fn pg_v8_mutation_write_set_commits_and_interleaved_conflict_aborts() {
        use aster_v8cell::V8SandboxCell;

        let deployment = fresh_deployment("v8ws");
        let (broker, plane, epoch) = pg_broker(&deployment);
        let cell = V8SandboxCell::new(
            TenantId::new(TENANT),
            DeploymentId::new(deployment.clone()),
            8,
        );
        let source = |insert_id: &str, n: i64| {
            format!(
                r#"
                async function main() {{
                  const syscall = async (name, args) =>
                    JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
                  const holder = await syscall("1.0/get", {{ id: "docs/spec" }});
                  if (holder !== null) return "occupied";
                  await syscall("1.0/insert", {{
                    table: "docs",
                    value: {{ _id: "{insert_id}", n: {n} }},
                  }});
                  return "claimed {insert_id}";
                }}
            "#
            )
        };

        let client_a = SessionWireClient::new(&broker);
        let client_b = SessionWireClient::new(&broker);
        let run_a = cell
            .execute_async_main_with_broker(
                &client_a,
                "cell-pg-ws-a",
                epoch,
                TenantId::new(TENANT),
                DeploymentId::new(deployment.clone()),
                1,
                Vec::new(),
                &source("docs/spec", 1),
            )
            .expect("cell A runs");
        let run_b = cell
            .execute_async_main_with_broker(
                &client_b,
                "cell-pg-ws-b",
                epoch,
                TenantId::new(TENANT),
                DeploymentId::new(deployment.clone()),
                1,
                Vec::new(),
                &source("docs/other", 2),
            )
            .expect("cell B runs");

        let spec = DocumentId::new("docs/spec");
        assert_eq!(run_a.output, Value::Text("claimed docs/spec".into()));
        assert_eq!(run_b.output, Value::Text("claimed docs/other".into()));
        assert_eq!(run_a.consumed_reads, vec![spec.clone()]);
        assert_eq!(run_b.consumed_reads, vec![spec.clone()]);
        assert_eq!(run_a.write_set.len(), 1);
        assert_eq!(run_a.write_set[0].0, spec);
        assert_eq!(run_b.write_set[0].0, DocumentId::new("docs/other"));

        let outcome = commit_request(
            &broker,
            client_a.session(),
            run_a.sealed_capsule.expect("A carries the sealed capsule"),
            run_a.consumed_reads,
            run_a.write_set,
        )
        .expect("commit A through the real fence");
        assert_eq!(outcome, WireCommitOutcome::Committed { ts: 2 });
        // The JS-born write is durable in the Postgres log.
        let written = plane
            .read_point(TENANT, &deployment, &spec, 2)
            .expect("read back A's write");
        assert_eq!(written.version, Some(2));

        let outcome = commit_request(
            &broker,
            client_b.session(),
            run_b.sealed_capsule.expect("B carries the sealed capsule"),
            run_b.consumed_reads,
            run_b.write_set,
        )
        .expect("fence answers B, structured");
        assert_eq!(outcome, WireCommitOutcome::Conflict { key: spec });
    }
}
