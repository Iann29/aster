use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster_broker::{BrokerError, CapsuleBrokerClient};
use aster_capsule::{
    doc_with_i64, DeploymentId, DocumentId, KeyInterval, ScanStop, SealContext, TenantId,
};
use aster_ipc::{write_frame, IpcRequest, IpcResponse, UdsCapsuleBrokerClient, WireCommitOutcome};

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
    // R4: the envelope carries the runtime's consumption ledger
    // (BTreeSet-ordered) so a harness can drive the Commit verb's
    // declared_reads from the real observations.
    assert!(
        stdout.contains("\"consumed_reads\":[\"counters/a\",\"counters/b\"]"),
        "stdout={stdout}"
    );

    // Attack simulation from the host test: get a capsule legitimately for
    // cell-a, then present it under cell-b over the real socket — on
    // cell-a's own session. The broker must reject the relabelled context
    // at the session table (C-CHANNEL), before the seal is even consulted.
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
    let session = client.session().expect("client holds the granted session");
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: SealContext::new("cell-b", 7),
            session: Some(session),
            capsule: sealed,
            key: DocumentId::new("counters/b"),
        })
        .expect("raw hydrate response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert_eq!(error.code, "session_context_mismatch", "got {error:?}");
        }
        other => panic!("wrong-cell hydrate should fail, got {other:?}"),
    }

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

/// The C-CHANNEL repair end-to-end over the real socket: the happy path is
/// green with the broker-minted session, and every channel violation the
/// theorem names is rejected — unbound hydrates, unknown session ids, and
/// capsules transplanted across sessions of the same cell_id (the
/// re-spawned-cell replay).
#[test]
fn session_binding_gates_wire_hydrates() {
    let temp = TempDir::new("aster-ipc-session");
    let socket = temp.path().join("broker.sock");
    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let context = SealContext::new("cell-sess", 7);
    let sealed = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            2,
            Vec::new(),
        )
        .expect("initial capsule through UDS");
    let session_a = client.session().expect("session held after initial");

    // Full bound roundtrip: initial + point + prefix through the client,
    // which presents the held session on every verb.
    let sealed_after_point = client
        .hydrate_point(&context, sealed.clone(), DocumentId::new("counters/a"))
        .expect("bound point hydrate");
    let sealed_after_prefix = client
        .hydrate_prefix(&context, sealed_after_point, "counters/".to_string(), 2)
        .expect("bound prefix hydrate");
    assert_eq!(sealed_after_prefix.capsule().ranges.len(), 1);
    let value = sealed_after_prefix
        .capsule()
        .get(&DocumentId::new("counters/a"))
        .and_then(|doc| doc.document.as_ref())
        .and_then(|doc| doc.get("value"));
    assert_eq!(value, Some(&aster_capsule::Value::Int(20)));

    // Unbound wire hydrate: session omitted entirely. Rejected before any
    // seal or store work.
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: context.clone(),
            session: None,
            capsule: sealed.clone(),
            key: DocumentId::new("counters/b"),
        })
        .expect("raw unbound response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert_eq!(error.code, "session_required", "got {error:?}");
        }
        other => panic!("unbound hydrate should fail, got {other:?}"),
    }

    // Fabricated session id: unguessable table key, so a made-up id is
    // simply unknown.
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: context.clone(),
            session: Some(aster_capsule::SessionBinding::from_bytes([0x42; 32])),
            capsule: sealed.clone(),
            key: DocumentId::new("counters/b"),
        })
        .expect("raw unknown-session response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert_eq!(error.code, "unknown_session", "got {error:?}");
        }
        other => panic!("unknown-session hydrate should fail, got {other:?}"),
    }

    // Re-spawned cell: a SECOND session for the identical cell_id/epoch.
    // The session-A capsule presented on session B passes the public
    // context checks and must die in the seal MAC (WrongSession).
    let _sealed_b = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            2,
            Vec::new(),
        )
        .expect("second initial capsule");
    let session_b = client.session().expect("session held after re-initial");
    assert_ne!(session_a, session_b, "each grant mints a fresh session");
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: context.clone(),
            session: Some(session_b),
            capsule: sealed,
            key: DocumentId::new("counters/b"),
        })
        .expect("raw cross-session response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert!(
                error.code.contains("WrongSession"),
                "expected seal-level wrong-session rejection, got {error:?}"
            );
        }
        other => panic!("cross-session capsule should fail, got {other:?}"),
    }

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

