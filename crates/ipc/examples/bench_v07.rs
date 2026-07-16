//! S10 measurement harness — the v0.7 write-path benchmark bin.
//!
//! Four modes, driven by `bench/run-v07.sh` (which owns process/DB
//! choreography and writes raw logs to `bench/results/v07/`):
//!
//! - `reseal`     — EQ2: per-trap hydrate cost vs. capsule size, over the
//!                  real UDS wire against a running brokerd. Grows a capsule
//!                  to n distinct keys (timing the climb = the cumulative
//!                  curve), then samples repeated hydrates of an
//!                  already-present key — the capsule size stays exactly n
//!                  (BTreeMap insert), so each sample is one full
//!                  verify(n) + read + reseal(n) round trip.
//! - `fence`      — EQ3: the commit fence in isolation, direct
//!                  `WritePlane::commit` against real Postgres: blind
//!                  commits (latency + sustained commits/s), p point-read
//!                  validations, w window validations over a ~1k-event log,
//!                  and the conflict-abort cost.
//! - `fence-seed` — advance a (tenant, deployment) log tip to a target ts
//!                  with blind commits, so a postgres-mode brokerd whose
//!                  read snapshot is pinned at the Convex fixture ts can
//!                  pass the fence's S-SNAPSHOT check (snapshot <= horizon).
//! - `e2e`        — EQ3/EQ4: the full theorem loop per transaction —
//!                  InitialCapsule (session mint) → real JS in a V8 isolate
//!                  (`1.0/get` read trap + `1.0/insert` write) →
//!                  consumed_reads + write_set → Commit verb over UDS →
//!                  Postgres fence. Reports the exec/commit split and
//!                  sustained serial tx/s.
//!
//! Every phase prints one `RESULT {json}` line (machine-readable, kept in
//! the committed raw logs) plus a human summary. Timings are captured with
//! `Instant` around the client-side call, so a sample includes request
//! serialization and UDS connect — what a cell pays per trap, plus, in
//! the reseal phase only, a client-side clone of the capsule the harness
//! makes before each call (review A4): a real cell never pays that O(n)
//! copy, so the fitted per-entry slope is a slight upper bound (order of
//! tens of µs inside the n=1000 sample).

use std::time::Instant;

