//! lixun-cli — thin CLI client for the Lixun daemon.

use anyhow::{Context, Result};
use bytes::{BufMut, BytesMut};
use clap::{Arg, ArgMatches, Command};
use lixun_core::SystemImpact;
use lixun_ipc::{ImpactProfileWire, PROTOCOL_VERSION, Request, Response};
use lixun_mutation::CliVerb;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const BUILTIN_VERBS: &[&str] = &[
    "toggle", "show", "hide", "search", "reindex", "status", "impact",
];

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

/// Recursively translate one [`CliVerb`] into the [`Command`] tree
/// that clap will parse. Verb names that collide with built-in
/// subcommands are dropped with a warning so the host always wins
/// — preserves the AGENTS.md §1 invariant that built-in verbs
/// remain stable regardless of which plugins are loaded.
fn build_command_from_verb(verb: &CliVerb) -> Command {
    let mut cmd = Command::new(leak_string(&verb.name)).about(verb.about.clone());
    for sub in &verb.subverbs {
        cmd = cmd.subcommand(build_command_from_verb(sub));
    }
    for arg in &verb.args {
        let name: &'static str = leak_string(&arg.name);
        cmd = cmd.arg(
            Arg::new(name)
                .long(name)
                .required(arg.required)
                .help(arg.help.clone()),
        );
    }
    cmd
}

