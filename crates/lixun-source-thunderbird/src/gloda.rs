//! Gloda source — Thunderbird global-messages-db.sqlite.

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Document, RowMenuDef, RowMenuItem, RowMenuVerb};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn find_profile() -> Option<PathBuf> {
    GlodaSource::find_profile()
}

/// Retry a fallible operation with bounded backoff for "busy" errors.
/// Generic over error type and classifier for testability.
fn with_busy_retry_generic<T, E, F, C>(backoffs: &[Duration], classify: C, mut f: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    C: Fn(&E) -> bool,
{
    let mut last_err = None;
    for i in 0..=backoffs.len() {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !classify(&e) {
                    return Err(e);
                }
                last_err = Some(e);
                if i < backoffs.len() {
                    std::thread::sleep(backoffs[i]);
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// Classify rusqlite errors as "busy" (DATABASE_BUSY or DATABASE_LOCKED).
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(ffi, _)
        if ffi.code == rusqlite::ErrorCode::DatabaseBusy
        || ffi.code == rusqlite::ErrorCode::DatabaseLocked)
}

/// Concrete retry helper for rusqlite operations with default backoffs.
fn with_busy_retry<T, F>(mut f: F) -> rusqlite::Result<T>
where
    F: FnMut() -> rusqlite::Result<T>,
{
    with_busy_retry_generic(
        &[
            Duration::from_millis(100),
            Duration::from_millis(500),
            Duration::from_millis(2000),
        ],
        is_busy,
        &mut f,
    )
}

pub struct GlodaSource {
    pub profile_path: PathBuf,
    pub last_key: u64,
    pub limit: u32,
}

impl GlodaSource {
    pub fn find_profile() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        let tb_path = PathBuf::from(&home).join(".thunderbird");
        if !tb_path.exists() {
            return None;
        }

        let profiles_ini = tb_path.join("profiles.ini");
        if let Ok(content) = std::fs::read_to_string(&profiles_ini)
            && let Some(path) = parse_profiles_ini_selected(&tb_path, &content)
        {
            return Some(path);
        }

        for entry in std::fs::read_dir(&tb_path).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(".default") {
                return Some(entry.path());
            }
        }
        None
    }

    pub fn new(profile_path: PathBuf, last_key: u64, limit: u32) -> Self {
        Self {
            profile_path,
            last_key,
            limit,
        }
    }

    fn open_db(&self) -> rusqlite::Result<Connection> {
        let db_path = self.profile_path.join("global-messages-db.sqlite");
        with_busy_retry(|| {
            Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        })
    }
}

fn parse_profiles_ini_selected(tb_path: &std::path::Path, content: &str) -> Option<PathBuf> {
    #[derive(Default)]
    struct ProfileSection {
        path: Option<String>,
        is_relative: bool,
    }

    let mut current_path: Option<String> = None;
    let mut current_is_relative = true;
    let mut current_default = false;
    let mut selected_path: Option<String> = None;
    let mut install_default_name: Option<String> = None;
    let mut current_section: Option<String> = None;
    let mut profiles: Vec<ProfileSection> = Vec::new();

    let flush_section = |path: &mut Option<String>,
                         is_relative: &mut bool,
                         default: &mut bool,
                         selected: &mut Option<String>,
                         section: Option<&str>,
                         all_profiles: &mut Vec<ProfileSection>| {
        if let Some(p) = path.take() {
            if section.is_some_and(|s| s.starts_with("Profile")) {
                all_profiles.push(ProfileSection {
                    path: Some(p.clone()),
                    is_relative: *is_relative,
                });
            }
            if *default {
                let resolved = if *is_relative {
                    tb_path.join(&p)
                } else {
                    PathBuf::from(&p)
                };
                *selected = Some(resolved.to_string_lossy().to_string());
            }
        }
        *is_relative = true;
        *default = false;
    };

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            flush_section(
                &mut current_path,
                &mut current_is_relative,
                &mut current_default,
                &mut selected_path,
                current_section.as_deref(),
                &mut profiles,
            );
            current_section = Some(line[1..line.len() - 1].to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Path" => current_path = Some(value.trim().to_string()),
            "IsRelative" => current_is_relative = value.trim() != "0",
            "Default"
                if current_section
                    .as_deref()
                    .is_some_and(|s| s.starts_with("Profile")) =>
            {
                current_default = value.trim() == "1"
            }
            "Default"
                if current_section
                    .as_deref()
                    .is_some_and(|s| s.starts_with("Install")) =>
            {
                install_default_name = Some(value.trim().to_string())
            }
            _ => {}
        }
    }

    flush_section(
        &mut current_path,
        &mut current_is_relative,
        &mut current_default,
        &mut selected_path,
        current_section.as_deref(),
        &mut profiles,
    );

    if let Some(default_name) = install_default_name.as_deref()
        && let Some(profile) = profiles
            .iter()
            .find(|p| p.path.as_deref() == Some(default_name))
        && let Some(path) = profile.path.as_deref()
    {
        return Some(if profile.is_relative {
            tb_path.join(path)
        } else {
            PathBuf::from(path)
        });
    }

    if let Some(path) = selected_path {
        return Some(PathBuf::from(path));
    }

    if let Some(default_name) = install_default_name {
        let direct = tb_path.join(&default_name);
        if direct.exists() {
            return Some(direct);
        }
    }

    None
}

