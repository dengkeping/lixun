//! The legacy Tantivy directory is generation zero. A small, atomically
//! replaced pointer selects subsequent generations without moving live mmaps.
use super::{INDEX_VERSION, INDEX_VERSION_FILE, plugin_schema, read_index_version};
use anyhow::{Result, ensure};
use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

fn pointer(root: &Path, marker: &str) -> Result<Option<String>> {
    match fs::read_to_string(root.join(marker)) {
        Ok(name) => {
            let suffix = name.strip_prefix("gen-").unwrap_or("");
            ensure!(
                !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
                "invalid index generation pointer"
            );
            ensure!(
                root.join(&name).symlink_metadata()?.is_dir(),
                "invalid index generation directory"
            );
            Ok(Some(name))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn set_pointer(root: &Path, marker: &str, name: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(name.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(root.join(marker))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub struct PendingGeneration {
    root: PathBuf,
    path: PathBuf,
    previous: PathBuf,
    // Held until publication or process exit. A failed attempt leaves PENDING
    // for the next process to resume against the current filesystem.
    _lock: File,
}

impl PendingGeneration {
    pub fn previous(&self) -> &Path {
        &self.previous
    }

    pub fn publish(self) -> Result<()> {
        let index = tantivy::Index::open_in_dir(&self.path)?;
        ensure!(
            index.validate_checksum()?.is_empty(),
            "replacement index checksum mismatch"
        );
        let reader = index.reader()?;
        tracing::info!(
            documents = reader.searcher().num_docs(),
            "validated replacement index"
        );
        File::open(&self.path)?.sync_all()?;
        set_pointer(
            &self.root,
            "CURRENT",
            self.path.file_name().unwrap().to_str().unwrap(),
        )?;
        // CURRENT is the commit point. An interruption before removing PENDING
        // is harmless: on restart CURRENT is checked first.
        if let Err(error) = fs::remove_file(self.root.join("PENDING")) {
            tracing::warn!(%error, "published index with stale pending marker");
        }
        Ok(())
    }
}

pub(super) fn active(root: &Path) -> Result<PathBuf> {
    Ok(pointer(root, "CURRENT")?.map_or_else(|| root.to_owned(), |name| root.join(name)))
}

fn compatible(dir: &Path, fingerprint: &str) -> bool {
    read_index_version(&dir.join(INDEX_VERSION_FILE)) == Some(INDEX_VERSION)
        && plugin_schema::read_fingerprint(dir).as_deref() == Some(fingerprint)
        && dir.join("meta.json").is_file()
}

pub(super) fn prepare(
    root: &Path,
    fingerprint: &str,
) -> Result<(PathBuf, Option<PendingGeneration>)> {
    fs::create_dir_all(root)?;
    let previous = active(root)?;
    if compatible(&previous, fingerprint) {
        return Ok((previous, None));
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("generation.lock"))?;
    lock.try_lock()?;
    let previous = active(root)?;
    if compatible(&previous, fingerprint) {
        return Ok((previous, None));
    }
    let pending = pointer(root, "PENDING")?
        .map(|name| root.join(name))
        .filter(|p| p != &previous && compatible(p, fingerprint));
    let path = match pending {
        Some(path) => path,
        None => {
            let path = tempfile::Builder::new()
                .prefix("gen-")
                .tempdir_in(root)?
                .keep();
            set_pointer(root, "PENDING", path.file_name().unwrap().to_str().unwrap())?;
            path
        }
    };
    Ok((
        path.clone(),
        Some(PendingGeneration {
            root: root.to_owned(),
            path,
            previous,
            _lock: lock,
        }),
    ))
}
