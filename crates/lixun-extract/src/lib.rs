//! Lixun Extract — text extraction from various file formats.

use anyhow::Result;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub mod cache;
pub mod ocr;
pub mod ocr_queue;
pub mod shell;

const MIB: u64 = 1024 * 1024;
const DEFAULT_MAX_INPUT_BYTES: u64 = 10 * MIB;
const DEFAULT_MAX_DECOMPRESS_BYTES: u64 = 64 * MIB;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 16 * MIB;
const DEFAULT_MAX_ZIP_ENTRIES: u32 = 10_000;
const ZIP_ENTRY_MAX_BYTES: u64 = 32 * MIB;
const RTF_MAX_TOKENS: u64 = 1_000_000;
const RTF_MAX_NESTING_DEPTH: u32 = 256;
const RTF_DEADLINE_CHECK_TOKENS: u64 = 1024;
const RTF_NO_PROGRESS_LIMIT: u32 = 1024;

pub trait Extractor {
    fn extract(&self, bytes: &[u8]) -> Result<String>;
    fn mime_types(&self) -> &'static [&'static str];
}

#[derive(Clone, Debug)]
pub struct ExtractorCapabilities {
    pub timeout: Duration,
    pub max_input_bytes: u64,
    pub max_decompress_bytes: u64,
    pub max_output_bytes: u64,
    pub max_zip_entries: u32,
    /// Cooperative in-process extraction deadline. `Duration::ZERO` disables it.
    pub deadline: Duration,
    pub has_pdftotext: bool,
    pub has_antiword: bool,
    pub has_catdoc: bool,
    pub has_libreoffice: bool,
    /// Tesseract OCR binary probed via `which`. When `false`, the OCR
    /// worker never spawns and enqueue decisions short-circuit to "no".
    pub has_tesseract: bool,
    /// `pdftoppm` (from poppler-utils). Required to rasterize scan PDFs
    /// before handing pages to tesseract. When `false`, PDF OCR is
    /// unavailable even if tesseract is installed; image OCR still works.
    pub has_pdftoppm: bool,
    /// Language codes reported by `tesseract --list-langs`, sorted.
    /// Empty when `has_tesseract == false`. Used by the OCR module to
    /// filter user-requested langs and compute the cache engine tag.
    pub tesseract_langs: Vec<String>,
    /// Mirrors `[ocr].enabled` from daemon config. Defaults to `false`;
    /// the daemon sets this explicitly at `init_capabilities` time when
    /// the user has opted in. Kept as part of capabilities (not a
    /// separate global) so every extraction call sees a consistent
    /// snapshot without locking.
    pub ocr_enabled: bool,
}

impl ExtractorCapabilities {
    /// All tools assumed available, no timeout. Safe default for tests and
    /// any call site that runs before `init_capabilities` is invoked.
    pub fn all_available_no_timeout() -> Self {
        Self {
            timeout: Duration::ZERO,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_decompress_bytes: DEFAULT_MAX_DECOMPRESS_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_zip_entries: DEFAULT_MAX_ZIP_ENTRIES,
            deadline: Duration::ZERO,
            has_pdftotext: true,
            has_antiword: true,
            has_catdoc: true,
            has_libreoffice: true,
            has_tesseract: true,
            has_pdftoppm: true,
            tesseract_langs: vec!["chi_sim".into(), "eng".into(), "rus".into()],
            ocr_enabled: true,
        }
    }

    /// Probe the environment for available extractors via `which::which`.
    /// Missing tools are logged at `warn` and disable their corresponding
    /// extractor; found tools log at `info` with the resolved path.
    pub fn probe(timeout: Duration) -> Self {
        Self::probe_with(timeout, which_command_exists, probe_tesseract_langs_system)
    }