use aster_broker::CapsuleBrokerClient;
use aster_capsule::{
    DeploymentId, Document, DocumentId, KeyInterval, ObservedWindow, ScanDirection, SealContext,
    TenantId, Value,
};
use aster_ipc::{IpcRequest, UdsCapsuleBrokerClient, WireCommitOutcome};
use aster_store_postgres::{CommitOutcome, FenceInput, WritePlane, WritePlaneConfig};
use aster_v8cell::V8SandboxCell;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let flags = Flags::parse(&args[2..]);
    match mode {
        "reseal" => reseal(&flags),
        "fence" => fence(&flags),
        "fence-seed" => fence_seed(&flags),
        "e2e" => e2e(&flags),
        other => {
            eprintln!(
                "usage: bench_v07 <reseal|fence|fence-seed|e2e> [--key=value ...]\n\
                 got mode {other:?}"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------- flags --

struct Flags(Vec<(String, String)>);

impl Flags {
    fn parse(rest: &[String]) -> Self {
        let mut out = Vec::new();
        for arg in rest {
            let Some(body) = arg.strip_prefix("--") else {
                eprintln!("bad flag {arg:?} (want --key=value)");
                std::process::exit(2);
            };
            let Some((key, value)) = body.split_once('=') else {
                eprintln!("bad flag {arg:?} (want --key=value)");
                std::process::exit(2);
            };
            out.push((key.to_string(), value.to_string()));
        }
        Self(out)
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn required(&self, key: &str) -> &str {
        self.get(key).unwrap_or_else(|| {
            eprintln!("missing required flag --{key}=...");
            std::process::exit(2);
        })
    }

    fn u64_or(&self, key: &str, default: u64) -> u64 {
        self.get(key)
            .map(|v| v.parse().expect("numeric flag"))
            .unwrap_or(default)
    }

    fn usize_or(&self, key: &str, default: usize) -> usize {
        self.get(key)
            .map(|v| v.parse().expect("numeric flag"))
            .unwrap_or(default)
    }

    fn usize_list(&self, key: &str, default: &[usize]) -> Vec<usize> {
        match self.get(key) {
            None => default.to_vec(),
            Some(raw) => raw
                .split(',')
                .map(|part| part.trim().parse().expect("numeric list flag"))
                .collect(),
        }
    }
}

// ----------------------------------------------------------------- stats --

/// Order statistics over raw per-sample nanoseconds. Median interpolates
/// nothing (UPPER-median for even n — `samples[count / 2]`; samples are
/// plentiful enough here that the distinction is noise). B1's bash-side
/// stats use the LOWER median over N=12 at 1 ms granularity — the
/// campaign report carries that convention caveat (review A3); p95 takes
/// the ceil-indexed order statistic.
struct Stats {
    count: usize,
    min_ns: u128,
    median_ns: u128,
    p95_ns: u128,
    max_ns: u128,
}

impl Stats {
    fn from(mut samples: Vec<u128>) -> Self {
        assert!(!samples.is_empty(), "no samples");
        samples.sort_unstable();
        let count = samples.len();
        let p95_idx = ((count as f64) * 0.95).ceil() as usize - 1;
        Self {
            count,
            min_ns: samples[0],
            median_ns: samples[count / 2],
            p95_ns: samples[p95_idx.min(count - 1)],
            max_ns: samples[count - 1],
        }
    }

    fn ms(ns: u128) -> f64 {
        ns as f64 / 1_000_000.0
    }

    fn json(&self) -> String {
        format!(
            "\"samples\":{},\"median_ms\":{:.4},\"p95_ms\":{:.4},\"min_ms\":{:.4},\"max_ms\":{:.4}",
            self.count,
            Self::ms(self.median_ns),
            Self::ms(self.p95_ns),
            Self::ms(self.min_ns),
            Self::ms(self.max_ns),
        )
    }
}

fn time<T>(op: impl FnOnce() -> T) -> (T, u128) {
    let start = Instant::now();
    let value = op();
    (value, start.elapsed().as_nanos())
}

/// Dump the raw per-sample series (chronological, ms, 3 decimals) so the
/// committed logs carry the full distribution, not just order statistics.
fn raw_line(phase: &str, samples: &[u128]) {
    let joined: Vec<String> = samples
        .iter()
        .map(|ns| format!("{:.3}", Stats::ms(*ns)))
        .collect();
    println!("RAW {phase} ms=[{}]", joined.join(","));
}

// ---------------------------------------------------------------- reseal --

/// EQ2 — per-trap cost as the capsule grows. Requires a running brokerd
/// (memory store is the right fixture here: the store read is a HashMap
/// hit, so the curve isolates what EQ2 is about — verify + merge + reseal
/// of a capsule with n entries crossing the wire twice per trap).
fn reseal(flags: &Flags) {
    let sock = flags.required("sock").to_string();
    let tenant = TenantId::new(flags.required("tenant").to_string());
    let deployment = DeploymentId::new(flags.required("deployment").to_string());
    let snapshot = flags.u64_or("snapshot", 1);
    let epoch = flags.u64_or("epoch", 1);
    let sizes = flags.usize_list("sizes", &[1, 10, 50, 100, 200, 500, 1000]);
    let samples = flags.usize_or("samples", 300);
    let warmup = flags.usize_or("warmup", 30);

    println!("# reseal: sizes={sizes:?} samples={samples} warmup={warmup}");
    for &n in &sizes {
        assert!(n >= 1, "capsule size must be >= 1");
        let client = UdsCapsuleBrokerClient::new(&sock);
        let context = SealContext::new(format!("bench-reseal-{n}"), epoch);
        let mut capsule = client
            .initial_capsule(
                &context,
                tenant.clone(),
                deployment.clone(),
                snapshot,
                Vec::new(),
            )
            .expect("initial capsule");

        // Climb to n entries, one hydrate per distinct key — the
        // "read-set built one trap at a time" cumulative cost.
        let climb_started = Instant::now();
        for i in 0..n {
            let key = DocumentId::new(format!("items/k{i:06}"));
            capsule = client
                .hydrate_point(&context, capsule, key)
                .expect("grow hydrate");
        }
        let climb_ns = climb_started.elapsed().as_nanos();
        assert_eq!(capsule.capsule().docs.len(), n, "capsule holds n entries");

        // Wire size of one trap request at this capsule size (the capsule
        // crosses the wire in BOTH directions of every hydrate).
        let probe_key = DocumentId::new("items/k000000");
        let request_bytes = serde_json::to_vec(&IpcRequest::HydratePoint {
            context: context.clone(),
            session: client.session(),
            capsule: capsule.clone(),
            key: probe_key.clone(),
        })
        .expect("serialize probe")
        .len();

        // Steady-state samples: re-hydrating a key already in the capsule
        // keeps `docs` at exactly n (BTreeMap insert), so every sample is
        // verify(n) + store read + merge + reseal(n) at constant size.
        for _ in 0..warmup {
            capsule = client
                .hydrate_point(&context, capsule, probe_key.clone())
                .expect("warmup hydrate");
        }
        let mut ns = Vec::with_capacity(samples);
        for _ in 0..samples {
            let (result, elapsed) =
                time(|| client.hydrate_point(&context, capsule.clone(), probe_key.clone()));
            capsule = result.expect("sampled hydrate");
            ns.push(elapsed);
        }
        assert_eq!(capsule.capsule().docs.len(), n, "size stayed constant");
        raw_line(&format!("reseal-n{n}"), &ns);
        let stats = Stats::from(ns);
        client.abort().expect("close session");

        println!(
            "RESULT {{\"phase\":\"reseal\",\"n\":{n},\"request_bytes\":{request_bytes},\
             \"climb_total_ms\":{:.3},{}}}",
            Stats::ms(climb_ns),
            stats.json()
        );
        println!(
            "  n={n:5}  per-trap median {:.3} ms  p95 {:.3} ms  (climb to n: {:.1} ms, request {request_bytes} B)",
            Stats::ms(stats.median_ns),
            Stats::ms(stats.p95_ns),
            Stats::ms(climb_ns),
        );
    }
}

// ----------------------------------------------------------------- fence --

fn connect_plane(flags: &Flags) -> WritePlane {
    WritePlane::connect(WritePlaneConfig {
        url: flags.required("db-url").to_string(),
        ..WritePlaneConfig::default()
    })
    .expect("connect write plane")
}

fn bench_doc(n: i64) -> Document {
    let mut doc = Document::new();
    doc.insert("value".to_string(), Value::Int(n));
    doc
}

/// One blind fence commit (no validations), returning the committed ts.
fn blind_commit(
    plane: &WritePlane,
    tenant: &str,
    deployment: &str,
    epoch: u64,
    snapshot: u64,
    key: &str,
) -> u64 {
    let writes = [(DocumentId::new(key), Some(bench_doc(1)))];
    match plane
        .commit(&FenceInput {
            tenant,
            deployment,
            committer_epoch: epoch,
            context_epoch: epoch,
            snapshot,
            read_points: &[],
            read_windows: &[],
            writes: &writes,
        })
        .expect("blind commit")
    {
        CommitOutcome::Committed { ts } => ts,
        other => panic!("blind commit did not land: {other:?}"),
    }
}

/// EQ3 — the fence in isolation against real Postgres. Fresh
/// (tenant, deployment) namespace per run (the write_plane_it isolation
/// convention); single serial committer, as the lease design mandates.
fn fence(flags: &Flags) {
    let tenant = flags.required("tenant").to_string();
    let deployment = flags.required("deployment").to_string();
    let blind_n = flags.usize_or("blind", 1500);
    let point_ps = flags.usize_list("points", &[1, 10, 50, 200]);
    let window_ws = flags.usize_list("windows", &[1, 10, 50]);
    let window_log = flags.usize_or("window-log", 1000);
    let val_samples = flags.usize_or("val-samples", 200);
    let conflict_n = flags.usize_or("conflicts", 100);

    let plane = connect_plane(flags);
    plane.ensure_schema().expect("ensure schema");
    let epoch = plane
        .acquire_lease(&tenant, &deployment, "bench-fence")
        .expect("acquire lease");
    println!("# fence: tenant={tenant} deployment={deployment} epoch={epoch}");

    // (a) Blind writes: warmup, then latency samples + sustained rate over
    // the same window. Snapshot rides the previous committed ts, so the
    // conflict window (s, h] is empty and the cost is pure fence + append.
    let mut snapshot = 0_u64;
    for i in 0..100 {
        snapshot = blind_commit(&plane, &tenant, &deployment, epoch, snapshot, &format!("warm/{i:06}"));
    }
    let mut ns = Vec::with_capacity(blind_n);
    let wall = Instant::now();
    for i in 0..blind_n {
        let key = format!("blind/{i:06}");
        let (ts, elapsed) = time(|| blind_commit(&plane, &tenant, &deployment, epoch, snapshot, &key));
        snapshot = ts;
        ns.push(elapsed);
    }
    let wall_s = wall.elapsed().as_secs_f64();
    raw_line("fence-blind", &ns);
    let stats = Stats::from(ns);
    let rate = blind_n as f64 / wall_s;
    println!(
        "RESULT {{\"phase\":\"fence-blind\",\"writes_per_commit\":1,{},\"wall_s\":{:.3},\"commits_per_s\":{:.1}}}",
        stats.json(),
        wall_s,
        rate
    );
    println!(
        "  blind commit: median {:.3} ms  p95 {:.3} ms  sustained {:.0} commits/s (N={blind_n})",
        Stats::ms(stats.median_ns),
        Stats::ms(stats.p95_ns),
        rate
    );

    // (b) p point-read validations per commit. The p declared keys exist
    // in the log below the snapshot; snapshot rides the previous commit,
    // so the window is empty and the sample isolates the validation
    // machinery itself (array bind + per-key index probes).
    for &p in &point_ps {
        let seed_writes: Vec<(DocumentId, Option<Document>)> = (0..p)
            .map(|i| (DocumentId::new(format!("pt{p}/{i:06}")), Some(bench_doc(1))))
            .collect();
        let seeded = plane
            .commit(&FenceInput {
                tenant: &tenant,
                deployment: &deployment,
                committer_epoch: epoch,
                context_epoch: epoch,
                snapshot,
                read_points: &[],
                read_windows: &[],
                writes: &seed_writes,
            })
            .expect("seed point keys");
        let CommitOutcome::Committed { ts } = seeded else {
            panic!("point-key seed did not land: {seeded:?}");
        };
        snapshot = ts;
        let declared: Vec<DocumentId> = (0..p)
            .map(|i| DocumentId::new(format!("pt{p}/{i:06}")))
            .collect();
        for i in 0..30 {
            let writes = [(
                DocumentId::new(format!("ptw{p}/warm{i:06}")),
                Some(bench_doc(1)),
            )];
            let outcome = plane
                .commit(&FenceInput {
                    tenant: &tenant,
                    deployment: &deployment,
                    committer_epoch: epoch,
                    context_epoch: epoch,
                    snapshot,
                    read_points: &declared,
                    read_windows: &[],
                    writes: &writes,
                })
                .expect("warmup point commit");
            let CommitOutcome::Committed { ts } = outcome else {
                panic!("warmup point commit: {outcome:?}");
            };
            snapshot = ts;
        }
        let mut ns = Vec::with_capacity(val_samples);
        for i in 0..val_samples {
            let writes = [(
                DocumentId::new(format!("ptw{p}/{i:06}")),
                Some(bench_doc(1)),
            )];
            let (outcome, elapsed) = time(|| {
                plane
                    .commit(&FenceInput {
                        tenant: &tenant,
                        deployment: &deployment,
                        committer_epoch: epoch,
                        context_epoch: epoch,
                        snapshot,
                        read_points: &declared,
                        read_windows: &[],
                        writes: &writes,
                    })
                    .expect("point commit")
            });
            let CommitOutcome::Committed { ts } = outcome else {
                panic!("point commit did not land: {outcome:?}");
            };
            snapshot = ts;
            ns.push(elapsed);
        }
        raw_line(&format!("fence-points-p{p}"), &ns);
        let stats = Stats::from(ns);
        println!(
            "RESULT {{\"phase\":\"fence-points\",\"p\":{p},{}}}",
            stats.json()
        );
        println!(
            "  p={p:4} point validations: median {:.3} ms  p95 {:.3} ms",
            Stats::ms(stats.median_ns),
            Stats::ms(stats.p95_ns)
        );
    }

    // (c) w window validations with ~window_log events inside (s, h].
    // The fence's window scan is ONE DISTINCT-key query over (s, h] plus
    // Rust-side interval matching, so the log population dominates and w
    // scales only the match loop. Windows are prefixes no logged key
    // matches, so every commit admits. Each sample appends one event, so
    // the window drifts from ~window_log to ~window_log + val_samples —
    // reported as-is.
    let window_snapshot = snapshot;
    for i in 0..window_log {
        snapshot = blind_commit(
            &plane,
            &tenant,
            &deployment,
            epoch,
            snapshot,
            &format!("seedwin/{i:06}"),
        );
    }
    for &w in &window_ws {
        let windows: Vec<ObservedWindow> = (0..w)
            .map(|j| ObservedWindow {
                interval: KeyInterval::Prefix(format!("win-{j:03}/")),
                direction: ScanDirection::Ascending,
                boundary: None,
            })
            .collect();
        let mut ns = Vec::with_capacity(val_samples);
        for i in 0..val_samples {
            let writes = [(
                DocumentId::new(format!("winw{w}/{i:06}")),
                Some(bench_doc(1)),
            )];
            let (outcome, elapsed) = time(|| {
                plane
                    .commit(&FenceInput {
                        tenant: &tenant,
                        deployment: &deployment,
                        committer_epoch: epoch,
                        context_epoch: epoch,
                        snapshot: window_snapshot,
                        read_points: &[],
                        read_windows: &windows,
                        writes: &writes,
                    })
                    .expect("window commit")
            });
            let CommitOutcome::Committed { .. } = outcome else {
                panic!("window commit did not land: {outcome:?}");
            };
            ns.push(elapsed);
        }
        raw_line(&format!("fence-windows-w{w}"), &ns);
        let stats = Stats::from(ns);
        println!(
            "RESULT {{\"phase\":\"fence-windows\",\"w\":{w},\"window_events\":\"~{window_log}+\",{}}}",
            stats.json()
        );
        println!(
            "  w={w:3} window validations over ~{window_log}-event window: median {:.3} ms  p95 {:.3} ms",
            Stats::ms(stats.median_ns),
            Stats::ms(stats.p95_ns)
        );
    }

    // (d) Conflict-abort cost: a blind write lands on the declared key
    // after the snapshot, so the fence finds the conflict in its point
    // scan and answers without appending (transaction rolls back).
    let mut tip = plane.snapshot_ts(&tenant, &deployment).expect("tip");
    let mut ns = Vec::with_capacity(conflict_n);
    for i in 0..conflict_n {
        let key = format!("cfl/{i:06}");
        let s = tip;
        tip = blind_commit(&plane, &tenant, &deployment, epoch, s, &key);
        let declared = [DocumentId::new(key)];
        let writes = [(
            DocumentId::new(format!("cflw/{i:06}")),
            Some(bench_doc(1)),
        )];
        let (outcome, elapsed) = time(|| {
            plane
                .commit(&FenceInput {
                    tenant: &tenant,
                    deployment: &deployment,
                    committer_epoch: epoch,
                    context_epoch: epoch,
                    snapshot: s,
                    read_points: &declared,
                    read_windows: &[],
                    writes: &writes,
                })
                .expect("conflict commit answers")
        });
        assert!(
            matches!(outcome, CommitOutcome::Conflict { .. }),
            "expected conflict, got {outcome:?}"
        );
        ns.push(elapsed);
    }
    raw_line("fence-conflict", &ns);
    let stats = Stats::from(ns);
    println!(
        "RESULT {{\"phase\":\"fence-conflict\",{}}}",
        stats.json()
    );
    println!(
        "  conflict-abort: median {:.3} ms  p95 {:.3} ms (N={conflict_n})",
        Stats::ms(stats.median_ns),
        Stats::ms(stats.p95_ns)
    );
}

// ------------------------------------------------------------ fence-seed --

/// Advance the (tenant, deployment) log tip to `--target-tip` with blind
/// commits. Run BEFORE booting a postgres-mode brokerd whose capsule
/// snapshot is pinned at the Convex fixture ts: capsules claim s =
/// fixture-ts, and the fence's S-SNAPSHOT check requires s <= horizon of
/// the aster log — two timestamp spaces this prototype does not unify
/// (the F9 scope cut; the paper says so). The lease this mode takes is
/// released to the brokerd, which acquires a fresh epoch at boot.
fn fence_seed(flags: &Flags) {
    let tenant = flags.required("tenant").to_string();
    let deployment = flags.required("deployment").to_string();
    let target = flags.u64_or("target-tip", 200);
    let plane = connect_plane(flags);
    plane.ensure_schema().expect("ensure schema");
    let epoch = plane
        .acquire_lease(&tenant, &deployment, "bench-fence-seed")
        .expect("acquire lease");
    let mut tip = plane.snapshot_ts(&tenant, &deployment).expect("tip");
    while tip < target {
        tip = blind_commit(
            &plane,
            &tenant,
            &deployment,
            epoch,
            tip,
            &format!("seed/{tip:06}"),
        );
    }
    println!("# fence-seed: tenant={tenant} deployment={deployment} tip={tip} (epoch {epoch})");
}

// ------------------------------------------------------------------- e2e --

/// EQ3/EQ4 — the full loop, one transaction per iteration:
/// InitialCapsule → V8 executes real JS (one `1.0/get` that traps and
/// hydrates the capsule from the Convex-schema Postgres fixture, one
/// `1.0/insert` that builds the write set in-cell) → Commit verb over UDS
/// → WritePlane fence in Postgres. A fresh V8 isolate per transaction
/// (`V8SandboxCell` reuse only amortizes process-level V8 init).
fn e2e(flags: &Flags) {
    let sock = flags.required("sock").to_string();
    let tenant = TenantId::new(flags.required("tenant").to_string());
    let deployment = DeploymentId::new(flags.required("deployment").to_string());
    let snapshot = flags.u64_or("snapshot", 200);
    let epoch = flags.u64_or("epoch", 1);
    let read_id = flags.required("read-id").to_string();
    let samples = flags.usize_or("samples", 500);
    let warmup = flags.usize_or("warmup", 30);

    let source = format!(
        r#"
        async function main() {{
          const syscall = async (name, args) =>
            JSON.parse(await Convex.asyncSyscall(name, JSON.stringify(args)));
          const doc = await syscall("1.0/get", {{ id: "{read_id}" }});
          await syscall("1.0/insert", {{
            table: "bench",
            value: {{ _id: "bench/out", n: 1 }},
          }});
          return doc === null ? "missing" : "ok";
        }}
    "#
    );

    let client = UdsCapsuleBrokerClient::new(&sock);
    let cell = V8SandboxCell::new(tenant.clone(), deployment.clone(), 64);
    let run_tx = |label: &str| -> (u128, u128) {
        let started = Instant::now();
        let result = cell
            .execute_async_main_with_broker(
                &client,
                "bench-e2e",
                epoch,
                tenant.clone(),
                deployment.clone(),
                snapshot,
                Vec::new(),
                &source,
            )
            .unwrap_or_else(|error| panic!("{label}: cell execution failed: {error}"));
        let exec_ns = started.elapsed().as_nanos();
        assert_eq!(result.traps, 1, "{label}: expected exactly one read trap");
        assert_eq!(
            result.output,
            Value::Text("ok".into()),
            "{label}: fixture read must resolve"
        );
        let capsule = result
            .sealed_capsule
            .unwrap_or_else(|| panic!("{label}: broker path must return the sealed capsule"));
        let commit_started = Instant::now();
        let outcome = client
            .commit(capsule, result.consumed_reads, result.write_set)
            .unwrap_or_else(|error| panic!("{label}: commit failed: {error}"));
        let commit_ns = commit_started.elapsed().as_nanos();
        assert!(
            matches!(outcome, WireCommitOutcome::Committed { .. }),
            "{label}: expected Committed, got {outcome:?}"
        );
        (exec_ns, commit_ns)
    };

    for i in 0..warmup {
        run_tx(&format!("warmup {i}"));
    }
    let mut exec = Vec::with_capacity(samples);
    let mut commit = Vec::with_capacity(samples);
    let mut total = Vec::with_capacity(samples);
    let wall = Instant::now();
    for i in 0..samples {
        let (exec_ns, commit_ns) = run_tx(&format!("sample {i}"));
        exec.push(exec_ns);
        commit.push(commit_ns);
        total.push(exec_ns + commit_ns);
    }
    let wall_s = wall.elapsed().as_secs_f64();
    let rate = samples as f64 / wall_s;
    raw_line("e2e-exec", &exec);
    raw_line("e2e-commit", &commit);
    let exec_stats = Stats::from(exec);
    let commit_stats = Stats::from(commit);
    let total_stats = Stats::from(total);
    println!(
        "RESULT {{\"phase\":\"e2e-total\",{},\"wall_s\":{:.3},\"tx_per_s\":{:.1}}}",
        total_stats.json(),
        wall_s,
        rate
    );
    println!("RESULT {{\"phase\":\"e2e-exec\",{}}}", exec_stats.json());
    println!("RESULT {{\"phase\":\"e2e-commit\",{}}}", commit_stats.json());
    println!(
        "  e2e tx: median {:.3} ms (exec {:.3} + commit {:.3})  p95 {:.3} ms  sustained {:.1} tx/s (N={samples})",
        Stats::ms(total_stats.median_ns),
        Stats::ms(exec_stats.median_ns),
        Stats::ms(commit_stats.median_ns),
        Stats::ms(total_stats.p95_ns),
        rate
    );
}
