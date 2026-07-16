//! Length-prefixed Unix-domain-socket IPC for Aster v0.3.
//!
//! v0.2 introduced `CapsuleBrokerClient` but kept the broker object in the
//! same process as the V8 cell. This crate turns that trait into a concrete
//! client transport: newline-free JSON frames prefixed by a big-endian u32
//! length over Unix-domain sockets.
//!
//! The library side is deliberately cell-safe: it contains no `MvccStore` and
//! no seal key. Broker binaries own those authorities and use the same wire
//! structs from the other side of the socket.

pub mod bundle;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aster_broker::{BrokerError, CapsuleBrokerClient, CommitOutcome};
use aster_capsule::{
    DeploymentId, Document, DocumentId, SealContext, SealedCapsule, SessionBinding, TenantId,
};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Maximum accepted frame size for prototype IPC.
///
/// A hostile cell should not be able to make the broker allocate unbounded
/// memory by claiming a huge length prefix. Production will likely make this
/// per-deployment and much lower for point-read traps.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Session ids on the wire (Repair C-CHANNEL): `InitialCapsule` mints one
/// and returns it in the grant; every later capsule verb must present it.
/// The broker treats the presented id purely as a lookup key into its own
/// session table — it rebuilds the bound `SealContext` from that table and
/// only checks the request's serialized `context` for equality, never
/// trusting it as authority. `session: None` on a capsule verb is an
/// explicit unbound request, which the broker rejects with a structured
/// error rather than falling back to unbound verification.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IpcRequest {
    InitialCapsule {
        context: SealContext,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
    },
    HydratePoint {
        context: SealContext,
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        key: DocumentId,
    },
    HydratePrefix {
        context: SealContext,
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        prefix: String,
        limit: usize,
    },
    LoadModuleBundle {
        context: SealContext,
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        path: String,
    },
    /// The write path (S9a): submit the sealed capsule, the declared
    /// dependency subset (the theorem's Variante B `S`), and the write
    /// set for fenced commit. Unlike the hydrate verbs there is no
    /// separate `context` claim — the capsule's own seal fields are the
    /// claimed context the broker checks against its session table before
    /// the seal MAC enforces the binding. ANY structured answer (success
    /// or rejection past the session gate) CLOSES the session: one
    /// session = one transaction attempt.
    Commit {
        session: Option<SessionBinding>,
        capsule: SealedCapsule,
        /// Declared point observations, by key. Every key must reference
        /// an atom the sealed capsule carries (B-SUBSET); a capsule key
        /// left undeclared is legal and demotes that dependency to an
        /// authorized blind write (Variante B omission, T2).
        declared_reads: Vec<DocumentId>,
        /// `Some(document)` puts, `None` deletes.
        writes: Vec<(DocumentId, Option<Document>)>,
    },
    /// No-commit end-of-life for a session: closes it without touching
    /// the fence. Unknown ids are rejected (`unknown_session`), never
    /// silently ignored.
    Abort {
        session: SessionBinding,
    },
    Shutdown,
}

/// What `InitialCapsule` hands back: the sealed capsule plus the freshly
/// minted session id the cell must present on every subsequent verb.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InitialCapsuleGrant {
    pub capsule: SealedCapsule,
    pub session: SessionBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IpcResponse {
    InitialCapsule(Result<InitialCapsuleGrant, WireBrokerError>),
    HydratePoint(Result<SealedCapsule, WireBrokerError>),
    HydratePrefix(Result<SealedCapsule, WireBrokerError>),
    LoadModuleBundle(Result<Option<ModuleBundle>, WireBrokerError>),
    Commit(Result<WireCommitOutcome, WireBrokerError>),
    Abort(Result<(), WireBrokerError>),
    ShutdownAck,
    Error(WireBrokerError),
}

/// Wire mirror of the fence's `CommitOutcome` — field-for-field parity
/// with `aster_broker::CommitOutcome` so the committer's answer crosses
/// the socket lossless.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WireCommitOutcome {
    Committed {
        ts: u64,
    },
    /// A committed write in `(s, h]` intersects a declared observation.
    Conflict {
        key: DocumentId,
    },
    /// The committer or the capsule context is stale relative to the
    /// storage lease authority.
    StaleEpoch {
        lease_epoch: u64,
    },
    /// The capsule claims a snapshot the log has never committed
    /// (S-SNAPSHOT violation — honest brokers cannot produce this).
    SnapshotBeyondHorizon {
        horizon: u64,
    },
    /// Validation history below the snapshot is no longer retained;
    /// retry from a fresh snapshot.
    RetentionViolated {
        low_watermark: u64,
    },
    /// Mutations-only path: an empty write set never enters the log.
    EmptyWriteSet,
}