    /// Testable core of `probe`: takes an injectable `command_exists`
    /// predicate and a langs probe closure so unit tests can run without
    /// relying on the host's installed binaries. Kept on the public type
    /// rather than a free fn because `#[cfg(test)]` helpers need access
    /// to all the private field assembly; exposing it outside the crate
    /// would be a footgun (callers should use `probe`).
    pub(crate) fn probe_with(
        timeout: Duration,
        command_exists: fn(&str) -> bool,
        langs_probe: fn() -> Vec<String>,
    ) -> Self {
        fn log_probe(name: &str, found: bool) -> bool {
            if found {
                tracing::info!("extractor tool {}: found", name);
            } else {
                tracing::warn!(
                    "extractor tool {}: NOT found — related content search disabled",
                    name
                );
            }
            found
        }
        let has_tesseract = log_probe("tesseract", command_exists("tesseract"));
        let tesseract_langs = if has_tesseract {
            let langs = langs_probe();
            if langs.is_empty() {
                tracing::warn!(
                    "tesseract found but --list-langs parse yielded 0 langs — disabling OCR"
                );
            } else {
                tracing::info!("tesseract found: {} langs ({:?})", langs.len(), langs);
            }
            langs
        } else {
            Vec::new()
        };
        // If langs probe returned empty, treat tesseract as absent — no
        // point keeping the binary claim when we can't actually OCR.
        let has_tesseract = has_tesseract && !tesseract_langs.is_empty();
        Self {
            timeout,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_decompress_bytes: DEFAULT_MAX_DECOMPRESS_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_zip_entries: DEFAULT_MAX_ZIP_ENTRIES,
            deadline: Duration::ZERO,
            has_pdftotext: log_probe("pdftotext", command_exists("pdftotext")),
            has_antiword: log_probe("antiword", command_exists("antiword")),
            has_catdoc: log_probe("catdoc", command_exists("catdoc")),
            has_libreoffice: log_probe("libreoffice", command_exists("libreoffice")),
            has_tesseract,
            has_pdftoppm: log_probe("pdftoppm", command_exists("pdftoppm")),
            tesseract_langs,
            ocr_enabled: false,
        }
    }
}

fn which_command_exists(name: &str) -> bool {
    which::which(name).is_ok()
}

/// Run `tesseract --list-langs` with a hard 5-second timeout and return
/// the sorted, de-duplicated list of language codes. Parses both stdout
/// and stderr: tesseract 3.x historically printed langs to stderr,
/// modern 4.x/5.x print to stdout; concatenating covers both.
///
/// Filtering uses a strict `^[a-z][a-z_]+$` shape — matches `eng`,
/// `rus`, `chi_sim`; rejects the header line `List of available
/// languages (N):` and stray blank lines. Returns an empty vec on any
/// execution error so the caller can degrade gracefully (treat as "no
/// tesseract").
fn probe_tesseract_langs_system() -> Vec<String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::thread;
    use wait_timeout::ChildExt;

    let mut child = match Command::new("tesseract")
        .arg("--list-langs")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("tesseract --list-langs spawn failed: {e}");
            return Vec::new();
        }
    };
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let so = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let se = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    match child.wait_timeout(Duration::from_secs(5)) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            let _ = so.join();
            let _ = se.join();
            tracing::warn!("tesseract --list-langs timed out after 5s");
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!("tesseract --list-langs wait failed: {e}");
            return Vec::new();
        }
    }
    let stdout_bytes = so.join().unwrap_or_default();
    let stderr_bytes = se.join().unwrap_or_default();
    parse_tesseract_langs(&stdout_bytes, &stderr_bytes)
}

/// Pure parser for `tesseract --list-langs` output. Separated from the
/// subprocess call so unit tests can exercise edge cases (stderr-only
/// output, mixed header lines, duplicates) without spawning anything.
fn parse_tesseract_langs(stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    let mut langs: Vec<String> = Vec::new();
    for stream in [stdout, stderr] {
        let s = String::from_utf8_lossy(stream);
        for line in s.lines() {
            let line = line.trim();
            // ^[a-z][a-z_]+$ — lower-case ASCII, at least 2 chars,
            // letters and underscore only. Accepts `eng`, `chi_sim`;
            // rejects `List of available languages (3):`, blank lines,
            // stray whitespace.
            let bytes = line.as_bytes();
            if bytes.len() < 2 {
                continue;
            }
            let shape_ok = bytes[0].is_ascii_lowercase()
                && bytes.iter().all(|b| b.is_ascii_lowercase() || *b == b'_');
            if shape_ok {
                langs.push(line.to_string());
            }
        }
    }
    langs.sort();
    langs.dedup();
    langs
}

static EXTRACTOR_CAPS: OnceLock<ExtractorCapabilities> = OnceLock::new();

/// Initialize global extractor capabilities. Must be called once at daemon
/// startup before any extraction. A second call is ignored (OnceLock semantics)
/// and logged to stderr rather than panicking so test harnesses can coexist.
pub fn init_capabilities(caps: ExtractorCapabilities) {
    if EXTRACTOR_CAPS.set(caps).is_err() {
        eprintln!("lixun-extract: init_capabilities called more than once; keeping first value");
    }
}

pub fn capabilities() -> ExtractorCapabilities {
    EXTRACTOR_CAPS
        .get()
        .cloned()
        .unwrap_or_else(ExtractorCapabilities::all_available_no_timeout)
}

pub fn extractor_for_ext(ext: &str) -> Option<Box<dyn Extractor>> {
    extractor_for_ext_with_caps(ext, &capabilities())
}

