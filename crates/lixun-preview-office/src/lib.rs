//! Office-document preview plugin.
//!
//! Covers docx/xlsx/pptx/odt/ods/odp/doc/xls/ppt/rtf. LibreOffice
//! in headless mode is the conversion engine — we pay the 1–15 s
//! first-page-to-PNG cost, cache the result, and then reuse the
//! cached PNG on every subsequent preview.
//!
//! # Why this plugin is the async case
//!
//! `soffice --headless --convert-to png` takes seconds, not
//! milliseconds. The plan's G2.8 Decision 6 forbids blocking the
//! GTK main thread in `build()` for >50 ms. This plugin is the
//! first tier-2 plugin that has to honour the placeholder +
//! worker pattern:
//!
//! 1. `build()` returns a `gtk::Box` with a spinner + "converting…"
//!    label immediately.
//! 2. A background thread (`std::thread::spawn`) runs the soffice
//!    command with a 15 s wall-clock timeout. On success it writes
//!    the PNG to the cache.
//! 3. The thread pushes the result onto an `async_channel`.
//! 4. A `glib::MainContext::spawn_future_local` task awaits the
//!    channel, then swaps the placeholder for a `gtk::Picture`
//!    pointing at the PNG (or an error label on failure).
//!
//! The `async_channel` + `spawn_future_local` combination is the
//! gtk4-rs 0.9+ replacement for the removed
//! `glib::MainContext::channel`, confirmed by the librarian
//! research we did in G2.8 pre-planning.
//!
//! # Cache
//!
//! Key: SHA-256(`path_bytes || "\0" || mtime_nanos`) hex, stored
//! as `<cache_dir>/office/<key>.png` where cache_dir comes from
//! the shared `PreviewConfig.cache_dir`. Hit rate is high for the
//! typical "open the same slide deck three times" UX.
//!
//! # Multi-page
//!
//! `soffice --convert-to png` emits only page 1. The plan flags
//! this as a documented v1 limitation; do not fight LibreOffice
//! over it here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_channel::{Receiver, Sender};
use gtk::glib;
use gtk::prelude::*;
use lixun_core::{Action, Hit};
use lixun_preview::{PreviewPlugin, PreviewPluginCfg, PreviewPluginEntry};
use sha2::{Digest, Sha256};

const CONVERT_TIMEOUT: Duration = Duration::from_secs(15);

const STRONG_EXTENSIONS: &[&str] = &[
    "docx", "xlsx", "pptx", "odt", "ods", "odp", "doc", "xls", "ppt", "rtf",
];

pub struct OfficePreview;

impl PreviewPlugin for OfficePreview {
    fn id(&self) -> &'static str {
        "office"
    }

    fn match_score(&self, hit: &Hit) -> u32 {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path,
            _ => return 0,
        };
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| STRONG_EXTENSIONS.iter().any(|&s| s.eq_ignore_ascii_case(e)))
        {
            return 75;
        }
        0
    }

    fn build(&self, hit: &Hit, _cfg: &PreviewPluginCfg<'_>) -> anyhow::Result<gtk::Widget> {
        let path = match &hit.action {
            Action::OpenFile { path } | Action::ShowInFileManager { path } => path.clone(),
            _ => anyhow::bail!("office plugin: hit has no openable path"),
        };

        let soffice_bin = find_soffice();
        if soffice_bin.is_none() {
            return Ok(error_widget(
                "libreoffice not installed — install libreoffice-still or \
                 libreoffice-fresh and retry.",
            ));
        }

        let cache_dir = office_cache_dir();
        let cache_key = compute_cache_key(&path);
        let cache_file = cache_dir.join(format!("{}.png", cache_key));

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(150);
        stack.add_css_class("lixun-preview-office");

        let placeholder = build_placeholder();
        stack.add_named(&placeholder, Some("loading"));

        if cache_file.exists() {
            tracing::info!(
                "office: cache hit path={:?} key={} file={:?}",
                path,
                cache_key,
                cache_file
            );
            let picture = build_picture(&cache_file);
            stack.add_named(&picture, Some("rendered"));
            stack.set_visible_child_name("rendered");
            return Ok(stack.upcast());
        }

        stack.set_visible_child_name("loading");

        let (tx, rx): (Sender<ConvertOutcome>, Receiver<ConvertOutcome>) = async_channel::bounded(1);
        let soffice_bin = soffice_bin.unwrap();
        let target_path = cache_file.clone();
        let source_path = path.clone();
        std::thread::spawn(move || {
            let outcome = run_soffice_convert(&soffice_bin, &source_path, &target_path);
            let _ = tx.send_blocking(outcome);
        });

        let stack_weak = stack.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let Ok(outcome) = rx.recv().await else {
                return;
            };
            let Some(stack) = stack_weak.upgrade() else {
                return;
            };
            match outcome {
                ConvertOutcome::Ok(png_path) => {
                    let picture = build_picture(&png_path);
                    stack.add_named(&picture, Some("rendered"));
                    stack.set_visible_child_name("rendered");
                }
                ConvertOutcome::Err(reason) => {
                    let err = error_widget(&format!("conversion failed: {}", reason));
                    stack.add_named(&err, Some("error"));
                    stack.set_visible_child_name("error");
                }
            }
        });

        Ok(stack.upcast())
    }
}