#[test]
fn prefix_hydrate_over_uds_carries_certificate() {
    let temp = TempDir::new("aster-ipc-prefix");
    let socket = temp.path().join("broker.sock");
    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let context = SealContext::new("cell-prefix", 7);
    let sealed = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            2,
            Vec::new(),
        )
        .expect("initial capsule through UDS");

    let sealed = client
        .hydrate_prefix(&context, sealed, "counters/".to_string(), 2)
        .expect("prefix hydrate through UDS");
    let capsule = sealed.capsule();
    assert_eq!(
        capsule.ranges.len(),
        1,
        "returned sealed capsule must carry the scan certificate"
    );
    let certificate = &capsule.ranges[0];
    certificate.validate().expect("wire certificate is valid");
    assert_eq!(certificate.interval, KeyInterval::Prefix("counters/".into()));
    assert_eq!(
        certificate.keys,
        vec![
            DocumentId::new("counters/a"),
            DocumentId::new("counters/b")
        ]
    );
    assert_eq!(certificate.stop, ScanStop::Boundary, "2 collected == limit 2");
    capsule
        .validate_structure()
        .expect("certificate keys cross-reference live docs");
    let value = capsule
        .get(&DocumentId::new("counters/a"))
        .and_then(|doc| doc.document.as_ref())
        .and_then(|doc| doc.get("value"));
    assert_eq!(value, Some(&aster_capsule::Value::Int(20)));

    // limit=0 can never carry a valid certificate — the broker process
    // must reject it with a typed wire error, not crash or fabricate.
    let response = client
        .raw_call(IpcRequest::HydratePrefix {
            context: context.clone(),
            session: client.session(),
            capsule: sealed.clone(),
            prefix: "counters/".to_string(),
            limit: 0,
        })
        .expect("raw zero-limit response");
    match response {
        IpcResponse::HydratePrefix(Err(error)) => {
            assert_eq!(error.code, "zero_scan_limit", "got {error:?}");
        }
        other => panic!("zero-limit prefix hydrate should fail, got {other:?}"),
    }

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

