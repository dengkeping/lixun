//! Browse the excerpts saved explicitly from previews.
use anyhow::Result;
use gtk::prelude::*;
use lixun_preview::citations::{self, Citation};
use std::collections::BTreeMap;
use std::rc::Rc;

pub(crate) fn install(app: &gtk::Application, parent: &gtk::ApplicationWindow) {
    let action = gtk::gio::SimpleAction::new("collections", None);
    let parent = parent.downgrade();
    action.connect_activate(move |_, _| {
        if let Some(parent) = parent.upgrade() {
            show(&parent);
        }
    });
    app.add_action(&action);
}

fn show(parent: &gtk::ApplicationWindow) {
    let window = gtk::Window::builder()
        .title("Quote collections")
        .transient_for(parent)
        .destroy_with_parent(true)
        .default_width(560)
        .default_height(420)
        .build();
    window.set_application(parent.application().as_ref());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_valign(gtk::Align::Start);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    let scroll = gtk::ScrolledWindow::builder().child(&content).build();
    window.set_child(Some(&scroll));
    match citations::items() {
        Ok(items) if items.is_empty() => content.append(&gtk::Label::new(Some(
            "Use “Save quote…” in a preview to start a collection.",
        ))),
        Ok(items) => {
            let mut groups = BTreeMap::new();
            for (_, item) in items {
                groups
                    .entry(item.collection)
                    .or_insert_with(Vec::new)
                    .push(item.citation);
            }
            for (name, rows) in groups {
                append_collection(&content, &name, rows);
            }
        }
        Err(e) => content.append(&gtk::Label::new(Some(&e.to_string()))),
    }
    window.present();
}

fn append_collection(content: &gtk::Box, name: &str, rows: Vec<Citation>) {
    let expander = gtk::Expander::builder()
        .label(format!("{name} ({})", rows.len()))
        .expanded(true)
        .build();
    let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let export = gtk::Button::with_label("Copy collection as Markdown");
    let feedback = gtk::Label::new(None);
    let markdown = rows
        .iter()
        .map(Citation::markdown)
        .collect::<Vec<_>>()
        .join("\n\n");
    let note = feedback.clone();
    export.connect_clicked(move |button| {
        button.clipboard().set_text(&markdown);
        note.set_text("Markdown copied");
    });
    group.append(&export);
    group.append(&feedback);
    for row in rows {
        append_quote(&group, Rc::new(row), &feedback);
    }
    expander.set_child(Some(&group));
    content.append(&expander);
}

fn source_note(row: &Citation) -> String {
    match row.source_note() {
        Ok(Some(note)) => note.into(),
        Ok(None) if row.file.is_none() => "Source version unavailable".into(),
        Ok(None) => String::new(),
        Err(e) => format!("Cannot check source: {e}"),
    }
}

fn append_quote(group: &gtk::Box, row: Rc<Citation>, feedback: &gtk::Label) {
    let note = source_note(&row);
    let label = if note.is_empty() {
        row.title.clone()
    } else {
        format!("{} · {note}", row.title)
    };
    let open = gtk::Button::with_label(&label);
    let quote = gtk::Label::builder()
        .label(&row.quote)
        .wrap(true)
        .selectable(true)
        .xalign(0.0)
        .build();
    group.append(&quote);
    group.append(&open);
    let feedback = feedback.clone();
    open.connect_clicked(move |_| {
        let note = source_note(&row);
        feedback.set_text(&match open_source(&row) {
            Ok(()) => note,
            Err(e) => format!("Cannot open source: {e}"),
        });
    });
}

fn open_source(cite: &Citation) -> Result<()> {
    if let (Some(locator), Some((path, _))) = (&cite.locator, &cite.file) {
        path.metadata()?;
        crate::ipc::open_cited_fragment(path, &cite.title, cite.mime.clone(), &cite.quote, locator);
    } else {
        gtk::gio::AppInfo::launch_default_for_uri(&cite.uri, None::<&gtk::gio::AppLaunchContext>)?;
    }
    Ok(())
}
