//! OCR via tesseract subprocess.
//!
//! Two entry points — [`ocr_image`] and [`ocr_pdf_pages`] — that turn
//! bytes into text. Both go through `tesseract` via the
//! [`shell::CommandRunner`] trait so tests can substitute a mock
//! runner without forking a real subprocess.
//!
//! This module is deliberately thin: no caching, no queue, no
//! enqueue decisions. Those responsibilities live in `cache.rs`
//! (DB-3/DB-5) and `ocr_queue.rs` + `ocr_tick.rs` (DB-10/DB-11).
//! The only I/O this module performs is the tesseract/pdftoppm
//! subprocess itself plus the tempfile scratch needed to hand the
//! input to those binaries.

use anyhow::{anyhow, Context, Result};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::shell::{self, CommandRunner, SystemRunner};
use crate::ExtractorCapabilities;

/// Extensions eligible for OCR enqueue (DB-13). Single source of
/// truth; the indexer consults this to decide whether an empty
/// extraction result should become a queued OCR job.
pub const OCR_CANDIDATES: &[&str] =
    &["pdf", "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];

/// `true` when `ext` (lowercase, no dot) is in [`OCR_CANDIDATES`].
pub fn is_ocr_candidate(ext: &str) -> bool {
    OCR_CANDIDATES.contains(&ext)
}

/// Cache engine tag for an OCR run. Part of the [`cache::CacheKey`]
/// input so a tesseract major/minor upgrade or a langs change
/// invalidates cached OCR results (DB-4).
///
/// Shape: `tesseract:<major>.<minor>+<lang1>+<lang2>+…`, langs sorted.
/// Capabilities stores `tesseract_langs` already sorted, so the
/// tag is stable for a given install.
pub fn engine_tag(caps: &ExtractorCapabilities, langs: &[String]) -> String {
    let version = tesseract_version_from_caps(caps);
    let mut sorted: Vec<&str> = langs.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let langs_part = sorted.join("+");
    format!("tesseract:{version}+{langs_part}")
}

fn tesseract_version_from_caps(_caps: &ExtractorCapabilities) -> &'static str {
    // Capabilities does not currently carry a tesseract version string.
    // Conservative default keeps the tag stable across patch-version
    // upgrades but forces a refresh when we bump this constant. A
    // future probe can promote this to a field without schema change
    // because the tag is rebuilt on every cache operation.
    "5"
}

/// OCR a single image. Returns `Ok(None)` when the image is smaller
/// than `min_side_px` on both axes (tesseract would return noise)
/// or when tesseract produces only whitespace.
///
/// Errors bubble up on tesseract failure, tempfile failure, or
/// unreadable image header.
pub fn ocr_image(
    bytes: &[u8],
    langs: &[String],
    caps: &ExtractorCapabilities,
    min_side_px: u32,
) -> Result<Option<String>> {
    let runner = SystemRunner::new(caps.timeout.as_secs());
    ocr_image_with(bytes, langs, min_side_px, &runner)
}

/// Testable core. The real API wraps this with [`SystemRunner`] from
/// `caps.timeout`.
pub fn ocr_image_with(
    bytes: &[u8],
    langs: &[String],
    min_side_px: u32,
    runner: &dyn CommandRunner,
) -> Result<Option<String>> {
    if min_side_px > 0 {
        match probe_image_dimensions(bytes) {
            Ok((w, h)) if w < min_side_px && h < min_side_px => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(e.context("image header decode failed")),
        }
    }

    let ext = infer_image_ext(bytes);
    let (path, _handle) = shell::write_temp_named("lixun-ocr", ext, bytes)?;
    let text = run_tesseract(runner, &path, langs)?;
    Ok(text_to_option(text))
}