pub(crate) fn extractor_for_ext_with_caps(
    ext: &str,
    caps: &ExtractorCapabilities,
) -> Option<Box<dyn Extractor>> {
    match ext {
        "pdf" if caps.has_pdftotext => Some(Box::new(PdfExtractor::new(caps.timeout))),
        "docx" | "xlsx" | "pptx" => Some(Box::new(OoxmlExtractor::new(caps.clone()))),
        "odt" => Some(Box::new(OdtExtractor::new(caps.clone()))),
        "rtf" => Some(Box::new(RtfExtractor::new(caps.clone()))),
        "doc" if caps.has_antiword => Some(Box::new(ShellDocExtractor::new(caps.timeout))),
        "xls" if caps.has_catdoc => Some(Box::new(ShellXlsExtractor::new(caps.timeout))),
        "ppt" if caps.has_libreoffice => Some(Box::new(ShellPptExtractor::new(caps.timeout))),
        _ => None,
    }
}

/// Extract text from raw bytes using an optional extension hint.
pub fn extract_bytes(bytes: &[u8], ext_hint: Option<&str>) -> Result<String> {
    let caps = capabilities();
    let ext = ext_hint.unwrap_or("").to_ascii_lowercase();

    if let Some(extractor) = extractor_for_ext_with_caps(&ext, &caps) {
        return extractor.extract(bytes);
    }

    if !bytes.contains(&0)
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        return Ok(text.to_string());
    }

    Ok(String::new())
}

/// Try to extract text from a file path.
pub fn extract_path(path: &Path) -> Result<String> {
    let caps = capabilities();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if let Some(extractor) = extractor_for_ext_with_caps(&ext, &caps) {
        let bytes = std::fs::read(path)?;
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extractor.extract(&bytes)));
        return match result {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("extractor panicked"),
        };
    }

    // Text-like: try encoding detection
    match ext.as_str() {
        "txt" | "md" | "log" | "csv" | "json" | "xml" | "html" | "htm" | "yaml" | "yml"
        | "toml" | "ini" | "cfg" | "conf" | "rs" | "py" | "js" | "ts" | "c" | "h" | "cpp"
        | "hpp" | "java" | "go" | "sh" | "css" | "scss" | "sql" | "rb" => {
            extract_text_file(path, &caps)
        }
        _ => {
            // Magic sniff for extension-less text files
            if ext.is_empty()
                && let Ok(mut f) = std::fs::File::open(path)
            {
                let mut buf = [0u8; 8192];
                let n = std::io::Read::read(&mut f, &mut buf).unwrap_or(0);
                let sniff = &buf[..n];
                if n > 0 && !sniff.contains(&0) && std::str::from_utf8(sniff).is_ok() {
                    return extract_text_file(path, &caps);
                }
            }
            Ok(String::new())
        }
    }
}

fn extract_text_file(path: &Path, caps: &ExtractorCapabilities) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let read_cap = file_len.min(caps.max_input_bytes);
    let mut bytes = Vec::with_capacity(read_cap.min(usize::MAX as u64) as usize);
    file.take(caps.max_input_bytes).read_to_end(&mut bytes)?;
    if let Ok(text) = String::from_utf8(bytes.clone()) {
        ensure_output_len(text.len(), caps.max_output_bytes, "text file output")?;
        return Ok(text);
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(&bytes, true);
    let encoding = detector.guess(None, true);
    let (text, _, _) = encoding.decode(&bytes);
    let text = text.to_string();
    ensure_output_len(text.len(), caps.max_output_bytes, "text file output")?;
    Ok(text)
}

fn ensure_input_len(len: usize, cap: u64, label: &str) -> Result<()> {
    let len = len as u64;
    if len > cap {
        anyhow::bail!("{label} size {len} exceeds cap {cap} bytes");
    }
    Ok(())
}

fn ensure_output_len(len: usize, cap: u64, label: &str) -> Result<()> {
    let len = len as u64;
    if len > cap {
        anyhow::bail!("{label} size {len} exceeds cap {cap} bytes");
    }
    Ok(())
}

fn push_capped_bytes(output: &mut Vec<u8>, addition: &[u8], cap: u64, label: &str) -> Result<()> {
    let len = (output.len() as u64)
        .checked_add(addition.len() as u64)
        .ok_or_else(|| anyhow::anyhow!("{label} size overflow"))?;
    if len > cap {
        anyhow::bail!("{label} size {len} exceeds cap {cap} bytes");
    }
    output.extend_from_slice(addition);
    Ok(())
}

fn push_capped_str(output: &mut String, addition: &str, cap: u64, label: &str) -> Result<()> {
    let len = (output.len() as u64)
        .checked_add(addition.len() as u64)
        .ok_or_else(|| anyhow::anyhow!("{label} size overflow"))?;
    if len > cap {
        anyhow::bail!("{label} size {len} exceeds cap {cap} bytes");
    }
    output.push_str(addition);
    Ok(())
}

