pub mod walk;

mod factory;

pub use factory::MaildirFactory;

use anyhow::Result;
use lixun_core::{
    Action, Category, DocId, Document, ExtraFieldValue, PluginFieldSpec, PluginFieldType,
    PluginValue, TextTokenizer,
};
use lixun_sources::source::{
    IndexerSource, Mutation, MutationSink, SourceContext, SourceEvent, SourceEventKind, WatchSpec,
};
use std::path::{Path, PathBuf};

pub const MAILDIR_FIELDS: &[PluginFieldSpec] = &[
    PluginFieldSpec {
        schema_name: "maildir_folder",
        query_alias: Some("folder"),
        ty: PluginFieldType::Keyword,
        stored: true,
        default_search: false,
        boost: 0.0,
    },
    PluginFieldSpec {
        schema_name: "maildir_flags",
        query_alias: Some("flags"),
        ty: PluginFieldType::Keyword,
        stored: true,
        default_search: false,
        boost: 0.0,
    },
    PluginFieldSpec {
        schema_name: "maildir_from",
        query_alias: Some("from"),
        ty: PluginFieldType::Text {
            tokenizer: TextTokenizer::Spotlight,
        },
        stored: true,
        default_search: true,
        boost: 3.0,
    },
    PluginFieldSpec {
        schema_name: "maildir_to",
        query_alias: Some("to"),
        ty: PluginFieldType::Text {
            tokenizer: TextTokenizer::Spotlight,
        },
        stored: true,
        default_search: true,
        boost: 2.5,
    },
];

pub struct MaildirSource {
    pub roots: Vec<PathBuf>,
    pub open_cmd: Vec<String>,
}

impl MaildirSource {
    pub fn new(roots: Vec<PathBuf>, open_cmd: Vec<String>) -> Self {
        Self { roots, open_cmd }
    }

    fn find_root_for(&self, msg_path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .find(|r| msg_path.starts_with(r))
            .map(|p| p.as_path())
    }

    fn parse_message(&self, root: &Path, msg_path: &Path) -> Result<Option<Document>> {
        let bytes = match std::fs::read(msg_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("maildir: skipping unreadable {}: {}", msg_path.display(), e);
                return Ok(None);
            }
        };

        let parsed = match mailparse::parse_mail(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "maildir: skipping unparseable {}: {}",
                    msg_path.display(),
                    e
                );
                return Ok(None);
            }
        };

        let header = |name: &str| -> Option<String> {
            parsed
                .headers
                .iter()
                .find(|h| h.get_key_ref().eq_ignore_ascii_case(name))
                .map(|h| h.get_value())
        };

        let subject = header("Subject").unwrap_or_else(|| "(no subject)".into());
        let from = header("From").unwrap_or_default();
        let to = header("To").unwrap_or_default();
        let cc = header("Cc").unwrap_or_default();
        let recipients_all = if cc.is_empty() {
            to.clone()
        } else {
            format!("{}, {}", to, cc)
        };

        let header_message_id = header("Message-ID").unwrap_or_default();
        let message_id = strip_angle_brackets(&header_message_id);

        let body_text = parsed.get_body().unwrap_or_default();

        let filename_str = msg_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let folder = walk::derive_folder(root, msg_path);
        let flags = walk::parse_flags(&filename_str);

        let path_str = msg_path.to_string_lossy().to_string();
        let doc_id = format!("maildir:{}", path_str);

        let action = if !self.open_cmd.is_empty() {
            let rendered: Vec<String> = self
                .open_cmd
                .iter()
                .map(|s| {
                    s.replace("{folder}", &folder)
                        .replace("{path}", &path_str)
                        .replace("{message_id}", &message_id)
                })
                .collect();
            Action::Exec {
                cmdline: rendered,
                working_dir: None,
            }
        } else {
            Action::OpenFile {
                path: msg_path.to_path_buf(),
            }
        };

        let mut extra: Vec<ExtraFieldValue> = Vec::new();
        extra.push(ExtraFieldValue {
            field: "maildir_folder",
            value: PluginValue::Text(folder.clone()),
        });
        for f in &flags {
            extra.push(ExtraFieldValue {
                field: "maildir_flags",
                value: PluginValue::Text((*f).to_string()),
            });
        }
        if !from.is_empty() {
            extra.push(ExtraFieldValue {
                field: "maildir_from",
                value: PluginValue::Text(from.clone()),
            });
        }
        if !recipients_all.is_empty() {
            extra.push(ExtraFieldValue {
                field: "maildir_to",
                value: PluginValue::Text(recipients_all.clone()),
            });
        }

        Ok(Some(Document {
            id: DocId(doc_id),
            category: Category::Mail,
            title: subject,
            subtitle: from.clone(),
            icon_name: Some("mail-message".into()),
            kind_label: Some("Email".into()),
            body: if body_text.is_empty() {
                None
            } else {
                Some(body_text)
            },
            path: path_str,
            mtime: 0,
            size: bytes.len() as u64,
            action,
            extract_fail: false,
            sender: if from.is_empty() { None } else { Some(from) },
            recipients: if recipients_all.is_empty() {
                None
            } else {
                Some(recipients_all)
            },
            source_instance: String::new(),
            secondary_action: None,
            extra,
        }))
    }
}

