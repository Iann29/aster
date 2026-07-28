#![cfg(feature = "postgres-it")]
use std::os::unix::fs::PermissionsExt;

use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster_convex_codec::DocumentIdV6;
use aster_ipc::{
    launch::{issue_launch_token, LaunchTokenClaims, LaunchTokenKey},
    UdsCapsuleBrokerClient,
};
use aster_store_postgres::{WritePlane, WritePlaneConfig};
use tokio_postgres::NoTls;

const TENANT: &str = "tenant-process-authoritative";
const LAUNCH_KEY: [u8; 32] = [0x24; 32];

struct BrokerProcess(Child);

impl Deref for BrokerProcess {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BrokerProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for BrokerProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
fn cell_commit_is_visible_to_next_fresh_cell_from_same_history() {
    let url = std::env::var("ASTER_DB_URL").expect("set ASTER_DB_URL for postgres-it");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let deployment = format!("dep-process-authoritative-{nonce}");
    reset_convex_fixture(&url);
    let temp = std::env::temp_dir().join(format!("aster-authoritative-{nonce}"));
    fs::create_dir_all(&temp).expect("create temp dir");
    let socket = temp.join("broker.sock");
    let policy = temp.join("policy.json");
    fs::write(
        &policy,
        r#"{
          "version": 1,
          "read_prefixes": ["docs/", "j"],
          "write_prefixes": ["docs/", "j"],
          "module_prefixes": ["functions/"],
          "insert_tables": ["messages"],
          "max_reads_per_transaction": 32,
          "max_writes_per_transaction": 8,
          "max_scan_limit": 32
        }"#,
    )
    .expect("write policy");
    let seal_key = temp.join("seal.key");
    fs::write(&seal_key, [0x42_u8; 32]).expect("write seal key");
    fs::set_permissions(&seal_key, fs::Permissions::from_mode(0o600))
        .expect("restrict seal key permissions");
    let launch_key = temp.join("launch.key");
    fs::write(&launch_key, LAUNCH_KEY).expect("write launch key");
    fs::set_permissions(&launch_key, fs::Permissions::from_mode(0o600))
        .expect("restrict launch key permissions");

    let mut broker = spawn_broker(&socket, &policy, &seal_key, &launch_key, &url, &deployment);
    wait_for_socket(&socket, &mut broker);

    let plane = WritePlane::connect(WritePlaneConfig {
        url: url.clone(),
        ..WritePlaneConfig::default()
    })
    .expect("connect observer plane");
    let epoch = plane
        .current_epoch(TENANT, &deployment)
        .expect("read broker epoch")
        .expect("broker acquired epoch");

    let mutation = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-authoritative-write",
        "cell-authoritative-write",
        1,
        r#"
            async function main() {
              const inserted = JSON.parse(await Convex.asyncSyscall(
                "1.0/insert",
                JSON.stringify({
                  table: "docs",
                  value: { _id: "docs/authoritative", n: 41 }
                })
              ));
              return inserted._id;
            }
        "#,
    );
    assert_success(&mutation, "mutation cell");
    let mutation_json: serde_json::Value =
        serde_json::from_slice(&mutation.stdout).expect("mutation envelope JSON");
    assert_eq!(mutation_json["transaction_status"], "committed");
    assert_eq!(mutation_json["attempts"], 1);
    assert_eq!(mutation_json["commit"]["Committed"]["ts"], 1);

    let query = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-authoritative-read",
        "cell-authoritative-read",
        2,
        r#"
            async function main() {
              const document = JSON.parse(await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({ id: "docs/authoritative" })
              ));
              return document.n + 1;
            }
        "#,
    );
    assert_success(&query, "query cell");
    let query_json: serde_json::Value =
        serde_json::from_slice(&query.stdout).expect("query envelope JSON");
    assert_eq!(query_json["output"], 42);
    assert_eq!(query_json["transaction_status"], "read_only");
    assert!(query_json["commit"].is_null());
    let minted_mutation = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-authoritative-mint",
        "cell-authoritative-mint",
        3,
        r#"
            async function main() {
              const inserted = JSON.parse(await Convex.asyncSyscall(
                "1.0/insert",
                JSON.stringify({
                  table: "messages",
                  value: { name: "minted" }
                })
              ));
              return inserted._id;
            }
        "#,
    );
    assert_success(&minted_mutation, "server-id mutation cell");
    let minted_json: serde_json::Value =
        serde_json::from_slice(&minted_mutation.stdout).expect("mint mutation envelope JSON");
    assert_eq!(minted_json["transaction_status"], "committed");
    assert_eq!(minted_json["commit"]["Committed"]["ts"], 2);
    let minted_id = minted_json["output"]
        .as_str()
        .expect("insert output is a document id");
    let decoded = DocumentIdV6::decode(minted_id).expect("broker minted canonical Convex IDv6");
    assert_eq!(decoded.table_number, 10_001);

    let minted_query_source = format!(
        r#"
            async function main() {{
              const document = JSON.parse(await Convex.asyncSyscall(
                "1.0/get",
                JSON.stringify({{ id: "{minted_id}" }})
              ));
              return document.name;
            }}
        "#
    );
    let minted_query = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-authoritative-minted-read",
        "cell-authoritative-minted-read",
        4,
        &minted_query_source,
    );
    assert_success(&minted_query, "server-id query cell");
    let minted_query_json: serde_json::Value =
        serde_json::from_slice(&minted_query.stdout).expect("minted query envelope JSON");
    assert_eq!(minted_query_json["output"], "minted");

    let denied_write = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-policy-denied",
        "cell-policy-denied",
        5,
        r#"
            async function main() {
              await Convex.asyncSyscall(
                "1.0/insert",
                JSON.stringify({
                  table: "secrets",
                  value: { _id: "secrets/forbidden", value: 1 }
                })
              );
              return 1;
            }
        "#,
    );
    assert_failure_contains(&denied_write, "policy_write_denied");

    let wrong_identity = cell_output(
        &socket,
        &deployment,
        epoch,
        "cell-actual",
        "cell-token-subject",
        6,
        "async function main() { return 1; }",
    );
    assert_failure_contains(&wrong_identity, "launch_token_rejected");
    let shutdown = UdsCapsuleBrokerClient::new(&socket)
        .shutdown()
        .expect_err("production broker must reject cell-socket shutdown");
    assert!(shutdown.to_string().contains("shutdown_disabled"));
    broker
        .kill()
        .expect("terminate broker through supervisor path");
    broker.wait().expect("wait broker");
    fs::remove_dir_all(temp).expect("remove temp dir");
}