fn validate_zip_archive<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    caps: &ExtractorCapabilities,
    label: &str,
) -> Result<()> {
    if archive.len() > caps.max_zip_entries as usize {
        anyhow::bail!(
            "{label} zip entry count {} exceeds cap {}",
            archive.len(),
            caps.max_zip_entries
        );
    }

    let mut total = 0_u64;
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name().to_string();
        let size = file.size();
        if size > ZIP_ENTRY_MAX_BYTES {
            anyhow::bail!(
                "{label} zip entry {name:?} uncompressed size {size} exceeds per-entry cap {ZIP_ENTRY_MAX_BYTES}"
            );
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("{label} total decompressed size overflow"))?;
        if total > caps.max_decompress_bytes {
            anyhow::bail!(
                "{label} total decompressed size {total} exceeds cap {} bytes",
                caps.max_decompress_bytes
            );
        }
    }

    Ok(())
}

fn read_zip_file_to_string(
    file: &mut zip::read::ZipFile<'_>,
    caps: &ExtractorCapabilities,
    label: &str,
) -> Result<String> {
    let size = file.size();
    if size > ZIP_ENTRY_MAX_BYTES {
        anyhow::bail!(
            "{label} zip entry {:?} uncompressed size {size} exceeds per-entry cap {ZIP_ENTRY_MAX_BYTES}",
            file.name()
        );
    }
    if size > caps.max_decompress_bytes {
        anyhow::bail!(
            "{label} zip entry {:?} uncompressed size {size} exceeds decompress cap {} bytes",
            file.name(),
            caps.max_decompress_bytes
        );
    }
    let mut content = String::new();
    file.take(size.saturating_add(1))
        .read_to_string(&mut content)?;
    Ok(content)
}

fn extract_rtf_with_reader<F>(
    bytes: &[u8],
    caps: &ExtractorCapabilities,
    mut read_next: F,
) -> Result<String>
where
    F: for<'a> FnMut(&'a [u8]) -> Result<(&'a [u8], rtf_grimoire::tokenizer::Token)>,
{
    let started = Instant::now();
    let mut input = bytes;
    let mut text_bytes: Vec<u8> = Vec::new();
    let mut token_count = 0_u64;
    let mut depth = 0_u32;
    let mut no_progress = 0_u32;

    while !input.is_empty() {
        if token_count.is_multiple_of(RTF_DEADLINE_CHECK_TOKENS) {
            check_rtf_deadline(started, caps.deadline)?;
        }

        let before_len = input.len();
        let (remaining, token) = read_next(input)?;
        if remaining.len() >= before_len {
            no_progress += 1;
            if no_progress > RTF_NO_PROGRESS_LIMIT {
                anyhow::bail!(
                    "RTF parser made no progress for more than {RTF_NO_PROGRESS_LIMIT} iterations"
                );
            }
            continue;
        }
        no_progress = 0;
        input = remaining;

        token_count += 1;
        if token_count > RTF_MAX_TOKENS {
            anyhow::bail!("RTF token count {token_count} exceeds cap {RTF_MAX_TOKENS}");
        }

        match token {
            rtf_grimoire::tokenizer::Token::StartGroup => {
                depth += 1;
                if depth > RTF_MAX_NESTING_DEPTH {
                    anyhow::bail!("RTF nesting depth {depth} exceeds cap {RTF_MAX_NESTING_DEPTH}");
                }
            }
            rtf_grimoire::tokenizer::Token::EndGroup => {
                depth = depth.saturating_sub(1);
            }
            rtf_grimoire::tokenizer::Token::Text(data) => {
                push_capped_bytes(&mut text_bytes, &data, caps.max_output_bytes, "RTF output")?;
            }
            rtf_grimoire::tokenizer::Token::ControlWord { name, .. }
                if name == "par" || name == "line" =>
            {
                push_capped_bytes(&mut text_bytes, b"\n", caps.max_output_bytes, "RTF output")?;
            }
            rtf_grimoire::tokenizer::Token::ControlSymbol('~') => {
                push_capped_bytes(&mut text_bytes, b" ", caps.max_output_bytes, "RTF output")?;
            }
            rtf_grimoire::tokenizer::Token::Newline(data) => {
                push_capped_bytes(&mut text_bytes, &data, caps.max_output_bytes, "RTF output")?;
            }
            _ => {}
        }
    }

    if let Ok(text) = String::from_utf8(text_bytes.clone()) {
        return Ok(text.trim().to_string());
    }
    let text: String = text_bytes.into_iter().map(|b| b as char).collect();
    Ok(text.trim().to_string())
}

fn check_rtf_deadline(started: Instant, deadline: Duration) -> Result<()> {
    if deadline != Duration::ZERO && started.elapsed() > deadline {
        anyhow::bail!("RTF deadline exceeded");
    }
    Ok(())
}

// --- PDF ---

pub struct PdfExtractor {
    runner: shell::SystemRunner,
}

impl PdfExtractor {
    pub fn new(timeout: Duration) -> Self {
        Self {
            runner: shell::SystemRunner::new(timeout.as_secs()),
        }
    }
}

impl Extractor for PdfExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        shell::extract_pdf(bytes, &self.runner)
    }

    fn mime_types(&self) -> &'static [&'static str] {
        &["application/pdf"]
    }
}

