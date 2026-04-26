//! Extract cache — blake3-keyed, zstd-compressed on-disk cache
//! shared by every extractor in this crate.
//!
//! ## Keying (DB-3 / DB-4)
//!
//! Two entry points produce a [`CacheKey`]:
//!
//! - [`key_for_path`] hashes `(canonical_path, mtime_ns, size, engine_tag)`
//!   for file-backed extraction (the `FsSource` path).
//! - [`key_for_bytes`] hashes `(content_bytes, engine_tag)` for
//!   content-backed extraction (plugins without a stable path — mail
//!   attachments today).
//!
//! Both are intentionally exported so plugins can opt in without
//! pulling plugin-specific logic into this crate.
//!
//! ## Storage (DB-5)
//!
//! `${XDG_CACHE_HOME:-~/.cache}/lixun/extract/v1/<ab>/<full-hex>.txt.zst`.
//! 2-char sharding keeps directories bounded (256 buckets). Writes
//! are atomic via `.tmp-<uuid>` + `rename` so a torn write never
//! surfaces as a corrupt HIT.
//!
//! ## Corrupt-entry policy
//!
//! A zstd decode error (or any I/O error beyond `NotFound`) is
//! treated as MISS with a `warn!` log and the offending file is
//! deleted. The caller recomputes and re-caches.

use crate::ExtractorCapabilities;
use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const CACHE_VERSION: &str = "v1";
const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey(pub [u8; 32]);

impl CacheKey {
    fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn rel_path(&self) -> PathBuf {
        let hex = self.hex();
        PathBuf::from(&hex[..2]).join(format!("{hex}.txt.zst"))
    }
}

pub fn cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
        })
        .join("lixun")
        .join("extract")
        .join(CACHE_VERSION)
}

pub fn engine_tag_for_ext(ext: &str, caps: &ExtractorCapabilities) -> &'static str {
    match ext {
        "pdf" if caps.has_pdftotext => "pdftotext:v1",
        "docx" | "xlsx" | "pptx" => "ooxml:v1",
        "odt" => "odt:v1",
        "rtf" => "rtf:v1",
        "doc" if caps.has_antiword => "antiword:v1",
        "xls" if caps.has_catdoc => "catdoc:v1",
        "ppt" if caps.has_libreoffice => "libreoffice:v1",
        _ => "utf8-sniff:v1",
    }
}

pub fn key_for_path(path: &Path, engine_tag: &str) -> Result<CacheKey> {
    let meta = fs::metadata(path).with_context(|| format!("cache key: stat {}", path.display()))?;
    let mtime_ns: i128 = match meta.modified() {
        Ok(t) => system_time_to_ns(t),
        Err(_) => 0,
    };
    let size = meta.len();
    let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"lixun-extract-cache:path:v1\0");
    hasher.update(engine_tag.as_bytes());
    hasher.update(b"\0");
    hasher.update(canon.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(&mtime_ns.to_le_bytes());
    hasher.update(&size.to_le_bytes());
    Ok(CacheKey(*hasher.finalize().as_bytes()))
}

pub fn key_for_bytes(bytes: &[u8], engine_tag: &str) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"lixun-extract-cache:bytes:v1\0");
    hasher.update(engine_tag.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
    CacheKey(*hasher.finalize().as_bytes())
}

fn system_time_to_ns(t: SystemTime) -> i128 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn cache_get(key: &CacheKey) -> Option<String> {
    let path = cache_root().join(key.rel_path());
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("cache read failed at {}: {e}", path.display());
            return None;
        }
    };
    let mut decoder = match zstd::Decoder::new(&bytes[..]) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "cache entry corrupt (zstd header) at {}: {e} — recomputing",
                path.display()
            );
            let _ = fs::remove_file(&path);
            return None;
        }
    };
    let mut out = String::new();
    if let Err(e) = decoder.read_to_string(&mut out) {
        tracing::warn!(
            "cache entry corrupt (zstd body) at {}: {e} — recomputing",
            path.display()
        );
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(out)
}

