use anyhow::Result;
use base64::Engine;
use mailparse::MailHeaderMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartEncoding {
    Base64,
    QuotedPrintable,
    SevenBit,
    EightBit,
    Binary,
}

impl PartEncoding {
    pub fn as_mime_str(&self) -> &'static str {
        match self {
            Self::Base64 => "base64",
            Self::QuotedPrintable => "quoted-printable",
            Self::SevenBit => "7bit",
            Self::EightBit => "8bit",
            Self::Binary => "binary",
        }
    }

    pub fn from_header(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "base64" => Self::Base64,
            "quoted-printable" => Self::QuotedPrintable,
            "7bit" => Self::SevenBit,
            "8bit" => Self::EightBit,
            "binary" => Self::Binary,
            _ => Self::SevenBit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MboxPart {
    pub mbox_path: PathBuf,
    pub message_id: Option<String>,
    pub part_index: usize,
    pub msg_byte_offset: u64,
    pub msg_length: u64,
    pub part_body_byte_offset: u64,
    pub part_body_length: u64,
    pub filename: String,
    pub mime: String,
    pub encoding: PartEncoding,
    pub subject: Option<String>,
    /// Parent message `Date:` header as Unix seconds (R4). `None`
    /// when the header is absent or unparseable; the attachment
    /// source then falls back to the 0 "unknown" mtime sentinel.
    pub date: Option<i64>,
}

pub fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if i + 4 <= bytes.len() && &bytes[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
        if i + 2 <= bytes.len() && &bytes[i..i + 2] == b"\n\n" {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

pub fn split_mbox_messages(mbox_bytes: &[u8]) -> Vec<(usize, &[u8])> {
    let mut results = Vec::new();
    if mbox_bytes.is_empty() {
        return results;
    }

    let mut starts: Vec<usize> = Vec::new();
    if mbox_bytes.starts_with(b"From ") {
        starts.push(0);
    }
    let mut i = 0;
    while i + 5 < mbox_bytes.len() {
        if mbox_bytes[i] == b'\n' && &mbox_bytes[i + 1..i + 6] == b"From " {
            starts.push(i + 1);
            i += 6;
        } else {
            i += 1;
        }
    }

    if starts.is_empty() {
        results.push((0, mbox_bytes));
        return results;
    }

    for (idx, &start) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() {
            starts[idx + 1]
        } else {
            mbox_bytes.len()
        };
        results.push((start, &mbox_bytes[start..end]));
    }

    results
}

pub fn decode_bytes(raw: &[u8], enc: PartEncoding) -> Result<Vec<u8>> {
    match enc {
        PartEncoding::Base64 => {
            let filtered: Vec<u8> = raw
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            Ok(base64::engine::general_purpose::STANDARD.decode(filtered)?)
        }
        PartEncoding::QuotedPrintable => {
            let decoded = quoted_printable::decode(raw, quoted_printable::ParseMode::Robust)?;
            let mut normalized = Vec::with_capacity(decoded.len());
            let mut i = 0;
            while i < decoded.len() {
                if decoded[i] == b'\r' {
                    normalized.push(b'\n');
                    if i + 1 < decoded.len() && decoded[i + 1] == b'\n' {
                        i += 2;
                        continue;
                    }
                } else {
                    normalized.push(decoded[i]);
                }
                i += 1;
            }
            Ok(normalized)
        }
        PartEncoding::SevenBit | PartEncoding::EightBit | PartEncoding::Binary => Ok(raw.to_vec()),
    }
}

pub fn sanitize_filename(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !matches!(*c, '/' | '\\' | '\0'))
        .collect();
    out = out.trim_start_matches('.').to_string();
    if out.is_empty() {
        out = "attachment".to_string();
    }
    if out.len() > 200 {
        let mut cut = 200;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

pub fn fallback_id(path: &Path, offset: u64) -> String {
    let mut hasher = DefaultHasher::new();
    path.as_os_str().as_encoded_bytes().hash(&mut hasher);
    format!("{:016x}-{offset}", hasher.finish())
}

pub fn parse_mbox_parts(path: &Path) -> Result<Vec<MboxPart>> {
    let bytes = std::fs::read(path)?;
    parse_mbox_parts_from_bytes(&bytes, path)
}

pub fn parse_mbox_parts_from_bytes(bytes: &[u8], path: &Path) -> Result<Vec<MboxPart>> {
    let mut results = Vec::new();

    for (msg_offset, msg_slice) in split_mbox_messages(bytes) {
        let Some((message_start_in_msg, message_bytes)) = strip_mbox_envelope(msg_slice) else {
            continue;
        };
        let Ok(parsed) = mailparse::parse_mail(message_bytes) else {
            continue;
        };
        let message_id = parsed.headers.get_first_value("Message-ID").map(|value| {
            value
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        });
        let subject = parsed.headers.get_first_value("Subject");
        // Headers are already parsed for Message-ID/Subject; reading
        // Date is the same cost. `mailparse::dateparse` yields Unix
        // seconds directly.
        let date = parsed
            .headers
            .get_first_value("Date")
            .and_then(|v| mailparse::dateparse(&v).ok());
        let mut part_index = 0;
        let mut ctx = WalkContext {
            msg_slice: message_bytes,
            msg_offset: msg_offset + message_start_in_msg,
            mbox_path: path,
            message_id: &message_id,
            subject: &subject,
            date,
            part_index: &mut part_index,
            results: &mut results,
        };
        walk_parts(&parsed, &mut ctx);
    }

    Ok(results)
}

fn strip_mbox_envelope(msg_slice: &[u8]) -> Option<(usize, &[u8])> {
    if msg_slice.starts_with(b"From ") {
        let start = if let Some(pos) = msg_slice.windows(2).position(|window| window == b"\r\n") {
            pos + 2
        } else if let Some(pos) = msg_slice.iter().position(|b| *b == b'\n') {
            pos + 1
        } else {
            return None;
        };
        return Some((start, &msg_slice[start..]));
    }

    Some((0, msg_slice))
}

struct WalkContext<'a> {
    msg_slice: &'a [u8],
    msg_offset: usize,
    mbox_path: &'a Path,
    message_id: &'a Option<String>,
    subject: &'a Option<String>,
    date: Option<i64>,
    part_index: &'a mut usize,
    results: &'a mut Vec<MboxPart>,
}

fn walk_parts(part: &mailparse::ParsedMail<'_>, ctx: &mut WalkContext<'_>) {
    if part.subparts.is_empty() && is_attachment(part) {
        let raw = part.raw_bytes;
        let part_start_in_msg = raw.as_ptr() as usize - ctx.msg_slice.as_ptr() as usize;
        if let Some(headers_end) = find_headers_end(raw) {
            let filename =
                extract_filename(part).unwrap_or_else(|| format!("attachment-{}", *ctx.part_index));
            let encoding = part
                .headers
                .get_first_value("Content-Transfer-Encoding")
                .map(|value| PartEncoding::from_header(&value))
                .unwrap_or(PartEncoding::SevenBit);
            let body_offset = ctx.msg_offset + part_start_in_msg + headers_end;
            let body_length = raw.len().saturating_sub(headers_end);

            ctx.results.push(MboxPart {
                mbox_path: ctx.mbox_path.to_path_buf(),
                message_id: ctx.message_id.clone(),
                part_index: *ctx.part_index,
                msg_byte_offset: ctx.msg_offset as u64,
                msg_length: ctx.msg_slice.len() as u64,
                part_body_byte_offset: body_offset as u64,
                part_body_length: body_length as u64,
                filename: sanitize_filename(&filename),
                mime: part.ctype.mimetype.clone(),
                encoding,
                subject: ctx.subject.clone(),
                date: ctx.date,
            });
            *ctx.part_index += 1;
        }
    }

    for subpart in &part.subparts {
        walk_parts(subpart, ctx);
    }
}

fn is_attachment(part: &mailparse::ParsedMail<'_>) -> bool {
    if let Some(disposition) = part.headers.get_first_value("Content-Disposition") {
        let disposition_lower = disposition.to_ascii_lowercase();
        if disposition_lower.contains("attachment") || disposition_lower.contains("filename=") {
            return true;
        }
    }

    part.ctype.params.contains_key("name")
}

fn extract_filename(part: &mailparse::ParsedMail<'_>) -> Option<String> {
    if let Some(disposition) = part.headers.get_first_value("Content-Disposition")
        && let Some(start) = disposition.to_ascii_lowercase().find("filename=")
    {
        let rest = &disposition[start + 9..];
        let filename = rest
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if !filename.is_empty() {
            return Some(filename);
        }
    }

    part.ctype.params.get("name").cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PDF_BASE64: &str = "JVBERi0xLjQKJeLjz9MKdHJhaWxlcjw8Pj4KJSVFT0Y=";
    const PDF_BYTES: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\ntrailer<<>>\n%%EOF";

    fn write_fixture(name: &str, content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    }

    fn base64_pdf_fixture(line_ending: &str) -> Vec<u8> {
        format!(
            concat!(
                "From alice@example.com Wed Jan 01 00:00:00 2025{e}",
                "From: alice@example.com{e}",
                "To: bob@example.com{e}",
                "Subject: Hello with PDF{e}",
                "Message-ID: <abc@example.com>{e}",
                "Content-Type: multipart/mixed; boundary=\"BB\"{e}",
                "MIME-Version: 1.0{e}",
                "{e}",
                "--BB{e}",
                "Content-Type: text/plain{e}",
                "{e}",
                "Hello body{e}",
                "--BB{e}",
                "Content-Type: application/pdf{e}",
                "Content-Disposition: attachment; filename=\"test.pdf\"{e}",
                "Content-Transfer-Encoding: base64{e}",
                "{e}",
                "{body}{e}",
                "--BB--{e}"
            ),
            e = line_ending,
            body = PDF_BASE64,
        )
        .into_bytes()
    }

    fn qp_text_fixture(line_ending: &str) -> Vec<u8> {
        format!(
            concat!(
                "From alice@example.com Wed Jan 01 00:00:00 2025{e}",
                "From: alice@example.com{e}",
                "Subject: Hello note{e}",
                "Message-ID: <qp@example.com>{e}",
                "Content-Type: multipart/mixed; boundary=\"BB\"{e}",
                "MIME-Version: 1.0{e}",
                "{e}",
                "--BB{e}",
                "Content-Type: text/plain; name=\"note.txt\"{e}",
                "Content-Disposition: attachment; filename=\"note.txt\"{e}",
                "Content-Transfer-Encoding: quoted-printable{e}",
                "{e}",
                "Hello=20World{e}",
                "--BB--{e}"
            ),
            e = line_ending,
        )
        .into_bytes()
    }

    #[test]
    fn test_find_headers_end_crlf() {
        assert_eq!(find_headers_end(b"Header: val\r\n\r\nbody"), Some(15));
    }

    #[test]
    fn test_find_headers_end_lf() {
        assert_eq!(find_headers_end(b"Header: val\n\nbody"), Some(13));
    }

    #[test]
    fn test_find_headers_end_missing() {
        assert_eq!(find_headers_end(b"Header: val"), None);
    }

    #[test]
    fn test_split_mbox_messages_single() {
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\nSubject: One\n\nBody\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, 0);
        assert_eq!(messages[0].1, mbox);
    }

    #[test]
    fn test_split_mbox_messages_two_lf() {
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\nSubject: One\n\nBody one\n\nFrom b@example.com Wed Jan 01 00:00:01 2025\nSubject: Two\n\nBody two\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, 0);
        assert_eq!(messages[1].0, find_bytes(mbox, b"\n\nFrom ") + 2);
    }

    #[test]
    fn test_split_mbox_messages_two_crlf() {
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\r\nSubject: One\r\n\r\nBody one\r\n\r\nFrom b@example.com Wed Jan 01 00:00:01 2025\r\nSubject: Two\r\n\r\nBody two\r\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].0, 0);
        assert_eq!(messages[1].0, find_bytes(mbox, b"\r\n\r\nFrom ") + 4);
    }

    #[test]
    fn test_split_mbox_messages_escaped_from_in_body_single() {
        // Well-formed mbox writers escape body lines starting with "From " as ">From ".
        // The escaped form must NOT be treated as a message boundary.
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\nSubject: One\n\nBody line\n>From nothing\nStill body\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_split_mbox_standard_format_no_blank_line() {
        // Standard mbox (RFC 4155): messages separated by just \n before "From "
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\nSubject: One\n\nBody one\nFrom b@example.com Wed Jan 01 00:00:01 2025\nSubject: Two\n\nBody two\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_split_mbox_standard_format_crlf_no_blank_line() {
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\r\nSubject: One\r\n\r\nBody one\r\nFrom b@example.com Wed Jan 01 00:00:01 2025\r\nSubject: Two\r\n\r\nBody two\r\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_split_mbox_escaped_from_in_body_not_split() {
        // ">From " in body must NOT be treated as new message (it's an escaped "From")
        let mbox = b"From a@example.com Wed Jan 01 00:00:00 2025\nSubject: One\n\nBody\n>From hacker@example.com\nMore body\nFrom b@example.com Wed Jan 01 00:00:01 2025\nSubject: Two\n\nBody two\n";
        let messages = split_mbox_messages(mbox);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_parse_single_part_base64_pdf() {
        let fixture = base64_pdf_fixture("\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        assert_eq!(part.filename, "test.pdf");
        assert_eq!(part.mime, "application/pdf");
        assert_eq!(part.encoding, PartEncoding::Base64);
        assert_eq!(part.message_id.as_deref(), Some("abc@example.com"));
    }

    #[test]
    fn test_parse_absolute_offsets_roundtrip() {
        let fixture = base64_pdf_fixture("\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        let start = part.part_body_byte_offset as usize;
        let end = start + part.part_body_length as usize;
        let raw = &fixture[start..end];
        let decoded = decode_bytes(raw, PartEncoding::Base64).unwrap();
        assert_eq!(decoded, PDF_BYTES);
    }

    #[test]
    fn test_parse_qp_text_attachment() {
        let fixture = qp_text_fixture("\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        assert_eq!(part.filename, "note.txt");
        assert_eq!(part.mime, "text/plain");
        assert_eq!(part.encoding, PartEncoding::QuotedPrintable);
        let start = part.part_body_byte_offset as usize;
        let end = start + part.part_body_length as usize;
        let decoded = decode_bytes(&fixture[start..end], part.encoding).unwrap();
        assert_eq!(decoded, b"Hello World\n");
    }

    #[test]
    fn test_parse_crlf_line_endings() {
        let fixture = base64_pdf_fixture("\r\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].filename, "test.pdf");
        assert_eq!(parts[0].mime, "application/pdf");
        assert_eq!(parts[0].encoding, PartEncoding::Base64);
        assert_eq!(parts[0].message_id.as_deref(), Some("abc@example.com"));
    }

    #[test]
    fn test_parse_nested_multipart() {
        let fixture = b"From alice@example.com Wed Jan 01 00:00:00 2025\nFrom: alice@example.com\nSubject: Nested multipart test\nMessage-ID: <nested@example.com>\nContent-Type: multipart/mixed; boundary=\"outer\"\nMIME-Version: 1.0\n\n--outer\nContent-Type: multipart/alternative; boundary=\"inner\"\n\n--inner\nContent-Type: text/plain\n\nPlain text version\n--inner\nContent-Type: text/html\n\n<html><body>HTML version</body></html>\n--inner--\n--outer\nContent-Type: application/pdf\nContent-Disposition: attachment; filename=\"nested.pdf\"\nContent-Transfer-Encoding: base64\n\nJVBERi0xLjQKJeLjz9MKdHJhaWxlcjw8Pj4KJSVFT0Y=\n--outer--\n";
        let (_dir, path) = write_fixture("nested", fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].filename, "nested.pdf");
        assert_eq!(parts[0].message_id.as_deref(), Some("nested@example.com"));
    }

    #[test]
    fn test_parse_no_attachments() {
        let fixture = b"From test@example.com Wed Jan 01 00:00:00 2025\nFrom: test@example.com\nSubject: No attachments\nContent-Type: text/plain\n\nJust a plain text message.\n";
        let (_dir, path) = write_fixture("plain", fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn test_decode_bytes_base64_roundtrip() {
        let encoded = b"SGVsbG8gV29ybGQ=";
        assert_eq!(
            decode_bytes(encoded, PartEncoding::Base64).unwrap(),
            b"Hello World"
        );
    }

    #[test]
    fn test_decode_bytes_quoted_printable() {
        assert_eq!(
            decode_bytes(b"Hello=20World", PartEncoding::QuotedPrintable).unwrap(),
            b"Hello World"
        );
    }

    #[test]
    fn test_decode_bytes_7bit_passthrough() {
        assert_eq!(
            decode_bytes(b"plain text", PartEncoding::SevenBit).unwrap(),
            b"plain text"
        );
    }

    #[test]
    fn test_sanitize_filename_traversal() {
        let sanitized = sanitize_filename("../../../etc/passwd");
        assert!(!sanitized.contains('/'));
    }

    #[test]
    fn test_fallback_id_deterministic() {
        let path = Path::new("/tmp/mail/Inbox");
        assert_eq!(fallback_id(path, 42), fallback_id(path, 42));
    }

    #[test]
    fn test_sanitize_filename_multibyte_no_panic_at_boundary() {
        // 3-byte UTF-8 chars (CJK ideograph) — 100 chars = 300 bytes. Truncate should not panic.
        let input = "あ".repeat(100);
        assert_eq!(input.len(), 300);
        let out = sanitize_filename(&input);
        // Result must be valid UTF-8 and <=200 bytes
        assert!(out.len() <= 200);
        // Must not panic — reaching this line means success
    }

    #[test]
    fn test_sanitize_filename_emoji_no_panic_at_boundary() {
        // 4-byte UTF-8 chars (emoji) — 60 emojis = 240 bytes
        let input = "😀".repeat(60);
        assert!(input.len() > 200);
        let out = sanitize_filename(&input);
        assert!(out.len() <= 200);
    }

    #[test]
    fn test_parse_date_header_to_unix_seconds() {
        let fixture = b"From alice@example.com Fri Nov 01 10:00:00 2024\nFrom: alice@example.com\nSubject: Dated\nDate: Fri, 01 Nov 2024 10:00:00 +0000\nMessage-ID: <dated@example.com>\nContent-Type: multipart/mixed; boundary=\"BB\"\nMIME-Version: 1.0\n\n--BB\nContent-Type: text/plain\n\nhi\n--BB\nContent-Type: text/plain; name=\"a.txt\"\nContent-Disposition: attachment; filename=\"a.txt\"\nContent-Transfer-Encoding: base64\n\naGk=\n--BB--\n";
        let (_dir, path) = write_fixture("dated", fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        // 2024-11-01T10:00:00Z
        assert_eq!(parts[0].date, Some(1_730_455_200));
    }

    #[test]
    fn test_parse_missing_date_header_is_none() {
        let fixture = base64_pdf_fixture("\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts = parse_mbox_parts(&path).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].date, None);
    }

    #[test]
    fn test_parse_mbox_parts_from_bytes_equivalence() {
        let fixture = base64_pdf_fixture("\n");
        let (_dir, path) = write_fixture("inbox", &fixture);
        let parts_from_path = parse_mbox_parts(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let parts_from_bytes = parse_mbox_parts_from_bytes(&bytes, &path).unwrap();
        assert_eq!(parts_from_bytes.len(), parts_from_path.len());
        assert_eq!(parts_from_bytes[0].filename, parts_from_path[0].filename);
        assert_eq!(
            parts_from_bytes[0].part_body_byte_offset,
            parts_from_path[0].part_body_byte_offset
        );
        assert_eq!(
            parts_from_bytes[0].part_body_length,
            parts_from_path[0].part_body_length
        );
        assert_eq!(
            parts_from_bytes[0].message_id,
            parts_from_path[0].message_id
        );
    }
}
