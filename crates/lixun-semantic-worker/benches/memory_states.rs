//! Memory-states harness for the semantic worker.
//!
//! Drives the worker embedders through four named states and snapshots
//! /proc/self/status + mallinfo2() at each sample point.
//! Output is JSON-Lines to stdout.
//!
//! Synthetic image fixtures: deterministic 64x64 RGB PNGs written to a
//! temp directory.  No binary fixtures are committed.
//!
//! Precondition: model cache is already populated so the harness does
//! not measure download time / memory.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use image::{ImageBuffer, Rgb};
use serde::Serialize;
use tokio::runtime::Runtime;

use lixun_semantic_worker::config::SemanticConfig;
use lixun_semantic_worker::embedder::{
    load_clip_text_embedder, load_image_embedder, load_text_embedder,
    ClipTextEmbedder, ImageEmbedder, TextEmbedder,
};
use lixun_semantic_worker::query_router::QueryRouter;
use lixun_semantic_worker::store::VectorStore;

#[derive(Parser, Debug)]
#[command(name = "memory-states-harness")]
struct Args {
    #[arg(long, default_value = "all")]
    states_mode: String,
}

#[derive(Serialize)]
struct MemorySample {
    state: String,
    t_ms: u64,
    vm_rss_kb: u64,
    vm_peak_kb: u64,
    vm_size_kb: u64,
    rss_anon_kb: u64,
    rss_file_kb: u64,
    rss_shmem_kb: u64,
    arena_bytes: u64,
}

struct ProcStatus {
    vm_rss_kb: u64,
    vm_peak_kb: u64,
    vm_size_kb: u64,
    rss_anon_kb: u64,
    rss_file_kb: u64,
    rss_shmem_kb: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let modes: Vec<&str> = match args.states_mode.as_str() {
        "all" => vec![
            "idle-startup",
            "idle-after-1h",
            "post-vision-burst",
            "active",
        ],
        m => vec![m],
    };

    let rt = Runtime::new().context("creating tokio runtime")?;

    let cache_dir = cache_dir();
    fs::create_dir_all(&cache_dir).context("creating cache dir")?;

    let cfg = SemanticConfig::default();

    let text_embedder = Arc::new(Mutex::new(
        load_text_embedder(&cfg.text_model, &cache_dir, 1, 1)
            .context("loading text embedder")?,
    ));
    let image_embedder = Arc::new(Mutex::new(
        load_image_embedder(&cfg.image_model, &cache_dir, 1, 1)
            .context("loading image embedder")?,
    ));
    let clip_text_embedder = Arc::new(Mutex::new(
        load_clip_text_embedder(&cache_dir, 1, 1)
            .context("loading CLIP text embedder")?,
    ));

    let vectors_dir = std::env::temp_dir().join("lixun-memory-harness-vectors");
    let _ = fs::remove_dir_all(&vectors_dir);
    let store = rt.block_on(async {
        VectorStore::open(&vectors_dir, 384, 512)
            .await
            .context("opening vector store")
    })?;

    // Pre-compute query-router anchors so the active state can
    // optionally exercise the router (kept in memory for completeness).
    let _router = build_query_router(&clip_text_embedder)?;
    let _ = store; // keep store alive for the lifetime of the harness

    let fixture_dir = std::env::temp_dir().join("lixun-memory-harness-images");
    let _ = fs::remove_dir_all(&fixture_dir);
    fs::create_dir_all(&fixture_dir).context("creating fixture dir")?;
    create_image_fixtures(&fixture_dir, 50).context("creating image fixtures")?;

    for mode in modes {
        match mode {
            "idle-startup" => run_idle_startup(
                &text_embedder,
                &image_embedder,
                &clip_text_embedder,
                &fixture_dir,
            )?,
            "idle-after-1h" => run_idle_after_1h(
                &text_embedder,
                &image_embedder,
                &clip_text_embedder,
                &fixture_dir,
            )?,
            "post-vision-burst" => {
                run_post_vision_burst(&image_embedder, &fixture_dir)?
            }
            "active" => run_active(
                &text_embedder,
                &image_embedder,
                &clip_text_embedder,
                &fixture_dir,
            )?,
            other => eprintln!("Warning: unknown state mode '{}'", other),
        }
    }

    let _ = fs::remove_dir_all(&fixture_dir);
    let _ = fs::remove_dir_all(&vectors_dir);

    Ok(())
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("lixun")
        .join("fastembed")
}

fn create_image_fixtures(dir: &Path, count: usize) -> Result<()> {
    for i in 0..count {
        let path = dir.join(format!("fixture_{:03}.png", i));
        let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(64, 64, |x, y| {
                let ii = i as u32;
                let r = ((x * 7 + y * 13 + ii * 31) % 256) as u8;
                let g = ((x * 11 + y * 17 + ii * 41) % 256) as u8;
                let b = ((x * 13 + y * 19 + ii * 47) % 256) as u8;
                Rgb([r, g, b])
            });
        img.save(&path)
            .with_context(|| format!("saving fixture {}", path.display()))?;
    }
    Ok(())
}

fn build_query_router(
    clip_text: &Arc<Mutex<ClipTextEmbedder>>,
) -> Result<QueryRouter> {
    let image_anchors = {
        let mut guard = clip_text.lock().unwrap();
        let texts: Vec<String> = QueryRouter::image_anchor_texts()
            .iter()
            .map(|s| s.to_string())
            .collect();
        guard.embed(texts).context("embedding image anchors")?
    };
    let text_anchors = {
        let mut guard = clip_text.lock().unwrap();
        let texts: Vec<String> = QueryRouter::text_anchor_texts()
            .iter()
            .map(|s| s.to_string())
            .collect();
        guard.embed(texts).context("embedding text anchors")?
    };
    Ok(QueryRouter::new(image_anchors, text_anchors, 0.05))
}

