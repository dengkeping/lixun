//! lixun — thin CLI client for the Lixun daemon.

use anyhow::{Context, Result};
use bytes::{BufMut, BytesMut};
use clap::{Parser, Subcommand};
use lixun_ipc::{PROTOCOL_VERSION, Request, Response};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "lixun", about = "Spotlight-like launcher for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Toggle the launcher window.
    Toggle,
    /// Show the launcher window.
    Show,
    /// Hide the launcher window.
    Hide,
    /// Search (for CLI usage, not GUI).
    Search {
        query: String,
        #[arg(short, long, default_value_t = 20)]
        limit: u32,
        /// Print a per-hit score breakdown (tantivy, category, prefix,
        /// acronym, recency, coord, stage-2) under each result. Useful
        /// when tuning RankingConfig knobs or debugging ordering.
        #[arg(long)]
        explain: bool,
    },
    /// Trigger a reindex.
    Reindex {
        #[arg(num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Show daemon status.
    Status {
        /// Print only the OCR queue + worker observability block.
        #[arg(long)]
        ocr: bool,
    },
}

async fn send_request(req: Request) -> Result<Response> {
    let socket_path = lixun_ipc::socket_path();

    let mut stream = UnixStream::connect(&socket_path).await.context(format!(
        "lixund not running; start with: systemctl --user start lixund\n(socket: {:?})",
        socket_path
    ))?;

    let json = serde_json::to_vec(&req)?;
    let total_len = (2 + json.len()) as u32;
    let mut buf = BytesMut::with_capacity(4 + 2 + json.len());
    buf.put_u32(total_len);
    buf.put_u16(PROTOCOL_VERSION);
    buf.put_slice(&json);
    stream.write_all(&buf).await?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let resp_len = u32::from_be_bytes(header) as usize;
    if resp_len < 2 {
        anyhow::bail!("response frame too short");
    }
    let mut version_buf = [0u8; 2];
    stream.read_exact(&mut version_buf).await?;
    let mut resp_buf = vec![0u8; resp_len - 2];
    stream.read_exact(&mut resp_buf).await?;

    let resp: Response = serde_json::from_slice(&resp_buf)?;
    Ok(resp)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Toggle => {
            let resp = send_request(Request::Toggle).await?;
            handle_response(resp, false);
        }
        Commands::Show => {
            let resp = send_request(Request::Show).await?;
            handle_response(resp, false);
        }
        Commands::Hide => {
            let resp = send_request(Request::Hide).await?;
            handle_response(resp, false);
        }
        Commands::Search {
            query,
            limit,
            explain,
        } => {
            let resp = send_request(Request::Search {
                q: query,
                limit,
                explain,
            })
            .await?;
            handle_response(resp, false);
        }
        Commands::Reindex { paths } => {
            let resp = send_request(Request::Reindex { paths }).await?;
            if matches!(resp, Response::Status { .. }) {
                println!("Reindex started in background. Check progress with: lixun status");
            } else {
                handle_response(resp, false);
            }
        }
        Commands::Status { ocr } => {
            let resp = send_request(Request::Status).await?;
            handle_response(resp, ocr);
        }
    }

    Ok(())
}