// --- OOXML (DOCX/XLSX/PPTX) ---

pub struct OoxmlExtractor {
    caps: ExtractorCapabilities,
}

impl OoxmlExtractor {
    pub fn new(caps: ExtractorCapabilities) -> Self {
        Self { caps }
    }
}

impl Extractor for OoxmlExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        ensure_input_len(bytes.len(), self.caps.max_input_bytes, "OOXML input")?;
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor)?;
        validate_zip_archive(&mut archive, &self.caps, "OOXML")?;

        // Try document.xml (DOCX), sheet XMLs (XLSX), slide XMLs (PPTX)
        let mut text = String::new();

        // DOCX: word/document.xml
        if let Ok(mut file) = archive.by_name("word/document.xml") {
            let content = read_zip_file_to_string(&mut file, &self.caps, "OOXML")?;
            push_capped_str(
                &mut text,
                &strip_xml_tags(&content),
                self.caps.max_output_bytes,
                "OOXML output",
            )?;
        }

        // XLSX: xl/worksheets/*.xml
        if text.is_empty() {
            for i in 0..archive.len() {
                if let Ok(mut file) = archive.by_index(i) {
                    let name = file.name().to_string();
                    if name.starts_with("xl/worksheets/") && name.ends_with(".xml") {
                        let content = read_zip_file_to_string(&mut file, &self.caps, "OOXML")?;
                        push_capped_str(
                            &mut text,
                            &strip_xml_tags(&content),
                            self.caps.max_output_bytes,
                            "OOXML output",
                        )?;
                    }
                }
            }
        }

        // PPTX: ppt/slides/*.xml
        if text.is_empty() {
            for i in 0..archive.len() {
                if let Ok(mut file) = archive.by_index(i) {
                    let name = file.name().to_string();
                    if name.starts_with("ppt/slides/") && name.ends_with(".xml") {
                        let content = read_zip_file_to_string(&mut file, &self.caps, "OOXML")?;
                        push_capped_str(
                            &mut text,
                            &strip_xml_tags(&content),
                            self.caps.max_output_bytes,
                            "OOXML output",
                        )?;
                    }
                }
            }
        }

        Ok(text.trim().to_string())
    }

    fn mime_types(&self) -> &'static [&'static str] {
        &[
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ]
    }
}

// --- ODT ---

pub struct OdtExtractor {
    caps: ExtractorCapabilities,
}

impl OdtExtractor {
    pub fn new(caps: ExtractorCapabilities) -> Self {
        Self { caps }
    }
}

impl Extractor for OdtExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        ensure_input_len(bytes.len(), self.caps.max_input_bytes, "ODT input")?;
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor)?;
        validate_zip_archive(&mut archive, &self.caps, "ODT")?;
        let mut file = archive
            .by_name("content.xml")
            .map_err(|_| anyhow::anyhow!("ODT: content.xml not found"))?;
        let content = read_zip_file_to_string(&mut file, &self.caps, "ODT")?;
        let text = strip_xml_tags(&content).trim().to_string();
        ensure_output_len(text.len(), self.caps.max_output_bytes, "ODT output")?;
        Ok(text)
    }

    fn mime_types(&self) -> &'static [&'static str] {
        &["application/vnd.oasis.opendocument.text"]
    }
}

// --- RTF ---

pub struct RtfExtractor {
    caps: ExtractorCapabilities,
}

impl RtfExtractor {
    pub fn new(caps: ExtractorCapabilities) -> Self {
        Self { caps }
    }
}

impl Extractor for RtfExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        ensure_input_len(bytes.len(), self.caps.max_input_bytes, "RTF input")?;
        extract_rtf_with_reader(bytes, &self.caps, |input| {
            rtf_grimoire::tokenizer::read_token(input)
                .map_err(|e| anyhow::anyhow!("RTF parse error: {:?}", e))
        })
    }

    fn mime_types(&self) -> &'static [&'static str] {
        &["application/rtf"]
    }
}

pub struct ShellDocExtractor {
    runner: shell::SystemRunner,
}

