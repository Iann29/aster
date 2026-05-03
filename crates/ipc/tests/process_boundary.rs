use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster_broker::CapsuleBrokerClient;
use aster_capsule::{DeploymentId, DocumentId, SealContext, TenantId};
use aster_ipc::{IpcRequest, IpcResponse, UdsCapsuleBrokerClient};

#[test]
fn process_separated_v8_cell_hydrates_over_uds_and_rejects_replay() {
    let temp = TempDir::new("aster-ipc-e2e");
    let socket = temp.path().join("broker.sock");
    let js = temp.path().join("main.js");
    fs::write(
        &js,
        r#"
            async function main() {
              const a = await Aster.read("counters/a", "value");
              const b = await Aster.read("counters/b", "value");
              return a + b;
            }
        "#,
    )
    .expect("write JS fixture");

    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    let cell = Command::new(env!("CARGO_BIN_EXE_aster_v8cell"))
        .env("ASTER_BROKER_SOCK", &socket)
        .env("ASTER_TENANT", "tenant-proc")
        .env("ASTER_DEPLOYMENT", "dep-proc")
        .env("ASTER_SNAPSHOT_TS", "2")
        .env("ASTER_CELL_ID", "cell-proc-1")
        .env("ASTER_LEASE_EPOCH", "7")
        .env("ASTER_PREWARM", "counters/a")
        .env("ASTER_JS", &js)
        .env("ASTER_MAX_TRAPS", "8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn v8 cell process");
    let output = cell.wait_with_output().expect("wait v8 cell");
    assert!(
        output.status.success(),
        "cell failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"output\":42"), "stdout={stdout}");
    assert!(stdout.contains("\"traps\":1"), "stdout={stdout}");

    // Attack simulation from the host test: get a capsule legitimately for
    // cell-a, then present it under cell-b over the real socket. The broker
    // process must reject the stale/wrong-cell capability before hydrating.
    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let tenant = TenantId::new("tenant-proc");
    let deployment = DeploymentId::new("dep-proc");
    let sealed = client
        .initial_capsule(
            &SealContext::new("cell-a", 7),
            tenant,
            deployment,
            2,
            Vec::new(),
        )
        .expect("initial capsule through UDS");
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: SealContext::new("cell-b", 7),
            capsule: sealed,
            key: DocumentId::new("counters/b"),
        })
        .expect("raw hydrate response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert!(
                error.message.contains("different cell") || error.code.contains("WrongCell"),
                "unexpected error: {error:?}"
            );
        }
        other => panic!("wrong-cell hydrate should fail, got {other:?}"),
    }

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

fn spawn_broker(socket: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_aster_brokerd"))
        .env("ASTER_BROKER_SOCK", socket)
        .env("ASTER_TENANT", "tenant-proc")
        .env("ASTER_DEPLOYMENT", "dep-proc")
        .env("ASTER_SEED_I64", "counters/a:value:20,counters/b:value:22")
        .env("ASTER_SEAL_SEED", "process-boundary-test-key")
        .env("ASTER_MAX_CONNECTIONS", "16")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn brokerd")
}

fn wait_for_socket(socket: &Path) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("broker socket did not appear at {}", socket.display());
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
