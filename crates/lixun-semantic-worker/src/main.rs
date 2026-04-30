//! Semantic-search sidecar binary.
//!
//! Spawned by `lixund` over an `AF_UNIX` socket; speaks
//! [`lixun_semantic_proto`]. Owns all heavyweight ML state
//! (`fastembed`, `lancedb`, the `BackfillJournal`) so the daemon
//! stays small and crash-isolated from ONNX/Lance failures.
//!
//! See `.local-plans/plans/semantic-sidecar.md` §3 for the full design.

#![allow(dead_code)]

mod ann;
mod config;
mod embedder;
mod ipc_doc_store;
mod journal;
mod query_router;
mod store;
mod worker;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tracing_subscriber::EnvFilter;

use lixun_mutation::AnnHandle;
use lixun_semantic_proto::{Cmd, ErrorCode, Msg, PROTOCOL_VERSION, WorkerCodec};

use crate::ann::LanceDbAnnHandle;
use crate::config::SemanticConfig;
use crate::embedder::{load_clip_text_embedder, load_image_embedder, load_text_embedder};
use crate::ipc_doc_store::IpcDocStore;
use crate::journal::BackfillJournal;
use crate::query_router::QueryRouter;
use crate::store::VectorStore;
use crate::worker::{EmbedJob, spawn_worker, start_backfill};

const REPLY_QUEUE_CAPACITY: usize = 256;

#[derive(Parser, Debug)]
#[command(
    name = "lixun-semantic-worker",
    about = "Lixun semantic-search sidecar"
)]
struct Args {
    /// Path to the AF_UNIX socket the daemon is listening on.
    #[arg(long)]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_env("LIXUN_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();
    tracing::info!(socket = %args.socket.display(), "semantic worker starting");

    let stream = UnixStream::connect(&args.socket)
        .await
        .with_context(|| format!("connecting to {}", args.socket.display()))?;
    let mut framed = Framed::new(stream, WorkerCodec::new());

