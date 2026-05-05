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

use aster_broker::{BrokerError, CapsuleBrokerClient};
use aster_capsule::{DeploymentId, DocumentId, SealContext, SealedCapsule, TenantId};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Maximum accepted frame size for prototype IPC.
///
/// A hostile cell should not be able to make the broker allocate unbounded
/// memory by claiming a huge length prefix. Production will likely make this
/// per-deployment and much lower for point-read traps.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
        capsule: SealedCapsule,
        key: DocumentId,
    },
    LoadModuleBundle {
        context: SealContext,
        capsule: SealedCapsule,
        path: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IpcResponse {
    InitialCapsule(Result<SealedCapsule, WireBrokerError>),
    HydratePoint(Result<SealedCapsule, WireBrokerError>),
    LoadModuleBundle(Result<Option<ModuleBundle>, WireBrokerError>),
    ShutdownAck,
    Error(WireBrokerError),
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
/// This type intentionally contains only a socket path. It has no store handle,
/// no seal key, and no way to read documents except by presenting a valid sealed
/// capsule to the broker process.
#[derive(Clone, Debug)]
pub struct UdsCapsuleBrokerClient {
    socket_path: PathBuf,
}

impl UdsCapsuleBrokerClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
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
            Ok(IpcResponse::InitialCapsule(result)) => {
                result.map_err(|error| error.to_broker_error())
            }
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