impl ShellDocExtractor {
    pub fn new(timeout: Duration) -> Self {
        Self {
            runner: shell::SystemRunner::new(timeout.as_secs()),
        }
    }
}

impl Extractor for ShellDocExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        shell::extract_doc(bytes, &self.runner)
    }
    fn mime_types(&self) -> &'static [&'static str] {
        &["application/msword"]
    }
}

pub struct ShellXlsExtractor {
    runner: shell::SystemRunner,
}

impl ShellXlsExtractor {
    pub fn new(timeout: Duration) -> Self {
        Self {
            runner: shell::SystemRunner::new(timeout.as_secs()),
        }
    }
}

impl Extractor for ShellXlsExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        shell::extract_xls(bytes, &self.runner)
    }
    fn mime_types(&self) -> &'static [&'static str] {
        &["application/vnd.ms-excel"]
    }
}

pub struct ShellPptExtractor {
    runner: shell::SystemRunner,
}

impl ShellPptExtractor {
    pub fn new(timeout: Duration) -> Self {
        Self {
            runner: shell::SystemRunner::new(timeout.as_secs()),
        }
    }
}

impl Extractor for ShellPptExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<String> {
        shell::extract_ppt(bytes, &self.runner)
    }
    fn mime_types(&self) -> &'static [&'static str] {
        &["application/vnd.ms-powerpoint"]
    }
}