fn handle_response(resp: Response, ocr_only: bool) {
    match resp {
        Response::Ok => {}
        Response::Hits(hits) => {
            for hit in hits {
                println!(
                    "{:.2} | {:?} | {} | {}",
                    hit.score, hit.category, hit.title, hit.subtitle
                );
            }
        }
        Response::HitsWithExtras {
            hits,
            calculation,
            explanations,
        } => {
            if let Some(c) = calculation {
                println!("= {} = {}", c.expr, c.result);
            }
            for (i, hit) in hits.iter().enumerate() {
                println!(
                    "{:.2} | {:?} | {} | {}",
                    hit.score, hit.category, hit.title, hit.subtitle
                );
                if let Some(expl) = explanations.get(i)
                    && !expl.is_empty()
                {
                    println!("    {}", expl);
                }
            }
        }
        Response::HitsWithExtrasV3 {
            hits,
            calculation,
            top_hit: _,
            explanations,
        } => {
            if let Some(c) = calculation {
                println!("= {} = {}", c.expr, c.result);
            }
            for (i, hit) in hits.iter().enumerate() {
                println!(
                    "{:.2} | {:?} | {} | {}",
                    hit.score, hit.category, hit.title, hit.subtitle
                );
                if let Some(expl) = explanations.get(i)
                    && !expl.is_empty()
                {
                    println!("    {}", expl);
                }
            }
        }
        Response::Status {
            indexed_docs,
            last_reindex,
            errors,
            watcher,
            writer,
            memory,
            reindex_in_progress,
            reindex_started,
            ocr,
        } => {
            if ocr_only {
                print!("{}", format_ocr_block(ocr.as_ref()));
                return;
            }
            println!("Indexed documents: {}", indexed_docs);
            println!("Last reindex: {:?}", last_reindex);
            println!("Errors: {}", errors);
            if reindex_in_progress {
                let started = reindex_started
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into());
                println!("Reindex: RUNNING (started {})", started);
            }
            if let Some(w) = watcher {
                println!(
                    "Watcher: {} directories ({} excluded, {} errors, {} overflow events)",
                    w.directories, w.excluded, w.errors, w.overflow_events
                );
            }
            if let Some(w) = writer {
                println!(
                    "Writer: {} commits, last latency {} ms, generation {}",
                    w.commits, w.last_commit_latency_ms, w.generation
                );
            }
            if let Some(m) = memory {
                println!(
                    "Memory: RSS {}, VmPeak {}, VmSize {}, VmSwap {}",
                    format_bytes(m.rss_bytes),
                    format_bytes(m.vm_peak_bytes),
                    format_bytes(m.vm_size_bytes),
                    format_bytes(m.vm_swap_bytes),
                );
            }
        }
        Response::Visibility { visible } => {
            println!("{}", if visible { "show" } else { "hide" });
        }
        Response::Queries(queries) => {
            for q in queries {
                println!("{}", q);
            }
        }
        Response::Error(msg) => {
            eprintln!("Error: {}", msg);
        }
    }
}

fn format_ocr_block(ocr: Option<&lixun_ipc::OcrStats>) -> String {
    let Some(s) = ocr else {
        return "OCR: disabled\n".to_string();
    };
    let last = match s.last_drain_at {
        Some(ts) if ts > 0 => format_unix_ts(ts),
        _ => "never".to_string(),
    };
    format!(
        "OCR:\n  queue depth: {} (pending: {}, failed: {})\n  drained: {}\n  last drain: {}\n",
        s.queue_total, s.queue_pending, s.queue_failed, s.drained_total, last
    )
}

fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("ts={ts}"))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_ipc::OcrStats;

    #[test]
    fn format_ocr_block_reports_disabled_when_none() {
        assert_eq!(format_ocr_block(None), "OCR: disabled\n");
    }

    #[test]
    fn format_ocr_block_reports_never_when_sentinel() {
        let stats = OcrStats {
            queue_total: 12,
            queue_pending: 10,
            queue_failed: 2,
            drained_total: 0,
            last_drain_at: None,
        };
        let out = format_ocr_block(Some(&stats));
        assert!(out.contains("queue depth: 12 (pending: 10, failed: 2)"));
        assert!(out.contains("drained: 0"));
        assert!(out.contains("last drain: never"));
    }

    #[test]
    fn format_ocr_block_renders_timestamp_when_present() {
        let stats = OcrStats {
            queue_total: 3,
            queue_pending: 1,
            queue_failed: 2,
            drained_total: 77,
            last_drain_at: Some(1_700_000_000),
        };
        let out = format_ocr_block(Some(&stats));
        assert!(out.contains("drained: 77"));
        assert!(!out.contains("last drain: never"));
        assert!(out.contains("last drain: 2023-"));
    }

    #[test]
    fn format_ocr_block_treats_zero_timestamp_as_never() {
        let stats = OcrStats {
            queue_total: 0,
            queue_pending: 0,
            queue_failed: 0,
            drained_total: 0,
            last_drain_at: Some(0),
        };
        assert!(format_ocr_block(Some(&stats)).contains("last drain: never"));
    }
}
