#![cfg(feature = "p2p")]

//! How fast a replicator takes in documents that each hold a large blob.
//!
//! Writers add `IqChunk` documents and push them to one receiver they hold as
//! a replicator. The run prints the receiver's intake per second and the
//! resident memory of the process.
//!
//! ```text
//! INTAKE_DOCS=160 INTAKE_BLOB=122880 INTAKE_WRITERS=1 \
//!   cargo test -p defra-node --features p2p --test blob_replication_intake -- --ignored --nocapture
//! ```
//!
//! `INTAKE_ROLE=receiver` and `INTAKE_ROLE=writers` split the run over two
//! processes that meet through the address file `INTAKE_ADDR_FILE`, so each
//! side has a memory figure and a profile of its own.
//!
//! `INTAKE_PROFILE` (`embedded`, the default, or `server`) and the
//! `INTAKE_OPT_*` variables choose the receiver's store options. Each run ends
//! with the engine's writes against the blob bytes taken in;
//! `INTAKE_REPORT_SECS` prints the store's size and those writes every so many
//! seconds, for a run long enough to see compaction settle.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "support/clock.rs"]
mod clock;
#[path = "support/outcome.rs"]
mod outcome;

use async_trait::async_trait;
use clock::sleep;
use db::merge::governance::{FieldValue, MergeCandidate, MergeValidator, MergeVerdict, MergeView};
use defra_node::{AccessHooks, Node, NodeBuilder, P2PConfig, PeerKeySource, SyncTuning};
use outcome::Result;

const SDL: &str = "type IqChunk { seq: Int @immutable \n bytes: Blob @immutable }";

fn env<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn tuning() -> SyncTuning {
    let defaults = SyncTuning::default();
    SyncTuning {
        rate_limit_rate: env("INTAKE_RATE", defaults.rate_limit_rate),
        rate_limit_burst: env("INTAKE_BURST", defaults.rate_limit_burst),
        max_concurrent_dag_fetches: env("INTAKE_DAG_FETCHES", defaults.max_concurrent_dag_fetches),
        max_concurrent_push_tasks: env("INTAKE_PUSH_TASKS", defaults.max_concurrent_push_tasks),
        max_pending_dags: env("INTAKE_PENDING_DAGS", defaults.max_pending_dags),
        ..defaults
    }
}

/// The store options: the profile `INTAKE_PROFILE` names (`server` or
/// `embedded`), with each engine option an `INTAKE_OPT_*` variable names set
/// over it.
fn store_options(stats: &Arc<regolith::Statistics>) -> storage::RegolithStoreOptions {
    let profile: String = env("INTAKE_PROFILE", "embedded".to_string());
    let mut options = match profile.as_str() {
        "server" => storage::RegolithStoreOptions::default(),
        _ => storage::RegolithStoreOptions::embedded(),
    };
    let mut engine = options.engine.clone().statistics(Some(Arc::clone(stats)));
    macro_rules! option {
        ($name:ident, $variable:literal) => {
            if let Ok(value) = std::env::var($variable) {
                engine = engine.$name(value.parse().expect($variable));
            }
        };
    }
    option!(write_buffer_size, "INTAKE_OPT_WRITE_BUFFER");
    option!(max_write_buffer_number, "INTAKE_OPT_WRITE_BUFFERS");
    option!(block_size, "INTAKE_OPT_BLOCK");
    option!(block_cache_size, "INTAKE_OPT_BLOCK_CACHE");
    option!(l0_compaction_trigger, "INTAKE_OPT_L0_TRIGGER");
    option!(level0_slowdown_writes_trigger, "INTAKE_OPT_L0_SLOWDOWN");
    option!(level0_stop_writes_trigger, "INTAKE_OPT_L0_STOP");
    option!(
        soft_pending_compaction_bytes_limit,
        "INTAKE_OPT_SOFT_PENDING"
    );
    option!(
        hard_pending_compaction_bytes_limit,
        "INTAKE_OPT_HARD_PENDING"
    );
    option!(level_base_bytes, "INTAKE_OPT_LEVEL_BASE");
    option!(target_file_size, "INTAKE_OPT_TARGET_FILE");
    option!(
        max_background_compactions,
        "INTAKE_OPT_BACKGROUND_COMPACTIONS"
    );
    option!(inline_compaction, "INTAKE_OPT_INLINE_COMPACTION");
    option!(max_value_size, "INTAKE_OPT_MAX_VALUE");
    option!(level_size_multiplier, "INTAKE_OPT_LEVEL_MULTIPLIER");
    if let Ok(style) = std::env::var("INTAKE_OPT_COMPACTION") {
        engine = engine.compaction_style(match style.as_str() {
            "universal" => regolith::CompactionStyle::Universal,
            _ => regolith::CompactionStyle::Level,
        });
    }
    if let Ok(codec) = std::env::var("INTAKE_OPT_COMPRESSION") {
        engine = engine.compression(match codec.as_str() {
            "none" => regolith::CompressionType::None,
            "snappy" => regolith::CompressionType::Snappy,
            _ => regolith::CompressionType::Lz4,
        });
    }
    options.engine = engine;
    options
}

