//! Thunderbird attachments source — parse mbox files for attachments.

use crate::mbox;
use anyhow::Result;
use lixun_core::{Action, Category, DocId, Document};
use lixun_sources::mime_icons;
use mime_guess::Mime;
use std::path::PathBuf;

pub struct ThunderbirdAttachmentsSource {
    pub profile_path: PathBuf,
    pub max_attachment_bytes: u64,
}

impl ThunderbirdAttachmentsSource {
    pub fn new(profile_path: PathBuf, max_attachment_bytes: u64) -> Self {
        Self {
            profile_path,
            max_attachment_bytes,
        }
    }
}

fn attachment_metadata(mime: &str) -> (Option<String>, Option<String>) {
    let Ok(parsed_mime) = mime.parse::<Mime>() else {
        return (Some("mail-attachment".into()), Some("Attachment".into()));
    };

    let human = mime_icons::human_kind(&parsed_mime);
    (
        Some(mime_icons::mime_to_icon_name(&parsed_mime)),
        Some(format!("Attachment · {human}")),
    )
}

impl ThunderbirdAttachmentsSource {
    pub fn index_all(&self) -> Result<Vec<Document>> {
        let mail_path = self.profile_path.join("Mail");
        let imap_path = self.profile_path.join("ImapMail");
        let mut docs = Vec::new();

        for base in [&mail_path, &imap_path] {
            if !base.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(base)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.ends_with(".msf") || name.starts_with('.') {
                    continue;
                }

                let Ok(mbox_bytes) = std::fs::read(path) else {
                    continue;
                };
                let Ok(parts) = mbox::parse_mbox_parts_from_bytes(&mbox_bytes, path) else {
                    continue;
                };

                for part in parts {
                    let id = DocId(format!(
                        "att:{}#{}",
                        part.message_id.clone().unwrap_or_else(|| mbox::fallback_id(
                            &part.mbox_path,
                            part.msg_byte_offset
                        )),
                        part.part_index
                    ));

                    let (body, extract_fail) = if part.part_body_length > self.max_attachment_bytes
                    {
                        (None, true)
                    } else {
                        let start = part.part_body_byte_offset as usize;
                        let end = start + part.part_body_length as usize;
                        if end > mbox_bytes.len() {
                            (None, true)
                        } else {
                            let raw = &mbox_bytes[start..end];
                            match mbox::decode_bytes(raw, part.encoding) {
                                Ok(decoded) => {
                                    let ext_hint = std::path::Path::new(&part.filename)
                                        .extension()
                                        .and_then(|ext| ext.to_str());
                                    match lixun_extract::cache::cached_extract_bytes(
                                        &decoded,
                                        ext_hint,
                                        &lixun_extract::capabilities(),
                                    ) {
                                        Ok(Some(text)) => (Some(text), false),
                                        Ok(None) => (None, false),
                                        Err(_) => (None, true),
                                    }
                                }
                                Err(_) => (None, true),
                            }
                        }
                    };
                    let (icon_name, kind_label) = attachment_metadata(&part.mime);

                    docs.push(Document {
                        id,
                        category: Category::Attachment,
                        title: part.filename.clone(),
                        subtitle: part
                            .subject
                            .clone()
                            .unwrap_or_else(|| "(no subject)".into()),
                        icon_name,
                        kind_label,
                        body,
                        path: part.mbox_path.to_string_lossy().to_string(),
                        mtime: 0,
                        size: part.part_body_length,
                        action: Action::OpenAttachment {
                            mbox_path: part.mbox_path.clone(),
                            byte_offset: part.part_body_byte_offset,
                            length: part.part_body_length,
                            mime: part.mime.clone(),
                            encoding: part.encoding.as_mime_str().to_string(),
                            suggested_filename: part.filename.clone(),
                        },
                        extract_fail,
                        sender: None,
                        recipients: None,
                        source_instance: "builtin:tb_attachments".into(),
                        secondary_action: part.message_id.as_ref().map(|mid| Action::OpenUri {
                            uri: format!("mid:{mid}"),
                        }),
                        extra: Vec::new(),
                    });
                }
            }
        }

        tracing::info!("Thunderbird attachments: {} documents", docs.len());
        Ok(docs)
    }
}

