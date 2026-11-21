#![cfg(feature = "p2p")]

//! How long a small write, and a small document's trip to another node, take
//! on a node that is also taking in documents with large blobs.
//!
//! Each process runs one node, named `SWL_NAME`, and meets the others through
//! address files in `SWL_DIR`. Every node subscribes to the small collection
//! `Note` and holds every peer named in `SWL_PEERS` as a replicator of it, so
//! a note travels by gossip and by push, as an application that wants every
//! peer to hold every small document does. A node with `SWL_CHUNKS_TO` set
//! writes `Chunk` documents (a 120 KiB blob each by default) at
//! `SWL_CHUNKS_PER_SEC` and replicates them, through a replication filter, to
//! that node only. A node with `SWL_NOTE_EVERY_MS` set writes a note that
//! often and times each write; every node times each note's arrival from
//! another node against the `at` the note carries.
//!
//! A write validator asks the node two small queries before each write, as an
//! application's rule does, and a merge validator reads named small fields of
//! every arriving document.
//!
//! Every `SWL_REPORT_SECS` the node prints one `report` line of JSON: its
//! store's size, the chunks it has written or holds, and the distribution of
//! its note writes' times and of notes' arrival times over the interval.
//!
//! ```text
//! SWL_NAME=receiver SWL_PEERS=writer0,watcher SWL_NOTE_EVERY_MS=500 \
//!   cargo test --profile profile -p defra-node --features p2p \
//!   --test small_writes_under_load -- --ignored --nocapture
//! ```

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "support/clock.rs"]
mod clock;
#[path = "support/outcome.rs"]
mod outcome;

use async_trait::async_trait;
use clock::sleep;
use db::merge::governance::{FieldValue, MergeCandidate, MergeValidator, MergeVerdict, MergeView};
use defra_node::{AccessHooks, Node, NodeBuilder, P2PConfig, PeerKeySource, SyncTuning};
use outcome::Result;

const SDL: &str = "
type Chunk {
    to: String @index @immutable
    confirmation: String @index @immutable
    seq: Int @immutable
    bytes: Blob @immutable
}

type Note {
    at: Int @immutable
    author: String @immutable
    seq: Int @immutable
    body: String @immutable
}