impl From<CommitOutcome> for WireCommitOutcome {
    fn from(value: CommitOutcome) -> Self {
        match value {
            CommitOutcome::Committed { ts } => Self::Committed { ts },
            CommitOutcome::Conflict { key } => Self::Conflict { key },
            CommitOutcome::StaleEpoch { lease_epoch } => Self::StaleEpoch { lease_epoch },
            CommitOutcome::SnapshotBeyondHorizon { horizon } => {
                Self::SnapshotBeyondHorizon { horizon }
            }
            CommitOutcome::RetentionViolated { low_watermark } => {
                Self::RetentionViolated { low_watermark }
            }
            CommitOutcome::EmptyWriteSet => Self::EmptyWriteSet,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModuleBundle {
    pub path: String,
    pub bytes_base64: String,
}

impl ModuleBundle {
    pub fn from_bytes(path: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            path: path.into(),
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    pub fn decode_bytes(&self) -> IpcResult<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(self.bytes_base64.as_bytes())
            .map_err(|err| IpcError::Protocol(format!("module bundle base64 decode: {err}")))
    }
}

/// Serializable broker error for the JSON wire.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireBrokerError {
    pub code: String,
    pub message: String,
}

impl WireBrokerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn to_broker_error(&self) -> BrokerError {
        BrokerError::Remote(format!("{}: {}", self.code, self.message))
    }
}

impl From<BrokerError> for WireBrokerError {
    fn from(value: BrokerError) -> Self {
        match &value {
            BrokerError::Seal(error) => Self::new(format!("seal_{error:?}"), value.to_string()),
            BrokerError::TenantMismatch => Self::new("tenant_mismatch", value.to_string()),
            BrokerError::DeploymentMismatch => Self::new("deployment_mismatch", value.to_string()),
            BrokerError::ZeroScanLimit => Self::new("zero_scan_limit", value.to_string()),
            BrokerError::Remote(_) => Self::new("remote", value.to_string()),
        }
    }
}

#[derive(Debug)]
pub enum IpcError {
    Io(std::io::Error),
    Json(serde_json::Error),
    FrameTooLarge { len: usize, max: usize },
    UnexpectedResponse(&'static str),
    Protocol(String),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC I/O error: {error}"),
            Self::Json(error) => write!(f, "IPC JSON error: {error}"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "IPC frame too large: {len} bytes > {max} bytes")
            }
            Self::UnexpectedResponse(expected) => {
                write!(f, "IPC response did not match request; expected {expected}")
            }
            Self::Protocol(message) => write!(f, "IPC protocol error: {message}"),
        }
    }
}

impl std::error::Error for IpcError {}

impl From<std::io::Error> for IpcError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for IpcError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub type IpcResult<T> = Result<T, IpcError>;

/// Serializes fully before touching the socket: an oversized message
/// returns `FrameTooLarge` with zero bytes on the wire, leaving the stream
/// clean. brokerd's `send_response` relies on that ordering to substitute
/// a small structured `response_too_large` frame — don't switch this to
/// streaming serialization without revisiting that fallback.
pub fn write_frame<T: Serialize>(stream: &mut UnixStream, message: &T) -> IpcResult<()> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge {
            len: bytes.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let len = u32::try_from(bytes.len()).map_err(|_| IpcError::FrameTooLarge {
        len: bytes.len(),
        max: MAX_FRAME_BYTES,
    })?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> IpcResult<T> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge {
            len,
            max: MAX_FRAME_BYTES,
        });
    }
    let mut bytes = vec![0_u8; len];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// UDS implementation of the v0.2 broker trait.