async fn start(dir: &Path) -> Result<Arc<Node>> {
    Ok(start_counted(dir, None).await?.0)
}

/// Accepts every chunk once it has read the chunk's `seq` from the arriving
/// composite, as an app's validator reads a small field beside the blob;
/// `named` reads it with `composite_fields_named`, otherwise with
/// `composite_fields`. Counts the time the reads take.
#[derive(Default)]
struct ReadsSeq {
    named: bool,
    nanos: AtomicU64,
    reads: AtomicUsize,
}

#[async_trait]
impl MergeValidator for ReadsSeq {
    async fn validate(
        &self,
        candidate: &MergeCandidate<'_>,
        view: &dyn MergeView,
    ) -> std::result::Result<MergeVerdict, String> {
        let began = Instant::now();
        let fields = match self.named {
            true => view.composite_fields_named(candidate.cid, &["seq"]).await?,
            false => view.composite_fields(candidate.cid).await?,
        };
        self.nanos
            .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.reads.fetch_add(1, Ordering::Relaxed);
        let has_seq = fields
            .into_iter()
            .flatten()
            .any(|(name, value)| name == "seq" && matches!(value, FieldValue::Value(_)));
        Ok(match has_seq {
            true => MergeVerdict::Accept,
            false => MergeVerdict::reject("a chunk without its seq"),
        })
    }
}

impl ReadsSeq {
    /// The validator `INTAKE_VALIDATOR` names: `fields`, `named`, or none.
    fn chosen() -> Option<Arc<Self>> {
        match std::env::var("INTAKE_VALIDATOR").as_deref() {
            Ok("fields") => Some(Arc::new(Self::default())),
            Ok("named") => Some(Arc::new(Self {
                named: true,
                ..Self::default()
            })),
            _ => None,
        }
    }

    fn print(&self) {
        let reads = self.reads.load(Ordering::Relaxed);
        println!(
            "validator ({}): {reads} reads, {:.3} ms a read",
            if self.named {
                "composite_fields_named"
            } else {
                "composite_fields"
            },
            self.nanos.load(Ordering::Relaxed) as f64 / 1e6 / reads.max(1) as f64
        );
    }
}

/// A node, and the counters of its store's engine.
async fn start_counted(
    dir: &Path,
    validator: Option<Arc<ReadsSeq>>,
) -> Result<(Arc<Node>, Arc<regolith::Statistics>)> {
    // Timers (write stalls among them) read the clock on every call, so
    // only `INTAKE_STATS_TIMERS` turns them on.
    let stats = Arc::new(regolith::Statistics::with_level(
        match std::env::var("INTAKE_STATS_TIMERS") {
            Ok(_) => regolith::StatsLevel::All,
            Err(_) => regolith::StatsLevel::ExceptHistogramOrTimers,
        },
    ));
    let mut builder = NodeBuilder::default();
    if let Some(validator) = validator {
        builder = builder
            .with_access_hooks(AccessHooks::new(["IqChunk"]).with_merge_validator(validator));
    }
    let node = builder
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
        .data_path(dir.to_path_buf())
        .build()
        .await?;
    node.add_collection(SDL).await?;
    Ok((Arc::new(node), stats))
}