fn strip_xml_tags(input: &str) -> String {
    let mut in_tag = false;
    let mut clean = String::new();
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => clean.push(c),
            _ => {}
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    const TEST_MIB: u64 = 1024 * 1024;

    fn capped_test_caps() -> ExtractorCapabilities {
        let mut caps = ExtractorCapabilities::all_available_no_timeout();
        caps.max_input_bytes = 10 * TEST_MIB;
        caps.max_decompress_bytes = 64 * TEST_MIB;
        caps.max_output_bytes = 16 * TEST_MIB;
        caps.max_zip_entries = 10_000;
        caps.deadline = Duration::ZERO;
        caps
    }

    fn assert_cap_error(err: anyhow::Error, expected: &str) {
        let message = err.to_string();
        assert!(
            message.contains(expected),
            "expected error containing {expected:?}, got {message:?}"
        );
    }

    fn zip_with_dummy_entries(entries: usize) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for i in 0..entries {
            writer
                .start_file(format!("dummy/{i}.txt"), options)
                .expect("start dummy entry");
            writer.write_all(b"x").expect("write dummy entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    fn docx_with_repeated_document_xml(uncompressed_bytes: u64) -> Vec<u8> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file("word/document.xml", options)
            .expect("start document.xml");
        let mut payload = std::io::repeat(b'a').take(uncompressed_bytes);
        std::io::copy(&mut payload, &mut writer).expect("write compressed document.xml");
        writer.finish().expect("finish zip").into_inner()
    }

    #[test]
    fn ooxml_rejects_when_entry_count_exceeds_cap() {
        let mut caps = capped_test_caps();
        caps.max_zip_entries = 10_000;
        let bytes = zip_with_dummy_entries(11_000);

        let err = OoxmlExtractor::new(caps).extract(&bytes).unwrap_err();

        assert_cap_error(err, "entry");
    }

    #[test]
    fn ooxml_rejects_zip_bomb_via_uncompressed_size() {
        let mut caps = capped_test_caps();
        caps.max_decompress_bytes = 64 * TEST_MIB;
        let bytes = docx_with_repeated_document_xml(100 * TEST_MIB);

        let err = OoxmlExtractor::new(caps).extract(&bytes).unwrap_err();

        assert_cap_error(err, "size");
    }

    #[test]
    fn rtf_caps_token_count() {
        let caps = capped_test_caps();
        let mut rtf = Vec::from(&b"{\\rtf1 "[..]);
        for _ in 0..1_100_000 {
            rtf.extend_from_slice(br"{\b x}");
        }
        rtf.push(b'}');

        let err = RtfExtractor::new(caps).extract(&rtf).unwrap_err();

        assert_cap_error(err, "token");
    }

    #[test]
    fn rtf_caps_output_bytes() {
        let mut caps = capped_test_caps();
        caps.max_output_bytes = 8;

        let err = RtfExtractor::new(caps)
            .extract(br"{\rtf1 abcdefghijklmnopqrstuvwxyz}")
            .unwrap_err();

        assert_cap_error(err, "output");
    }

    #[test]
    fn rtf_must_advance_guard_breaks_pathological_loop() {
        let caps = capped_test_caps();
        let err = extract_rtf_with_reader(b"{\\rtf1 x}", &caps, |input| {
            Ok((input, rtf_grimoire::tokenizer::Token::Text(b"x".to_vec())))
        })
        .unwrap_err();

        assert_cap_error(err, "progress");
    }

    #[test]
    fn text_file_caps_input_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(100 * TEST_MIB).unwrap();
        let mut caps = capped_test_caps();
        caps.max_input_bytes = TEST_MIB;

        let result = extract_text_file(&path, &caps).unwrap();

        assert!(result.len() <= TEST_MIB as usize);
    }

    #[test]
    fn caps_default_all_available_no_timeout() {
        let caps = ExtractorCapabilities::all_available_no_timeout();

        assert_eq!(caps.max_input_bytes, 10 * TEST_MIB);
        assert_eq!(caps.max_decompress_bytes, 64 * TEST_MIB);
        assert_eq!(caps.max_output_bytes, 16 * TEST_MIB);
        assert_eq!(caps.max_zip_entries, 10_000);
        assert_eq!(caps.deadline, Duration::ZERO);
    }

    #[test]
    fn test_strip_xml_tags() {
        assert_eq!(
            strip_xml_tags("<root>hello <b>world</b></root>"),
            "hello world"
        );
    }

    #[test]
    fn test_strip_xml_tags_empty() {
        assert_eq!(strip_xml_tags(""), "");
    }

    #[test]
    fn test_strip_xml_tags_only_tags() {
        assert_eq!(strip_xml_tags("<a><b><c></c></b></a>"), "");
    }

    #[test]
    fn test_strip_xml_tags_no_tags() {
        assert_eq!(strip_xml_tags("plain text"), "plain text");
    }

    #[test]
    fn test_strip_xml_tags_nested() {
        assert_eq!(
            strip_xml_tags("<p>hello <span>nested <b>deep</b></span> world</p>"),
            "hello nested deep world"
        );
    }

    #[test]
    fn test_strip_xml_tags_with_attributes() {
        assert_eq!(
            strip_xml_tags("<div class='foo' id='bar'>content</div>"),
            "content"
        );
    }

    #[test]
    fn test_extractor_for_ext_known_types() {
        assert!(
            extractor_for_ext("pdf").is_some()
                || std::process::Command::new("pdftotext").output().is_err()
        );
        assert!(extractor_for_ext("docx").is_some());
        assert!(extractor_for_ext("xlsx").is_some());
        assert!(extractor_for_ext("pptx").is_some());
        assert!(extractor_for_ext("odt").is_some());
        assert!(extractor_for_ext("rtf").is_some());
    }

    #[test]
    fn test_extractor_for_ext_unknown() {
        assert!(extractor_for_ext("xyz").is_none());
        assert!(extractor_for_ext("jpg").is_none());
        assert!(extractor_for_ext("png").is_none());
    }

    #[test]
    fn test_extract_text_file_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "hello utf8 world").unwrap();
        let result = extract_path(&path).unwrap();
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_extract_path_unknown_ext() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.xyz");
        std::fs::write(&path, "some binary data").unwrap();
        let result = extract_path(&path).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_path_nonexistent() {
        let result = extract_path(std::path::Path::new("/nonexistent/file.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn test_magic_sniff_readme_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("README");
        std::fs::write(&path, "Hello README content").unwrap();
        let content = extract_path(&path).unwrap();
        assert!(content.contains("Hello README"));
    }

    #[test]
    fn test_magic_sniff_makefile_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Makefile");
        std::fs::write(&path, "all:\n\techo hi\n").unwrap();
        let content = extract_path(&path).unwrap();
        assert!(content.contains("echo hi"));
    }

    #[test]
    fn test_magic_sniff_binary_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"abc\0def\0binary").unwrap();
        let content = extract_path(&path).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn test_magic_sniff_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("EMPTY");
        std::fs::write(&path, b"").unwrap();
        let content = extract_path(&path).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn test_magic_sniff_non_utf8_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("latin1bin");
        std::fs::write(&path, b"\xff\xfe\xfd").unwrap();
        let content = extract_path(&path).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn test_extractor_for_ext_respects_caps_struct() {
        let caps = ExtractorCapabilities {
            timeout: std::time::Duration::from_secs(15),
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_decompress_bytes: DEFAULT_MAX_DECOMPRESS_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_zip_entries: DEFAULT_MAX_ZIP_ENTRIES,
            deadline: Duration::ZERO,
            has_pdftotext: false,
            has_antiword: false,
            has_catdoc: false,
            has_libreoffice: false,
            has_tesseract: false,
            has_pdftoppm: false,
            tesseract_langs: Vec::new(),
            ocr_enabled: false,
        };
        assert!(extractor_for_ext_with_caps("pdf", &caps).is_none());
        assert!(extractor_for_ext_with_caps("doc", &caps).is_none());
        assert!(extractor_for_ext_with_caps("xls", &caps).is_none());
        assert!(extractor_for_ext_with_caps("ppt", &caps).is_none());
        assert!(extractor_for_ext_with_caps("docx", &caps).is_some());
        assert!(extractor_for_ext_with_caps("odt", &caps).is_some());
        assert!(extractor_for_ext_with_caps("rtf", &caps).is_some());
    }

    #[test]
    fn test_extractor_for_ext_all_available_no_timeout() {
        let caps = ExtractorCapabilities::all_available_no_timeout();
        assert!(extractor_for_ext_with_caps("pdf", &caps).is_some());
        assert!(extractor_for_ext_with_caps("doc", &caps).is_some());
        assert!(extractor_for_ext_with_caps("rtf", &caps).is_some());
        assert!(extractor_for_ext_with_caps("xyz", &caps).is_none());
    }

    #[test]
    fn all_available_no_timeout_sets_ocr_true() {
        let caps = ExtractorCapabilities::all_available_no_timeout();
        assert!(caps.has_tesseract);
        assert!(caps.has_pdftoppm);
        assert!(caps.ocr_enabled);
        assert!(!caps.tesseract_langs.is_empty());
        assert!(caps.tesseract_langs.contains(&"eng".to_string()));
        assert!(caps.tesseract_langs.contains(&"rus".to_string()));
        assert!(caps.tesseract_langs.contains(&"chi_sim".to_string()));
        let mut sorted = caps.tesseract_langs.clone();
        sorted.sort();
        assert_eq!(caps.tesseract_langs, sorted);
    }

    fn no_command_exists(_: &str) -> bool {
        false
    }
    fn all_commands_exist(_: &str) -> bool {
        true
    }
    fn only_tesseract_exists(name: &str) -> bool {
        name == "tesseract"
    }
    fn langs_empty() -> Vec<String> {
        Vec::new()
    }
    fn langs_eng_rus() -> Vec<String> {
        vec!["eng".into(), "rus".into()]
    }

    #[test]
    fn probe_without_tesseract_zeroes_ocr_fields() {
        let caps = ExtractorCapabilities::probe_with(
            Duration::from_secs(15),
            no_command_exists,
            langs_empty,
        );
        assert!(!caps.has_tesseract);
        assert!(!caps.has_pdftoppm);
        assert!(caps.tesseract_langs.is_empty());
        assert!(!caps.ocr_enabled);
        assert!(!caps.has_pdftotext);
    }

    #[test]
    fn probe_with_tesseract_populates_langs() {
        let caps = ExtractorCapabilities::probe_with(
            Duration::from_secs(15),
            all_commands_exist,
            langs_eng_rus,
        );
        assert!(caps.has_tesseract);
        assert!(caps.has_pdftoppm);
        assert_eq!(caps.tesseract_langs, vec!["eng", "rus"]);
        assert!(
            !caps.ocr_enabled,
            "ocr_enabled stays false until daemon flips it"
        );
    }

    #[test]
    fn probe_with_tesseract_but_empty_langs_disables_ocr() {
        let caps = ExtractorCapabilities::probe_with(
            Duration::from_secs(15),
            only_tesseract_exists,
            langs_empty,
        );
        assert!(
            !caps.has_tesseract,
            "empty langs must downgrade to has_tesseract=false"
        );
        assert!(caps.tesseract_langs.is_empty());
    }

    #[test]
    fn parse_tesseract_langs_stdout_modern() {
        let stdout = b"List of available languages (3):\neng\nrus\nchi_sim\n";
        let langs = parse_tesseract_langs(stdout, b"");
        assert_eq!(langs, vec!["chi_sim", "eng", "rus"]);
    }

    #[test]
    fn parse_tesseract_langs_stderr_legacy() {
        let stderr = b"List of available languages (2):\neng\nrus\n";
        let langs = parse_tesseract_langs(b"", stderr);
        assert_eq!(langs, vec!["eng", "rus"]);
    }

    #[test]
    fn parse_tesseract_langs_dedups_across_streams() {
        let langs = parse_tesseract_langs(b"eng\nrus\n", b"eng\nchi_sim\n");
        assert_eq!(langs, vec!["chi_sim", "eng", "rus"]);
    }

    #[test]
    fn parse_tesseract_langs_rejects_headers_and_noise() {
        let stdout = b"List of available languages (3):\neng\n  rus  \nABC\n123\n\nchi_sim\nosd\n";
        let langs = parse_tesseract_langs(stdout, b"");
        assert_eq!(
            langs,
            vec!["chi_sim", "eng", "osd", "rus"],
            "uppercase `ABC`, digits `123`, header line and blanks must be rejected; `rus` passes after trim"
        );
    }
}