/// Query messages from a Gloda connection. Extracted for testability against
/// an in-memory rusqlite `Connection` seeded with the real Gloda schema.
pub fn query_messages(
    conn: &Connection,
    last_key: u64,
    limit: u32,
) -> rusqlite::Result<Vec<Document>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.messageKey, m.headerMessageID, \
                mt.c1subject, mt.c3author, mt.c4recipients, mt.c0body \
         FROM messages m \
         LEFT JOIN messagesText_content mt ON m.id = mt.docid \
         WHERE m.id > ? AND m.deleted = 0 \
         ORDER BY m.id ASC \
         LIMIT ?",
    )?;

    let rows = stmt.query_map((last_key as i64, limit as i64), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;

    let mut docs = Vec::new();
    for row in rows {
        let (id, message_key, header_message_id, subject, author, recipients, body_opt) = row?;

        let header_id = header_message_id
            .filter(|s| !s.is_empty())
            .map(strip_angle_brackets);
        let message_id = header_id
            .or_else(|| message_key.map(|k| k.to_string()))
            .unwrap_or_default();

        let author_clean = author.filter(|s| !s.is_empty());
        let recipients_clean = recipients.filter(|s| !s.is_empty());

        docs.push(Document {
            id: DocId(format!("mail:{}", id)),
            category: Category::Mail,
            title: subject.unwrap_or_else(|| "(no subject)".into()),
            subtitle: author_clean.clone().unwrap_or_default(),
            icon_name: Some("mail-message".into()),
            kind_label: Some("Email".into()),
            body: body_opt.filter(|s| !s.is_empty()),
            path: format!("thunderbird:{}", id),
            mtime: 0,
            size: 0,
            action: Action::OpenUri {
                uri: format!("mid:{}", message_id),
            },
            extract_fail: false,
            sender: author_clean,
            recipients: recipients_clean,
            source_instance: "builtin:gloda".into(),
            secondary_action: None,
            extra: Vec::new(),
        });
    }

    Ok(docs)
}

