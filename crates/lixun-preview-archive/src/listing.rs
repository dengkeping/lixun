//! Pure, GTK-free archive enumeration.
//!
//! Lists the entries of an archive **without extracting any payload
//! to disk**. Every supported container is read header-only:
//!
//! - `zip` walks the central directory (`ZipArchive::by_index`),
//! - `tar` (and its compressed variants) streams entry headers via
//!   `tar::Archive::entries`, layering the matching decompressor
//!   (`flate2` / `zstd` / `bzip2`) over the file reader,
//! - `7z` parses the archive metadata via `sevenz_rust2::Archive::read`.
//!
//! All listing functions are deterministic and side-effect free
//! (beyond reading the source file), so they are unit-tested in
//! isolation from the widget layer.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// A single archive member as surfaced to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Result of enumerating an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveListing {
    pub entries: Vec<Entry>,
    /// `true` when the listing was cut short at `cap` entries.
    pub truncated: bool,
}

/// Why a listing could not be produced.
///
/// Every variant maps to a human-readable inline message; nothing
/// here ever panics or extracts to disk.
#[derive(Debug)]
pub enum ArchiveError {
    /// The path's extension is not a container this plugin handles.
    Unsupported,
    /// The archive (or an entry) requires a password.
    Encrypted,
    /// The archive header/structure could not be parsed.
    Malformed(String),
    /// Underlying I/O failure (open/read).
    Io(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::Unsupported => f.write_str("unsupported archive format"),
            ArchiveError::Encrypted => {
                f.write_str("encrypted archive: cannot list without a password")
            }
            ArchiveError::Malformed(why) => write!(f, "malformed archive: {why}"),
            ArchiveError::Io(why) => write!(f, "cannot read archive: {why}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

/// Containers this plugin can enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar,
    TarGz,
    TarZst,
    TarBz2,
    SevenZ,
}

/// Classify a path by extension alone — cheap, no I/O.
///
/// Double extensions (`.tar.gz`, `.tar.zst`, `.tar.bz2`) and the
/// `.tgz` shorthand are recognised before the single-extension
/// fallbacks so a `.gz`-suffixed tarball is never mistaken for a
/// bare gzip stream.
pub fn detect_format(path: &Path) -> Option<Format> {
    let lower = path.to_string_lossy().to_ascii_lowercase();

    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Some(Format::TarGz);
    }
    if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        return Some(Format::TarZst);
    }
    if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") || lower.ends_with(".tbz") {
        return Some(Format::TarBz2);
    }
    if lower.ends_with(".tar") {
        return Some(Format::Tar);
    }
    if lower.ends_with(".zip") {
        return Some(Format::Zip);
    }
    if lower.ends_with(".7z") {
        return Some(Format::SevenZ);
    }
    None
}

/// Enumerate up to `cap` entries of the archive at `path`.
///
/// Returns [`ArchiveError::Unsupported`] when the extension is not
/// recognised. Never extracts payloads and never panics on
/// malformed input — parse failures become [`ArchiveError::Malformed`].
pub fn list_entries(path: &Path, cap: usize) -> Result<ArchiveListing, ArchiveError> {
    match detect_format(path) {
        Some(Format::Zip) => list_zip(path, cap),
        Some(Format::Tar) => list_tar(Box::new(open(path)?), cap),
        Some(Format::TarGz) => list_tar(Box::new(flate2::read::GzDecoder::new(open(path)?)), cap),
        Some(Format::TarZst) => {
            let dec = zstd::stream::read::Decoder::new(open(path)?)
                .map_err(|e| ArchiveError::Malformed(e.to_string()))?;
            list_tar(Box::new(dec), cap)
        }
        Some(Format::TarBz2) => list_tar(Box::new(bzip2::read::BzDecoder::new(open(path)?)), cap),
        Some(Format::SevenZ) => list_7z(path, cap),
        None => Err(ArchiveError::Unsupported),
    }
}

fn open(path: &Path) -> Result<File, ArchiveError> {
    File::open(path).map_err(|e| ArchiveError::Io(e.to_string()))
}

fn list_zip(path: &Path, cap: usize) -> Result<ArchiveListing, ArchiveError> {
    let file = open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(map_zip_err)?;

    let total = archive.len();
    let take = total.min(cap);
    let mut entries = Vec::with_capacity(take);

    for i in 0..take {
        // `by_index_raw` reads the central-directory record only and
        // never sets up a decryption stream, so password-protected
        // archives (where just the payload is encrypted) still list
        // their entry names, sizes, and directory flags.
        let entry = archive.by_index_raw(i).map_err(map_zip_err)?;
        entries.push(Entry {
            name: entry.name().to_string(),
            size: entry.size(),
            is_dir: entry.is_dir(),
        });
    }

    Ok(ArchiveListing {
        entries,
        truncated: total > take,
    })
}

fn map_zip_err(err: zip::result::ZipError) -> ArchiveError {
    match err {
        zip::result::ZipError::Io(e) => ArchiveError::Io(e.to_string()),
        zip::result::ZipError::InvalidArchive(why) => ArchiveError::Malformed(why.to_string()),
        zip::result::ZipError::UnsupportedArchive(why) => {
            // The zip crate reports password-protected entries through
            // this variant; surface it as an encryption state so the
            // UI can explain why no listing is available.
            if why.eq_ignore_ascii_case("Password required to decrypt file") {
                ArchiveError::Encrypted
            } else {
                ArchiveError::Malformed(why.to_string())
            }
        }
        zip::result::ZipError::FileNotFound => {
            ArchiveError::Malformed("entry not found".to_string())
        }
        other => ArchiveError::Malformed(other.to_string()),
    }
}