///
/// This type intentionally contains only a socket path plus the current
/// session id. It has no store handle, no seal key, and no way to read
/// documents except by presenting a valid sealed capsule to the broker
/// process.
///
/// Session handling: `initial_capsule` stores the broker-minted session id
/// and every later capsule verb presents it automatically. Internal shared
/// state (`Arc<Mutex<..>>`, so clones share the slot) was chosen over a
/// returned session handle because it keeps the `CapsuleBrokerClient` trait
/// signature unchanged — the v8cell execute path drives the trait and must
/// stay oblivious to wire-only concerns. A later `initial_capsule` on the
/// same client replaces the held session, matching brokerd, where every
/// InitialCapsule mints a fresh session; `commit`/`abort` clear it, because
/// the broker closes the session on those verbs (one session = one
/// transaction attempt).
#[derive(Clone, Debug)]
pub struct UdsCapsuleBrokerClient {
    socket_path: PathBuf,
    session: Arc<Mutex<Option<SessionBinding>>>,
}

impl UdsCapsuleBrokerClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// The session id held from the last successful `initial_capsule`, if
    /// any. Exposed for tests and tooling that build raw wire requests.
    pub fn session(&self) -> Option<SessionBinding> {
        *self.session.lock().expect("session slot lock")
    }

    fn store_session(&self, session: SessionBinding) {
        *self.session.lock().expect("session slot lock") = Some(session);
    }

    fn clear_session(&self) {
        *self.session.lock().expect("session slot lock") = None;
    }

    pub fn shutdown(&self) -> IpcResult<()> {
        let response = self.call(IpcRequest::Shutdown)?;
        match response {
            IpcResponse::ShutdownAck => Ok(()),
            IpcResponse::Error(error) => Err(IpcError::Protocol(format!(
                "broker rejected shutdown: {}: {}",
                error.code, error.message
            ))),
            _ => Err(IpcError::UnexpectedResponse("ShutdownAck")),
        }
    }

    pub fn raw_call(&self, request: IpcRequest) -> IpcResult<IpcResponse> {
        self.call(request)
    }

    pub fn load_module_bundle(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        path: impl Into<String>,
    ) -> IpcResult<Option<Vec<u8>>> {
        match self.call(IpcRequest::LoadModuleBundle {
            context: context.clone(),
            session: self.session(),
            capsule,
            path: path.into(),
        })? {
            IpcResponse::LoadModuleBundle(Ok(Some(bundle))) => Ok(Some(bundle.decode_bytes()?)),
            IpcResponse::LoadModuleBundle(Ok(None)) => Ok(None),
            IpcResponse::LoadModuleBundle(Err(error)) | IpcResponse::Error(error) => {
                Err(IpcError::Protocol(format!(
                    "broker rejected module load: {}: {}",
                    error.code, error.message
                )))
            }
            _ => Err(IpcError::UnexpectedResponse("LoadModuleBundle")),
        }
    }

    /// Close the loop: submit the sealed capsule, the declared dependency
    /// subset (Variante B), and the write set for fenced commit, on the
    /// held session. The broker closes the session on every structured
    /// answer (one session = one transaction attempt), so the held id is
    /// dropped before returning — the next transaction starts with a
    /// fresh `initial_capsule`. Structured broker rejections fold into
    /// `IpcError::Protocol` (same shape as `load_module_bundle`); fence
    /// outcomes — including `Conflict` — are the `Ok` value.
    pub fn commit(
        &self,
        capsule: SealedCapsule,
        declared_reads: Vec<DocumentId>,
        writes: Vec<(DocumentId, Option<Document>)>,
    ) -> IpcResult<WireCommitOutcome> {
        let response = self.call(IpcRequest::Commit {
            session: self.session(),
            capsule,
            declared_reads,
            writes,
        })?;
        match response {
            IpcResponse::Commit(result) => {
                // Every structured Commit answer leaves no live broker-side
                // session: the broker closes a table-registered session even
                // when the gate rejects the attempt (context mismatch), and
                // the other gate rejections mean nothing was registered —
                // so dropping the held id here can never orphan one.
                self.clear_session();
                result.map_err(|error| {
                    IpcError::Protocol(format!(
                        "broker rejected commit: {}: {}",
                        error.code, error.message
                    ))
                })
            }
            IpcResponse::Error(error) => Err(IpcError::Protocol(format!(
                "broker rejected commit: {}: {}",
                error.code, error.message
            ))),
            _ => Err(IpcError::UnexpectedResponse("Commit")),
        }
    }

    /// End the held session without committing (the no-commit half of
    /// session end-of-life). The slot is cleared on any structured
    /// answer — an `unknown_session` rejection means the session is gone
    /// either way.
    pub fn abort(&self) -> IpcResult<()> {
        let Some(session) = self.session() else {
            return Err(IpcError::Protocol(
                "abort requires a session held from initial_capsule".into(),
            ));
        };
        let response = self.call(IpcRequest::Abort { session })?;
        match response {
            IpcResponse::Abort(result) => {
                self.clear_session();
                result.map_err(|error| {
                    IpcError::Protocol(format!(
                        "broker rejected abort: {}: {}",
                        error.code, error.message
                    ))
                })
            }
            IpcResponse::Error(error) => Err(IpcError::Protocol(format!(
                "broker rejected abort: {}: {}",
                error.code, error.message
            ))),
            _ => Err(IpcError::UnexpectedResponse("Abort")),
        }
    }

    fn call(&self, request: IpcRequest) -> IpcResult<IpcResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        write_frame(&mut stream, &request)?;
        read_frame(&mut stream)
    }
}