fn reset_convex_fixture(url: &str) {
    let runtime = tokio::runtime::Runtime::new().expect("create fixture runtime");
    runtime.block_on(async {
        let (client, connection) = tokio_postgres::connect(url, NoTls)
            .await
            .expect("connect fixture database");
        let connection_task = tokio::spawn(async move {
            connection.await.expect("drive fixture connection");
        });
        client
            .batch_execute("DROP SCHEMA IF EXISTS convex_dev CASCADE")
            .await
            .expect("drop old Convex fixture");
        client
            .batch_execute(include_str!(
                "../../store-postgres/tests/fixtures/schema.sql"
            ))
            .await
            .expect("create Convex fixture");
        client
            .batch_execute(include_str!("../../store-postgres/tests/fixtures/seed.sql"))
            .await
            .expect("seed Convex fixture");
        drop(client);
        connection_task.await.expect("join fixture connection");
    });
}

fn spawn_broker(
    socket: &Path,
    policy: &Path,
    seal_key: &Path,
    launch_key: &Path,
    url: &str,
    deployment: &str,
) -> BrokerProcess {
    BrokerProcess(
        Command::new(env!("CARGO_BIN_EXE_aster_brokerd"))
            .env("ASTER_BROKER_SOCK", socket)
            .env("ASTER_TENANT", TENANT)
            .env("ASTER_DEPLOYMENT", deployment)
            .env("ASTER_SEAL_KEY_FILE", seal_key)
            .env("ASTER_LAUNCH_KEY_FILE", launch_key)
            .env("ASTER_STORE", "postgres")
            .env("ASTER_DB_URL", url)
            .env("ASTER_DB_SCHEMA", "convex_dev")
            .env("ASTER_POLICY_FILE", policy)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn broker"),
    )
}

fn cell_output(
    socket: &Path,
    deployment: &str,
    epoch: u64,
    cell_id: &str,
    token_cell_id: &str,
    nonce: u8,
    source: &str,
) -> Output {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs();
    let launch_token = issue_launch_token(
        &LaunchTokenKey::from_bytes(LAUNCH_KEY),
        &LaunchTokenClaims {
            cell_id: token_cell_id.to_string(),
            tenant: TENANT.to_string(),
            deployment: deployment.to_string(),
            lease_epoch: epoch,
            expires_at_unix_s: now + 60,
            nonce: [nonce; 16],
        },
    )
    .expect("issue launch token");
    Command::new(env!("CARGO_BIN_EXE_aster_v8cell"))
        .env("ASTER_BROKER_SOCK", socket)
        .env("ASTER_TENANT", TENANT)
        .env("ASTER_DEPLOYMENT", deployment)
        .env("ASTER_CELL_ID", cell_id)
        .env("ASTER_LEASE_EPOCH", epoch.to_string())
        .env("ASTER_LAUNCH_TOKEN", launch_token)
        .env("ASTER_JS_INLINE", source)
        .env("ASTER_MAX_TRAPS", "16")
        .env("ASTER_MAX_RETRIES", "3")
        .output()
        .expect("run cell")
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_socket(socket: &PathBuf, broker: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if socket.exists() {
            return;
        }
        if let Some(status) = broker.try_wait().expect("broker status") {
            panic!("broker exited before readiness: {status}");
        }

        thread::sleep(Duration::from_millis(20));
    }
    panic!("broker socket did not appear at {}", socket.display());
}

fn assert_failure_contains(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "cell unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(needle),
        "stderr did not contain {needle:?}: {stderr}"
    );
}