fn read_proc_status() -> ProcStatus {
    let mut status = ProcStatus {
        vm_rss_kb: 0,
        vm_peak_kb: 0,
        vm_size_kb: 0,
        rss_anon_kb: 0,
        rss_file_kb: 0,
        rss_shmem_kb: 0,
    };

    if let Ok(file) = fs::File::open("/proc/self/status") {
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(val) = line.strip_prefix("VmRSS:") {
                status.vm_rss_kb = parse_kb(val);
            } else if let Some(val) = line.strip_prefix("VmPeak:") {
                status.vm_peak_kb = parse_kb(val);
            } else if let Some(val) = line.strip_prefix("VmSize:") {
                status.vm_size_kb = parse_kb(val);
            } else if let Some(val) = line.strip_prefix("RssAnon:") {
                status.rss_anon_kb = parse_kb(val);
            } else if let Some(val) = line.strip_prefix("RssFile:") {
                status.rss_file_kb = parse_kb(val);
            } else if let Some(val) = line.strip_prefix("RssShmem:") {
                status.rss_shmem_kb = parse_kb(val);
            }
        }
    }

    status
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn read_mallinfo2() -> u64 {
    #[cfg(target_os = "linux")]
    unsafe {
        let info = libc::mallinfo2();
        info.arena as u64
    }
    #[cfg(not(target_os = "linux"))]
    0
}

fn sample_memory(state: &str, start: Instant) -> MemorySample {
    let t_ms = start.elapsed().as_millis() as u64;
    let proc = read_proc_status();
    let arena = read_mallinfo2();

    MemorySample {
        state: state.to_string(),
        t_ms,
        vm_rss_kb: proc.vm_rss_kb,
        vm_peak_kb: proc.vm_peak_kb,
        vm_size_kb: proc.vm_size_kb,
        rss_anon_kb: proc.rss_anon_kb,
        rss_file_kb: proc.rss_file_kb,
        rss_shmem_kb: proc.rss_shmem_kb,
        arena_bytes: arena,
    }
}

fn emit(sample: &MemorySample) {
    println!("{}", serde_json::to_string(sample).unwrap());
}

fn run_idle_startup(
    _text: &Arc<Mutex<TextEmbedder>>,
    _image: &Arc<Mutex<ImageEmbedder>>,
    _clip_text: &Arc<Mutex<ClipTextEmbedder>>,
    _fixture_dir: &Path,
) -> Result<()> {
    let start = Instant::now();
    emit(&sample_memory("idle-startup", start));
    thread::sleep(Duration::from_secs(5));
    emit(&sample_memory("idle-startup", start));
    thread::sleep(Duration::from_secs(25));
    emit(&sample_memory("idle-startup", start));
    Ok(())
}

fn run_idle_after_1h(
    text: &Arc<Mutex<TextEmbedder>>,
    image: &Arc<Mutex<ImageEmbedder>>,
    clip_text: &Arc<Mutex<ClipTextEmbedder>>,
    fixture_dir: &Path,
) -> Result<()> {
    let fixtures: Vec<PathBuf> = (0..50)
        .map(|i| fixture_dir.join(format!("fixture_{:03}.png", i)))
        .collect();

    let interval = Duration::from_millis(600); // ~100 queries in 60 s
    for i in 0..100 {
        if i % 2 == 0 {
            let mut guard = text.lock().unwrap();
            let _ = guard.embed(vec![format!("q{}", i)]);
        } else {
            let mut guard = image.lock().unwrap();
            let _ = guard.embed(vec![fixtures[i % fixtures.len()].clone()]);
        }
        if i % 5 == 0 {
            let mut guard = clip_text.lock().unwrap();
            let _ = guard.embed(vec![format!("clip q{}", i)]);
        }
        thread::sleep(interval);
    }

    thread::sleep(Duration::from_secs(60));

    let start = Instant::now();
    emit(&sample_memory("idle-after-1h", start));
    Ok(())
}

fn run_post_vision_burst(
    image: &Arc<Mutex<ImageEmbedder>>,
    fixture_dir: &Path,
) -> Result<()> {
    let fixtures: Vec<PathBuf> = (0..50)
        .map(|i| fixture_dir.join(format!("fixture_{:03}.png", i)))
        .collect();

    for fixture in &fixtures {
        let mut guard = image.lock().unwrap();
        let _ = guard.embed(vec![fixture.clone()]);
    }

    thread::sleep(Duration::from_secs(1));

    let start = Instant::now();
    emit(&sample_memory("post-vision-burst", start));
    Ok(())
}

fn run_active(
    text: &Arc<Mutex<TextEmbedder>>,
    image: &Arc<Mutex<ImageEmbedder>>,
    clip_text: &Arc<Mutex<ClipTextEmbedder>>,
    fixture_dir: &Path,
) -> Result<()> {
    let fixtures: Vec<PathBuf> = (0..50)
        .map(|i| fixture_dir.join(format!("fixture_{:03}.png", i)))
        .collect();

    for i in 0..60 {
        match i % 3 {
            0 => {
                let mut guard = text.lock().unwrap();
                let _ = guard.embed(vec![format!("active q{}", i)]);
            }
            1 => {
                let mut guard = image.lock().unwrap();
                let _ = guard.embed(vec![fixtures[i % fixtures.len()].clone()]);
            }
            2 => {
                let mut guard = clip_text.lock().unwrap();
                let _ = guard.embed(vec![format!("active clip q{}", i)]);
            }
            _ => unreachable!(),
        }
        thread::sleep(Duration::from_secs(1));
    }

    let start = Instant::now();
    emit(&sample_memory("active", start));
    Ok(())
}