fn list_tar(reader: Box<dyn Read>, cap: usize) -> Result<ArchiveListing, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let iter = archive
        .entries()
        .map_err(|e| ArchiveError::Malformed(e.to_string()))?;

    let mut entries = Vec::new();
    let mut truncated = false;

    for item in iter {
        let entry = item.map_err(|e| ArchiveError::Malformed(e.to_string()))?;
        if entries.len() >= cap {
            truncated = true;
            break;
        }
        let header = entry.header();
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from("<invalid path>"));
        let size = header.size().unwrap_or(0);
        let is_dir = header.entry_type().is_dir();
        entries.push(Entry { name, size, is_dir });
    }

    Ok(ArchiveListing { entries, truncated })
}

fn list_7z(path: &Path, cap: usize) -> Result<ArchiveListing, ArchiveError> {
    let mut file = open(path)?;
    let archive = sevenz_rust2::Archive::read(&mut file, &sevenz_rust2::Password::empty())
        .map_err(map_7z_err)?;

    let total = archive.files.len();
    let take = total.min(cap);
    let mut entries = Vec::with_capacity(take);

    for entry in archive.files.iter().take(take) {
        entries.push(Entry {
            name: entry.name().to_string(),
            size: entry.size(),
            is_dir: entry.is_directory(),
        });
    }

    Ok(ArchiveListing {
        entries,
        truncated: total > take,
    })
}

fn map_7z_err(err: sevenz_rust2::Error) -> ArchiveError {
    match err {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => {
            ArchiveError::Encrypted
        }
        sevenz_rust2::Error::Io(e, _) => ArchiveError::Io(e.to_string()),
        other => ArchiveError::Malformed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "lixun-archive-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    #[test]
    fn detect_known_extensions() {
        assert_eq!(detect_format(Path::new("a.zip")), Some(Format::Zip));
        assert_eq!(detect_format(Path::new("a.tar")), Some(Format::Tar));
        assert_eq!(detect_format(Path::new("a.tar.gz")), Some(Format::TarGz));
        assert_eq!(detect_format(Path::new("a.tgz")), Some(Format::TarGz));
        assert_eq!(detect_format(Path::new("a.tar.zst")), Some(Format::TarZst));
        assert_eq!(detect_format(Path::new("a.tar.bz2")), Some(Format::TarBz2));
        assert_eq!(detect_format(Path::new("a.7z")), Some(Format::SevenZ));
    }

    #[test]
    fn detect_is_case_insensitive() {
        assert_eq!(detect_format(Path::new("PHOTOS.ZIP")), Some(Format::Zip));
        assert_eq!(
            detect_format(Path::new("Backup.Tar.GZ")),
            Some(Format::TarGz)
        );
    }

    #[test]
    fn detect_rejects_unknown() {
        assert_eq!(detect_format(Path::new("a.gz")), None);
        assert_eq!(detect_format(Path::new("a.txt")), None);
        assert_eq!(detect_format(Path::new("noext")), None);
    }

    #[test]
    fn unsupported_extension_errors() {
        let err = list_entries(Path::new("/tmp/whatever.txt"), 10).unwrap_err();
        assert!(matches!(err, ArchiveError::Unsupported));
    }

    #[test]
    fn lists_zip_entries() {
        let path = tmp("list.zip");
        {
            let file = File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.add_directory("dir/", opts).unwrap();
            zw.start_file("dir/hello.txt", opts).unwrap();
            zw.write_all(b"hello world").unwrap();
            zw.start_file("root.bin", opts).unwrap();
            zw.write_all(b"abc").unwrap();
            zw.finish().unwrap();
        }

        let listing = list_entries(&path, 5000).unwrap();
        assert!(!listing.truncated);

        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"dir/"));
        assert!(names.contains(&"dir/hello.txt"));
        assert!(names.contains(&"root.bin"));

        let dir = listing.entries.iter().find(|e| e.name == "dir/").unwrap();
        assert!(dir.is_dir);
        let file = listing
            .entries
            .iter()
            .find(|e| e.name == "dir/hello.txt")
            .unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 11);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encrypted_payload_zip_still_lists_names() {
        let path = tmp("encrypted.zip");
        {
            let file = File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .with_aes_encryption(zip::AesMode::Aes256, "secret");
            zw.start_file("secret.txt", opts).unwrap();
            zw.write_all(b"classified payload").unwrap();
            zw.finish().unwrap();
        }

        let listing = list_entries(&path, 5000).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"secret.txt"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zip_cap_marks_truncated() {
        let path = tmp("cap.zip");
        {
            let file = File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for i in 0..6 {
                zw.start_file(format!("f{i}.txt"), opts).unwrap();
                zw.write_all(b"x").unwrap();
            }
            zw.finish().unwrap();
        }

        let listing = list_entries(&path, 3).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lists_tar_entries() {
        let path = tmp("list.tar");
        {
            let file = File::create(&path).unwrap();
            let mut builder = tar::Builder::new(file);
            let data = b"tar payload";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "inside.txt", &data[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let listing = list_entries(&path, 5000).unwrap();
        assert!(!listing.truncated);
        let entry = listing
            .entries
            .iter()
            .find(|e| e.name == "inside.txt")
            .unwrap();
        assert_eq!(entry.size, 11);
        assert!(!entry.is_dir);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_archive_does_not_panic() {
        let path = tmp("broken.zip");
        std::fs::write(&path, b"not a real zip file at all").unwrap();
        let err = list_entries(&path, 10).unwrap_err();
        assert!(matches!(
            err,
            ArchiveError::Malformed(_) | ArchiveError::Io(_)
        ));
        let _ = std::fs::remove_file(&path);
    }
}
