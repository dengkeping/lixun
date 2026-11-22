//! Named query definitions. Opening one feeds the live launcher entry.
use lixun_core::{Action, Hit};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::window::CategoryChips;
use anyhow::{Result, ensure};
use gtk::prelude::*;
use lixun_core::Category;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct SavedSearch {
    name: String,
    query: String,
    category: Option<Category>,
    #[serde(default)]
    seen: Option<BTreeSet<String>>,
}

fn save(dir: &Path, search: &SavedSearch) -> Result<()> {
    let name = search.name.trim();
    ensure!(
        !name.is_empty() && name.len() <= 100,
        "Choose a name of 1–100 bytes"
    );
    ensure!(!search.query.trim().is_empty(), "Enter a search first");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = serialize(dir, search)?;
    tmp.persist_noclobber(record_path(dir, name))?;
    Ok(())
}

fn record_path(dir: &Path, name: &str) -> std::path::PathBuf {
    let filename: String = name.bytes().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{filename}.json"))
}

fn serialize(dir: &Path, search: &SavedSearch) -> Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&serde_json::to_vec(search)?)?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

#[derive(Default)]
pub(crate) struct Visits {
    last: RefCell<Option<(u64, BTreeSet<String>)>>,
    requested: RefCell<Option<(u64, std::path::PathBuf, bool)>>,
}

pub(crate) struct VisitSummary {
    pub message: String,
    pub only_new: bool,
    pub fresh: BTreeSet<String>,
}

struct ChangeSet {
    fresh: BTreeSet<String>,
    baseline: bool,
}

fn record_visit(path: &Path, ids: &BTreeSet<String>) -> Result<ChangeSet> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing search directory"))?;
    let lock = std::fs::File::open(dir)?;
    lock.lock()?;
    let mut record: SavedSearch = serde_json::from_reader(std::fs::File::open(path)?)?;
    let baseline = record.seen.is_none();
    let seen = record.seen.get_or_insert_default();
    let fresh: BTreeSet<_> = ids.difference(seen).cloned().collect();
    if baseline || !fresh.is_empty() {
        seen.extend(fresh.iter().cloned());
        serialize(dir, &record)?.persist(path)?;
    }
    Ok(ChangeSet {
        fresh: if baseline { BTreeSet::new() } else { fresh },
        baseline,
    })
}

impl Visits {
    pub(crate) fn pending(&self, epoch: u64) -> bool {
        self.requested
            .borrow()
            .as_ref()
            .is_some_and(|r| r.0 == epoch)
    }

    pub(crate) fn finish(&self, epoch: u64, hits: &mut Vec<Hit>) -> Option<VisitSummary> {
        let ids: BTreeSet<_> = hits
            .iter()
            .filter(|h| !matches!(h.action, Action::ReplaceQuery { .. }))
            .map(|h| h.id.0.clone())
            .collect();
        let request = self.requested.take();
        let note = request.filter(|r| r.0 == epoch).map(|(_, path, only)| {
            match record_visit(&path, &ids) {
                Ok(changes) => {
                    let message = if changes.baseline {
                        "Tracking started for this search".into()
                    } else {
                        format!("{} new since last view", changes.fresh.len())
                    };
                    if only {
                        hits.retain(|h| !changes.baseline && changes.fresh.contains(&h.id.0));
                    }
                    VisitSummary {
                        message,
                        only_new: only,
                        fresh: changes.fresh,
                    }
                }
                Err(e) => VisitSummary {
                    message: format!("Could not update saved search: {e}"),
                    only_new: false,
                    fresh: Default::default(),
                },
            }
        });
        *self.last.borrow_mut() = Some((epoch, ids));
        note
    }
}

fn list(dir: &Path) -> Result<Vec<SavedSearch>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut searches: Vec<SavedSearch> = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            searches.push(serde_json::from_str(&std::fs::read_to_string(path)?)?);
        }
    }
    searches.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(searches)
}

struct SearchPicker {
    chips: Rc<CategoryChips>,
    entry: gtk::Entry,
    epoch: Arc<AtomicU64>,
    visits: Rc<Visits>,
}

impl SearchPicker {
    fn populate(&self, dir: &Path, pop: &gtk::Popover, content: &gtk::Box, error: &gtk::Label) {
        match list(dir) {
            Ok(searches) => {
                for search in searches {
                    content.append(&self.row(dir, pop, &search));
                }
            }
            Err(e) => error.set_text(&e.to_string()),
        }
    }