enum ConvertOutcome {
    Ok(PathBuf),
    Err(String),
}

fn build_placeholder() -> gtk::Widget {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    vbox.set_halign(gtk::Align::Center);
    vbox.set_valign(gtk::Align::Center);
    vbox.set_hexpand(true);
    vbox.set_vexpand(true);

    let spinner = gtk::Spinner::new();
    spinner.set_size_request(48, 48);
    spinner.start();
    vbox.append(&spinner);

    let label = gtk::Label::new(Some("Converting document…"));
    label.add_css_class("lixun-preview-office-loading");
    vbox.append(&label);

    vbox.upcast()
}

fn build_picture(png_path: &Path) -> gtk::Widget {
    match gdk::Texture::from_filename(png_path) {
        Ok(texture) => {
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_vexpand(true);

            let scroll = gtk::ScrolledWindow::new();
            scroll.set_child(Some(&picture));
            scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
            scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
            scroll.upcast()
        }
        Err(e) => error_widget(&format!(
            "rendered PNG at {:?} failed to load: {}",
            png_path, e
        )),
    }
}

fn error_widget(msg: &str) -> gtk::Widget {
    let label = gtk::Label::new(Some(msg));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_margin_top(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label.set_margin_end(24);
    label.add_css_class("lixun-preview-office-error");
    label.upcast()
}

fn run_soffice_convert(bin: &Path, src: &Path, dest: &Path) -> ConvertOutcome {
    let Some(parent) = dest.parent() else {
        return ConvertOutcome::Err("cache path has no parent".into());
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        return ConvertOutcome::Err(format!("create cache dir {:?}: {}", parent, e));
    }

    let outdir = match tempdir_sibling(parent) {
        Ok(p) => p,
        Err(e) => return ConvertOutcome::Err(format!("tempdir: {}", e)),
    };

    let src_abs = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());

    let mut child = match Command::new(bin)
        .arg("--headless")
        .arg("--nologo")
        .arg("--nolockcheck")
        .arg("--norestore")
        .arg("--convert-to")
        .arg("png")
        .arg("--outdir")
        .arg(&outdir)
        .arg(&src_abs)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ConvertOutcome::Err(format!("spawn soffice: {}", e)),
    };

    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = done_tx.send(status);
    });

    let exit_status = match done_rx.recv_timeout(CONVERT_TIMEOUT) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return ConvertOutcome::Err(format!("wait soffice: {}", e)),
        Err(_) => {
            kill_pid(pid);
            return ConvertOutcome::Err(format!(
                "soffice exceeded {}s timeout — killed pid={}",
                CONVERT_TIMEOUT.as_secs(),
                pid
            ));
        }
    };

    if !exit_status.success() {
        return ConvertOutcome::Err(format!("soffice exited {:?}", exit_status.code()));
    }

    let produced = match find_png_in(&outdir) {
        Some(p) => p,
        None => {
            return ConvertOutcome::Err(format!(
                "soffice succeeded but produced no .png in {:?}",
                outdir
            ));
        }
    };

    if let Err(e) = std::fs::rename(&produced, dest).or_else(|_| {
        std::fs::copy(&produced, dest).map(|_| {
            std::fs::remove_file(&produced).ok();
        })
    }) {
        return ConvertOutcome::Err(format!(
            "moving {:?} -> {:?}: {}",
            produced, dest, e
        ));
    }

    let _ = std::fs::remove_dir_all(&outdir);
    ConvertOutcome::Ok(dest.to_path_buf())
}