/// S9a closes the loop over the real socket: initial → hydrate → commit
/// through the in-memory fence. The commit CLOSES the session (one
/// session = one transaction attempt), the identical replayed request
/// dies at the session gate, a second reader whose declared absence-read
/// was hit by the first commit gets a structured Conflict, and Abort is
/// the no-commit close.
#[test]
fn commit_verb_closes_the_loop_and_the_session() {
    let temp = TempDir::new("aster-ipc-commit");
    let socket = temp.path().join("broker.sock");
    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    let context = SealContext::new("cell-commit", 7);
    let tenant = TenantId::new("tenant-proc");
    let deployment = DeploymentId::new("dep-proc");

    // Reader-writer: observe counters/a (prewarm) + counters/b (trap),
    // then commit a write declaring both reads (Variante B subset —
    // here, the full observation set).
    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let sealed = client
        .initial_capsule(
            &context,
            tenant.clone(),
            deployment.clone(),
            2,
            vec![DocumentId::new("counters/a")],
        )
        .expect("initial capsule");
    let session = client.session().expect("session held");
    let sealed = client
        .hydrate_point(&context, sealed, DocumentId::new("counters/b"))
        .expect("hydrate counters/b");
    let replay_capsule = sealed.clone();
    let outcome = client
        .commit(
            sealed,
            vec![DocumentId::new("counters/a"), DocumentId::new("counters/b")],
            vec![(
                DocumentId::new("counters/total"),
                Some(doc_with_i64("value", 42)),
            )],
        )
        .expect("commit over UDS");
    assert_eq!(outcome, WireCommitOutcome::Committed { ts: 3 });
    assert!(
        client.session().is_none(),
        "client drops the session the broker closed"
    );

    // Session end-of-life: the same bytes replayed on the closed session
    // die at the session gate. CE2.1: replay of the ISSUED capsule is
    // inevitable under bearer semantics — session EOL is what bounds it.
    let response = client
        .raw_call(IpcRequest::Commit {
            session: Some(session),
            capsule: replay_capsule.clone(),
            declared_reads: vec![DocumentId::new("counters/a")],
            writes: vec![(
                DocumentId::new("counters/total"),
                Some(doc_with_i64("value", 43)),
            )],
        })
        .expect("raw replay response");
    match response {
        IpcResponse::Commit(Err(error)) => {
            assert_eq!(error.code, "unknown_session", "got {error:?}");
        }
        other => panic!("replay after close should fail, got {other:?}"),
    }
    // ... and so does any hydrate on the closed session.
    let response = client
        .raw_call(IpcRequest::HydratePoint {
            context: context.clone(),
            session: Some(session),
            capsule: replay_capsule,
            key: DocumentId::new("counters/b"),
        })
        .expect("raw post-commit hydrate response");
    match response {
        IpcResponse::HydratePoint(Err(error)) => {
            assert_eq!(error.code, "unknown_session", "got {error:?}");
        }
        other => panic!("post-commit hydrate should fail, got {other:?}"),
    }

    // A second reader still snapshotted at s=2 observes counters/total as
    // ABSENT, declares that read, and must conflict: the first commit
    // wrote it at ts 3 ∈ (2, h] (the absence-flip case, through the wire).
    let sealed = client
        .initial_capsule(&context, tenant.clone(), deployment.clone(), 2, Vec::new())
        .expect("second initial capsule");
    let sealed = client
        .hydrate_point(&context, sealed, DocumentId::new("counters/total"))
        .expect("hydrate counters/total");
    let outcome = client
        .commit(
            sealed,
            vec![DocumentId::new("counters/total")],
            vec![(
                DocumentId::new("counters/other"),
                Some(doc_with_i64("value", 1)),
            )],
        )
        .expect("conflicting commit over UDS");
    assert_eq!(
        outcome,
        WireCommitOutcome::Conflict {
            key: DocumentId::new("counters/total")
        }
    );

    // Abort: the no-commit close. Double-close surfaces as unknown.
    let _sealed = client
        .initial_capsule(&context, tenant, deployment, 2, Vec::new())
        .expect("third initial capsule");
    let session = client.session().expect("session held");
    client.abort().expect("abort over UDS");
    assert!(client.session().is_none(), "abort drops the held session");
    let response = client
        .raw_call(IpcRequest::Abort { session })
        .expect("raw double-abort response");
    match response {
        IpcResponse::Abort(Err(error)) => {
            assert_eq!(error.code, "unknown_session", "got {error:?}");
        }
        other => panic!("double abort should fail, got {other:?}"),
    }

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

#[test]
fn broker_outlives_many_sequential_connections() {
    let temp = TempDir::new("aster-ipc-manyconn");
    let socket = temp.path().join("broker.sock");
    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    // Regression for the v0.6 lifetime budget: ASTER_MAX_CONNECTIONS counted
    // every connection served since boot and exited the accept loop past the
    // cap, so a busy broker (one connection per trap) died mid-workload.
    // Serve well past the old test cap of 16 and prove the broker only exits
    // on the Shutdown verb.
    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let context = SealContext::new("cell-many", 7);
    for _ in 0..48 {
        client
            .initial_capsule(
                &context,
                TenantId::new("tenant-proc"),
                DeploymentId::new("dep-proc"),
                2,
                Vec::new(),
            )
            .expect("initial capsule");
    }
    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

/// One bad client must never kill the broker — and with it the in-process
/// session table every other live cell depends on. Two hostile shapes,
/// several times each: a peer that sends a valid request and hangs up
/// without reading (the broker's response write hits EPIPE — Rust ignores
/// SIGPIPE), and a peer that connects and sends nothing (read fails, and
/// the bad_request reply lands on the same closed peer). Before C4 either
/// one `?`-propagated out of the accept loop and exited the process.
#[test]
fn broker_survives_clients_that_close_before_reading() {
    let temp = TempDir::new("aster-ipc-epipe");
    let socket = temp.path().join("broker.sock");
    let mut broker = spawn_broker(&socket);
    wait_for_socket(&socket);

    for _ in 0..4 {
        // Valid request, immediate close: the broker still has to read and
        // handle before it writes, so our close wins the race and its
        // response write meets a hung-up peer.
        let mut stream = UnixStream::connect(&socket).expect("connect");
        write_frame(
            &mut stream,
            &IpcRequest::InitialCapsule {
                context: SealContext::new("cell-epipe", 7),
                tenant: TenantId::new("tenant-proc"),
                deployment: DeploymentId::new("dep-proc"),
                snapshot_ts: 2,
                prewarm: Vec::new(),
            },
        )
        .expect("write doomed request");
        drop(stream);

        // No bytes at all: the broker's read_frame errors out.
        let stream = UnixStream::connect(&socket).expect("connect");
        drop(stream);
    }

    // The broker outlived every hostile connection and still serves a
    // normal grant + hydrate roundtrip.
    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let context = SealContext::new("cell-after", 7);
    let sealed = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            2,
            Vec::new(),
        )
        .expect("initial capsule after hostile clients");
    let sealed = client
        .hydrate_point(&context, sealed, DocumentId::new("counters/a"))
        .expect("hydrate after hostile clients");
    let value = sealed
        .capsule()
        .get(&DocumentId::new("counters/a"))
        .and_then(|doc| doc.document.as_ref())
        .and_then(|doc| doc.get("value"));
    assert_eq!(value, Some(&aster_capsule::Value::Int(20)));

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

/// A hydrate whose RESPONSE outgrows the 1 MiB frame cap must come back as
/// a structured `response_too_large` error — with the broker still alive —
/// never a dead socket (before C4 the broker exited on it, wiping every
/// session).
///
/// Growth engine: every hydrate_prefix appends a full RangeCertificate
/// (all matched keys) to the capsule, so re-hydrating the same prefix
/// grows it by ~one certificate (~100 KiB here) per round. The request
/// always carries one certificate fewer than the response it produces, so
/// the response crosses the cap strictly first — the loop below cannot
/// die on a client-side FrameTooLarge.
#[test]
fn oversized_response_returns_structured_error_and_broker_survives() {
    let temp = TempDir::new("aster-ipc-toolarge");
    let socket = temp.path().join("broker.sock");
    let count = 200_usize;
    let seed = big_prefix_seed("bulk/", count, 500);
    let mut broker = spawn_broker_with_seed(&socket, &seed);
    wait_for_socket(&socket);

    // Unset ASTER_SNAPSHOT_TS makes brokerd pin the post-seed store head:
    // one ts per seeded doc.
    let snapshot_ts = count as u64;
    let client = UdsCapsuleBrokerClient::new(socket.clone());
    let context = SealContext::new("cell-big", 7);
    let mut sealed = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            snapshot_ts,
            Vec::new(),
        )
        .expect("initial capsule");

    let mut rounds = 0_usize;
    let error = loop {
        rounds += 1;
        assert!(rounds <= 32, "response never crossed the frame cap");
        match client.hydrate_prefix(&context, sealed.clone(), "bulk/".to_string(), count) {
            Ok(next) => sealed = next,
            Err(error) => break error,
        }
    };
    match &error {
        BrokerError::Remote(message) => assert!(
            message.starts_with("response_too_large"),
            "expected response_too_large, got {message:?}"
        ),
        other => panic!("expected remote response_too_large error, got {other:?}"),
    }

    // The broker survived the oversized response and still serves small
    // hydrates on a fresh grant.
    let context = SealContext::new("cell-small", 7);
    let sealed = client
        .initial_capsule(
            &context,
            TenantId::new("tenant-proc"),
            DeploymentId::new("dep-proc"),
            snapshot_ts,
            Vec::new(),
        )
        .expect("initial capsule after oversized response");
    let sealed = client
        .hydrate_prefix(&context, sealed, "bulk/".to_string(), 1)
        .expect("small hydrate after oversized response");
    assert_eq!(sealed.capsule().ranges[0].keys.len(), 1);

    client.shutdown().expect("shutdown broker");
    assert!(broker.wait().expect("broker wait").success());
}

fn spawn_broker(socket: &Path) -> Child {
    spawn_broker_with_seed(socket, "counters/a:value:20,counters/b:value:22")
}

fn spawn_broker_with_seed(socket: &Path, seed: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_aster_brokerd"))
        .env("ASTER_BROKER_SOCK", socket)
        .env("ASTER_TENANT", "tenant-proc")
        .env("ASTER_DEPLOYMENT", "dep-proc")
        .env("ASTER_SEED_I64", seed)
        .env("ASTER_SEAL_SEED", "process-boundary-test-key")
        // Memory-mode lease stand-in (S9a): the broker only mints sessions
        // for contexts claiming ITS epoch, so this must match the epoch 7
        // every context in this file claims.
        .env("ASTER_LEASE_EPOCH", "7")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn brokerd")
}

/// `count` seed entries under `prefix`, each key padded to `key_len` bytes —
/// one prefix hydrate over these yields a certificate of roughly
/// `count * key_len` bytes. Sized to stay under Linux's 128 KiB
/// single-env-var cap (MAX_ARG_STRLEN).
fn big_prefix_seed(prefix: &str, count: usize, key_len: usize) -> String {
    (0..count)
        .map(|i| {
            let mut key = format!("{prefix}{i:04}");
            while key.len() < key_len {
                key.push('x');
            }
            format!("{key}:value:{i}")
        })
        .collect::<Vec<_>>()
        .join(",")
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