    fn row(&self, dir: &Path, pop: &gtk::Popover, search: &SavedSearch) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let path = record_path(dir, &search.name);
        for (label, only) in [(search.name.as_str(), false), ("New only", true)] {
            let button = gtk::Button::with_label(label);
            let (chips, entry, popup) = (self.chips.clone(), self.entry.clone(), pop.downgrade());
            let (epoch, visits, path, query) = (
                self.epoch.clone(),
                self.visits.clone(),
                path.clone(),
                search.query.clone(),
            );
            let category = search.category;
            button.connect_clicked(move |_| {
                let index = match category {
                    Some(Category::App) => 1,
                    Some(Category::File) => 2,
                    Some(Category::Mail) => 3,
                    Some(Category::Attachment) => 4,
                    _ => 0,
                };
                if let Some(pop) = popup.upgrade() {
                    pop.popdown();
                }
                chips.activate_index(index);
                entry.set_text("");
                entry.set_text(&query);
                visits
                    .requested
                    .replace(Some((epoch.load(Ordering::SeqCst), path.clone(), only)));
                entry.set_position(-1);
                entry.grab_focus();
            });
            row.append(&button);
        }
        row
    }
}

pub(crate) fn install(
    app: &gtk::Application,
    entry: &gtk::Entry,
    chips: Rc<CategoryChips>,
    category: Rc<std::cell::Cell<Option<Category>>>,
    state: &Path,
    open: Rc<std::cell::Cell<u32>>,
    epoch: Arc<AtomicU64>,
) -> Rc<Visits> {
    let visits = Rc::new(Visits::default());
    let result = visits.clone();
    let dir = state.join("saved-searches");
    let entry = entry.clone();
    let action = gtk::gio::SimpleAction::new("saved-searches", None);
    let picker = SearchPicker {
        chips,
        entry: entry.clone(),
        epoch: epoch.clone(),
        visits: visits.clone(),
    };
    action.connect_activate(move |_, _| {
        let pop = gtk::Popover::new();
        pop.set_widget_name("saved-searches");
        pop.set_parent(&entry);
        let open_count = open.clone();
        open_count.set(open_count.get() + 1);
        pop.connect_closed(move |p| {
            open_count.set(open_count.get().saturating_sub(1));
            p.unparent();
        });
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let name = gtk::Entry::builder()
            .placeholder_text("Name this search")
            .build();
        let submit = gtk::Button::with_label("Save current search");
        let snapshot = visits
            .last
            .borrow()
            .as_ref()
            .filter(|(e, _)| *e == epoch.load(Ordering::SeqCst))
            .map(|(_, ids)| ids.clone());
        submit.set_sensitive(!entry.text().trim().is_empty() && snapshot.is_some());
        let save_button = submit.downgrade();
        name.connect_activate(move |_| {
            if let Some(button) = save_button.upgrade()
                && button.is_sensitive()
            {
                button.emit_clicked();
            }
        });
        let focus = name.downgrade();
        content.append(&name);
        content.append(&submit);
        let error = gtk::Label::new(None);
        content.append(&error);
        picker.populate(&dir, &pop, &content, &error);
        let search = entry.text().to_string();
        let category = category.get();
        let dir = dir.clone();
        let popup = pop.downgrade();
        submit.connect_clicked(move |_| {
            let record = SavedSearch {
                name: name.text().trim().into(),
                query: search.clone(),
                category,
                seen: snapshot.clone(),
            };
            match save(&dir, &record) {
                Ok(()) => {
                    if let Some(pop) = popup.upgrade() {
                        pop.popdown();
                    }
                }
                Err(e) => error.set_text(&e.to_string()),
            }
        });
        pop.set_child(Some(&content));
        pop.popup();
        if let Some(name) = focus.upgrade() {
            name.grab_focus();
        }
    });
    app.add_action(&action);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgrades_legacy_records_and_tracks_distinct_results_across_visits() {
        let dir = tempfile::tempdir().unwrap();
        let path = record_path(dir.path(), "sample");
        std::fs::write(
            &path,
            r#"{"name":"sample","query":"report","category":null}"#,
        )
        .unwrap();
        let ids = BTreeSet::from(["old".into()]);
        assert!(record_visit(&path, &ids).unwrap().baseline);
        assert!(record_visit(&path, &ids).unwrap().fresh.is_empty());
        let next = BTreeSet::from(["later".into()]);
        assert_eq!(record_visit(&path, &next).unwrap().fresh, next);
        assert!(record_visit(&path, &ids).unwrap().fresh.is_empty());
    }

    #[test]
    fn roundtrips_filter_and_never_overwrites_a_name() {
        let d = tempfile::tempdir().unwrap();
        let q = SavedSearch {
            name: "../目录".into(),
            query: "title:notes -draft".into(),
            category: Some(Category::File),
            seen: None,
        };
        save(d.path(), &q).unwrap();
        assert!(save(d.path(), &q).is_err());
        let stored = list(d.path()).unwrap().pop().unwrap();
        assert_eq!(stored.name, q.name);
        assert_eq!(stored.query, q.query);
        assert_eq!(stored.category, q.category);
    }
}
