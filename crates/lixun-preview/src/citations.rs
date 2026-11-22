//! User-selected excerpts and the source addresses supplied by preview plugins.
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    inode: u64,
    length: u64,
    seconds: i64,
    nanos: i64,
}

impl Revision {
    pub fn read(path: &Path) -> Result<Self> {
        let meta = path.metadata()?;
        Ok(Self {
            inode: meta.ino(),
            length: meta.len(),
            seconds: meta.mtime(),
            nanos: meta.mtime_nsec(),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Citation {
    pub title: String,
    pub quote: String,
    pub uri: String,
    pub file: Option<(PathBuf, Revision)>,
    /// Source-owned locator understood by the preview plugin.
    pub locator: Option<String>,
    pub mime: Option<String>,
}

impl Citation {
    pub fn source_note(&self) -> Result<Option<&'static str>> {
        let Some((path, original)) = &self.file else {
            return Ok(None);
        };
        match Revision::read(path) {
            Ok(current) if current == *original => Ok(None),
            Ok(_) => Ok(Some("Source has changed")),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(Some("Source is missing"))
            }
            Err(e) => Err(e),
        }
    }

    pub fn markdown(&self) -> String {
        let title = self
            .title
            .replace('\\', "\\\\")
            .replace('[', "\\[")
            .replace(']', "\\]")
            .replace(['\r', '\n'], " ");
        let uri = self
            .uri
            .replace('<', "%3C")
            .replace('>', "%3E")
            .replace('\n', "%0A")
            .replace('\r', "%0D");
        let excerpt = self
            .quote
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{excerpt}\n\n[{title}](<{uri}>)")
    }
}

#[derive(Serialize, Deserialize)]
pub struct Item {
    pub collection: String,
    pub citation: Citation,
}

fn directory() -> Result<PathBuf> {
    Ok(dirs::state_dir()
        .context("Cannot locate user state directory")?
        .join("lixun/collections"))
}

fn store(dir: &Path, item: &Item) -> Result<()> {
    ensure!(
        !item.collection.trim().is_empty() && item.collection.len() <= 100,
        "Choose a collection name of 1–100 bytes"
    );
    ensure!(
        !item.citation.quote.trim().is_empty(),
        "Select a passage first"
    );
    ensure!(
        item.citation.quote.len() <= 65536,
        "Select a passage smaller than 64 KiB"
    );
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), item)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    let path = tmp.path().with_extension("json");
    tmp.persist_noclobber(path)?;
    Ok(())
}

pub fn items() -> Result<Vec<(PathBuf, Item)>> {
    let dir = directory()?;
    let entries = match dir.read_dir() {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") && entry.file_type()?.is_file() {
            let item: Item = serde_json::from_reader(std::fs::File::open(&path)?)?;
            rows.push((entry.metadata()?.modified()?, path, item));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(rows
        .into_iter()
        .map(|(_, path, item)| (path, item))
        .collect())
}

/// Shared keyboard-accessible control. The plugin captures the selection and its address.
pub fn capture_button(capture: impl Fn() -> Result<Citation> + 'static) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder().label("Save quote…").build();
    let popup = gtk::Popover::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let name = gtk::Entry::builder()
        .placeholder_text("Collection name")
        .text("Quotes")
        .build();
    let save = gtk::Button::with_label("Add to collection");
    let message = gtk::Label::new(None);
    for widget in [
        name.upcast_ref::<gtk::Widget>(),
        save.upcast_ref(),
        message.upcast_ref(),
    ] {
        content.append(widget);
    }
    popup.set_child(Some(&content));
    button.set_popover(Some(&popup));
    let submit = save.downgrade();
    name.connect_activate(move |_| {
        if let Some(b) = submit.upgrade() {
            b.emit_clicked();
        }
    });
    // Opening the name entry can transfer GTK's primary selection away from
    // the document. Snapshot the excerpt as the popover opens.
    let passage = std::rc::Rc::new(std::cell::RefCell::new(None));
    let current = passage.clone();
    let submit = save.downgrade();
    let status = message.clone();
    popup.connect_show(move |_| {
        let result = capture();
        status.set_text(
            &result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default(),
        );
        if let Some(button) = submit.upgrade() {
            button.set_sensitive(result.is_ok());
        }
        current.replace(result.ok());
    });
    save.connect_clicked(move |button| {
        let excerpt = passage.borrow();
        let Some(citation) = excerpt.as_ref() else {
            return;
        };
        let result = directory().and_then(|dir| {
            store(
                &dir,
                &Item {
                    collection: name.text().trim().to_owned(),
                    citation: citation.clone(),
                },
            )
        });
        match result {
            Ok(()) => {
                message.set_text("Quote saved");
                button.set_sensitive(false);
            }
            Err(e) => message.set_text(&e.to_string()),
        }
    });
    button
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn separate_writes_preserve_quotes_links_and_source_revisions() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("my document");
        std::fs::write(&file, "old").unwrap();
        let citation = Citation {
            title: "Memo [final]".into(),
            quote: "first\nsecond".into(),
            uri: "file:///my%20document#page=2".into(),
            locator: Some("page=2".into()),
            mime: None,
            file: Some((file.clone(), Revision::read(&file).unwrap())),
        };
        assert_eq!(citation.source_note().unwrap(), None);
        assert_eq!(
            citation.markdown(),
            "> first\n> second\n\n[Memo \\[final\\]](<file:///my%20document#page=2>)"
        );
        let item = Item {
            collection: "../quotes".into(),
            citation,
        };
        let dir = root.path().join("collections");
        store(&dir, &item).unwrap();
        store(&dir, &item).unwrap();
        let items: Vec<_> = dir.read_dir().unwrap().map(|p| p.unwrap().path()).collect();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved: Item = serde_json::from_slice(&std::fs::read(&items[0]).unwrap()).unwrap();
        assert_eq!(saved.citation.locator.as_deref(), Some("page=2"));
        std::fs::write(&file, "revised document").unwrap();
        assert_eq!(
            saved.citation.source_note().unwrap(),
            Some("Source has changed")
        );
        std::fs::remove_file(file).unwrap();
        assert_eq!(
            saved.citation.source_note().unwrap(),
            Some("Source is missing")
        );
    }
}