fn strip_angle_brackets(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('<') && t.ends_with('>') {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn maildir_doc_id_for(path: &Path) -> String {
    format!("maildir:{}", path.to_string_lossy())
}

impl IndexerSource for MaildirSource {
    fn kind(&self) -> &'static str {
        "maildir"
    }

    fn extra_fields(&self) -> &'static [PluginFieldSpec] {
        MAILDIR_FIELDS
    }

    fn watch_paths(&self, _ctx: &SourceContext) -> Result<Vec<WatchSpec>> {
        Ok(self
            .roots
            .iter()
            .filter(|p| p.exists())
            .map(|p| WatchSpec {
                path: p.clone(),
                recursive: true,
            })
            .collect())
    }

    fn on_fs_events(
        &self,
        ctx: &SourceContext,
        events: &[SourceEvent],
        sink: &dyn MutationSink,
    ) -> Result<()> {
        for event in events {
            if !walk::is_message_path(&event.path) {
                continue;
            }
            let Some(root) = self.find_root_for(&event.path) else {
                continue;
            };
            match &event.kind {
                SourceEventKind::Created | SourceEventKind::Modified => {
                    if let Some(mut doc) = self.parse_message(root, &event.path)? {
                        doc.source_instance = ctx.instance_id.to_string();
                        sink.emit(Mutation::Upsert(Box::new(doc)))?;
                    }
                }
                SourceEventKind::Removed => {
                    sink.emit(Mutation::Delete {
                        doc_id: maildir_doc_id_for(&event.path),
                    })?;
                }
                SourceEventKind::Renamed { from } => {
                    sink.emit(Mutation::Delete {
                        doc_id: maildir_doc_id_for(from),
                    })?;
                    if let Some(mut doc) = self.parse_message(root, &event.path)? {
                        doc.source_instance = ctx.instance_id.to_string();
                        sink.emit(Mutation::Upsert(Box::new(doc)))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn reindex_full(&self, ctx: &SourceContext, sink: &dyn MutationSink) -> Result<()> {
        let instance_id = ctx.instance_id.to_string();
        sink.emit(Mutation::DeleteSourceInstance {
            instance_id: instance_id.clone(),
        })?;

        const BATCH: usize = 500;
        let mut batch: Vec<Document> = Vec::with_capacity(BATCH);
        let mut count = 0usize;
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            for msg_path in walk::walk_messages(root) {
                if let Some(mut doc) = self.parse_message(root, &msg_path)? {
                    doc.source_instance = instance_id.clone();
                    batch.push(doc);
                    count += 1;
                    if batch.len() >= BATCH {
                        sink.emit(Mutation::UpsertMany(std::mem::take(&mut batch)))?;
                    }
                }
            }
        }
        if !batch.is_empty() {
            sink.emit(Mutation::UpsertMany(batch))?;
        }
        tracing::info!(
            "maildir source '{}': reindex_full emitted {} docs",
            ctx.instance_id,
            count
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CaptureSink(Mutex<Vec<Mutation>>);

    impl MutationSink for CaptureSink {
        fn emit(&self, m: Mutation) -> Result<()> {
            self.0.lock().unwrap().push(m);
            Ok(())
        }
    }

    fn write_message(dir: &Path, filename: &str, content: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(filename);
        std::fs::write(&path, content).unwrap();
        path
    }

    const BASIC_EML: &str =
        "From: alice@example.com\r\nTo: bob@example.com, carol@example.com\r\nSubject: Weekly sync\r\nMessage-ID: <week-1@example.com>\r\n\r\nSee you tomorrow.\r\n";

    #[test]
    fn reindex_full_emits_delete_then_upsert() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Mail");
        let inbox_cur = root.join("INBOX").join("cur");
        write_message(&inbox_cur, "1.M.host:2,S", BASIC_EML);

        let source = MaildirSource::new(vec![root.clone()], vec![]);
        let state = tempfile::tempdir().unwrap();
        let ctx = SourceContext {
            instance_id: "personal-mail",
            state_dir: state.path(),
        };
        let sink = CaptureSink(Mutex::new(Vec::new()));
        source.reindex_full(&ctx, &sink).unwrap();

        let mutations = sink.0.into_inner().unwrap();
        assert_eq!(
            mutations.len(),
            2,
            "expected DeleteSourceInstance + 1 UpsertMany"
        );
        assert!(matches!(
            mutations[0],
            Mutation::DeleteSourceInstance { .. }
        ));
        let Mutation::UpsertMany(docs) = &mutations[1] else {
            panic!("expected UpsertMany");
        };
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert_eq!(doc.title, "Weekly sync");
        assert_eq!(doc.sender.as_deref(), Some("alice@example.com"));
        assert_eq!(doc.source_instance, "personal-mail");
        assert!(doc.extra.iter().any(|v| v.field == "maildir_folder"
            && matches!(&v.value, PluginValue::Text(s) if s == "INBOX")));
        assert!(doc.extra.iter().any(|v| v.field == "maildir_flags"
            && matches!(&v.value, PluginValue::Text(s) if s == "seen")));
    }

    #[test]
    fn on_fs_events_removed_emits_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Mail");
        let cur = root.join("INBOX").join("cur");
        std::fs::create_dir_all(&cur).unwrap();
        let ghost = cur.join("99.M.host:2,");

        let source = MaildirSource::new(vec![root.clone()], vec![]);
        let state = tempfile::tempdir().unwrap();
        let ctx = SourceContext {
            instance_id: "p",
            state_dir: state.path(),
        };
        let sink = CaptureSink(Mutex::new(Vec::new()));
        let events = vec![SourceEvent {
            path: ghost.clone(),
            kind: SourceEventKind::Removed,
        }];
        source.on_fs_events(&ctx, &events, &sink).unwrap();

        let mutations = sink.0.into_inner().unwrap();
        assert_eq!(mutations.len(), 1);
        match &mutations[0] {
            Mutation::Delete { doc_id } => {
                assert_eq!(doc_id, &maildir_doc_id_for(&ghost));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn action_exec_with_templated_open_cmd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Mail");
        let cur = root.join("INBOX").join("cur");
        let msg = write_message(&cur, "1.M.host:2,", BASIC_EML);

        let source = MaildirSource::new(
            vec![root.clone()],
            vec!["neomutt".into(), "-f".into(), "{folder}".into()],
        );
        let doc = source.parse_message(&root, &msg).unwrap().unwrap();
        match doc.action {
            Action::Exec { cmdline, .. } => {
                assert_eq!(cmdline, vec!["neomutt", "-f", "INBOX"]);
            }
            _ => panic!("expected Action::Exec"),
        }
    }

    #[test]
    fn empty_open_cmd_yields_open_file_action() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Mail");
        let cur = root.join("INBOX").join("cur");
        let msg = write_message(&cur, "1.M.host:2,", BASIC_EML);

        let source = MaildirSource::new(vec![root.clone()], vec![]);
        let doc = source.parse_message(&root, &msg).unwrap().unwrap();
        assert!(matches!(doc.action, Action::OpenFile { .. }));
    }

    #[test]
    fn skips_tmp_dir_and_unparseable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Mail");
        let cur = root.join("INBOX").join("cur");
        let tmp_dir = root.join("INBOX").join("tmp");
        write_message(&cur, "1.M.host:2,", BASIC_EML);
        write_message(&tmp_dir, "junk", BASIC_EML);

        let source = MaildirSource::new(vec![root.clone()], vec![]);
        let ctx = SourceContext {
            instance_id: "p",
            state_dir: tmp.path(),
        };
        let sink = CaptureSink(Mutex::new(Vec::new()));
        source.reindex_full(&ctx, &sink).unwrap();

        let count_upserts: usize = sink
            .0
            .into_inner()
            .unwrap()
            .iter()
            .map(|m| match m {
                Mutation::Upsert(_) => 1usize,
                Mutation::UpsertMany(docs) => docs.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(count_upserts, 1, "tmp/ must be skipped");
    }
}