    /* Handshake: the daemon speaks first. A version mismatch is
    answered with Msg::Error{code=Version} so the daemon can log a
    clear reason rather than seeing a silent EOF. */
    let first = framed
        .next()
        .await
        .context("worker: socket closed before handshake")?
        .context("worker: handshake decode failed")?;
    let Cmd::Handshake { proto_version } = first else {
        let _ = framed
            .send(Msg::Error {
                req_id: 0,
                code: ErrorCode::BadRequest,
                detail: "expected Handshake".into(),
            })
            .await;
        anyhow::bail!("first frame was not Cmd::Handshake");
    };
    if proto_version != PROTOCOL_VERSION {
        let _ = framed
            .send(Msg::Error {
                req_id: 0,
                code: ErrorCode::Version,
                detail: format!("daemon proto={proto_version} worker proto={PROTOCOL_VERSION}"),
            })
            .await;
        anyhow::bail!("proto version mismatch");
    }
    framed
        .send(Msg::HandshakeOk {
            proto_version: PROTOCOL_VERSION,
            worker_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await
        .context("worker: send HandshakeOk")?;

    let (data_root, cache_root) = data_and_cache_roots()?;
    /* Phase 1 ships with the daemon's defaults. A future Cmd will
    let the daemon push the live `[semantic]` config block. */
    let cfg = SemanticConfig::default();

    let cache_dir = cache_root.join(&cfg.cache_subdir);
    let text_embedder = load_text_embedder(&cfg.text_model, &cache_dir, 1, 1)
        .with_context(|| format!("loading text embedder '{}'", cfg.text_model))?;
    let image_embedder = load_image_embedder(&cfg.image_model, &cache_dir, 1, 1)
        .with_context(|| format!("loading image embedder '{}'", cfg.image_model))?;
    let clip_text_embedder =
        load_clip_text_embedder(&cache_dir, 1, 1).context("loading CLIP text embedder")?;
    let text_dim = text_embedder.dim();
    let image_dim = image_embedder.dim();
    let text_embedder = Arc::new(Mutex::new(text_embedder));
    let image_embedder = Arc::new(Mutex::new(image_embedder));
    let clip_text_embedder = Arc::new(Mutex::new(clip_text_embedder));

    let vectors_dir = data_root.join("vectors");
    let store = Arc::new(
        VectorStore::open(&vectors_dir, text_dim, image_dim)
            .await
            .with_context(|| format!("opening LanceDB at {}", vectors_dir.display()))?,
    );

    let journal_path = data_root.join("semantic-backfill.sqlite");
    let journal = Arc::new(Mutex::new(
        BackfillJournal::open(&journal_path)
            .with_context(|| format!("opening journal at {}", journal_path.display()))?,
    ));

    let runtime = tokio::runtime::Handle::current();
    let worker_handle = spawn_worker(
        cfg.clone(),
        cfg.effective_batch_size(32),
        store.clone(),
        journal.clone(),
        runtime,
        text_embedder.clone(),
        image_embedder.clone(),
        clip_text_embedder.clone(),
    )
    .context("spawning embed worker thread")?;
    let embed_tx = worker_handle.sender();

    let ann = Arc::new(LanceDbAnnHandle::new());
    let _ = ann.install_store(store.clone());
    let _ = ann.install_text_embedder(text_embedder.clone());
    let _ = ann.install_clip_text_embedder(clip_text_embedder.clone());

    let ann_for_router = ann.clone();
    let clip_for_router = clip_text_embedder.clone();
    tokio::spawn(async move {
        tracing::info!("embedding query router anchors...");
        let image_anchor_embeddings: Vec<Vec<f32>> = {
            let mut embedder = clip_for_router.lock().unwrap();
            let texts: Vec<String> = QueryRouter::image_anchor_texts()
                .iter()
                .map(|s| s.to_string())
                .collect();
            match embedder.embed(texts) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("failed to embed image anchors: {e}");
                    return;
                }
            }
        };
        let text_anchor_embeddings: Vec<Vec<f32>> = {
            let mut embedder = clip_for_router.lock().unwrap();
            let texts: Vec<String> = QueryRouter::text_anchor_texts()
                .iter()
                .map(|s| s.to_string())
                .collect();
            match embedder.embed(texts) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("failed to embed text anchors: {e}");
                    return;
                }
            }
        };
        let query_router = Arc::new(QueryRouter::new(
            image_anchor_embeddings,
            text_anchor_embeddings,
            0.05,
        ));
        let _ = ann_for_router.install_query_router(query_router);
        tracing::info!("query router ready");
    });

    /* Reads come from the dispatch loop, writes from spawned ANN
    tasks. A bounded mpsc funnels every outbound Msg through a
    single writer task that owns the sink half — this avoids
    sharing the Framed sink under a Mutex while still allowing the
    dispatch loop to issue concurrent searches. */
    let (sink, mut stream) = framed.split();
    let (reply_tx, mut reply_rx) = mpsc::channel::<Msg>(REPLY_QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(msg) = reply_rx.recv().await {
            if let Err(e) = sink.send(msg).await {
                tracing::error!("sink write failed: {e}");
                break;
            }
        }
    });

    /* The IpcDocStore proxies Tantivy reads back to the daemon over
    the same socket. Its Sender feeds the same writer funnel as
    every other outbound Msg; the dispatch loop routes incoming
    Cmd::CallbackReply frames into its pending map via deliver(). */
    let ipc_doc_store = Arc::new(IpcDocStore::new(reply_tx.clone()));

    tracing::info!("semantic worker ready");

    while let Some(frame) = stream.next().await {
        let cmd = match frame {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("decode error, exiting: {e}");
                std::process::exit(2);
            }
        };
        match cmd {
            Cmd::Handshake { .. } => {
                let _ = reply_tx
                    .send(Msg::Error {
                        req_id: 0,
                        code: ErrorCode::BadRequest,
                        detail: "duplicate Handshake".into(),
                    })
                    .await;
            }
            Cmd::Embed { docs } => {
                for doc in docs {
                    if embed_tx.send(EmbedJob::Upsert(doc)).await.is_err() {
                        tracing::error!("embed channel closed; worker thread died");
                        std::process::exit(3);
                    }
                }
            }
            Cmd::Delete { doc_id } => {
                if embed_tx.send(EmbedJob::Delete(doc_id)).await.is_err() {
                    tracing::error!("embed channel closed; worker thread died");
                    std::process::exit(3);
                }
            }
            Cmd::SearchText { req_id, query, k } => {
                let ann = ann.clone();
                let tx = reply_tx.clone();
                tokio::spawn(async move {
                    let reply = match ann.search_text(&query, k as usize).await {
                        Ok(hits) => Msg::SearchResult { req_id, hits },
                        Err(e) => Msg::Error {
                            req_id,
                            code: ErrorCode::Internal,
                            detail: format!("{e:#}"),
                        },
                    };
                    let _ = tx.send(reply).await;
                });
            }
            Cmd::SearchImage { req_id, query, k } => {
                let ann = ann.clone();
                let tx = reply_tx.clone();
                tokio::spawn(async move {
                    let reply = match ann.search_image(&query, k as usize).await {
                        Ok(hits) => Msg::SearchResult { req_id, hits },
                        Err(e) => Msg::Error {
                            req_id,
                            code: ErrorCode::Internal,
                            detail: format!("{e:#}"),
                        },
                    };
                    let _ = tx.send(reply).await;
                });
            }
            Cmd::ClassifyQuery { req_id, query } => {
                let ann = ann.clone();
                let tx = reply_tx.clone();
                tokio::spawn(async move {
                    let reply = match ann.classify_query(&query).await {
                        Ok(modality) => Msg::ClassifyResult { req_id, modality },
                        Err(e) => Msg::Error {
                            req_id,
                            code: ErrorCode::Internal,
                            detail: format!("{e:#}"),
                        },
                    };
                    let _ = tx.send(reply).await;
                });
            }
            Cmd::BackfillStart { req_id } => {
                let store: Arc<dyn lixun_mutation::DocStore> = ipc_doc_store.clone();
                let journal = journal.clone();
                let embed_tx = embed_tx.clone();
                let tx = reply_tx.clone();
                tokio::spawn(async move {
                    let reply = match start_backfill(store, journal, embed_tx).await {
                        Ok((submitted, total)) => Msg::BackfillComplete {
                            req_id,
                            submitted,
                            total,
                        },
                        Err(e) => Msg::Error {
                            req_id,
                            code: ErrorCode::Internal,
                            detail: format!("{e:#}"),
                        },
                    };
                    let _ = tx.send(reply).await;
                });
            }
            Cmd::CallbackReply { req_id, resp } => {
                ipc_doc_store.deliver(req_id, resp);
            }
            Cmd::Shutdown => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }

    /* Order matters. The embed-worker thread parks on `rx.recv()`
    and only returns once every `EmbedJob` Sender is dropped — so
    we must drop both `embed_tx` (held by this scope) and the
    `WorkerHandle` (which owns the original Sender) before the
    process can exit cleanly. Spawned ANN/backfill tasks also
    hold `reply_tx` clones; ignore them and bound the writer wait
    so a stuck task can't keep the binary alive. */
    drop(embed_tx);
    drop(worker_handle);
    drop(ipc_doc_store);
    drop(ann);
    drop(reply_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    tracing::info!("semantic worker exiting");
    Ok(())
}

fn data_and_cache_roots() -> Result<(PathBuf, PathBuf)> {
    if let Ok(p) = std::env::var("LIXUN_SEMANTIC_DATA_DIR") {
        let root = PathBuf::from(p);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating data dir {}", root.display()))?;
        return Ok((root.clone(), root));
    }
    let data = dirs::data_dir()
        .context("XDG data dir unavailable")?
        .join("lixun")
        .join("semantic");
    let cache = dirs::cache_dir()
        .context("XDG cache dir unavailable")?
        .join("lixun");
    std::fs::create_dir_all(&data)
        .with_context(|| format!("creating data dir {}", data.display()))?;
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("creating cache dir {}", cache.display()))?;
    Ok((data, cache))
}
