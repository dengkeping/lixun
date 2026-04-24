//! Applications source — scan .desktop files.

use anyhow::Result;
use lixun_core::{Action, Category, DocId, Document};
use std::path::{Path, PathBuf};

struct DesktopEntry {
    pub desktop_id: String,
    pub desktop_file: PathBuf,
    pub name: String,
    pub exec: String,
    pub terminal: bool,
    pub working_dir: Option<PathBuf>,
    pub icon: Option<String>,
    pub subtitle: String,
}

/// Applications source — scans XDG_DATA_DIRS for .desktop files.
pub struct AppsSource {
    pub search_dirs: Vec<PathBuf>,
}

impl Default for AppsSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AppsSource {
    pub fn new() -> Self {
        let mut dirs = Vec::new();

        if let Ok(xdd) = std::env::var("XDG_DATA_DIRS") {
            for dir in xdd.split(':') {
                dirs.push(PathBuf::from(dir).join("applications"));
            }
        } else {
            dirs.push(PathBuf::from("/usr/share/applications"));
            dirs.push(PathBuf::from("/usr/local/share/applications"));
        }

        if let Ok(local) = std::env::var("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(local).join("applications"));
        } else if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/applications"));
        }

        Self { search_dirs: dirs }
    }

    fn desktop_id_from_path(path: &Path) -> Option<String> {
        let stem = path.file_stem()?.to_string_lossy();
        Some(format!("{}.desktop", stem))
    }

    fn parse_desktop_file(path: &Path) -> Option<DesktopEntry> {
        let content = std::fs::read_to_string(path).ok()?;

        let mut name = String::new();
        let mut exec = String::new();
        let mut terminal = false;
        let mut icon = None;
        let mut generic_name = None;
        let mut comment = None;
        let mut in_desktop_entry = false;

        for line in content.lines() {
            if line.trim() == "[Desktop Entry]" {
                in_desktop_entry = true;
                continue;
            }
            if line.starts_with('[') {
                in_desktop_entry = false;
                continue;
            }
            if !in_desktop_entry {
                continue;
            }

            if let Some((key, val)) = line.split_once('=') {
                match key.trim() {
                    "Name" if name.is_empty() => {
                        name = val.trim().to_string();
                    }
                    "Exec" => {
                        exec = val.trim().to_string();
                        exec = exec
                            .split_whitespace()
                            .filter(|segment| !segment.starts_with('%'))
                            .collect::<Vec<_>>()
                            .join(" ");
                    }
                    "Terminal" => {
                        terminal = val.trim().eq_ignore_ascii_case("true");
                    }
                    "Icon" => {
                        let value = val.trim();
                        if !value.is_empty() {
                            icon = Some(value.to_string());
                        }
                    }
                    "GenericName" => {
                        let value = val.trim();
                        if !value.is_empty() {
                            generic_name = Some(value.to_string());
                        }
                    }
                    "Comment" => {
                        let value = val.trim();
                        if !value.is_empty() {
                            comment = Some(value.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        if name.is_empty() || exec.is_empty() {
            return None;
        }

        let desktop_id = Self::desktop_id_from_path(path)?;
        let subtitle = generic_name
            .or(comment)
            .unwrap_or_else(|| desktop_id.clone());

        Some(DesktopEntry {
            desktop_id,
            desktop_file: path.to_path_buf(),
            name,
            exec,
            terminal,
            working_dir: None,
            icon,
            subtitle,
        })
    }
}

impl AppsSource {
    pub fn index_all(&self) -> Result<Vec<Document>> {
        let mut docs = Vec::new();

        for dir in &self.search_dirs {
            if !dir.exists() {
                continue;
            }

            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();

                if !path
                    .extension()
                    .map(|ext| ext == "desktop")
                    .unwrap_or(false)
                {
                    continue;
                }

                if let Some(entry) = Self::parse_desktop_file(&path) {
                    docs.push(Document {
                        id: DocId(format!("app:{}", entry.desktop_id)),
                        category: Category::App,
                        title: entry.name,
                        subtitle: entry.subtitle,
                        icon_name: entry.icon,
                        kind_label: Some("Application".into()),
                        body: None,
                        path: path.to_string_lossy().to_string(),
                        mtime: 0,
                        size: 0,
                        secondary_action: None,
                        action: Action::Launch {
                            exec: entry.exec,
                            terminal: entry.terminal,
                            desktop_id: Some(entry.desktop_id.clone()),
                            desktop_file: Some(entry.desktop_file.clone()),
                            working_dir: entry.working_dir,
                        },
                        extract_fail: false,
                        sender: None,
                        recipients: None,
                        source_instance: "builtin:apps".into(),
                        extra: Vec::new(),
                    });
                }
            }
        }

        tracing::info!("Applications: indexed {} apps", docs.len());
        Ok(docs)
    }
}

impl crate::source::IndexerSource for AppsSource {
    fn kind(&self) -> &'static str {
        "apps"
    }

    fn watch_paths(
        &self,
        _ctx: &crate::source::SourceContext,
    ) -> Result<Vec<crate::source::WatchSpec>> {
        Ok(self
            .search_dirs
            .iter()
            .filter(|p| p.exists())
            .map(|p| crate::source::WatchSpec {
                path: p.clone(),
                recursive: true,
            })
            .collect())
    }

    fn on_fs_events(
        &self,
        ctx: &crate::source::SourceContext,
        events: &[crate::source::SourceEvent],
        sink: &dyn crate::source::MutationSink,
    ) -> Result<()> {
        let instance_id = ctx.instance_id.to_string();
        let docs = self.index_all()?;
        let n = docs.len();
        if !docs.is_empty() {
            let mut batch: Vec<Document> = Vec::with_capacity(n);
            for mut doc in docs {
                doc.source_instance = instance_id.clone();
                batch.push(doc);
            }
            sink.emit(crate::source::Mutation::UpsertMany(batch))?;
        }
        tracing::info!(
            "apps: {} fs event(s) -> reindexed {} application(s)",
            events.len(),
            n
        );
        Ok(())
    }

    fn reindex_full(
        &self,
        ctx: &crate::source::SourceContext,
        sink: &dyn crate::source::MutationSink,
    ) -> Result<()> {
        let instance_id = ctx.instance_id.to_string();
        sink.emit(crate::source::Mutation::DeleteSourceInstance {
            instance_id: instance_id.clone(),
        })?;

        let docs = self.index_all()?;
        if !docs.is_empty() {
            let mut batch: Vec<Document> = Vec::with_capacity(docs.len());
            for mut doc in docs {
                doc.source_instance = instance_id.clone();
                batch.push(doc);
            }
            sink.emit(crate::source::Mutation::UpsertMany(batch))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_desktop_file_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("firefox.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=Firefox
Exec=/usr/bin/firefox %u
Icon=firefox
Type=Application
Terminal=false
"#,
        )
        .unwrap();

        let result = AppsSource::parse_desktop_file(&path);
        assert!(result.is_some());

        let entry = result.unwrap();
        assert_eq!(entry.desktop_id, "firefox.desktop");
        assert_eq!(entry.name, "Firefox");
        assert_eq!(entry.exec, "/usr/bin/firefox");
        assert!(!entry.terminal);
        assert_eq!(entry.icon.as_deref(), Some("firefox"));
    }

    #[test]
    fn test_parse_desktop_file_reads_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("firefox.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=Firefox
Exec=/usr/bin/firefox
Icon=firefox
"#,
        )
        .unwrap();

        let entry = AppsSource::parse_desktop_file(&path).unwrap();
        assert_eq!(entry.icon.as_deref(), Some("firefox"));
    }

    #[test]
    fn test_parse_desktop_file_subtitle_precedence() {
        let tmp = tempfile::tempdir().unwrap();

        let generic_path = tmp.path().join("generic.desktop");
        fs::write(
            &generic_path,
            r#"[Desktop Entry]
Name=Generic
Exec=/usr/bin/generic
GenericName=Preferred Subtitle
Comment=Fallback Subtitle
"#,
        )
        .unwrap();
        assert_eq!(
            AppsSource::parse_desktop_file(&generic_path)
                .unwrap()
                .subtitle,
            "Preferred Subtitle"
        );

        let comment_path = tmp.path().join("comment.desktop");
        fs::write(
            &comment_path,
            r#"[Desktop Entry]
Name=Comment
Exec=/usr/bin/comment
Comment=Comment Subtitle
"#,
        )
        .unwrap();
        assert_eq!(
            AppsSource::parse_desktop_file(&comment_path)
                .unwrap()
                .subtitle,
            "Comment Subtitle"
        );

        let fallback_path = tmp.path().join("desktop-id.desktop");
        fs::write(
            &fallback_path,
            r#"[Desktop Entry]
Name=Fallback
Exec=/usr/bin/fallback
"#,
        )
        .unwrap();
        assert_eq!(
            AppsSource::parse_desktop_file(&fallback_path)
                .unwrap()
                .subtitle,
            "desktop-id.desktop"
        );
    }

    #[test]
    fn test_parse_desktop_file_absolute_icon_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absolute.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=Firefox
Exec=/usr/bin/firefox
Icon=/usr/share/icons/hicolor/128x128/apps/firefox.png
"#,
        )
        .unwrap();

        let entry = AppsSource::parse_desktop_file(&path).unwrap();
        assert_eq!(
            entry.icon.as_deref(),
            Some("/usr/share/icons/hicolor/128x128/apps/firefox.png")
        );
    }

    #[test]
    fn test_parse_desktop_file_removes_field_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=Test
Exec=/usr/bin/test %f %F %u %U
"#,
        )
        .unwrap();

        let entry = AppsSource::parse_desktop_file(&path).unwrap();
        assert_eq!(entry.exec, "/usr/bin/test");
    }

    #[test]
    fn test_parse_desktop_file_terminal_true() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("terminal.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=Terminal
Exec=/usr/bin/terminal
Terminal=true
"#,
        )
        .unwrap();

        let entry = AppsSource::parse_desktop_file(&path).unwrap();
        assert!(entry.terminal);
    }

    #[test]
    fn test_parse_desktop_file_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("noname.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Exec=/usr/bin/something
"#,
        )
        .unwrap();

        assert!(AppsSource::parse_desktop_file(&path).is_none());
    }

    #[test]
    fn test_parse_desktop_file_missing_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("noexec.desktop");
        fs::write(
            &path,
            r#"[Desktop Entry]
Name=No Exec
"#,
        )
        .unwrap();

        assert!(AppsSource::parse_desktop_file(&path).is_none());
    }

    #[test]
    fn test_parse_desktop_file_ignores_non_desktop_section() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.desktop");
        fs::write(
            &path,
            r#"[Desktop Action NewWindow]
Name=New Window
Exec=/usr/bin/test --new-window

[Desktop Entry]
Name=Test App
Exec=/usr/bin/test
"#,
        )
        .unwrap();

        let entry = AppsSource::parse_desktop_file(&path).unwrap();
        assert_eq!(entry.name, "Test App");
        assert_eq!(entry.exec, "/usr/bin/test");
    }

    #[test]
    fn test_index_all_with_temp_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let apps_dir = tmp.path().join("applications");
        fs::create_dir_all(&apps_dir).unwrap();

        fs::write(
            apps_dir.join("app1.desktop"),
            r#"[Desktop Entry]
Name=App One
Exec=/usr/bin/app1
"#,
        )
        .unwrap();

        fs::write(
            apps_dir.join("app2.desktop"),
            r#"[Desktop Entry]
Name=App Two
Exec=/usr/bin/app2
"#,
        )
        .unwrap();

        let source = AppsSource {
            search_dirs: vec![apps_dir],
        };

        let docs = source.index_all().unwrap();
        assert_eq!(docs.len(), 2);

        let names: Vec<_> = docs.iter().map(|doc| &doc.title).collect();
        assert!(names.contains(&&"App One".to_string()));
        assert!(names.contains(&&"App Two".to_string()));
        assert!(docs
            .iter()
            .all(|doc| doc.kind_label.as_deref() == Some("Application")));
    }
}
