//! Plugin-aware fs watcher: single notify watcher that dispatches fs events
//! to every registered `IndexerSource` via `on_fs_events`.
//!
//! Each source instance declares its watched roots via `IndexerSource::watch_paths`.
//! Events are routed to the instance whose watched root is the longest prefix of
//! the event path. Per-instance 5s cooldown prevents dispatch storms from burst
//! events. The actual event handling runs on `spawn_blocking` so source impls
//! may do sync I/O without stalling the tokio runtime.

use crate::registry::SourceRegistry;
use crate::writer_sink::WriterSink;
use anyhow::Result;
use lixun_sources::source::{SourceContext, SourceEvent, SourceEventKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(2000);
const PER_INSTANCE_COOLDOWN: Duration = Duration::from_secs(5);
const EVENT_CHANNEL_CAP: usize = 1024;

pub async fn start(registry: Arc<SourceRegistry>, sink: Arc<WriterSink>) -> Result<()> {
    let mut route: HashMap<PathBuf, (usize, bool)> = HashMap::new();
    for (idx, inst) in registry.instances.iter().enumerate() {
        if inst.source.kind() == "fs" {
            continue;
        }
        let ctx = SourceContext {
            instance_id: &inst.instance_id,
            state_dir: &inst.state_dir,
        };
        let specs = match inst.source.watch_paths(&ctx) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "plugin fs watcher: watch_paths({}) failed: {}",
                    inst.instance_id,
                    e
                );
                continue;
            }
        };
        for spec in specs {
            route.insert(spec.path, (idx, spec.recursive));
        }
    }

    if route.is_empty() {
        tracing::info!("plugin fs watcher: no sources expose watch_paths; idle");
        return Ok(());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(EVENT_CHANNEL_CAP);
    let tx_cb = tx.clone();
    let dropped_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dropped_cb = Arc::clone(&dropped_counter);
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if let Ok(event) = res
                && let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = tx_cb.try_send(event)
            {
                let prev = dropped_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if prev == 0 || prev.is_multiple_of(100) {
                    tracing::warn!(
                        "plugin fs watcher: event queue full (cap={}), dropping event (total dropped={})",
                        EVENT_CHANNEL_CAP,
                        prev + 1
                    );
                }
            }
        },
        Config::default(),
    )?;

    let mut instances_watched: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (path, (idx, recursive)) in &route {
        let mode = if *recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(path, mode) {
            Ok(()) => {
                instances_watched.insert(*idx);
            }
            Err(e) => {
                tracing::warn!("plugin fs watcher: watch({:?}) failed: {}", path, e);
            }
        }
    }
    tracing::info!(
        "plugin fs watcher: watching {} root(s) across {} instance(s)",
        route.len(),
        instances_watched.len()
    );

    drop(tx);

    let mut last_dispatch: HashMap<usize, Instant> = HashMap::new();

    loop {
        let first = match rx.recv().await {
            Some(e) => e,
            None => break,
        };
        let mut batch = vec![first];
        while let Ok(Some(e)) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {
            batch.push(e);
        }

        let mut per_instance: HashMap<usize, Vec<SourceEvent>> = HashMap::new();
        for event in batch {
            let mapped_kind = map_notify_kind(&event);
            for path in &event.paths {
                if let Some(idx) = longest_prefix_instance(&route, path) {
                    per_instance.entry(idx).or_default().push(SourceEvent {
                        path: path.clone(),
                        kind: mapped_kind.clone(),
                    });
                }
            }
        }

        for (idx, events) in per_instance {
            let now = Instant::now();
            let in_cooldown = last_dispatch
                .get(&idx)
                .map(|t| now.duration_since(*t) < PER_INSTANCE_COOLDOWN)
                .unwrap_or(false);
            if in_cooldown {
                tracing::debug!(
                    "plugin fs watcher: cooldown, skipping {} events for instance {}",
                    events.len(),
                    registry.instances[idx].instance_id
                );
                continue;
            }
            last_dispatch.insert(idx, now);

            let inst = &registry.instances[idx];
            let instance_id = inst.instance_id.clone();
            let state_dir = inst.state_dir.clone();
            let source = Arc::clone(&inst.source);
            let sink = Arc::clone(&sink);
            let event_count = events.len();

            tokio::task::spawn_blocking(move || {
                let ctx = SourceContext {
                    instance_id: &instance_id,
                    state_dir: &state_dir,
                };
                if let Err(e) = source.on_fs_events(&ctx, &events, sink.as_ref()) {
                    tracing::warn!(
                        "plugin fs watcher: on_fs_events({}, {} event(s)) failed: {}",
                        instance_id,
                        event_count,
                        e
                    );
                }
            });
        }
    }

    Ok(())
}

fn map_notify_kind(event: &Event) -> SourceEventKind {
    match event.kind {
        EventKind::Create(_) => SourceEventKind::Created,
        EventKind::Remove(_) => SourceEventKind::Removed,
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both))
            if event.paths.len() == 2 =>
        {
            SourceEventKind::Renamed {
                from: event.paths[0].clone(),
            }
        }
        _ => SourceEventKind::Modified,
    }
}

fn longest_prefix_instance(route: &HashMap<PathBuf, (usize, bool)>, path: &Path) -> Option<usize> {
    route
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, (idx, _))| *idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_picks_deepest_matching_root() {
        let mut route: HashMap<PathBuf, (usize, bool)> = HashMap::new();
        route.insert(PathBuf::from("/home/u/Mail"), (0, true));
        route.insert(PathBuf::from("/home/u/Mail/INBOX"), (1, true));
        route.insert(PathBuf::from("/home/u/Apps"), (2, true));

        assert_eq!(
            longest_prefix_instance(&route, Path::new("/home/u/Mail/INBOX/new/x")),
            Some(1)
        );
        assert_eq!(
            longest_prefix_instance(&route, Path::new("/home/u/Mail/Sent/y")),
            Some(0)
        );
        assert_eq!(
            longest_prefix_instance(&route, Path::new("/home/u/Apps/test.desktop")),
            Some(2)
        );
        assert_eq!(
            longest_prefix_instance(&route, Path::new("/home/u/Other/z")),
            None
        );
    }

    #[test]
    fn map_notify_kind_create_remove_rename_modify() {
        let create = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/a")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(matches!(map_notify_kind(&create), SourceEventKind::Created));

        let remove = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![PathBuf::from("/a")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(matches!(map_notify_kind(&remove), SourceEventKind::Removed));

        let modify = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("/a")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(matches!(
            map_notify_kind(&modify),
            SourceEventKind::Modified
        ));

        let rename = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![PathBuf::from("/old"), PathBuf::from("/new")],
            attrs: notify::event::EventAttributes::default(),
        };
        match map_notify_kind(&rename) {
            SourceEventKind::Renamed { from } => assert_eq!(from, PathBuf::from("/old")),
            _ => panic!("expected Renamed"),
        }
    }

    #[test]
    fn longest_prefix_none_when_no_match() {
        let mut route: HashMap<PathBuf, (usize, bool)> = HashMap::new();
        route.insert(PathBuf::from("/x"), (0, true));
        assert!(longest_prefix_instance(&route, Path::new("/y/z")).is_none());
    }
}
