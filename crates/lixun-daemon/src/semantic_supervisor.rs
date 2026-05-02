//! Out-of-process supervisor for the semantic-search worker.
//!
//! Spawns `lixun-semantic-worker`, owns the AF_UNIX listener,
//! handshakes, then runs duplex IPC: incoming `Msg` frames route to
//! the appropriate pending request via the
//! [`SemanticConnection`](lixun_source_semantic_stub::SemanticConnection)
//! installed in the stub plugin; outgoing `Cmd` frames flow from
//! the stub through the same connection back to the worker.
//!
//! Crash handling: a worker exit triggers an exponential backoff
//! restart (1s → 2s → 4s → 8s, capped at 60 s) without a retry
//! limit. Each restart fails any in-flight requests so callers stop
//! waiting on a dead socket. The supervisor never panics.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use lixun_semantic_proto::{CallbackResp, Cmd, DaemonCodec, ErrorCode, Msg, PROTOCOL_VERSION};
use lixun_source_semantic_stub::{SemanticConnection, install_connection};
use tokio::net::UnixListener;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::codec::Framed;

const HANDSHAKE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const WRITER_QUEUE_CAPACITY: usize = 256;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Probe for the semantic worker binary.
///
/// Search order: `LIXUN_SEMANTIC_WORKER` env var, then any executable
/// named `lixun-semantic-worker` on `$PATH`, then the canonical
/// distro install path `/usr/lib/lixun/lixun-semantic-worker`. The
/// returned path is the one the supervisor will exec.
pub fn probe_worker_binary() -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("LIXUN_SEMANTIC_WORKER") {
        let p = PathBuf::from(env);
        if is_executable(&p) {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("lixun-semantic-worker");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    let distro = PathBuf::from("/usr/lib/lixun/lixun-semantic-worker");
    if is_executable(&distro) {
        return Some(distro);
    }
    None
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Run the semantic-worker supervisor on the current task.
///
/// Returns only when the parent runtime is dropped — the function
/// loops forever, restarting the worker on crash with backoff. The
/// connection is installed into [`lixun_source_semantic_stub`]'s
/// global slot the first time the handshake succeeds, so plugin
/// build calls in `register_plugin_sources` will see a live
/// connection from then on.
pub async fn supervise(worker_path: PathBuf) {
    let socket_dir = runtime_socket_dir();
    if let Err(e) = std::fs::create_dir_all(&socket_dir) {
        tracing::error!(
            dir = %socket_dir.display(),
            "semantic supervisor: cannot create socket dir, giving up: {e:#}"
        );
        return;
    }

    let mut backoff = MIN_BACKOFF;
    let mut conn: Option<Arc<SemanticConnection>> = None;
    loop {
        let socket_path = socket_dir.join(format!(
            "semantic-{}-{}.sock",
            std::process::id(),
            random_suffix()
        ));
        let _ = std::fs::remove_file(&socket_path);

        match run_one_session(&worker_path, &socket_path, conn.clone()).await {
            Ok(installed_conn) => {
                /* The session held the worker until shutdown / EOF.
                Reuse the same SemanticConnection across restarts so
                callers holding an Arc<SemanticConnection> from the
                stub keep working — only the underlying writer
                channel was rebound. */
                if conn.is_none() {
                    conn = Some(installed_conn);
                }
                tracing::warn!(
                    "semantic worker exited cleanly; restarting in {}s",
                    backoff.as_secs()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "semantic worker session failed: {e:#}; restarting in {}s",
                    backoff.as_secs()
                );
            }
        }

        if let Some(c) = &conn {
            c.fail_all_pending(lixun_source_semantic_stub_error(
                ErrorCode::Internal,
                "worker disconnected",
            ));
        }

        let _ = std::fs::remove_file(&socket_path);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn lixun_source_semantic_stub_error(
    code: ErrorCode,
    detail: &str,
) -> lixun_source_semantic_stub::SemanticIpcError {
    lixun_source_semantic_stub::SemanticIpcError {
        code,
        detail: detail.to_string(),
    }
}

/// One full worker session: bind, spawn, handshake, run duplex IO,
/// return when the worker exits or the connection drops.
async fn run_one_session(
    worker_path: &Path,
    socket_path: &Path,
    existing_conn: Option<Arc<SemanticConnection>>,
) -> Result<Arc<SemanticConnection>> {
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;

    let log_filter = std::env::var("LIXUN_LOG").unwrap_or_else(|_| "info".to_string());
    let mut child: Child = Command::new(worker_path)
        .arg("--socket")
        .arg(socket_path)
        .env("LIXUN_LOG", &log_filter)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", worker_path.display()))?;

    forward_child_logs(&mut child);

    let (stream, _) = match timeout(HANDSHAKE_ACCEPT_TIMEOUT, listener.accept()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            kill_child(&mut child).await;
            return Err(anyhow!("accept failed: {e}"));
        }
        Err(_) => {
            kill_child(&mut child).await;
            return Err(anyhow!(
                "worker did not connect within {}s",
                HANDSHAKE_ACCEPT_TIMEOUT.as_secs()
            ));
        }
    };
    let mut framed = Framed::new(stream, DaemonCodec::new());

    framed
        .send(Cmd::Handshake {
            proto_version: PROTOCOL_VERSION,
        })
        .await
        .context("send handshake")?;

    let ack = match timeout(HANDSHAKE_REPLY_TIMEOUT, framed.next()).await {
        Ok(Some(Ok(m))) => m,
        Ok(Some(Err(e))) => {
            kill_child(&mut child).await;
            return Err(anyhow!("decode handshake reply: {e}"));
        }
        Ok(None) => {
            kill_child(&mut child).await;
            return Err(anyhow!("worker closed socket before handshake reply"));
        }
        Err(_) => {
            kill_child(&mut child).await;
            return Err(anyhow!("worker did not handshake within timeout"));
        }
    };
    match ack {
        Msg::HandshakeOk {
            proto_version,
            worker_version,
        } => {
            if proto_version != PROTOCOL_VERSION {
                kill_child(&mut child).await;
                return Err(anyhow!(
                    "worker proto={proto_version} daemon proto={PROTOCOL_VERSION}"
                ));
            }
            tracing::info!(
                worker_version = %worker_version,
                "semantic worker handshake ok"
            );
        }
        other => {
            kill_child(&mut child).await;
            return Err(anyhow!("unexpected first reply: {:?}", other));
        }
    }

    /* Build (or rebind) the SemanticConnection. The first session
    creates it and installs into the stub crate; later sessions
    rebind only the writer channel so existing AnnHandle and
    Broadcaster Arc clones keep working transparently. Phase 2
    rebinds by constructing a fresh SemanticConnection — Phase 3
    will revisit if reuse becomes important. */
    let (writer_tx, mut writer_rx) = mpsc::channel::<Cmd>(WRITER_QUEUE_CAPACITY);
    let conn = match existing_conn {
        Some(c) => c,
        None => {
            let c = SemanticConnection::new(writer_tx.clone());
            install_connection(c.clone());
            c
        }
    };

    let (sink, mut stream) = framed.split();
    let writer_sink = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(cmd) = writer_rx.recv().await {
            if let Err(e) = sink.send(cmd).await {
                tracing::warn!("semantic supervisor: sink write failed: {e}");
                break;
            }
        }
    });

    let conn_for_reader = conn.clone();
    let writer_for_reader = writer_tx.clone();
    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            let msg = match frame {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("semantic supervisor: decode error: {e}");
                    break;
                }
            };
            handle_msg(msg, &conn_for_reader, &writer_for_reader).await;
        }
    });

    /* Wait for whichever happens first: child exit (crash or
    Shutdown), reader EOF, or writer-sink failure. Drop everything
    on the way out so the next loop iteration restarts cleanly. */
    tokio::select! {
        status = child.wait() => {
            tracing::warn!("semantic worker exited: {status:?}");
        }
        _ = reader => {
            tracing::warn!("semantic supervisor: reader exited");
            kill_child(&mut child).await;
        }
        _ = writer_sink => {
            tracing::warn!("semantic supervisor: writer exited");
            kill_child(&mut child).await;
        }
    }

    Ok(conn)
}