/// Clap's builder takes `&'static str` for ids and long flags. Verb
/// names arrive at runtime over IPC, so we leak each unique string
/// once into the program's global allocation table — bounded by the
/// daemon's manifest size, which is itself bounded by the registered
/// plugin count.
fn leak_string(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn root_command(plugin_verbs: &[CliVerb]) -> Command {
    let mut root = Command::new("lixun-cli")
        .about("Spotlight-like launcher for Linux")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(Command::new("toggle").about("Toggle the launcher window."))
        .subcommand(Command::new("show").about("Show the launcher window."))
        .subcommand(Command::new("hide").about("Hide the launcher window."))
        .subcommand(
            Command::new("search")
                .about("Search (for CLI usage, not GUI).")
                .arg(Arg::new("query").required(true))
                .arg(
                    Arg::new("limit")
                        .short('l')
                        .long("limit")
                        .default_value("20")
                        .value_parser(clap::value_parser!(u32)),
                )
                .arg(
                    Arg::new("explain")
                        .long("explain")
                        .action(clap::ArgAction::SetTrue)
                        .help("Print a per-hit score breakdown under each result."),
                ),
        )
        .subcommand(
            Command::new("reindex").about("Trigger a reindex.").arg(
                Arg::new("paths")
                    .num_args(0..)
                    .value_parser(clap::value_parser!(PathBuf)),
            ),
        )
        .subcommand(
            Command::new("status").about("Show daemon status.").arg(
                Arg::new("ocr")
                    .long("ocr")
                    .action(clap::ArgAction::SetTrue)
                    .help("Print only the OCR queue + worker observability block."),
            ),
        )
        .subcommand(
            Command::new("impact")
                .about("Get/set the global system-impact preset.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(Command::new("get").about("Print the current impact level."))
                .subcommand(
                    Command::new("set")
                        .about("Switch to a new impact level.")
                        .arg(
                            Arg::new("level")
                                .required(true)
                                .value_parser(clap::builder::PossibleValuesParser::new([
                                    "unlimited",
                                    "high",
                                    "medium",
                                    "low",
                                ]))
                                .help("One of: unlimited, high, medium, low."),
                        )
                        .arg(
                            Arg::new("persist")
                                .long("persist")
                                .action(clap::ArgAction::SetTrue)
                                .help("Also write the level into ~/.config/lixun/config.toml."),
                        ),
                )
                .subcommand(
                    Command::new("explain")
                        .about("Print the resolved profile knob table for the current level."),
                ),
        );

    for verb in plugin_verbs {
        if BUILTIN_VERBS.contains(&verb.name.as_str()) {
            eprintln!(
                "warning: plugin verb '{}' shadowed by built-in; ignoring",
                verb.name
            );
            continue;
        }
        root = root.subcommand(build_command_from_verb(verb));
    }
    root
}

/// Walk the matched subcommand chain (`A -> B -> C`) producing the
/// `verb_path` slice the daemon expects and a JSON object of every
/// leaf-level argument keyed by name. Booleans and integers are
/// preserved as JSON booleans / numbers; everything else stringifies.
fn collect_plugin_invocation(head: &str, matches: &ArgMatches) -> (Vec<String>, serde_json::Value) {
    let mut path = vec![head.to_string()];
    let mut current = matches;
    while let Some((sub, sub_m)) = current.subcommand() {
        path.push(sub.to_string());
        current = sub_m;
    }
    let mut args = serde_json::Map::new();
    for id in current.ids() {
        let name = id.as_str().to_string();
        if let Some(value) = current.get_one::<String>(&name) {
            args.insert(name, serde_json::Value::String(value.clone()));
        } else if let Some(value) = current.get_one::<bool>(&name) {
            args.insert(name, serde_json::Value::Bool(*value));
        } else if let Some(value) = current.get_one::<u64>(&name) {
            args.insert(name, serde_json::Value::Number((*value).into()));
        }
    }
    (path, serde_json::Value::Object(args))
}

#[tokio::main]
async fn main() -> Result<()> {
    let plugin_verbs: Vec<CliVerb> = match send_request(Request::EnumeratePlugins).await {
        Ok(Response::PluginManifest(manifest)) => manifest.verbs,
        Ok(Response::PluginError(msg)) => {
            eprintln!("warning: plugin enumeration error: {msg}");
            Vec::new()
        }
        Ok(_) => Vec::new(),
        Err(_) => Vec::new(),
    };

    let cmd = root_command(&plugin_verbs);
    let matches = cmd.get_matches();
    let (sub_name, sub_matches) = matches
        .subcommand()
        .expect("clap subcommand_required is set");

    match sub_name {
        "toggle" => handle_response(send_request(Request::Toggle).await?, false),
        "show" => handle_response(send_request(Request::Show).await?, false),
        "hide" => handle_response(send_request(Request::Hide).await?, false),
        "search" => {
            let query = sub_matches
                .get_one::<String>("query")
                .cloned()
                .unwrap_or_default();
            let limit = *sub_matches.get_one::<u32>("limit").unwrap_or(&20);
            let explain = sub_matches.get_flag("explain");
            let resp = send_request(Request::Search {
                q: query,
                limit,
                explain,
            })
            .await?;
            handle_response(resp, false);
        }
        "reindex" => {
            let paths: Vec<PathBuf> = sub_matches
                .get_many::<PathBuf>("paths")
                .map(|it| it.cloned().collect())
                .unwrap_or_default();
            let resp = send_request(Request::Reindex { paths }).await?;
            if matches!(resp, Response::Status { .. }) {
                println!("Reindex started in background. Check progress with: lixun-cli status");
            } else {
                handle_response(resp, false);
            }
        }
        "status" => {
            let ocr_only = sub_matches.get_flag("ocr");
            let resp = send_request(Request::Status).await?;
            handle_response(resp, ocr_only);
        }
        "impact" => {
            let (impact_sub, impact_m) = sub_matches
                .subcommand()
                .expect("clap subcommand_required is set");
            match impact_sub {
                "get" => {
                    let resp = send_request(Request::ImpactGet).await?;
                    handle_impact_response(resp, ImpactRender::Get);
                }
                "explain" => {
                    let resp = send_request(Request::ImpactExplain).await?;
                    handle_impact_response(resp, ImpactRender::Explain);
                }
                "set" => {
                    let level_str = impact_m
                        .get_one::<String>("level")
                        .expect("clap required arg")
                        .clone();
                    let level = SystemImpact::from_str(&level_str).map_err(|e| {
                        anyhow::anyhow!("error: invalid level \"{}\"; {}", level_str, e)
                    })?;
                    let persist = impact_m.get_flag("persist");
                    let resp = send_request(Request::ImpactSet { level, persist }).await?;
                    handle_impact_response(resp, ImpactRender::Set);
                }
                other => anyhow::bail!("unknown impact subcommand: {}", other),
            }
        }
        other => {
            let (verb_path, args) = collect_plugin_invocation(other, sub_matches);
            let resp = send_request(Request::PluginCommand { verb_path, args }).await?;
            handle_plugin_response(resp);
        }
    }

    Ok(())
}

enum ImpactRender {
    Get,
    Set,
    Explain,
}

fn handle_impact_response(resp: Response, render: ImpactRender) {
    match resp {
        Response::ImpactSnapshot {
            level,
            profile,
            applied_hot,
            requires_restart,
            persisted,
        } => match render {
            ImpactRender::Get => {
                println!("level: {}", level);
            }
            ImpactRender::Set => {
                println!("level: {}", level);
                if applied_hot.is_empty() {
                    println!("applied_hot: (none)");
                } else {
                    println!("applied_hot:");
                    for k in &applied_hot {
                        println!("  {}", k);
                    }
                }
                if requires_restart.is_empty() {
                    println!("requires_restart: (none)");
                } else {
                    println!("requires_restart:");
                    for k in &requires_restart {
                        println!("  {}", k);
                    }
                }
                if persisted {
                    let path = dirs::config_dir()
                        .map(|p| p.join("lixun/config.toml"))
                        .unwrap_or_else(|| PathBuf::from("~/.config/lixun/config.toml"));
                    println!("persisted to: {}", path.display());
                }
            }
            ImpactRender::Explain => {
                print!("{}", format_impact_table(&profile));
            }
        },
        Response::Error(msg) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        other => {
            eprintln!("Error: unexpected response shape: {:?}", other);
            std::process::exit(1);
        }
    }
}

fn format_impact_table(p: &ImpactProfileWire) -> String {
    let rows: [(&str, String); 19] = [
        ("level", p.level.to_string()),
        ("tokio_worker_threads", p.tokio_worker_threads.to_string()),
        ("onnx_intra_threads", p.onnx_intra_threads.to_string()),
        ("onnx_inter_threads", p.onnx_inter_threads.to_string()),
        ("rayon_threads", p.rayon_threads.to_string()),
        ("tantivy_heap_bytes", p.tantivy_heap_bytes.to_string()),
        ("tantivy_num_threads", p.tantivy_num_threads.to_string()),
        ("embed_batch_hint", p.embed_batch_hint.to_string()),
        (
            "embed_concurrency_hint",
            match p.embed_concurrency_hint {
                Some(n) => n.to_string(),
                None => "none".to_string(),
            },
        ),
        ("ocr_jobs_per_tick", p.ocr_jobs_per_tick.to_string()),
        ("ocr_adaptive_throttle", p.ocr_adaptive_throttle.to_string()),
        ("ocr_nice_level", p.ocr_nice_level.to_string()),
        ("ocr_io_class_idle", p.ocr_io_class_idle.to_string()),
        (
            "ocr_worker_interval_secs",
            p.ocr_worker_interval_secs.to_string(),
        ),
        (
            "extract_cache_max_bytes",
            p.extract_cache_max_bytes.to_string(),
        ),
        ("max_file_size_bytes", p.max_file_size_bytes.to_string()),
        ("gloda_batch_size", p.gloda_batch_size.to_string()),
        ("daemon_nice", p.daemon_nice.to_string()),
        ("daemon_sched_idle", p.daemon_sched_idle.to_string()),
    ];
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!("level: {}\n", p.level));
    out.push_str(&format!(
        "{:width$}  {}\n",
        "knob",
        "value",
        width = key_width
    ));
    out.push_str(&format!(
        "{:-<width$}  {:-<10}\n",
        "",
        "",
        width = key_width
    ));
    for (k, v) in &rows {
        out.push_str(&format!("{:width$}  {}\n", k, v, width = key_width));
    }
    out
}

fn handle_plugin_response(resp: Response) {
    match resp {
        Response::PluginResult(value) => match value {
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => println!("{s}"),
            other => match serde_json::to_string_pretty(&other) {
                Ok(s) => println!("{s}"),
                Err(e) => eprintln!("Error: failed to render plugin response: {e}"),
            },
        },
        Response::PluginError(msg) => {
            eprintln!("Error: {msg}");
            std::process::exit(1);
        }
        Response::Error(msg) => {
            eprintln!("Error: {msg}");
            std::process::exit(1);
        }
        other => {
            eprintln!("Error: unexpected response shape: {:?}", other);
            std::process::exit(1);
        }
    }
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
        Response::PluginManifest(_) | Response::PluginResult(_) | Response::PluginError(_) => {
            eprintln!("Error: plugin response routed to built-in handler");
        }
        Response::ImpactSnapshot { .. } => {
            eprintln!("Error: impact response routed to built-in handler");
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