/// What the store's engine wrote and read, against `payload` bytes of blobs
/// taken in: the write amplification of the WAL, flushes and compaction.
fn print_engine(stats: &regolith::Statistics, payload: f64) {
    use regolith::Ticker;
    let mib = |ticker| stats.get_ticker(ticker) as f64 / 1_048_576.0;
    let payload = payload / 1_048_576.0;
    let written = mib(Ticker::WalBytesWritten)
        + mib(Ticker::FlushBytesWritten)
        + mib(Ticker::CompactionBytesWritten);
    println!(
        "engine: payload {payload:.1} MiB; user writes {:.1} MiB; wal {:.1} MiB; flush {:.1} MiB in {}; compaction read {:.1} MiB written {:.1} MiB in {}; device writes {:.1}x payload; reads {:.1} MiB, block cache misses {}; write stalls {:.1}s",
        mib(Ticker::BytesWritten),
        mib(Ticker::WalBytesWritten),
        mib(Ticker::FlushBytesWritten),
        stats.get_ticker(Ticker::FlushCount),
        mib(Ticker::CompactionBytesRead),
        mib(Ticker::CompactionBytesWritten),
        stats.get_ticker(Ticker::CompactionCount),
        written / payload.max(f64::MIN_POSITIVE),
        mib(Ticker::BytesRead),
        stats.get_ticker(Ticker::BlockCacheMiss),
        stats.get_ticker(Ticker::WriteStallMicros) as f64 / 1e6,
    );
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

async fn held(node: &Node) -> usize {
    let response = node.execute("query { IqChunk { seq } }").await;
    if response.has_errors() {
        panic!("count failed: {:?}", response.errors);
    }
    response
        .data
        .and_then(|data| data["IqChunk"].as_array().map(Vec::len))
        .unwrap_or(0)
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

/// The size of the files under `dir`, and when `INTAKE_DISK_DETAIL` is set a
/// line of that size by file extension.
fn disk_mib(dir: &Path) -> f64 {
    let by_kind = disk_by_kind(dir);
    if std::env::var("INTAKE_DISK_DETAIL").is_ok() {
        let detail: Vec<String> = by_kind
            .iter()
            .map(|(kind, bytes)| format!("{kind} {:.1} MiB", *bytes as f64 / 1_048_576.0))
            .collect();
        println!("store files: {}", detail.join(", "));
    }
    by_kind.values().sum::<u64>() as f64 / 1_048_576.0
}

/// The bytes of the files under `dir`, by file extension.
fn disk_by_kind(dir: &Path) -> std::collections::BTreeMap<String, u64> {
    fn sizes(dir: &Path, by_kind: &mut std::collections::BTreeMap<String, u64>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => sizes(&entry.path(), by_kind),
                Ok(metadata) => {
                    let kind = entry
                        .path()
                        .extension()
                        .map(|kind| kind.to_string_lossy().into_owned())
                        .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
                    *by_kind.entry(kind).or_default() += metadata.len();
                }
                Err(_) => {}
            }
        }
    }
    let mut by_kind = std::collections::BTreeMap::new();
    sizes(dir, &mut by_kind);
    by_kind
}

/// Prints the bytes of the values under each key family of the store in
/// `dir`, which no node has open.
async fn print_key_families(dir: &Path) -> Result<()> {
    use storage::corekv::{IterOptions, Store};

    fn store_dir(dir: &Path) -> Option<std::path::PathBuf> {
        let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        if entries.iter().any(|entry| entry.file_name() == "LOCK") {
            return Some(dir.to_path_buf());
        }
        entries.iter().find_map(|entry| store_dir(&entry.path()))
    }
    let store = storage::RegolithStore::open(store_dir(dir).expect("a store"))?;
    let txn = store.new_txn(true).await?;
    let mut iter = txn.iterator(IterOptions::new()).await?;
    let mut families = std::collections::BTreeMap::<String, (u64, u64)>::new();
    while let Some(pair) = iter.next().await? {
        let key = String::from_utf8_lossy(&pair.key);
        let family: String = match key.strip_prefix('/') {
            Some(path) => path.split('/').take(2).collect::<Vec<_>>().join("/"),
            None => key.chars().take(1).collect(),
        };
        let family: String = family.chars().take(24).collect();
        if families
            .get(&family)
            .is_none_or(|seen: &(u64, u64)| seen.0 < 8)
            || std::env::var("INTAKE_KEY_ALL").is_ok()
        {
            println!(
                "key {:?} value {} B",
                pair.key.escape_ascii().to_string(),
                pair.value.len()
            );
        }
        let entry = families.entry(family).or_default();
        entry.0 += 1;
        entry.1 += pair.value.len() as u64;
    }
    iter.close().await?;
    for (family, (keys, bytes)) in families {
        println!(
            "keys {family:<26} {keys:>7} values {:>8.1} MiB",
            bytes as f64 / 1_048_576.0
        );
    }
    Ok(())
}