type Member {
    subject: String @index @immutable
    role: String @immutable
}
";

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn list(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// The sync allowances an application that replicates large blobs raises.
fn tuning() -> SyncTuning {
    let defaults = SyncTuning::default();
    SyncTuning {
        rate_limit_rate: env("SWL_RATE", 5000.0),
        rate_limit_burst: env("SWL_BURST", 5000),
        max_concurrent_dag_fetches: env("SWL_DAG_FETCHES", 32),
        max_concurrent_push_tasks: env("SWL_PUSH_TASKS", 32),
        ..defaults
    }
}

/// The server profile with compression off, which suits blobs of noise, and
/// each engine option an `SWL_OPT_*` variable names set over it.
fn store_options(stats: &Arc<regolith::Statistics>) -> storage::RegolithStoreOptions {
    let mut options = match env("SWL_PROFILE", "server".to_string()).as_str() {
        "embedded" => storage::RegolithStoreOptions::embedded(),
        _ => storage::RegolithStoreOptions::default(),
    };
    let mut engine = options
        .engine
        .clone()
        .statistics(Some(Arc::clone(stats)))
        .compression(regolith::CompressionType::None);
    macro_rules! option {
        ($name:ident, $variable:literal) => {
            if let Ok(value) = std::env::var($variable) {
                engine = engine.$name(value.parse().expect($variable));
            }
        };
    }
    option!(write_buffer_size, "SWL_OPT_WRITE_BUFFER");
    option!(max_write_buffer_number, "SWL_OPT_WRITE_BUFFERS");
    option!(block_cache_size, "SWL_OPT_BLOCK_CACHE");
    option!(l0_compaction_trigger, "SWL_OPT_L0_TRIGGER");
    option!(level0_slowdown_writes_trigger, "SWL_OPT_L0_SLOWDOWN");
    option!(level0_stop_writes_trigger, "SWL_OPT_L0_STOP");
    option!(level_base_bytes, "SWL_OPT_LEVEL_BASE");
    option!(target_file_size, "SWL_OPT_TARGET_FILE");
    option!(max_background_compactions, "SWL_OPT_BACKGROUND_COMPACTIONS");
    options.engine = engine;
    match env("SWL_DURABILITY", "immediate".to_string()).as_str() {
        "eventual" => options.with_durability(storage::backends::DurabilityMode::Eventual),
        _ => options,
    }
}

/// The times of one kind of event over a report's interval, in ms.
#[derive(Default)]
struct Times {
    samples: Mutex<Vec<f64>>,
    failures: AtomicUsize,
}

impl Times {
    fn add(&self, ms: f64) {
        self.samples.lock().unwrap().push(ms);
    }

    /// The interval's count, median, 99th percentile and worst, emptied.
    fn take(&self) -> serde_json::Value {
        let mut samples = std::mem::take(&mut *self.samples.lock().unwrap());
        samples.sort_by(f64::total_cmp);
        let at = |q: f64| -> f64 {
            if samples.is_empty() {
                return 0.0;
            }
            let index = ((samples.len() as f64 - 1.0) * q).round() as usize;
            samples[index]
        };
        serde_json::json!({
            "n": samples.len(),
            "p50": at(0.5),
            "p99": at(0.99),
            "max": samples.last().copied().unwrap_or(0.0),
            "failed": self.failures.swap(0, Ordering::Relaxed),
        })
    }
}

/// Reads the arriving document's small fields, as an application's merge
/// validator reads what its rule needs, and accepts it.
struct ReadsSmallFields;

#[async_trait]
impl MergeValidator for ReadsSmallFields {
    async fn validate(
        &self,
        candidate: &MergeCandidate<'_>,
        view: &dyn MergeView,
    ) -> std::result::Result<MergeVerdict, String> {
        let names: &[&str] = match candidate.collection.name.as_str() {
            "Chunk" => &["to", "confirmation", "seq"],
            "Note" => &["at", "author", "seq"],
            _ => &["subject", "role"],
        };
        let fields = view
            .composite_fields_named(candidate.cid, names)
            .await?
            .into_iter()
            .flatten();
        let held = fields
            .filter(|(_, value)| matches!(value, FieldValue::Value(_)))
            .count();
        Ok(match held {
            0 => MergeVerdict::reject("no small field held"),
            _ => MergeVerdict::Accept,
        })
    }
}

/// Before each write, asks the node for one document by ID and for the
/// members with a subject, as an application's rule looks up what a new
/// document refers to.
struct AsksTwoQueries {
    node: std::sync::OnceLock<std::sync::Weak<Node>>,
    last: Mutex<Option<String>>,
    nanos: AtomicU64,
    calls: AtomicU64,
}

#[async_trait]
impl query::access_hooks::WriteValidator for AsksTwoQueries {
    async fn validate_write(
        &self,
        request: &query::access_hooks::WriteRequest<'_>,
    ) -> std::result::Result<(), String> {
        let began = Instant::now();
        let Some(node) = self.node.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(());
        };
        let last = self.last.lock().unwrap().clone();
        if let Some(doc_id) = last {
            let query = match request.collection.name.as_str() {
                "Chunk" => format!("query {{ Chunk(docID: \"{doc_id}\") {{ seq }} }}"),
                _ => format!("query {{ Note(docID: \"{doc_id}\") {{ seq }} }}"),
            };
            let response = node.execute(&query).await;
            if response.has_errors() {
                return Err(format!("{:?}", response.errors));
            }
        }
        let response = node
            .execute("query { Member(filter: {subject: {_eq: \"m1\"}}) { role } }")
            .await;
        if response.has_errors() {
            return Err(format!("{:?}", response.errors));
        }
        self.nanos
            .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct Started {
    node: Arc<Node>,
    stats: Arc<regolith::Statistics>,
    rule: Arc<AsksTwoQueries>,
}

async fn start(dir: &Path) -> Result<Started> {
    // Timers read the clock on every call, so only `SWL_STATS_TIMERS` turns
    // them on; they time each WAL sync.
    let stats = Arc::new(regolith::Statistics::with_level(
        match std::env::var("SWL_STATS_TIMERS") {
            Ok(_) => regolith::StatsLevel::All,
            Err(_) => regolith::StatsLevel::ExceptHistogramOrTimers,
        },
    ));
    let rule = Arc::new(AsksTwoQueries {
        node: Default::default(),
        last: Default::default(),
        nanos: Default::default(),
        calls: Default::default(),
    });
    let mut hooks = AccessHooks::new(["Chunk", "Note", "Member"])
        .with_merge_validator(Arc::new(ReadsSmallFields));
    if env("SWL_WRITE_VALIDATOR", 1) == 1 {
        hooks = hooks.with_write_validator(rule.clone());
    }
    let mut builder = NodeBuilder::default()
        .with_access_hooks(hooks)
        .with_p2p(P2PConfig {
            port: Some(0),
            peer_key: PeerKeySource::Store,
            bind_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            relay_mode: p2p::iroh::IrohRelayModeConfig::Disabled,
            discovery: p2p::iroh::IrohDiscoveryConfig::Disabled,
            sync: tuning(),
            ..Default::default()
        })
        .with_regolith_options(store_options(&stats))
        .data_path(dir.to_path_buf());
    let workers: usize = env("SWL_WORKERS", 2);
    if workers > 0 {
        builder = builder.with_workers(workers::Config {
            workers: workers::Workers::Count(workers),
            placement: workers::Placement::Unpinned,
            slow_task_after: Duration::from_millis(env("SWL_SLOW_TASK_MS", 1000)),
            ..Default::default()
        });
    }
    let node = Arc::new(builder.build().await?);
    let _ = rule.node.set(Arc::downgrade(&node));
    if node.get_collection("Note")?.is_none() {
        node.add_collection(SDL).await?;
        for (subject, role) in [("m1", "observer"), ("m2", "station"), ("m3", "operator")] {
            let response = node
                .execute(&format!(
                    "mutation {{ add_Member(input: {{subject: \"{subject}\", role: \"{role}\"}}) {{ _docID }} }}"
                ))
                .await;
            assert!(!response.has_errors(), "{:?}", response.errors);
        }
    }
    Ok(Started { node, stats, rule })
}

async fn listen_addr(node: &Node) -> Result<String> {
    let ops = node.p2p_system().expect("p2p").ops();
    loop {
        let addrs = ops.listen_addresses().await?;
        if let Some(addr) = addrs.into_iter().find(|addr| addr.contains("/p2p/")) {
            return Ok(addr);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn address_of(dir: &Path, name: &str) -> String {
    let file = dir.join(format!("{name}.addr"));
    loop {
        if let Ok(addr) = std::fs::read_to_string(&file) {
            if !addr.is_empty() {
                return addr;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn disk_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => total += disk_bytes(&entry.path()),
            Ok(metadata) => total += metadata.len(),
            Err(_) => {}
        }
    }
    total
}

fn resident_mib() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse().ok())
        .unwrap_or(0.0);
    pages * 4096.0 / 1_048_576.0
}

/// CPU seconds of the process's threads whose names start with `prefix`.
fn thread_cpu_seconds(prefix: &str) -> f64 {
    let ticks: u64 = std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|name| name.starts_with(prefix))
        })
        .filter_map(|task| std::fs::read_to_string(task.path().join("stat")).ok())
        .filter_map(|stat| {
            let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
        })
        .sum();
    ticks as f64 / 100.0
}

/// Writes chunks for `to` at `per_second`, each in its own mutation.
async fn write_chunks(
    node: Arc<Node>,
    name: String,
    to: String,
    per_second: f64,
    written: Arc<AtomicUsize>,
    times: Arc<Times>,
) {
    let blob: usize = env("SWL_CHUNK_BLOB", 122_880);
    let per_recording: usize = env("SWL_CHUNKS_PER_RECORDING", 470);
    let began = Instant::now();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ wall_ms();
    for seq in 0..usize::MAX {
        let due = Duration::from_secs_f64(seq as f64 / per_second);
        if let Some(wait) = due.checked_sub(began.elapsed()) {
            sleep(wait).await;
        }
        let bytes: Vec<u8> = (0..blob)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let confirmation = format!("{name}-{}", seq / per_recording);
        let started = Instant::now();
        let response = node
            .execute(&format!(
                "mutation {{ add_Chunk(input: {{to: \"{to}\", confirmation: \"{confirmation}\", seq: {seq}, bytes: \"{}\"}}) {{ _docID }} }}",
                hex::encode(bytes)
            ))
            .await;
        if response.has_errors() {
            times.failures.fetch_add(1, Ordering::Relaxed);
            eprintln!("chunk write failed: {:?}", response.errors);
            continue;
        }
        times.add(started.elapsed().as_secs_f64() * 1000.0);
        written.fetch_add(1, Ordering::Relaxed);
    }
}

/// Writes a note every `every`, timing each write; with `SWL_NOTE_BURST`
/// set, that many notes one after another every `every` instead, as a node
/// that asks for the next hour's passes all at once does.
async fn write_notes(started: Arc<Started>, name: String, every: Duration, times: Arc<Times>) {
    let began = Instant::now();
    let burst: u64 = env("SWL_NOTE_BURST", 1).max(1);
    for seq in 0..u64::MAX {
        let due = every * (seq / burst) as u32;
        if let Some(wait) = due.checked_sub(began.elapsed()) {
            sleep(wait).await;
        }
        let at = wall_ms();
        let clock = Instant::now();
        let response = started
            .node
            .execute(&format!(
                "mutation {{ add_Note(input: {{at: {at}, author: \"{name}\", seq: {seq}, body: \"a small document of a few hundred bytes, as an offer or a request is\"}}) {{ _docID }} }}"
            ))
            .await;
        let took = clock.elapsed().as_secs_f64() * 1000.0;
        if response.has_errors() {
            times.failures.fetch_add(1, Ordering::Relaxed);
            println!(
                "{}",
                serde_json::json!({"note_write_failed": {"seq": seq, "ms": took, "error": format!("{:?}", response.errors)}})
            );
            continue;
        }
        times.add(took);
        let doc_id = response
            .data
            .as_ref()
            .and_then(|data| data["add_Note"][0]["_docID"].as_str())
            .map(str::to_string);
        *started.rule.last.lock().unwrap() = doc_id;
    }
}

/// Times each note that arrives from another node, from the `at` it carries
/// to the moment the node reports it held.
async fn time_arrivals(node: Arc<Node>, name: String, times: Arc<Times>, held: Arc<AtomicUsize>) {
    let Ok(Some(collection)) = node.get_collection("Note") else {
        return;
    };
    let mut changes = node.subscribe_collection_changes([collection.collection_id.clone()]);
    while let Some(batch) = changes.recv().await {
        let arrived = wall_ms();
        for change in batch
            .changes
            .iter()
            .filter(|change| !change.has_local_write)
        {
            let response = node
                .execute(&format!(
                    "query {{ Note(docID: \"{}\") {{ at author }} }}",
                    change.doc_id
                ))
                .await;
            let Some(note) = response
                .data
                .as_ref()
                .and_then(|data| data["Note"].as_array())
                .and_then(|notes| notes.first().cloned())
            else {
                continue;
            };
            if note["author"].as_str() == Some(name.as_str()) {
                continue;
            }
            if let Some(at) = note["at"].as_u64() {
                times.add(arrived.saturating_sub(at) as f64);
                held.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Counts the chunks that arrive from other nodes.
async fn count_chunks(node: Arc<Node>, arrived: Arc<AtomicUsize>) {
    let Ok(Some(collection)) = node.get_collection("Chunk") else {
        return;
    };
    let mut changes = node.subscribe_collection_changes([collection.collection_id.clone()]);
    while let Some(batch) = changes.recv().await {
        let remote = batch
            .changes
            .iter()
            .filter(|change| !change.has_local_write)
            .count();
        arrived.fetch_add(remote, Ordering::Relaxed);
    }
}

#[test]
#[ignore = "a measurement, run by hand"]
fn small_writes_under_load() -> Result<()> {
    let dir = PathBuf::from(env("SWL_DIR", "/tmp/swl".to_string()));
    let name: String = env("SWL_NAME", "receiver".to_string());
    let peers = list("SWL_PEERS");
    let chunks_to: String = env("SWL_CHUNKS_TO", String::new());
    let chunks_per_sec: f64 = env("SWL_CHUNKS_PER_SEC", 0.0);
    let note_every_ms: u64 = env("SWL_NOTE_EVERY_MS", 0);
    let report_secs: u64 = env("SWL_REPORT_SECS", 10);
    let run_secs: u64 = env("SWL_SECS", 600);
    if let Ok(filter) = std::env::var("SWL_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
    edge::testing::edge()
        .block_on(async move {
            std::fs::create_dir_all(&dir)?;
            let store = dir.join(&name);
            let started = Arc::new(start(&store).await?);
            let node = Arc::clone(&started.node);
            let ops = node.p2p_system().expect("p2p").ops().clone();
            std::fs::write(dir.join(format!("{name}.addr")), listen_addr(&node).await?)?;
            ops.add_collections(vec!["Note".into(), "Member".into()]).await?;
            for peer in &peers {
                let addr = address_of(&dir, peer).await;
                ops.connect_peer(&addr).await?;
                ops.add_replicator(
                    vec!["Note".into(), "Member".into()],
                    Some(&addr),
                    Default::default(),
                    Vec::new(),
                    None,
                )
                .await?;
                if peer == &chunks_to {
                    let mut filters = defra_p2p_adapter::ReplicationFilters::new();
                    filters.insert(
                        "Chunk".to_string(),
                        defra_p2p_adapter::ReplicationFilter::eq(
                            "to",
                            serde_json::Value::String(chunks_to.clone()),
                        ),
                    );
                    ops.add_replicator(
                        vec!["Chunk".into(), "Note".into(), "Member".into()],
                        Some(&addr),
                        filters,
                        Vec::new(),
                        None,
                    )
                    .await?;
                }
            }
            let edge = edge::testing::edge();
            let written = Arc::new(AtomicUsize::new(0));
            let chunk_times = Arc::new(Times::default());
            let note_times = Arc::new(Times::default());
            let arrival_times = Arc::new(Times::default());
            let arrived_notes = Arc::new(AtomicUsize::new(0));
            let arrived_chunks = Arc::new(AtomicUsize::new(0));
            let mut services = Vec::new();
            services.push(edge.spawn_service(time_arrivals(
                Arc::clone(&node),
                name.clone(),
                Arc::clone(&arrival_times),
                Arc::clone(&arrived_notes),
            )));
            services.push(edge.spawn_service(count_chunks(
                Arc::clone(&node),
                Arc::clone(&arrived_chunks),
            )));
            sleep(Duration::from_secs(env("SWL_SETTLE_SECS", 5))).await;
            if !chunks_to.is_empty() && chunks_per_sec > 0.0 {
                services.push(edge.spawn_service(write_chunks(
                    Arc::clone(&node),
                    name.clone(),
                    chunks_to.clone(),
                    chunks_per_sec,
                    Arc::clone(&written),
                    Arc::clone(&chunk_times),
                )));
            }
            if note_every_ms > 0 {
                services.push(edge.spawn_service(write_notes(
                    Arc::clone(&started),
                    name.clone(),
                    Duration::from_millis(note_every_ms),
                    Arc::clone(&note_times),
                )));
            }
            let began = Instant::now();
            let mut tick = 0u64;
            while began.elapsed() < Duration::from_secs(run_secs) {
                tick += report_secs;
                let due = Duration::from_secs(tick);
                if let Some(wait) = due.checked_sub(began.elapsed()) {
                    sleep(wait).await;
                }
                use regolith::Ticker;
                let stats = &started.stats;
                let calls = started.rule.calls.load(Ordering::Relaxed).max(1);
                let sync = stats.get_histogram_snapshot(regolith::Histogram::WalWriteTime);
                println!(
                    "{}",
                    serde_json::json!({
                        "report": name,
                        "t": tick,
                        "store_mib": disk_bytes(&store) as f64 / 1_048_576.0,
                        "rss_mib": resident_mib(),
                        "chunks_written": written.load(Ordering::Relaxed),
                        "chunks_arrived": arrived_chunks.load(Ordering::Relaxed),
                        "notes_arrived": arrived_notes.load(Ordering::Relaxed),
                        "chunk_write_ms": chunk_times.take(),
                        "note_write_ms": note_times.take(),
                        "note_arrival_ms": arrival_times.take(),
                        "rule_ms": started.rule.nanos.load(Ordering::Relaxed) as f64 / 1e6 / calls as f64,
                        "stall_s": stats.get_ticker(Ticker::WriteStallMicros) as f64 / 1e6,
                        "wal_syncs": stats.get_ticker(Ticker::WalSyncCount),
                        "wal_sync_ms": {"n": sync.count, "avg": sync.average() as f64 / 1000.0, "max": sync.max as f64 / 1000.0},
                        "compactions": stats.get_ticker(Ticker::CompactionCount),
                        "compaction_written_mib": stats.get_ticker(Ticker::CompactionBytesWritten) as f64 / 1_048_576.0,
                        "cpu_workers_s": thread_cpu_seconds("defra-worker"),
                        "cpu_compaction_s": thread_cpu_seconds("regolith-compac"),
                        "cpu_edge_s": thread_cpu_seconds("defra-node-edge"),
                    })
                );
            }
            drop(services);
            Ok(())
        })
        .unwrap()
}