impl CapsuleBrokerClient for UdsCapsuleBrokerClient {
    fn initial_capsule(
        &self,
        context: &SealContext,
        tenant: TenantId,
        deployment: DeploymentId,
        snapshot_ts: u64,
        prewarm: Vec<DocumentId>,
    ) -> Result<SealedCapsule, BrokerError> {
        match self.call(IpcRequest::InitialCapsule {
            context: context.clone(),
            tenant,
            deployment,
            snapshot_ts,
            prewarm,
        }) {
            Ok(IpcResponse::InitialCapsule(result)) => match result {
                Ok(grant) => {
                    self.store_session(grant.session);
                    Ok(grant.capsule)
                }
                Err(error) => Err(error.to_broker_error()),
            },
            Ok(IpcResponse::Error(error)) => Err(error.to_broker_error()),
            Ok(_) => Err(BrokerError::Remote(
                IpcError::UnexpectedResponse("InitialCapsule").to_string(),
            )),
            Err(error) => Err(BrokerError::Remote(error.to_string())),
        }
    }

    fn hydrate_point(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        key: DocumentId,
    ) -> Result<SealedCapsule, BrokerError> {
        match self.call(IpcRequest::HydratePoint {
            context: context.clone(),
            session: self.session(),
            capsule,
            key,
        }) {
            Ok(IpcResponse::HydratePoint(result)) => {
                result.map_err(|error| error.to_broker_error())
            }
            Ok(IpcResponse::Error(error)) => Err(error.to_broker_error()),
            Ok(_) => Err(BrokerError::Remote(
                IpcError::UnexpectedResponse("HydratePoint").to_string(),
            )),
            Err(error) => Err(BrokerError::Remote(error.to_string())),
        }
    }

    fn hydrate_prefix(
        &self,
        context: &SealContext,
        capsule: SealedCapsule,
        prefix: String,
        limit: usize,
    ) -> Result<SealedCapsule, BrokerError> {
        match self.call(IpcRequest::HydratePrefix {
            context: context.clone(),
            session: self.session(),
            capsule,
            prefix,
            limit,
        }) {
            Ok(IpcResponse::HydratePrefix(result)) => {
                result.map_err(|error| error.to_broker_error())
            }
            Ok(IpcResponse::Error(error)) => Err(error.to_broker_error()),
            Ok(_) => Err(BrokerError::Remote(
                IpcError::UnexpectedResponse("HydratePrefix").to_string(),
            )),
            Err(error) => Err(BrokerError::Remote(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_json_message() {
        let (mut left, mut right) = UnixStream::pair().expect("socketpair");
        let request = IpcRequest::Shutdown;
        write_frame(&mut left, &request).expect("write");
        let decoded: IpcRequest = read_frame(&mut right).expect("read");
        assert_eq!(decoded, request);
    }

    #[test]
    fn module_bundle_base64_round_trips() {
        let raw = b"zip bytes \x00\xff";
        let bundle = ModuleBundle::from_bytes("messages.js", raw);
        assert_eq!(bundle.path, "messages.js");
        assert_eq!(bundle.decode_bytes().expect("decode"), raw);
    }
}