/// OCR every page of a PDF via `pdftoppm` (rasterize) + `tesseract`
/// (read). Returns `Ok(None)` when no page yielded any text.
///
/// Page numbering is 1-based in both `pdftoppm` and the emitted page
/// separator (`--- page N ---`).
pub fn ocr_pdf_pages(
    bytes: &[u8],
    langs: &[String],
    caps: &ExtractorCapabilities,
    max_pages: Option<usize>,
) -> Result<Option<String>> {
    let runner = SystemRunner::new(caps.timeout.as_secs());
    ocr_pdf_pages_with(bytes, langs, max_pages, &runner)
}

/// Testable core. See [`ocr_pdf_pages`].
pub fn ocr_pdf_pages_with(
    bytes: &[u8],
    langs: &[String],
    max_pages: Option<usize>,
    runner: &dyn CommandRunner,
) -> Result<Option<String>> {
    let (pdf_path, _pdf_handle) = shell::write_temp_named("lixun-ocr", "pdf", bytes)?;
    let pdf_path_str = pdf_path
        .to_str()
        .context("non-UTF8 tempfile path for PDF")?;

    let total = probe_pdf_page_count(runner, pdf_path_str)?;
    // max_pages=0 should have been normalized to None upstream (AM-3),
    // but defend here too so a stray zero cannot silently drop every
    // page of a scan PDF.
    let limit = match max_pages {
        Some(0) | None => total,
        Some(n) => total.min(n),
    };

    // pdftoppm writes siblings next to the stem path. Use a directory
    // inside the system tempdir so leftover .png files are cleaned up
    // when the `TempDir` drops at end of scope even if a call bails.
    let out_dir = tempfile::Builder::new()
        .prefix("lixun-ocr-pages-")
        .tempdir()?;
    let stem = out_dir.path().join("page");
    let stem_str = stem.to_str().context("non-UTF8 tempfile stem")?;

    let mut pages_out: Vec<String> = Vec::new();
    for page in 1..=limit {
        let page_str = page.to_string();
        runner.run(
            "pdftoppm",
            &[
                "-r", "200", "-png", "-f", &page_str, "-l", &page_str, pdf_path_str, stem_str,
            ],
            None,
        )?;

        let page_png = pdftoppm_output_path(&stem, page, limit);
        let page_png_str = page_png
            .to_str()
            .context("non-UTF8 pdftoppm output path")?;
        let text = run_tesseract(runner, Path::new(page_png_str), langs)?;
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            pages_out.push(format!("--- page {page} ---\n\n{trimmed}"));
        }
    }

    if pages_out.is_empty() {
        return Ok(None);
    }
    Ok(Some(pages_out.join("\n\n")))
}

fn run_tesseract(runner: &dyn CommandRunner, image_path: &Path, langs: &[String]) -> Result<String> {
    let image_path_str = image_path
        .to_str()
        .context("non-UTF8 image path for tesseract")?;
    let langs_arg = if langs.is_empty() {
        "eng".to_string()
    } else {
        langs.join("+")
    };

    // OMP_THREAD_LIMIT=1 is applied at spawn time; CommandRunner's
    // trait signature does not carry env vars, so enforcement lives
    // inside SystemRunner (it sets the env for every tesseract it
    // spawns). The trait also does not expose env, so mock runners
    // simply ignore it — acceptable because the mock never invokes
    // the real tesseract.
    runner.run(
        "tesseract",
        &[
            image_path_str,
            "stdout",
            "-l",
            &langs_arg,
            "--psm",
            "3",
        ],
        None,
    )
}

fn probe_pdf_page_count(runner: &dyn CommandRunner, pdf_path_str: &str) -> Result<usize> {
    // `pdfinfo` (same poppler package as pdftotext) emits "Pages: N".
    let out = runner.run("pdfinfo", &[pdf_path_str], None)?;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            let rest = rest.trim();
            if let Ok(n) = rest.parse::<usize>()
                && n > 0
            {
                return Ok(n);
            }
        }
    }
    Err(anyhow!("pdfinfo emitted no usable 'Pages:' line"))
}