impl lixun_sources::source::IndexerSource for ThunderbirdAttachmentsSource {
    fn kind(&self) -> &'static str {
        "tb_attachments"
    }

    fn watch_paths(
        &self,
        _ctx: &lixun_sources::source::SourceContext,
    ) -> Result<Vec<lixun_sources::source::WatchSpec>> {
        let mut out = Vec::new();
        for rel in ["Mail", "ImapMail"] {
            let p = self.profile_path.join(rel);
            if p.exists() {
                out.push(lixun_sources::source::WatchSpec {
                    path: p,
                    recursive: true,
                });
            }
        }
        Ok(out)
    }

    fn on_fs_events(
        &self,
        _ctx: &lixun_sources::source::SourceContext,
        events: &[lixun_sources::source::SourceEvent],
        _sink: &dyn lixun_sources::source::MutationSink,
    ) -> Result<()> {
        tracing::info!(
            "attachments: {} fs event(s) observed; no-op (full reindex requires explicit `lixun reindex`)",
            events.len()
        );
        Ok(())
    }

    fn reindex_on_schema_wipe(&self) -> bool {
        false
    }

    fn reindex_full(
        &self,
        ctx: &lixun_sources::source::SourceContext,
        sink: &dyn lixun_sources::source::MutationSink,
    ) -> Result<()> {
        sink.emit(lixun_sources::source::Mutation::DeleteSourceInstance {
            instance_id: ctx.instance_id.to_string(),
        })?;

        let instance_id = ctx.instance_id.to_string();
        let docs = self.index_all()?;
        if !docs.is_empty() {
            let mut batch: Vec<lixun_core::Document> = Vec::with_capacity(docs.len());
            for mut doc in docs {
                doc.source_instance = instance_id.clone();
                batch.push(doc);
            }
            sink.emit(lixun_sources::source::Mutation::UpsertMany(batch))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use tempfile::tempdir;

    #[test]
    fn test_thunderbird_attachments_source_integration() {
        let dir = tempdir().unwrap();
        let profile = dir.path();
        let inbox_dir = profile.join("Mail").join("Local Folders");
        std::fs::create_dir_all(&inbox_dir).unwrap();

        let body = base64::engine::general_purpose::STANDARD.encode("UNIQUE_TEST_MARKER");
        let mbox = format!(
            "From alice@example.com Wed Jan 01 00:00:00 2025\nFrom: alice@example.com\nSubject: Searchable attachment\nMessage-ID: <integration@example.com>\nContent-Type: multipart/mixed; boundary=\"BB\"\nMIME-Version: 1.0\n\n--BB\nContent-Type: text/plain\n\nhello\n--BB\nContent-Type: text/plain; name=\"note.txt\"\nContent-Disposition: attachment; filename=\"note.txt\"\nContent-Transfer-Encoding: base64\n\n{body}\n--BB--\n"
        );
        std::fs::write(inbox_dir.join("Inbox"), mbox).unwrap();

        let source = ThunderbirdAttachmentsSource::new(profile.to_path_buf(), 100 * 1024 * 1024);
        let docs = source.index_all().unwrap();

        assert!(docs.iter().any(|doc| {
            doc.body
                .as_deref()
                .is_some_and(|body| body.contains("UNIQUE_TEST_MARKER"))
        }));
    }

    #[test]
    fn test_attachment_metadata_with_missing_mime_falls_back() {
        let (icon_name, kind_label) = attachment_metadata("");
        assert_eq!(icon_name.as_deref(), Some("mail-attachment"));
        assert_eq!(kind_label.as_deref(), Some("Attachment"));
    }

    #[test]
    fn test_attachment_metadata_with_pdf_mime_uses_helper() {
        let (icon_name, kind_label) = attachment_metadata("application/pdf");
        assert_eq!(icon_name.as_deref(), Some("application-pdf"));
        assert_eq!(kind_label.as_deref(), Some("Attachment · PDF Document"));
    }

    #[test]
    fn attachment_with_message_id_has_openuri_secondary() {
        let dir = tempdir().unwrap();
        let profile = dir.path();
        let inbox_dir = profile.join("Mail").join("Local Folders");
        std::fs::create_dir_all(&inbox_dir).unwrap();

        let body = base64::engine::general_purpose::STANDARD.encode("payload");
        let mbox = format!(
            "From alice@example.com Wed Jan 01 00:00:00 2025\nFrom: alice@example.com\nSubject: With id\nMessage-ID: <mid-present@example.com>\nContent-Type: multipart/mixed; boundary=\"BB\"\nMIME-Version: 1.0\n\n--BB\nContent-Type: text/plain\n\nhi\n--BB\nContent-Type: text/plain; name=\"a.txt\"\nContent-Disposition: attachment; filename=\"a.txt\"\nContent-Transfer-Encoding: base64\n\n{body}\n--BB--\n"
        );
        std::fs::write(inbox_dir.join("Inbox"), mbox).unwrap();

        let source = ThunderbirdAttachmentsSource::new(profile.to_path_buf(), 100 * 1024 * 1024);
        let docs = source.index_all().unwrap();
        let attachment = docs
            .iter()
            .find(|d| matches!(d.category, Category::Attachment))
            .expect("attachment document present");
        match &attachment.secondary_action {
            Some(Action::OpenUri { uri }) => {
                assert_eq!(uri, "mid:mid-present@example.com");
            }
            other => panic!("expected OpenUri secondary, got {:?}", other),
        }
    }

    #[test]
    fn attachment_without_message_id_has_no_secondary() {
        let dir = tempdir().unwrap();
        let profile = dir.path();
        let inbox_dir = profile.join("Mail").join("Local Folders");
        std::fs::create_dir_all(&inbox_dir).unwrap();

        let body = base64::engine::general_purpose::STANDARD.encode("payload");
        let mbox = format!(
            "From alice@example.com Wed Jan 01 00:00:00 2025\nFrom: alice@example.com\nSubject: No id\nContent-Type: multipart/mixed; boundary=\"BB\"\nMIME-Version: 1.0\n\n--BB\nContent-Type: text/plain\n\nhi\n--BB\nContent-Type: text/plain; name=\"a.txt\"\nContent-Disposition: attachment; filename=\"a.txt\"\nContent-Transfer-Encoding: base64\n\n{body}\n--BB--\n"
        );
        std::fs::write(inbox_dir.join("Inbox"), mbox).unwrap();

        let source = ThunderbirdAttachmentsSource::new(profile.to_path_buf(), 100 * 1024 * 1024);
        let docs = source.index_all().unwrap();
        let attachment = docs
            .iter()
            .find(|d| matches!(d.category, Category::Attachment))
            .expect("attachment document present");
        assert!(
            attachment.secondary_action.is_none(),
            "expected no secondary when Message-ID header absent, got {:?}",
            attachment.secondary_action
        );
    }
}