async fn write(
    writer: Arc<Node>,
    index: usize,
    docs: usize,
    blob: usize,
    per_second: f64,
) -> Vec<String> {
    let mut written = Vec::with_capacity(docs);
    let began = Instant::now();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ index as u64;
    let mut slowest = Duration::ZERO;
    for seq in 0..docs {
        if per_second > 0.0 {
            let due = Duration::from_secs_f64(seq as f64 / per_second);
            if let Some(wait) = due.checked_sub(began.elapsed()) {
                sleep(wait).await;
            }
        }
        let bytes: Vec<u8> = (0..blob)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let started = Instant::now();
        let response = writer
            .execute(&format!(
                "mutation {{ add_IqChunk(input: {{seq: {}, bytes: \"{}\"}}) {{ _docID }} }}",
                index * 1_000_000 + seq,
                hex::encode(bytes)
            ))
            .await;
        slowest = slowest.max(started.elapsed());
        if response.has_errors() {
            panic!("write failed: {:?}", response.errors);
        }
        written.extend(
            response
                .data
                .as_ref()
                .and_then(|data| data["add_IqChunk"][0]["_docID"].as_str())
                .map(str::to_string),
        );
    }
    println!(
        "writer {index}: wrote {docs} in {:.1}s, slowest write {:.0} ms",
        began.elapsed().as_secs_f64(),
        slowest.as_secs_f64() * 1000.0
    );
    written
}

async fn start_writers(dir: &Path, receiver: &str, count: usize) -> Result<Vec<Arc<Node>>> {
    let mut writers = Vec::new();
    for index in 0..count {
        let writer = start(&dir.join(format!("a{index}"))).await?;
        let ops = writer.p2p_system().expect("p2p").ops().clone();
        ops.connect_peer(receiver).await?;
        ops.add_replicator(
            vec!["IqChunk".into()],
            Some(receiver),
            Default::default(),
            Vec::new(),
            None,
        )
        .await?;
        writers.push(writer);
    }
    sleep(Duration::from_secs(1)).await;
    Ok(writers)
}

async fn run_writers(
    writers: &[Arc<Node>],
    docs: usize,
    blob: usize,
    per_second: f64,
) -> Vec<String> {
    let edge = edge::testing::edge();
    let tasks: Vec<_> = writers
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, writer)| {
            edge.spawn(write(writer, index, docs, blob, per_second))
                .within(Duration::MAX)
        })
        .collect();
    let mut written = Vec::new();
    for task in tasks {
        written.extend(task.await.expect("writer task"));
    }
    written
}

/// Counts the documents the receiver merges from its change feed, so the
/// count costs the receiver no reads.
fn count_arrivals(receiver: &Node) -> (Arc<AtomicUsize>, edge::Service) {
    let arrived = Arc::new(AtomicUsize::new(0));
    let mut changes = receiver.subscribe_document_changes();
    let counter = Arc::clone(&arrived);
    let counting = edge::testing::edge().spawn_service(async move {
        while let Some(batch) = changes.recv().await {
            assert!(!batch.resync_required, "the change feed overflowed");
            counter.fetch_add(batch.changes.len(), Ordering::Relaxed);
        }
    });
    (arrived, counting)
}