async fn handle_msg(msg: Msg, conn: &Arc<SemanticConnection>, writer: &mpsc::Sender<Cmd>) {
    match msg {
        Msg::HandshakeOk { .. } => {
            tracing::warn!("semantic supervisor: unexpected late HandshakeOk");
        }
        Msg::SearchResult { req_id, hits } => {
            conn.complete_search(req_id, Ok(hits));
        }
        Msg::ClassifyResult { req_id, modality } => {
            conn.complete_classify(req_id, Ok(modality));
        }
        Msg::BackfillComplete {
            req_id,
            submitted,
            total,
        } => {
            conn.complete_backfill(
                req_id,
                Ok(lixun_source_semantic_stub::BackfillStats { submitted, total }),
            );
        }
        Msg::Error {
            req_id,
            code,
            detail,
        } => {
            let err = lixun_source_semantic_stub::SemanticIpcError {
                code,
                detail: detail.clone(),
            };
            conn.complete_search(req_id, Err(err.clone()));
            conn.complete_classify(req_id, Err(err.clone()));
            conn.complete_backfill(req_id, Err(err));
            if req_id == 0 {
                tracing::warn!("semantic worker async error: {detail}");
            }
        }
        Msg::CallAllDocIds { req_id } => {
            let writer = writer.clone();
            tokio::spawn(async move {
                let resp = match lixun_source_semantic_stub::current_doc_store() {
                    Some(store) => match store.all_doc_ids().await {
                        Ok(set) => CallbackResp::AllDocIds {
                            ids: set.into_iter().collect(),
                        },
                        Err(e) => CallbackResp::Error {
                            code: ErrorCode::Callback,
                            detail: format!("all_doc_ids: {e:#}"),
                        },
                    },
                    None => CallbackResp::Error {
                        code: ErrorCode::Callback,
                        detail: "doc store not installed".into(),
                    },
                };
                let _ = writer.send(Cmd::CallbackReply { req_id, resp }).await;
            });
        }
        Msg::CallHydrateDoc { req_id, doc_id } => {
            let writer = writer.clone();
            tokio::spawn(async move {
                let resp = match lixun_source_semantic_stub::current_doc_store() {
                    Some(store) => match store.hydrate_doc(&doc_id).await {
                        /* CallbackResp::HydrateDoc carries Option<Hit>
                        only — ScoreBreakdown is intentionally
                        dropped at the wire (see proto crate's
                        type doc-comment). The worker's
                        start_backfill ignores it. */
                        Ok(Some((hit, _bd))) => CallbackResp::HydrateDoc { hit: Some(hit) },
                        Ok(None) => CallbackResp::HydrateDoc { hit: None },
                        Err(e) => CallbackResp::Error {
                            code: ErrorCode::Callback,
                            detail: format!("hydrate_doc({doc_id}): {e:#}"),
                        },
                    },
                    None => CallbackResp::Error {
                        code: ErrorCode::Callback,
                        detail: "doc store not installed".into(),
                    },
                };
                let _ = writer.send(Cmd::CallbackReply { req_id, resp }).await;
            });
        }
        Msg::CallGetBody { req_id, doc_id } => {
            let writer = writer.clone();
            tokio::spawn(async move {
                let resp = match lixun_source_semantic_stub::current_doc_store() {
                    Some(store) => match store.get_body(&doc_id).await {
                        Ok(body) => CallbackResp::GetBody { body },
                        Err(e) => CallbackResp::Error {
                            code: ErrorCode::Callback,
                            detail: format!("get_body({doc_id}): {e:#}"),
                        },
                    },
                    None => CallbackResp::Error {
                        code: ErrorCode::Callback,
                        detail: "doc store not installed".into(),
                    },
                };
                let _ = writer.send(Cmd::CallbackReply { req_id, resp }).await;
            });
        }
    }
}