/// `pdftoppm` zero-pads the page number to the width of the max page
/// number. For a 12-page PDF, page 3 is `<stem>-03.png`; for a 9-page
/// PDF it is `<stem>-3.png`. Mirror that rule so the tesseract read
/// step finds the file pdftoppm actually wrote.
fn pdftoppm_output_path(stem: &Path, page: usize, total_pages: usize) -> PathBuf {
    let width = total_pages.to_string().len();
    let name = format!(
        "{}-{:0width$}.png",
        stem.file_name()
            .and_then(|o| o.to_str())
            .unwrap_or("page"),
        page,
        width = width
    );
    stem.with_file_name(name)
}

fn probe_image_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("failed to guess image format")?;
    let (w, h) = reader.into_dimensions()?;
    Ok((w, h))
}

/// Pick an extension for the tempfile so pdftoppm/tesseract can
/// identify the format. Falls back to `png` which tesseract handles
/// via leptonica regardless of the on-disk extension; tesseract does
/// not actually require the extension to be accurate.
fn infer_image_ext(bytes: &[u8]) -> &'static str {
    let reader = match image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(_) => return "png",
    };
    match reader.format() {
        Some(image::ImageFormat::Png) => "png",
        Some(image::ImageFormat::Jpeg) => "jpg",
        Some(image::ImageFormat::Gif) => "gif",
        Some(image::ImageFormat::Bmp) => "bmp",
        Some(image::ImageFormat::Tiff) => "tiff",
        Some(image::ImageFormat::WebP) => "webp",
        _ => "png",
    }
}