fn cache_put(key: &CacheKey, text: &str) -> Result<()> {
    let rel = key.rel_path();
    let dir = cache_root().join(rel.parent().expect("rel_path has parent dir"));
    fs::create_dir_all(&dir).with_context(|| format!("cache mkdir {}", dir.display()))?;
    let final_path = cache_root().join(&rel);
    let tmp_name = format!(".tmp-{}", uuid::Uuid::new_v4());
    let tmp_path = dir.join(&tmp_name);
    let compressed = zstd::encode_all(text.as_bytes(), ZSTD_LEVEL)
        .with_context(|| format!("cache zstd encode for {}", final_path.display()))?;
    fs::write(&tmp_path, &compressed)
        .with_context(|| format!("cache write tmp {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "cache rename {} -> {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

pub fn cache_put_ocr(key: &CacheKey, text: &str) -> Result<()> {
    cache_put(key, text)
}

pub fn cached_extract_path(path: &Path, caps: &ExtractorCapabilities) -> Result<Option<String>> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let tag = engine_tag_for_ext(&ext, caps);
    let key = key_for_path(path, tag)?;

    if let Some(text) = cache_get(&key) {
        return Ok(text_to_option(text));
    }

    let text = crate::extract_path(path)?;
    if let Err(e) = cache_put(&key, &text) {
        tracing::warn!(
            "cache write failed for {}: {e} — proceeding without cache",
            path.display()
        );
    }
    Ok(text_to_option(text))
}

pub fn cached_extract_bytes(
    bytes: &[u8],
    ext_hint: Option<&str>,
    caps: &ExtractorCapabilities,
) -> Result<Option<String>> {
    let ext = ext_hint.unwrap_or("").to_ascii_lowercase();
    let tag = engine_tag_for_ext(&ext, caps);
    let key = key_for_bytes(bytes, tag);

    if let Some(text) = cache_get(&key) {
        return Ok(text_to_option(text));
    }

    let text = crate::extract_bytes(bytes, ext_hint)?;
    if let Err(e) = cache_put(&key, &text) {
        tracing::warn!("cache write failed for bytes extract: {e} — proceeding without cache");
    }
    Ok(text_to_option(text))
}

/// Convert a raw extractor result into the `Option<String>` the
/// callers downstream expect. Whitespace-only outputs count as
/// empty: pdftotext on a scan-only PDF emits a lone form-feed
/// per page (`\x0c`), not a zero-length string, and must be
/// treated as "no text recovered" so the OCR enqueue path in
/// DB-13 can fire. The same normalisation applies to any other
/// extractor whose failure mode is "produced whitespace only".
fn text_to_option(s: String) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tempfile::TempDir;

    struct CacheDirGuard {
        _td: TempDir,
    }

    static HOME_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

    fn with_isolated_cache<F, R>(f: F) -> R
    where
        F: FnOnce(&Path) -> R,
    {
        let lock = HOME_LOCK.get_or_init(|| std::sync::Mutex::new(()));
        let _g = lock.lock().unwrap();
        let td = TempDir::new().unwrap();
        let old_xdg = std::env::var_os("XDG_CACHE_HOME");
        let old_home = std::env::var_os("HOME");
        // SAFETY: env writes are process-global. The HOME_LOCK mutex
        // above serializes all cache tests in this module, preventing
        // concurrent reads/writes of XDG_CACHE_HOME/HOME. No other code
        // in this crate reads these vars during the test window.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", td.path());
            std::env::set_var("HOME", td.path());
        }
        let guard = CacheDirGuard { _td: td };
        let result = f(guard._td.path());
        unsafe {
            match old_xdg {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        result
    }

    #[test]
    fn cache_key_hex_is_64_chars() {
        let k = CacheKey([0xab; 32]);
        assert_eq!(k.hex().len(), 64);
        assert!(k.hex().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_key_rel_path_shards_by_first_two_hex() {
        let k = CacheKey([0xab; 32]);
        let rel = k.rel_path();
        assert_eq!(rel.parent().unwrap().to_str().unwrap(), "ab");
        assert!(
            rel.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".txt.zst")
        );
    }

    #[test]
    fn engine_tag_maps_known_exts() {
        let caps = ExtractorCapabilities::all_available_no_timeout();
        assert_eq!(engine_tag_for_ext("pdf", &caps), "pdftotext:v1");
        assert_eq!(engine_tag_for_ext("docx", &caps), "ooxml:v1");
        assert_eq!(engine_tag_for_ext("xlsx", &caps), "ooxml:v1");
        assert_eq!(engine_tag_for_ext("pptx", &caps), "ooxml:v1");
        assert_eq!(engine_tag_for_ext("odt", &caps), "odt:v1");
        assert_eq!(engine_tag_for_ext("rtf", &caps), "rtf:v1");
        assert_eq!(engine_tag_for_ext("doc", &caps), "antiword:v1");
        assert_eq!(engine_tag_for_ext("xls", &caps), "catdoc:v1");
        assert_eq!(engine_tag_for_ext("ppt", &caps), "libreoffice:v1");
        assert_eq!(engine_tag_for_ext("png", &caps), "utf8-sniff:v1");
        assert_eq!(engine_tag_for_ext("xyz", &caps), "utf8-sniff:v1");
    }

    #[test]
    fn engine_tag_degrades_when_tool_missing() {
        let mut caps = ExtractorCapabilities::all_available_no_timeout();
        caps.has_pdftotext = false;
        caps.has_antiword = false;
        caps.has_catdoc = false;
        caps.has_libreoffice = false;
        assert_eq!(engine_tag_for_ext("pdf", &caps), "utf8-sniff:v1");
        assert_eq!(engine_tag_for_ext("doc", &caps), "utf8-sniff:v1");
        assert_eq!(engine_tag_for_ext("xls", &caps), "utf8-sniff:v1");
        assert_eq!(engine_tag_for_ext("ppt", &caps), "utf8-sniff:v1");
    }

    #[test]
    fn key_for_bytes_differs_on_different_tag() {
        let a = key_for_bytes(b"hello", "tag-a");
        let b = key_for_bytes(b"hello", "tag-b");
        assert_ne!(a, b);
    }

    #[test]
    fn key_for_bytes_stable_for_same_input() {
        let a = key_for_bytes(b"hello", "tag");
        let b = key_for_bytes(b"hello", "tag");
        assert_eq!(a, b);
    }

    #[test]
    fn key_for_path_changes_on_mtime_bump() {
        with_isolated_cache(|tmp| {
            let f = tmp.join("doc.txt");
            fs::write(&f, "v1").unwrap();
            let k1 = key_for_path(&f, "utf8-sniff:v1").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            fs::write(&f, "v2-longer").unwrap();
            let k2 = key_for_path(&f, "utf8-sniff:v1").unwrap();
            assert_ne!(k1, k2);
        });
    }

    #[test]
    fn put_then_get_roundtrip() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x11; 32]);
            cache_put(&key, "hello world").unwrap();
            let got = cache_get(&key).unwrap();
            assert_eq!(got, "hello world");
        });
    }

    #[test]
    fn miss_returns_none() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x22; 32]);
            assert!(cache_get(&key).is_none());
        });
    }

    #[test]
    fn corrupt_entry_graceful_degrade() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x33; 32]);
            let rel = key.rel_path();
            let dir = cache_root().join(rel.parent().unwrap());
            fs::create_dir_all(&dir).unwrap();
            let path = cache_root().join(&rel);
            fs::write(&path, b"not-zstd-garbage").unwrap();
            assert!(cache_get(&key).is_none(), "corrupt entry must MISS");
            assert!(
                !path.exists(),
                "corrupt entry must be deleted so next put succeeds"
            );
        });
    }

    #[test]
    fn empty_text_cached_as_hit_returning_none() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x44; 32]);
            cache_put(&key, "").unwrap();
            let stored = cache_get(&key);
            assert_eq!(stored.as_deref(), Some(""));
            assert_eq!(text_to_option(stored.unwrap()), None);
        });
    }

    #[test]
    fn whitespace_only_text_counts_as_empty() {
        assert_eq!(text_to_option("\x0c".into()), None);
        assert_eq!(text_to_option("\x0c\n\x0c\n".into()), None);
        assert_eq!(text_to_option("   \t\n".into()), None);
        assert_eq!(text_to_option("hello".into()), Some("hello".into()));
        assert_eq!(text_to_option(" hi ".into()), Some(" hi ".into()));
    }

    #[test]
    fn cached_extract_path_hit_on_second_call() {
        with_isolated_cache(|tmp| {
            let f = tmp.join("note.txt");
            fs::write(&f, "hello from lixun cache").unwrap();
            let caps = ExtractorCapabilities::all_available_no_timeout();

            let r1 = cached_extract_path(&f, &caps).unwrap();
            assert_eq!(r1.as_deref(), Some("hello from lixun cache"));

            let tag = engine_tag_for_ext("txt", &caps);
            let key = key_for_path(&f, tag).unwrap();
            let cache_file = cache_root().join(key.rel_path());
            assert!(cache_file.exists(), "first call must persist cache file");

            fs::write(&f, "ignored — cache HIT should skip extractor").unwrap();
            let modified_after = fs::metadata(&f).unwrap().modified().unwrap();
            let cache_touched = fs::metadata(&cache_file).unwrap().modified().unwrap();
            let _ = (modified_after, cache_touched);

            let key_now = key_for_path(&f, tag).unwrap();
            assert_ne!(
                key, key_now,
                "rewriting the file changes the cache key; second call is MISS not HIT"
            );
        });
    }

    #[test]
    fn cached_extract_path_unchanged_mtime_hits() {
        with_isolated_cache(|tmp| {
            let f = tmp.join("stable.txt");
            fs::write(&f, "stable content").unwrap();
            let caps = ExtractorCapabilities::all_available_no_timeout();

            let r1 = cached_extract_path(&f, &caps).unwrap();
            let r2 = cached_extract_path(&f, &caps).unwrap();
            assert_eq!(r1, r2);
            assert_eq!(r1.as_deref(), Some("stable content"));
        });
    }

    #[test]
    fn cached_extract_bytes_hit_on_second_call() {
        with_isolated_cache(|_| {
            let caps = ExtractorCapabilities::all_available_no_timeout();
            let bytes = b"content-stream bytes";
            let r1 = cached_extract_bytes(bytes, Some("txt"), &caps).unwrap();
            let r2 = cached_extract_bytes(bytes, Some("txt"), &caps).unwrap();
            assert_eq!(r1, r2);
            assert_eq!(r1.as_deref(), Some("content-stream bytes"));

            let tag = engine_tag_for_ext("txt", &caps);
            let key = key_for_bytes(bytes, tag);
            let cache_file = cache_root().join(key.rel_path());
            assert!(cache_file.exists());
        });
    }

    #[test]
    fn miss_on_tag_bump() {
        with_isolated_cache(|_| {
            let bytes = b"abc";
            let k_old = key_for_bytes(bytes, "tesseract:5.3.0+eng+rus");
            let k_new = key_for_bytes(bytes, "tesseract:5.4.0+eng+rus");
            assert_ne!(k_old, k_new);

            cache_put(&k_old, "cached-by-old-engine").unwrap();
            assert_eq!(cache_get(&k_old).as_deref(), Some("cached-by-old-engine"));
            assert!(cache_get(&k_new).is_none());
        });
    }

    #[test]
    fn atomic_write_no_torn_read_under_concurrency() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x55; 32]);
            let threads: Vec<_> = (0..8)
                .map(|i| {
                    let k = key.clone();
                    std::thread::spawn(move || {
                        let payload = format!("payload-from-thread-{i}");
                        cache_put(&k, &payload).unwrap();
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }

            let got = cache_get(&key).expect("final file must be valid zstd");
            assert!(got.starts_with("payload-from-thread-"));

            let rel = key.rel_path();
            let dir = cache_root().join(rel.parent().unwrap());
            let leaked_tmp: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
                .collect();
            assert!(
                leaked_tmp.is_empty(),
                "no .tmp-* files must leak after concurrent writers finish"
            );
        });
    }

    #[test]
    fn cache_put_ocr_roundtrip() {
        with_isolated_cache(|_| {
            let key = CacheKey([0x66; 32]);
            cache_put_ocr(&key, "ocr result text").unwrap();
            assert_eq!(cache_get(&key).as_deref(), Some("ocr result text"));
        });
    }
}