/// Strip a single matching pair of angle brackets from an RFC-5322 Message-ID.
/// Returns the input unchanged if it is not a balanced `<...>` pair.
fn strip_angle_brackets(s: String) -> String {
    if s.len() >= 2 && s.starts_with('<') && s.ends_with('>') {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

const GLODA_CURSOR_FILE: &str = "gloda_cursor.json";

fn doc_gloda_id(doc: &Document) -> Option<u64> {
    doc.id.0.strip_prefix("mail:").and_then(|s| s.parse().ok())
}

fn read_cursor(state_dir: &std::path::Path) -> u64 {
    let path = state_dir.join(GLODA_CURSOR_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<u64>(s.trim()).unwrap_or(0),
        Err(_) => 0,
    }
}

fn write_cursor(state_dir: &std::path::Path, last_key: u64) -> Result<()> {
    let _ = std::fs::create_dir_all(state_dir);
    let path = state_dir.join(GLODA_CURSOR_FILE);
    std::fs::write(path, serde_json::to_string(&last_key)?)?;
    Ok(())
}

impl lixun_sources::source::IndexerSource for GlodaSource {
    fn kind(&self) -> &'static str {
        "gloda"
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(30))
    }

    fn row_menu(&self) -> RowMenuDef {
        RowMenuDef {
            items: vec![
                RowMenuItem {
                    label: "Open".into(),
                    verb: RowMenuVerb::Open,
                    visibility: Default::default(),
                },
                RowMenuItem {
                    label: "Copy subject".into(),
                    verb: RowMenuVerb::Copy,
                    visibility: Default::default(),
                },
                RowMenuItem {
                    label: "Info".into(),
                    verb: RowMenuVerb::Info,
                    visibility: Default::default(),
                },
            ],
        }
    }

    fn on_tick(
        &self,
        ctx: &lixun_sources::source::SourceContext,
        sink: &dyn lixun_sources::source::MutationSink,
    ) -> Result<()> {
        let db_path = self.profile_path.join("global-messages-db.sqlite");
        if !db_path.exists() {
            return Ok(());
        }

        let cursor = read_cursor(ctx.state_dir);
        let batch_limit = self.limit;
        let result: rusqlite::Result<Vec<Document>> = with_busy_retry(|| {
            let conn = self.open_db()?;
            query_messages(&conn, cursor, batch_limit)
        });

        let docs = match result {
            Ok(d) => d,
            Err(e) if is_busy(&e) => {
                tracing::warn!("gloda busy after retries; deferring to next tick");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        if docs.is_empty() {
            return Ok(());
        }

        let instance_id = ctx.instance_id.to_string();
        let mut new_cursor = cursor;
        let mut batch: Vec<Document> = Vec::with_capacity(docs.len());
        for mut doc in docs {
            if let Some(gid) = doc_gloda_id(&doc) {
                new_cursor = new_cursor.max(gid);
            }
            doc.source_instance = instance_id.clone();
            batch.push(doc);
        }
        sink.emit(lixun_sources::source::Mutation::UpsertMany(batch))?;

        if new_cursor > cursor {
            let _ = write_cursor(ctx.state_dir, new_cursor);
        }
        Ok(())
    }

    fn reindex_full(
        &self,
        ctx: &lixun_sources::source::SourceContext,
        sink: &dyn lixun_sources::source::MutationSink,
    ) -> Result<()> {
        sink.emit(lixun_sources::source::Mutation::DeleteSourceInstance {
            instance_id: ctx.instance_id.to_string(),
        })?;
        let _ = std::fs::remove_file(ctx.state_dir.join(GLODA_CURSOR_FILE));

        let db_path = self.profile_path.join("global-messages-db.sqlite");
        if !db_path.exists() {
            return Ok(());
        }

        let mut cursor: u64 = 0;
        let batch_limit = self.limit;
        loop {
            let c = cursor;
            let result: rusqlite::Result<Vec<Document>> = with_busy_retry(|| {
                let conn = self.open_db()?;
                query_messages(&conn, c, batch_limit)
            });
            let docs = match result {
                Ok(d) => d,
                Err(e) if is_busy(&e) => {
                    tracing::warn!("gloda busy during reindex_full; aborting this pass");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };

            if docs.is_empty() {
                break;
            }

            let instance_id = ctx.instance_id.to_string();
            let mut new_cursor = cursor;
            let mut batch: Vec<Document> = Vec::with_capacity(docs.len());
            for mut doc in docs {
                if let Some(gid) = doc_gloda_id(&doc) {
                    new_cursor = new_cursor.max(gid);
                }
                doc.source_instance = instance_id.clone();
                batch.push(doc);
            }
            sink.emit(lixun_sources::source::Mutation::UpsertMany(batch))?;

            if new_cursor == cursor {
                break;
            }
            cursor = new_cursor;
        }

        let _ = write_cursor(ctx.state_dir, cursor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn setup_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                folderID INTEGER,
                messageKey INTEGER,
                conversationID INTEGER NOT NULL DEFAULT 0,
                date INTEGER,
                headerMessageID TEXT,
                deleted INTEGER NOT NULL DEFAULT 0,
                jsonAttributes TEXT,
                notability INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE 'messagesText_content' (
                docid INTEGER PRIMARY KEY,
                'c0body', 'c1subject', 'c2attachmentNames', 'c3author', 'c4recipients'
            );",
        )
        .unwrap();
    }

    #[test]
    fn test_gloda_query_against_real_schema() {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn.execute(
            "INSERT INTO messages (id, messageKey, headerMessageID, deleted, conversationID) \
             VALUES (1, 42, '<abc@example.com>', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messagesText_content (docid, c0body, c1subject, c3author, c4recipients) \
             VALUES (1, 'Hello body', 'Hello', 'alice@test', 'bob@test, carol@test')",
            [],
        )
        .unwrap();

        let docs = query_messages(&conn, 0, 1000).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.title, "Hello");
        assert_eq!(d.subtitle, "alice@test");
        assert_eq!(d.sender.as_deref(), Some("alice@test"));
        assert_eq!(d.recipients.as_deref(), Some("bob@test, carol@test"));
        assert!(d.body.is_some());
        assert!(d.body.as_ref().unwrap().contains("Hello body"));
        match &d.action {
            Action::OpenUri { uri } => assert_eq!(uri, "mid:abc@example.com"),
            _ => panic!("expected OpenUri action"),
        }
    }

    #[test]
    fn test_gloda_missing_author_and_recipients_are_none() {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn.execute(
            "INSERT INTO messages (id, messageKey, headerMessageID, deleted, conversationID) \
             VALUES (1, 1, '<x@y>', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messagesText_content (docid, c0body, c1subject) \
             VALUES (1, 'b', 's')",
            [],
        )
        .unwrap();

        let docs = query_messages(&conn, 0, 1000).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(docs[0].sender.is_none());
        assert!(docs[0].recipients.is_none());
    }

    #[test]
    fn test_gloda_deleted_filtered() {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn.execute(
            "INSERT INTO messages (id, messageKey, headerMessageID, deleted, conversationID) \
             VALUES (1, 1, '<one@x>', 0, 0), (2, 2, '<two@x>', 1, 0)",
            [],
        )
        .unwrap();

        let docs = query_messages(&conn, 0, 1000).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id.0, "mail:1");
    }

    #[test]
    fn test_gloda_missing_fts_row() {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn.execute(
            "INSERT INTO messages (id, messageKey, headerMessageID, deleted, conversationID) \
             VALUES (1, 1, '<no-fts@x>', 0, 0)",
            [],
        )
        .unwrap();

        let docs = query_messages(&conn, 0, 1000).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].title, "(no subject)");
        assert!(docs[0].body.is_none());
    }

    #[test]
    fn test_gloda_fallback_to_message_key() {
        let conn = Connection::open_in_memory().unwrap();
        setup_schema(&conn);
        conn.execute(
            "INSERT INTO messages (id, messageKey, headerMessageID, deleted, conversationID) \
             VALUES (1, 42, NULL, 0, 0)",
            [],
        )
        .unwrap();

        let docs = query_messages(&conn, 0, 1000).unwrap();
        assert_eq!(docs.len(), 1);
        match &docs[0].action {
            Action::OpenUri { uri } => assert_eq!(uri, "mid:42"),
            _ => panic!("expected OpenUri action"),
        }
    }

    #[test]
    fn test_strip_angle_brackets() {
        assert_eq!(strip_angle_brackets("<a>".into()), "a");
        assert_eq!(strip_angle_brackets("<a@b.com>".into()), "a@b.com");
        assert_eq!(strip_angle_brackets("noangles".into()), "noangles");
        assert_eq!(strip_angle_brackets("<unbalanced".into()), "<unbalanced");
        assert_eq!(strip_angle_brackets("".into()), "");
    }

    #[test]
    fn retries_on_busy_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<i32, i32> = with_busy_retry_generic(
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            |e: &i32| *e == 5,
            move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 { Err(5) } else { Ok(42) }
            },
        );
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn non_busy_error_not_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<i32, i32> = with_busy_retry_generic(
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            |e: &i32| *e == 5,
            move || {
                let _ = c.fetch_add(1, Ordering::SeqCst);
                Err(99)
            },
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn all_retries_exhausted() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result: Result<i32, i32> = with_busy_retry_generic(
            &[Duration::ZERO, Duration::ZERO, Duration::ZERO],
            |e: &i32| *e == 5,
            move || {
                let _ = c.fetch_add(1, Ordering::SeqCst);
                Err(5)
            },
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn parse_profiles_ini_prefers_install_default_when_present() {
        let tb = PathBuf::from("/home/u/.thunderbird");
        let ini = r#"[Profile1]
Name=default
IsRelative=1
Path=nrfubs4x.default
Default=1

[InstallFDC34C9F024745EB]
Default=f0b29pli.default-release
Locked=1

[Profile0]
Name=default-release
IsRelative=1
Path=f0b29pli.default-release
"#;

        let parsed = parse_profiles_ini_selected(&tb, ini).unwrap();
        assert_eq!(parsed, tb.join("f0b29pli.default-release"));
    }
}