fn text_to_option(text: String) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every `run` invocation and returns canned output from a
    /// queue. Also creates the target PNG file on `pdftoppm` calls so
    /// the subsequent tesseract call in [`ocr_pdf_pages_with`] finds a
    /// real file at the expected path (tesseract is itself mocked too,
    /// so the file's content never matters).
    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        responses: Mutex<Vec<Result<String>>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, cmd: &str, args: &[&str], _input: Option<&[u8]>) -> Result<String> {
            let args_vec: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            self.calls
                .lock()
                .unwrap()
                .push((cmd.to_string(), args_vec.clone()));

            if cmd == "pdftoppm" {
                let out_path = args_vec
                    .iter()
                    .rev()
                    .nth(0)
                    .cloned()
                    .unwrap_or_default();
                let f_idx = args_vec.iter().position(|a| a == "-f").unwrap();
                let page: usize = args_vec[f_idx + 1].parse().unwrap();
                // `pdfinfo` was mocked first in every PDF test, so the
                // width comes from the total pages the test declared.
                let total = args_vec
                    .iter()
                    .position(|a| a == "-l")
                    .map(|i| args_vec[i + 1].parse::<usize>().unwrap_or(page))
                    .unwrap_or(page);
                let max_guess = std::cmp::max(page, total);
                let width = max_guess.to_string().len();
                let file_name = format!(
                    "{}-{:0width$}.png",
                    std::path::Path::new(&out_path)
                        .file_name()
                        .and_then(|o| o.to_str())
                        .unwrap_or("page"),
                    page,
                    width = width
                );
                let final_path = std::path::Path::new(&out_path).with_file_name(file_name);
                std::fs::write(&final_path, b"fake-png").unwrap();
            }

            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(String::new())
            } else {
                responses.remove(0)
            }
        }
    }

    fn one_pixel_png() -> Vec<u8> {
        // Minimal 1x1 transparent PNG. Good enough for the dimensions
        // probe; tesseract never sees real bytes in the mocked tests.
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    fn large_png_300x300() -> Vec<u8> {
        let img = image::RgbImage::new(300, 300);
        let mut bytes: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn ocr_candidates_contains_expected_exts() {
        for ext in ["pdf", "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"] {
            assert!(is_ocr_candidate(ext), "expected {ext} to be candidate");
        }
        assert!(!is_ocr_candidate("docx"));
        assert!(!is_ocr_candidate("PDF"));
    }

    #[test]
    fn engine_tag_sorts_and_dedups_langs() {
        let caps = ExtractorCapabilities::all_available_no_timeout();
        let tag = engine_tag(
            &caps,
            &["rus".into(), "eng".into(), "eng".into(), "chi_sim".into()],
        );
        assert_eq!(tag, "tesseract:5+chi_sim+eng+rus");
    }

    #[test]
    fn engine_tag_empty_langs_yields_empty_suffix() {
        let caps = ExtractorCapabilities::all_available_no_timeout();
        let tag = engine_tag(&caps, &[]);
        assert_eq!(tag, "tesseract:5+");
    }

    #[test]
    fn ocr_image_mocked_runner_returns_expected_text() {
        let mock = MockRunner::new(vec![Ok("  Hello OCR\n".into())]);
        let bytes = large_png_300x300();
        let text = ocr_image_with(&bytes, &["eng".into()], 0, &mock).unwrap();
        assert_eq!(text.as_deref(), Some("  Hello OCR\n"));
        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "tesseract");
        assert!(calls[0].1.contains(&"stdout".into()));
        assert!(calls[0].1.contains(&"eng".into()));
    }

    #[test]
    fn ocr_image_whitespace_only_returns_none() {
        let mock = MockRunner::new(vec![Ok("   \n\t\n".into())]);
        let bytes = large_png_300x300();
        let text = ocr_image_with(&bytes, &["eng".into()], 0, &mock).unwrap();
        assert!(text.is_none());
    }

    #[test]
    fn ocr_image_below_min_side_returns_none() {
        let mock = MockRunner::new(vec![]);
        let bytes = one_pixel_png();
        let text = ocr_image_with(&bytes, &["eng".into()], 200, &mock).unwrap();
        assert!(text.is_none());
        assert_eq!(mock.calls().len(), 0, "tesseract must not be spawned");
    }

    #[test]
    fn ocr_image_large_enough_runs_tesseract() {
        let mock = MockRunner::new(vec![Ok("READ".into())]);
        let bytes = large_png_300x300();
        let text = ocr_image_with(&bytes, &["eng".into()], 200, &mock).unwrap();
        assert_eq!(text.as_deref(), Some("READ"));
    }

    #[test]
    fn ocr_image_joins_langs_with_plus() {
        let mock = MockRunner::new(vec![Ok("ok".into())]);
        let bytes = large_png_300x300();
        let _ = ocr_image_with(
            &bytes,
            &["eng".into(), "rus".into(), "chi_sim".into()],
            0,
            &mock,
        )
        .unwrap();
        let calls = mock.calls();
        let langs_arg = &calls[0].1;
        let l_idx = langs_arg.iter().position(|a| a == "-l").unwrap();
        assert_eq!(langs_arg[l_idx + 1], "eng+rus+chi_sim");
    }

    #[test]
    fn ocr_pdf_pages_iterates_all_pages_when_unlimited() {
        let mock = MockRunner::new(vec![
            Ok("Pages: 3\n".into()),
            Ok(String::new()),
            Ok("page one".into()),
            Ok(String::new()),
            Ok("page two".into()),
            Ok(String::new()),
            Ok("page three".into()),
        ]);
        let pdf = b"%PDF-1.4 fake";
        let text = ocr_pdf_pages_with(pdf, &["eng".into()], None, &mock)
            .unwrap()
            .unwrap();
        assert!(text.contains("page one"));
        assert!(text.contains("page two"));
        assert!(text.contains("page three"));
        assert!(text.contains("--- page 1 ---"));
        assert!(text.contains("--- page 3 ---"));
    }

    #[test]
    fn ocr_pdf_pages_respects_max_pages() {
        let mock = MockRunner::new(vec![
            Ok("Pages: 10\n".into()),
            Ok(String::new()),
            Ok("one".into()),
            Ok(String::new()),
            Ok("two".into()),
            Ok(String::new()),
            Ok("three".into()),
        ]);
        let pdf = b"%PDF-1.4 fake";
        let text = ocr_pdf_pages_with(pdf, &["eng".into()], Some(3), &mock)
            .unwrap()
            .unwrap();
        assert!(text.contains("one"));
        assert!(text.contains("three"));
        assert!(!text.contains("--- page 4 ---"));
        let calls = mock.calls();
        let tesseract_calls = calls.iter().filter(|(c, _)| c == "tesseract").count();
        assert_eq!(tesseract_calls, 3);
    }

    #[test]
    fn ocr_pdf_pages_none_when_all_pages_empty() {
        let mock = MockRunner::new(vec![
            Ok("Pages: 2\n".into()),
            Ok(String::new()),
            Ok("   \n".into()),
            Ok(String::new()),
            Ok("\t\t".into()),
        ]);
        let pdf = b"%PDF-1.4 fake";
        let text = ocr_pdf_pages_with(pdf, &["eng".into()], None, &mock).unwrap();
        assert!(text.is_none());
    }

    #[test]
    fn ocr_pdf_pages_propagates_runner_error() {
        let mock = MockRunner::new(vec![
            Ok("Pages: 1\n".into()),
            Ok(String::new()),
            Err(anyhow!("tesseract crashed")),
        ]);
        let pdf = b"%PDF-1.4 fake";
        let err = ocr_pdf_pages_with(pdf, &["eng".into()], None, &mock).unwrap_err();
        assert!(err.to_string().contains("tesseract crashed"));
    }

    #[test]
    fn ocr_pdf_pages_errors_when_pdfinfo_fails() {
        let mock = MockRunner::new(vec![Err(anyhow!("no pdfinfo"))]);
        let pdf = b"%PDF-1.4 fake";
        let err = ocr_pdf_pages_with(pdf, &["eng".into()], None, &mock).unwrap_err();
        assert!(err.to_string().contains("no pdfinfo"));
    }

    #[test]
    fn ocr_pdf_pages_errors_when_pdfinfo_emits_no_page_line() {
        let mock = MockRunner::new(vec![Ok("Title: test\nProducer: fake\n".into())]);
        let pdf = b"%PDF-1.4 fake";
        let err = ocr_pdf_pages_with(pdf, &["eng".into()], None, &mock).unwrap_err();
        assert!(err.to_string().contains("Pages:"));
    }

    #[test]
    fn pdftoppm_output_path_zero_pads_for_multi_digit_totals() {
        let stem = std::path::PathBuf::from("/tmp/page");
        let single = pdftoppm_output_path(&stem, 3, 9);
        assert_eq!(single, std::path::PathBuf::from("/tmp/page-3.png"));
        let padded = pdftoppm_output_path(&stem, 3, 12);
        assert_eq!(padded, std::path::PathBuf::from("/tmp/page-03.png"));
        let padded_hundred = pdftoppm_output_path(&stem, 7, 100);
        assert_eq!(padded_hundred, std::path::PathBuf::from("/tmp/page-007.png"));
    }

    #[test]
    fn infer_image_ext_identifies_png() {
        let bytes = one_pixel_png();
        assert_eq!(infer_image_ext(&bytes), "png");
    }

    #[test]
    fn infer_image_ext_unknown_falls_back_to_png() {
        assert_eq!(infer_image_ext(b"not an image"), "png");
    }

    #[test]
    #[ignore]
    fn real_tesseract_reads_hello_fixture() -> Result<()> {
        if std::env::var("LIXUN_TEST_REAL_TESSERACT").as_deref() != Ok("1") {
            return Ok(());
        }
        let bytes = std::fs::read("tests/fixtures/ocr-eng-hello.png")?;
        let caps = ExtractorCapabilities::all_available_no_timeout();
        let text = ocr_image(&bytes, &["eng".into()], &caps, 0)?.unwrap_or_default();
        assert!(
            text.to_ascii_uppercase().contains("HELLO"),
            "expected HELLO in OCR output, got: {text:?}"
        );
        Ok(())
    }
}