fn find_png_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("png"))
        {
            return Some(path);
        }
    }
    None
}

fn tempdir_sibling(parent: &Path) -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = parent.join(format!("work-{}-{}", pid, nanos));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(250));
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn office_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"))
        .join("lixun/preview/office")
}

fn compute_cache_key(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    let mtime_nanos = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(mtime_nanos.to_be_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn find_soffice() -> Option<PathBuf> {
    for name in ["libreoffice", "soffice"] {
        if let Ok(path_env) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_env) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

inventory::submit! {
    PreviewPluginEntry {
        factory: || Box::new(OfficePreview),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lixun_core::paths::canonical_fs_doc_id;
    use lixun_core::{Category, DocId};

    fn file_hit(path: impl Into<PathBuf>) -> Hit {
        let path = path.into();
        Hit {
            id: DocId(canonical_fs_doc_id(&path)),
            category: Category::File,
            title: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            subtitle: path.display().to_string(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::OpenFile { path },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        }
    }

    #[test]
    fn docx_scores_seventyfive() {
        let hit = file_hit("/tmp/x.docx");
        assert_eq!(OfficePreview.match_score(&hit), 75);
    }

    #[test]
    fn xlsx_uppercase_scores_seventyfive() {
        let hit = file_hit("/tmp/data.XLSX");
        assert_eq!(OfficePreview.match_score(&hit), 75);
    }

    #[test]
    fn odt_scores_seventyfive() {
        let hit = file_hit("/tmp/doc.odt");
        assert_eq!(OfficePreview.match_score(&hit), 75);
    }

    #[test]
    fn txt_scores_zero() {
        let hit = file_hit("/tmp/x.txt");
        assert_eq!(OfficePreview.match_score(&hit), 0);
    }

    #[test]
    fn launch_scores_zero() {
        let hit = Hit {
            id: DocId("app:firefox".into()),
            category: Category::App,
            title: "Firefox".into(),
            subtitle: String::new(),
            icon_name: None,
            kind_label: None,
            score: 1.0,
            action: Action::Launch {
                exec: "firefox".into(),
                terminal: false,
                desktop_id: None,
                desktop_file: None,
                working_dir: None,
            },
            extract_fail: false,
            sender: None,
            recipients: None,
            body: None,
            secondary_action: None,
        };
        assert_eq!(OfficePreview.match_score(&hit), 0);
    }

    #[test]
    fn cache_key_depends_on_path_and_mtime() {
        let tmp = std::env::temp_dir().join(format!("lixun-office-cachekey-{}", std::process::id()));
        std::fs::write(&tmp, b"hello").unwrap();
        let first = compute_cache_key(&tmp);
        // Touch to bump mtime.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&tmp, b"hello (modified)").unwrap();
        let second = compute_cache_key(&tmp);
        std::fs::remove_file(&tmp).ok();
        assert_ne!(
            first, second,
            "cache key must change when mtime advances (got {} twice)",
            first
        );
    }

    #[test]
    fn cache_key_is_hex_and_long() {
        let tmp = std::env::temp_dir().join(format!("lixun-office-hex-{}", std::process::id()));
        std::fs::write(&tmp, b"x").unwrap();
        let k = compute_cache_key(&tmp);
        std::fs::remove_file(&tmp).ok();
        assert_eq!(k.len(), 64, "sha-256 hex is 64 chars, got {}", k.len());
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hex_encode_matches_known_vector() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x1a]), "00ff1a");
    }

    #[test]
    fn find_png_in_scans_directory() {
        let dir = std::env::temp_dir().join(format!("lixun-office-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), b"").unwrap();
        std::fs::write(dir.join("result.png"), b"\x89PNG").unwrap();
        let found = find_png_in(&dir);
        std::fs::remove_dir_all(&dir).ok();
        let found = found.expect("find_png_in must return a path");
        assert!(found.to_string_lossy().ends_with("result.png"));
    }

    #[test]
    fn find_png_in_empty_returns_none() {
        let dir = std::env::temp_dir().join(format!("lixun-office-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let found = find_png_in(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert!(found.is_none());
    }
}
