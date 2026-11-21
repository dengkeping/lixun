//! Named query definitions. Opening one feeds the live launcher entry.
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::rc::Rc;

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
    let filename: String = name.bytes().map(|b| format!("{b:02x}")).collect();
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(serde_json::to_string(search)?.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(dir.join(format!("{filename}.json")))?;
    Ok(())
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

pub(crate) fn install(
    app: &gtk::Application,
    entry: &gtk::Entry,
    chips: Rc<CategoryChips>,
    category: Rc<std::cell::Cell<Option<Category>>>,
    state: &Path,
    open: Rc<std::cell::Cell<u32>>,
) {
    let dir = state.join("saved-searches");
    let entry = entry.clone();
    let action = gtk::gio::SimpleAction::new("saved-searches", None);
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
        submit.set_sensitive(!entry.text().trim().is_empty());
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
        match list(&dir) {
            Ok(searches) => {
                for search in searches {
                    let button = gtk::Button::with_label(&search.name);
                    let chips = chips.clone();
                    let entry = entry.clone();
                    let popup = pop.downgrade();
                    button.connect_clicked(move |_| {
                        let index = match search.category {
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
                        // Even when the same query is already visible, request fresh results.
                        entry.set_text("");
                        entry.set_text(&search.query);
                        entry.set_position(-1);
                        entry.grab_focus();
                    });
                    content.append(&button);
                }
            }
            Err(e) => error.set_text(&e.to_string()),
        }
        let search = entry.text().to_string();
        let category = category.get();
        let dir = dir.clone();
        let popup = pop.downgrade();
        submit.connect_clicked(move |_| {
            let record = SavedSearch {
                name: name.text().trim().into(),
                query: search.clone(),
                category,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrips_filter_and_never_overwrites_a_name() {
        let d = tempfile::tempdir().unwrap();
        let q = SavedSearch {
            name: "../目录".into(),
            query: "title:notes -draft".into(),
            category: Some(Category::File),
        };
        save(d.path(), &q).unwrap();
        assert!(save(d.path(), &q).is_err());
        let stored = list(d.path()).unwrap().pop().unwrap();
        assert_eq!(stored.name, q.name);
        assert_eq!(stored.query, q.query);
        assert_eq!(stored.category, q.category);
    }
}
