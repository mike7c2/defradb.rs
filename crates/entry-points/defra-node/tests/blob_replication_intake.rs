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

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "support/clock.rs"]
mod clock;
#[path = "support/outcome.rs"]
mod outcome;

use clock::sleep;
use defra_node::{Node, NodeBuilder, P2PConfig, PeerKeySource, SyncTuning};
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
fn store_options() -> storage::RegolithStoreOptions {
    let profile: String = env("INTAKE_PROFILE", "embedded".to_string());
    let mut options = match profile.as_str() {
        "server" => storage::RegolithStoreOptions::default(),
        _ => storage::RegolithStoreOptions::embedded(),
    };
    let mut engine = options.engine.clone();
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
    options.engine = engine;
    options
}

async fn start(dir: &Path) -> Result<Arc<Node>> {
    let node = NodeBuilder::default()
        .with_p2p(P2PConfig {
            port: Some(0),
            peer_key: PeerKeySource::Store,
            bind_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            relay_mode: p2p::iroh::IrohRelayModeConfig::Disabled,
            discovery: p2p::iroh::IrohDiscoveryConfig::Disabled,
            sync: tuning(),
            ..Default::default()
        })
        .with_regolith_options(store_options())
        .data_path(dir.to_path_buf())
        .build()
        .await?;
    node.add_collection(SDL).await?;
    Ok(Arc::new(node))
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

fn disk_mib(dir: &Path) -> f64 {
    fn bytes(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => bytes(&entry.path()),
                Ok(metadata) => metadata.len(),
                Err(_) => 0,
            })
            .sum()
    }
    bytes(dir) as f64 / 1_048_576.0
}

async fn write(writer: Arc<Node>, index: usize, docs: usize, blob: usize, per_second: f64) {
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
    }
    println!(
        "writer {index}: wrote {docs} in {:.1}s, slowest write {:.0} ms",
        began.elapsed().as_secs_f64(),
        slowest.as_secs_f64() * 1000.0
    );
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

async fn run_writers(writers: &[Arc<Node>], docs: usize, blob: usize, per_second: f64) {
    let edge = edge::testing::edge();
    let tasks: Vec<_> = writers
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, writer)| edge.spawn(write(writer, index, docs, blob, per_second)))
        .collect();
    for task in tasks {
        task.await.expect("writer task");
    }
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
        }
        sleep(Duration::from_millis(20)).await;
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
                    let receiver = start(&tmp.path().join("b")).await?;
                    let (arrived, _counting) = count_arrivals(&receiver);
                    std::fs::write(&addr_file, listen_addr(&receiver).await?)?;
                    while arrived.load(Ordering::Relaxed) == 0 {
                        sleep(Duration::from_millis(5)).await;
                    }
                    let began = Instant::now();
                    let (got, all) = watch(&receiver, &arrived, want, began, limit).await;
                    println!(
                        "receiver held {got} of {want} after {all:.1}s from its first = {:.1} docs/s, rss {:.0} MiB, store {:.0} MiB",
                        got as f64 / all,
                        resident_mib(),
                        disk_mib(&tmp.path().join("b"))
                    );
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
                _ => {
                    let receiver = start(&tmp.path().join("b")).await?;
                    let (arrived, _counting) = count_arrivals(&receiver);
                    let addr = listen_addr(&receiver).await?;
                    let writers = start_writers(tmp.path(), &addr, count).await?;
                    let began = Instant::now();
                    let watching = {
                        let receiver = receiver.clone();
                        edge::testing::edge()
                            .spawn(async move {
                                watch(&receiver, &arrived, want, began, limit).await
                            })
                    };
                    run_writers(&writers, docs, blob, per_second).await;
                    let wrote = began.elapsed().as_secs_f64();
                    let (got, all) = watching.await.expect("watch task");
                    println!(
                        "{count} writer(s), {docs} docs each of {blob} B: wrote {want} in {wrote:.1}s; receiver held {got} after {all:.1}s = {:.1} docs/s, rss {:.0} MiB, receiver store {:.0} MiB",
                        got as f64 / all,
                        resident_mib(),
                        disk_mib(&tmp.path().join("b"))
                    );
                }
            }
            Ok(())
        })
        .unwrap()
}