async fn kill_child(child: &mut Child) {
    if let Some(id) = child.id() {
        tracing::debug!(pid = id, "semantic supervisor: killing worker child");
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
}

fn forward_child_logs(child: &mut Child) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    if let Some(out) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "semantic_worker", "{line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "semantic_worker", "{line}");
            }
        });
    }
}

fn runtime_socket_dir() -> PathBuf {
    if let Some(rt) = dirs::runtime_dir() {
        return rt.join("lixun");
    }
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/lixun-{uid}"))
}

fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        return format!("{nanos:08x}");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decide whether the worker supervisor should spawn, based on the
/// raw `[semantic]` config block the operator wrote. Returns false
/// when the section is absent, when `enabled` is missing, when
/// `enabled = false`, or when the value is the wrong type — in all
/// cases spawning a 400-MB-model sidecar against the operator's
/// opt-out (or silence) is wrong. Mirrors the gating logic the stub
/// plugin factory applies to its own registration so the two stay in
/// sync: either both run or neither runs.
pub fn should_spawn(raw: Option<&toml::Value>) -> bool {
    raw.and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod should_spawn_tests {
    use super::should_spawn;

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).expect("test fixture parses")
    }

    #[test]
    fn missing_section_returns_false() {
        assert!(!should_spawn(None));
    }

    #[test]
    fn empty_section_returns_false() {
        assert!(!should_spawn(Some(&val(""))));
    }

    #[test]
    fn enabled_true_returns_true() {
        assert!(should_spawn(Some(&val("enabled = true"))));
    }

    #[test]
    fn enabled_false_returns_false() {
        assert!(!should_spawn(Some(&val("enabled = false"))));
    }

    #[test]
    fn malformed_enabled_returns_false() {
        assert!(!should_spawn(Some(&val(r#"enabled = "yes""#))));
    }

    #[test]
    fn extra_fields_ignored_when_disabled() {
        assert!(!should_spawn(Some(&val(
            r#"
            enabled = false
            text_model = "bge-small-en-v1.5"
            "#
        ))));
    }

    #[test]
    fn extra_fields_ignored_when_enabled() {
        assert!(should_spawn(Some(&val(
            r#"
            enabled = true
            text_model = "bge-small-en-v1.5"
            batch_size = 32
            "#
        ))));
    }
}