/// Prints the receiver's count once a second until it holds `want` documents
/// or `limit` passes; returns what a query finds it holds, and when.
async fn watch(
    receiver: &Node,
    arrived: &AtomicUsize,
    want: usize,
    began: Instant,
    limit: Duration,
    report: &Report,
) -> (usize, f64) {
    let mut last = 0;
    let mut tick = 0u64;
    loop {
        let got = arrived.load(Ordering::Relaxed);
        let now = began.elapsed();
        if got >= want || now >= limit {
            return (held(receiver).await, now.as_secs_f64());
        }
        if now.as_secs() > tick {
            tick = now.as_secs();
            println!(
                "t={tick:>4}s held {got:>6} (+{:>4}) rss {:.0} MiB",
                got - last,
                resident_mib()
            );
            last = got;
            if report.every > 0 && tick.is_multiple_of(report.every) {
                report.print(tick, got);
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// Every `INTAKE_REPORT_SECS` seconds, the receiver's store against the blob
/// bytes it has taken in: its size on disk (space amplification) and what
/// its engine has written (write amplification).
struct Report {
    dir: std::path::PathBuf,
    stats: Arc<regolith::Statistics>,
    blob: usize,
    every: u64,
}

impl Report {
    fn new(dir: std::path::PathBuf, stats: Arc<regolith::Statistics>, blob: usize) -> Self {
        Self {
            dir,
            stats,
            blob,
            every: env("INTAKE_REPORT_SECS", 0),
        }
    }

    fn print(&self, tick: u64, got: usize) {
        use regolith::Ticker;
        let mib = |bytes: u64| bytes as f64 / 1_048_576.0;
        let ticker = |ticker| mib(self.stats.get_ticker(ticker));
        let payload = mib((got * self.blob) as u64).max(f64::MIN_POSITIVE);
        let files = disk_by_kind(&self.dir);
        let kind = |name: &str| mib(files.get(name).copied().unwrap_or(0));
        let store = mib(files.values().sum());
        let written = ticker(Ticker::WalBytesWritten)
            + ticker(Ticker::FlushBytesWritten)
            + ticker(Ticker::CompactionBytesWritten);
        println!(
            "report t={tick} docs={got} payload_mib={payload:.1} store_mib={store:.1} sst_mib={:.1} log_mib={:.1} space_x={:.2} wal_mib={:.1} flush_mib={:.1} compaction_read_mib={:.1} compaction_written_mib={:.1} compactions={} written_x={:.1} rss_mib={:.0}",
            kind("sst"),
            kind("log"),
            store / payload,
            ticker(Ticker::WalBytesWritten),
            ticker(Ticker::FlushBytesWritten),
            ticker(Ticker::CompactionBytesRead),
            ticker(Ticker::CompactionBytesWritten),
            self.stats.get_ticker(Ticker::CompactionCount),
            written / payload,
            resident_mib(),
        );
    }
}

#[test]
#[ignore = "a measurement, run by hand"]
fn blob_replication_intake() -> Result<()> {
    let docs: usize = env("INTAKE_DOCS", 160);
    let blob: usize = env("INTAKE_BLOB", 122_880);
    let count: usize = env("INTAKE_WRITERS", 1);
    let per_second: f64 = env("INTAKE_PER_SECOND", 0.0);
    let limit = Duration::from_secs(env("INTAKE_LIMIT_SECS", 900));
    let role: String = env("INTAKE_ROLE", "both".to_string());
    let addr_file: String = env("INTAKE_ADDR_FILE", String::new());
    let want = docs * count;
    if let Ok(filter) = std::env::var("INTAKE_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }

    edge::testing::edge()
        .block_on(async move {
            let tmp = match std::env::var("INTAKE_DIR") {
                Ok(dir) => tempfile::tempdir_in(dir)?,
                Err(_) => tempfile::tempdir()?,
            };
            match role.as_str() {
                "receiver" => {
                    let validator = ReadsSeq::chosen();
                    let (receiver, stats) =
                        start_counted(&tmp.path().join("b"), validator.clone()).await?;
                    let (arrived, _counting) = count_arrivals(&receiver);
                    std::fs::write(&addr_file, listen_addr(&receiver).await?)?;
                    while arrived.load(Ordering::Relaxed) == 0 {
                        sleep(Duration::from_millis(5)).await;
                    }
                    let began = Instant::now();
                    let report = Report::new(tmp.path().join("b"), Arc::clone(&stats), blob);
                    let (got, all) =
                        watch(&receiver, &arrived, want, began, limit, &report).await;
                    println!(
                        "receiver held {got} of {want} after {all:.1}s from its first = {:.1} docs/s, rss {:.0} MiB, store {:.0} MiB",
                        got as f64 / all,
                        resident_mib(),
                        disk_mib(&tmp.path().join("b"))
                    );
                    print_engine(&stats, (got * blob) as f64);
                    validator.iter().for_each(|validator| validator.print());
                }
                "writers" => {
                    let receiver = std::fs::read_to_string(&addr_file)?;
                    let writers = start_writers(tmp.path(), &receiver, count).await?;
                    let began = Instant::now();
                    run_writers(&writers, docs, blob, per_second).await;
                    println!(
                        "writers wrote {want} in {:.1}s, rss {:.0} MiB",
                        began.elapsed().as_secs_f64(),
                        resident_mib()
                    );
                    sleep(limit).await;
                }
                "sync" => {
                    let receiver = start(&tmp.path().join("b")).await?;
                    let writer = start(&tmp.path().join("a0")).await?;
                    let written = run_writers(&[writer.clone()], docs, blob, 0.0).await;
                    let ops = receiver.p2p_system().expect("p2p").ops().clone();
                    ops.connect_peer(&listen_addr(&writer).await?).await?;
                    let began = Instant::now();
                    let timeout = std::env::var("INTAKE_SYNC_TIMEOUT_SECS")
                        .ok()
                        .and_then(|secs| secs.parse().ok())
                        .map(Duration::from_secs);
                    let outcome = ops.sync_documents("IqChunk", written, timeout).await;
                    let took = began.elapsed().as_secs_f64();
                    let at_return = held(&receiver).await;
                    sleep(Duration::from_secs(30)).await;
                    println!(
                        "sync_documents of {docs} returned {:?} after {took:.1}s holding {at_return}; 30s later {}",
                        outcome.map_err(|error| error.to_string()),
                        held(&receiver).await
                    );
                }
                _ => {
                    let validator = ReadsSeq::chosen();
                    let (receiver, stats) =
                        start_counted(&tmp.path().join("b"), validator.clone()).await?;
                    let (arrived, _counting) = count_arrivals(&receiver);
                    let addr = listen_addr(&receiver).await?;
                    let writers = start_writers(tmp.path(), &addr, count).await?;
                    let began = Instant::now();
                    let watching = {
                        let receiver = receiver.clone();
                        let report = Report::new(tmp.path().join("b"), Arc::clone(&stats), blob);
                        edge::testing::edge()
                            .spawn(async move {
                                watch(&receiver, &arrived, want, began, limit, &report).await
                            })
                            .within(Duration::MAX)
                    };
                    run_writers(&writers, docs, blob, per_second).await;
                    let wrote = began.elapsed().as_secs_f64();
                    let (got, all) = watching.await.expect("watch task");
                    println!(
                        "{count} writer(s), {docs} docs each of {blob} B: wrote {want} in {wrote:.1}s; receiver held {got} after {all:.1}s = {:.1} docs/s, rss {:.0} MiB, receiver store {:.0} MiB, first writer's store {:.0} MiB",
                        got as f64 / all,
                        resident_mib(),
                        disk_mib(&tmp.path().join("b")),
                        disk_mib(&tmp.path().join("a0"))
                    );
                    print_engine(&stats, (got * blob) as f64);
                    validator.iter().for_each(|validator| validator.print());
                    if std::env::var("INTAKE_KEY_DETAIL").is_ok() {
                        drop(writers);
                        match Arc::try_unwrap(receiver) {
                            Ok(receiver) => receiver.shutdown().await?,
                            Err(_) => panic!("the receiver is still shared"),
                        }
                        print_key_families(&tmp.path().join("b")).await?;
                    }
                }
            }
            Ok(())
        })
        .unwrap()
}
