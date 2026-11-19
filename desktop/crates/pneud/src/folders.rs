//! Folder capture, in-place sending and descriptor-relative private trees.
//! Neither side keeps a copy of the stream: the sender reads files in place
//! and the receiver writes content straight into its private tree, keeping
//! only the manifest bytes in the stage file. No peer path is opened as a
//! host path.
use crate::{
    platform::fs,
    storage::{Folder, Stage},
};
use anyhow::{Context, Result, ensure};
use pneu_proto::{FileBegin, FolderEntry, FolderEntryType, FolderManifest, Hash, NackCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
    sync::Arc,
};

#[derive(Debug, thiserror::Error)]
#[error("invalid folder manifest or local path")]
pub(crate) struct Invalid;
#[derive(Debug, thiserror::Error)]
#[error("folder entry checksum mismatch")]
pub(crate) struct HashMismatch;
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Details {
    pub items: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_size: Option<u64>,
    pub links: u64,
    pub special: u64,
    pub failed: u64,
    pub names: Vec<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub diagnostic_count: u64,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}
impl Details {
    fn diagnostic(&mut self, name: &str, reason: &str) {
        self.failed_reason(name, reason);
        self.diagnostic_count += 1;
        if self.diagnostics.len() < 3 {
            self.diagnostics.push(format!(
                "{}: {}",
                name.chars().take(255).collect::<String>(),
                reason.chars().take(255).collect::<String>()
            ));
        }
    }
    fn failed_entry(&mut self, name: &str, reason: &str, kind: fs::CaptureType) {
        if matches!(kind, fs::CaptureType::Directory) {
            self.diagnostic(name, reason);
        } else {
            self.failed_reason(name, reason);
        }
    }
    fn failed_reason(&mut self, name: &str, reason: &str) {
        self.failed += 1;
        if self.names.len() < 3 {
            self.names.push(name.chars().take(255).collect());
            self.reasons.push(reason.chars().take(255).collect());
        }
    }
}
/// A captured folder is sent from its files in place. Only the manifest and
/// this proof of what was read during Preparing are kept on the sender.
pub(crate) struct Capture {
    pub manifest: Vec<u8>,
    pub sha256: Hash,
    pub size: u64,
    pub details: Details,
    pub sources: Sources,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Sources {
    pub root: fs::Identity,
    pub manifest_sha256: String,
    /// One version per file entry, in manifest order.
    pub files: Vec<fs::Version>,
}
/// A file changed after Preparing. The transfer fails rather than sending
/// content different from its manifest.
#[derive(Debug, thiserror::Error)]
#[error("Folder changed after Preparing. Send the folder again.")]
pub(crate) struct SourceChanged;
struct Walker<'a> {
    entries: Vec<FolderEntry>,
    versions: Vec<fs::Version>,
    size: u64,
    details: Details,
    limit: u64,
    total: u64,
    check: &'a dyn Fn() -> Result<()>,
    progress: &'a dyn Fn(u64, u64, u64),
}
fn clutter(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini") || name.starts_with("._")
}
impl Walker<'_> {
    fn walk(&mut self, directory: &File, prefix: &str) -> Result<()> {
        let mut names = fs::entries_limited(directory, 10_002)?.collect::<Result<Vec<_>>>()?;
        ensure!(names.len() <= 10_002, TooMany);
        names.sort();
        for name in names {
            (self.check)()?;
            let Some(name_str) = name.to_str() else {
                self.details
                    .diagnostic("Non-Unicode name", "Couldn't represent entry name");
                continue;
            };
            if name_str == "." || name_str == ".." || clutter(name_str) {
                continue;
            }
            let path = if prefix.is_empty() {
                name_str.to_owned()
            } else {
                format!("{prefix}/{name_str}")
            };
            let (kind, enumerated) = match fs::capture_entry(directory, &name) {
                Ok(entry) => entry,
                Err(e) if fs::is_link(&e) => {
                    self.details.links += 1;
                    continue;
                }
                Err(error) => {
                    self.details.diagnostic(name_str, &error.to_string());
                    continue;
                }
            };
            match kind {
                fs::CaptureType::Link => {
                    self.details.links += 1;
                    continue;
                }
                fs::CaptureType::Special => {
                    self.details.special += 1;
                    continue;
                }
                _ => {}
            }
            if pneu_proto::validate_folder_path(&path).is_err() {
                self.details
                    .failed_reason(name_str, "Unsupported name or path");
                continue;
            }
            let file = match fs::open_read_at(directory, &name) {
                Ok(file) => file,
                Err(error) if fs::is_link(&error) => {
                    self.details.links += 1;
                    continue;
                }
                Err(error) => {
                    self.details
                        .failed_entry(name_str, &error.to_string(), kind);
                    continue;
                }
            };
            if fs::identity(&file)? != enumerated {
                self.details
                    .failed_entry(name_str, "Entry changed while capturing", kind);
                continue;
            }
            let meta = file.metadata()?;
            if meta.is_dir() {
                ensure!(self.entries.len() < 10_000, TooMany);
                let start = self.entries.len();
                let versions = self.versions.len();
                let data_start = self.size;
                let details = self.details.clone();
                self.entries.push(FolderEntry {
                    path: path.clone(),
                    entry_type: FolderEntryType::Directory,
                    size: None,
                    sha256: None,
                });
                // An inaccessible/replaced subtree contributes one bounded diagnostic,
                // never a guessed number of unknown descendants.
                let subtree = self.walk(&file, &path);
                if let Err(error) = &subtree {
                    if error.downcast_ref::<TooLarge>().is_some() {
                        return Err(TooLarge.into());
                    }
                    if error.downcast_ref::<TooMany>().is_some() {
                        return Err(TooMany.into());
                    }
                    (self.check)()?;
                }
                if subtree.is_err() || fs::identity_at(directory, &name).ok() != Some(enumerated) {
                    self.entries.truncate(start);
                    self.versions.truncate(versions);
                    self.size = data_start;
                    self.details = details;
                    self.details
                        .diagnostic(name_str, "Couldn't enumerate or safely capture subtree");
                }
            } else if meta.is_file() {
                if fs::single_link(&file).is_err() {
                    self.details.special += 1;
                    continue;
                }
                ensure!(self.entries.len() < 10_000, TooMany);
                let start = self.size;
                // Hash in place; nothing is copied. The version proves later
                // reads (the stream hash pass and sending) see the same file.
                let result = (|| -> Result<(Hash, fs::Version)> {
                    let before = fs::Version::read_regular(&file)?;
                    let mut file = file;
                    let mut hash = Sha256::new();
                    let mut buffer = vec![0; pneu_proto::CHUNK_SIZE];
                    loop {
                        (self.check)()?;
                        let count = file.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        self.size = self
                            .size
                            .checked_add(count as u64)
                            .context("folder size overflow")?;
                        ensure!(self.size <= self.limit, TooLarge);
                        hash.update(&buffer[..count]);
                        (self.progress)(self.details.items, self.size / 2, self.total);
                    }
                    ensure!(
                        before.same_content_metadata(&fs::Version::read_regular(&file)?)
                            && fs::identity_at(directory, &name)? == enumerated,
                        "file changed during capture"
                    );
                    Ok((Hash::new(hex::encode(hash.finalize()))?, before))
                })();
                match result {
                    Ok((hash, version)) => {
                        self.entries.push(FolderEntry {
                            path,
                            entry_type: FolderEntryType::File,
                            size: Some(self.size - start),
                            sha256: Some(hash),
                        });
                        self.versions.push(version);
                        self.details.items += 1;
                    }
                    Err(error) => {
                        self.size = start;
                        if error.downcast_ref::<TooLarge>().is_some() {
                            return Err(error);
                        }
                        (self.check)()?;
                        self.details.failed_reason(name_str, &error.to_string());
                    }
                }
            } else {
                self.details.special += 1;
            }
            (self.progress)(self.details.items, self.size / 2, self.total);
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
#[error("folder exceeds send size limit")]
struct TooLarge;
#[derive(Debug, thiserror::Error)]
#[error("Folder contains more than 10,000 entries")]
struct TooMany;
fn count_tree(
    directory: &File,
    prefix: &str,
    count: &mut u64,
    bytes: &mut u64,
    entries: &mut usize,
    check: &dyn Fn() -> Result<()>,
    progress: &dyn Fn(u64, u64, u64),
) -> Result<()> {
    check()?;
    for name in fs::entries_limited(directory, 10_002)? {
        check()?;
        let name = name?;
        let text = name.to_string_lossy();
        if text == "." || text == ".." || clutter(&text) {
            continue;
        }
        *entries += 1;
        ensure!(*entries <= 10_000, TooMany);
        let Some(text) = name.to_str() else {
            continue;
        };
        let path = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        if pneu_proto::validate_folder_path(&path).is_err() {
            continue;
        }
        let Ok((kind, _)) = fs::capture_entry(directory, &name) else {
            continue;
        };
        if matches!(kind, fs::CaptureType::Directory) {
            let Ok(file) = fs::open_directory_at(directory, &name) else {
                continue;
            };
            if let Err(error) = count_tree(&file, &path, count, bytes, entries, check, progress) {
                if error.downcast_ref::<TooMany>().is_some() {
                    return Err(error);
                }
                check()?;
            }
        } else if matches!(kind, fs::CaptureType::File) {
            let Ok(file) = fs::metadata_file(directory, &name) else {
                continue;
            };
            if fs::single_link(&file).is_err() {
                continue;
            }
            *count += 1;
            *bytes = bytes.saturating_add(file.metadata()?.len());
            progress(*count, 0, 0);
        }
    }
    Ok(())
}
/// Open a captured regular file in place by its manifest path. Every component
/// is opened without following links; the captured version proves identity,
/// size and modification state, whatever the path now resolves through.
pub(crate) fn open_source(root: &File, path: &str, version: &fs::Version) -> Result<File> {
    let mut directory: Option<File> = None;
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        let opened = fs::open_read_at(directory.as_ref().unwrap_or(root), component)
            .map_err(|_| SourceChanged)?;
        if components.peek().is_none() {
            ensure!(
                fs::Version::read_regular(&opened).is_ok_and(|v| v.same_content_metadata(version))
                    && fs::single_link(&opened).is_ok(),
                SourceChanged
            );
            return Ok(opened);
        }
        ensure!(opened.metadata()?.is_dir(), SourceChanged);
        directory = Some(opened);
    }
    Err(Invalid.into())
}
/// Read a whole captured file in place, checking its size, hash and version.
fn verify_source(
    root: &File,
    entry: &FolderEntry,
    version: &fs::Version,
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut file = open_source(root, &entry.path, version)?;
    let size = entry.size.ok_or(Invalid)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; pneu_proto::CHUNK_SIZE];
    let mut read = 0;
    loop {
        let n = file.read(&mut buffer).map_err(|_| SourceChanged)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        ensure!(read <= size, SourceChanged);
        hash.update(&buffer[..n]);
        each(&buffer[..n])?;
    }
    ensure!(
        read == size
            && Some(hex::encode(hash.finalize()).as_str())
                == entry.sha256.as_ref().map(Hash::as_str)
            && fs::Version::read_regular(&file).is_ok_and(|v| v.same_content_metadata(version)),
        SourceChanged
    );
    Ok(())
}
pub(crate) fn capture(
    path: &Path,
    limit: u64,
    check: &dyn Fn() -> Result<()>,
    progress: &dyn Fn(u64, u64, u64),
) -> Result<Capture> {
    let root = Folder::read(path)?;
    let (mut items, mut bytes, mut entries) = (0, 0, 0);
    count_tree(
        &root.file,
        "",
        &mut items,
        &mut bytes,
        &mut entries,
        check,
        progress,
    )?;
    let name = path.file_name().context("folder name")?.to_string_lossy();
    let limit = limit.min(pneu_proto::MAX_FILE_SIZE);
    let too_large = || {
        format!(
            "{name} is over {}",
            if limit == pneu_proto::MAX_FILE_SIZE {
                "64 GB".into()
            } else {
                size_label(limit)
            }
        )
    };
    ensure!(bytes <= limit, "{}", too_large());
    let mut walker = Walker {
        entries: Vec::new(),
        versions: Vec::new(),
        size: 0,
        details: Details::default(),
        limit,
        total: bytes,
        check,
        progress,
    };
    progress(items, 0, bytes);
    let result = walker.walk(&root.file, "");
    if let Err(error) = result {
        if error.downcast_ref::<TooLarge>().is_some() {
            anyhow::bail!("{}", too_large());
        }
        return Err(error);
    }
    // The manifest carries every file's hash, so the stream hash
    // SHA(manifest || content) needs a second in-place read. A file that
    // changes in between is skipped like one that changed during capture.
    loop {
        ensure!(
            Folder::read(path)?.identity()? == root.identity()?,
            "capture root changed"
        );
        ensure!(walker.details.items > 0, "{name} is empty");
        let manifest = serde_json::to_vec(&FolderManifest {
            entries: walker.entries.clone(),
        })?;
        let size = walker
            .size
            .checked_add(manifest.len() as u64)
            .context("folder size overflow")?;
        ensure!(size <= walker.limit, "{}", too_large());
        let mut stream = Sha256::new();
        stream.update(&manifest);
        let mut done = 0u64;
        let mut changed = None;
        let mut versions = walker.versions.iter();
        for (index, entry) in walker.entries.iter().enumerate() {
            if entry.entry_type != FolderEntryType::File {
                continue;
            }
            let version = versions.next().context("folder versions")?;
            let result = verify_source(&root.file, entry, version, |bytes| {
                check()?;
                stream.update(bytes);
                done += bytes.len() as u64;
                progress(walker.details.items, (walker.size + done) / 2, walker.size);
                Ok(())
            });
            match result {
                Ok(()) => {}
                Err(error) if error.downcast_ref::<SourceChanged>().is_some() => {
                    changed = Some(index);
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        let Some(index) = changed else {
            walker.details.content_size = Some(walker.size);
            return Ok(Capture {
                sources: Sources {
                    root: root.identity()?,
                    manifest_sha256: hex::encode(Sha256::digest(&manifest)),
                    files: walker.versions,
                },
                manifest,
                sha256: Hash::new(hex::encode(stream.finalize()))?,
                size,
                details: walker.details,
            });
        };
        let file = walker.entries[..index]
            .iter()
            .filter(|e| e.entry_type == FolderEntryType::File)
            .count();
        let entry = walker.entries.remove(index);
        walker.versions.remove(file);
        walker.size -= entry.size.unwrap_or(0);
        walker.details.items -= 1;
        let base = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        walker
            .details
            .failed_reason(base, "file changed during capture");
    }
}

/// Reads a captured folder's stream in place: manifest bytes, then each file
/// in manifest order. Every file entered from its start is hashed as it is
/// read and checked at its end. A resumed mid-file suffix is hashed inline;
/// receiver integrity checks request a restart at zero if its prefix differs.
pub(crate) struct FolderSource {
    root: Arc<File>,
    manifest: Vec<u8>,
    files: Vec<(FolderEntry, fs::Version, u64)>,
    total: u64,
    position: u64,
    index: usize,
    current: Option<Current>,
    #[cfg(test)]
    read_delay: std::time::Duration,
}
struct Current {
    file: File,
    read: u64,
    hash: Sha256,
    complete: bool,
}
impl FolderSource {
    pub(crate) fn open(
        path: &Path,
        manifest: Vec<u8>,
        begin: &FileBegin,
        sources: &Sources,
    ) -> Result<Self> {
        ensure!(
            hex::encode(Sha256::digest(&manifest)) == sources.manifest_sha256,
            "staged folder manifest changed"
        );
        let parsed = FolderManifest::parse(&manifest, begin).map_err(|_| Invalid)?;
        let root = Folder::read(path).map_err(|_| SourceChanged)?;
        ensure!(root.identity()? == sources.root, SourceChanged);
        let entries: Vec<_> = parsed
            .entries
            .into_iter()
            .filter(|e| e.entry_type == FolderEntryType::File)
            .collect();
        ensure!(entries.len() == sources.files.len(), Invalid);
        let mut start = manifest.len() as u64;
        let mut files = Vec::with_capacity(entries.len());
        for (entry, version) in entries.into_iter().zip(&sources.files) {
            // Cheap metadata proof before BEGIN; content is hashed as it is sent.
            open_source(&root.file, &entry.path, version)?;
            let size = entry.size.ok_or(Invalid)?;
            files.push((entry, version.clone(), start));
            start += size;
        }
        ensure!(start == begin.size, Invalid);
        Ok(Self {
            root: root.file,
            manifest,
            files,
            total: start,
            position: 0,
            index: 0,
            current: None,
            #[cfg(test)]
            read_delay: std::time::Duration::ZERO,
        })
    }
    fn size(&self, index: usize) -> u64 {
        self.files[index].0.size.unwrap_or(0)
    }
    /// Position the stream at a receiver's READY offset.
    pub(crate) fn seek(&mut self, offset: u64) -> Result<()> {
        ensure!(offset <= self.total, "folder offset beyond stream");
        self.current = None;
        self.position = offset;
        // Files ending at or before the offset were delivered already.
        self.index = self.files.partition_point(|(_, _, start)| *start < offset);
        if self.index > 0 {
            let previous = self.index - 1;
            let start = self.files[previous].2;
            let local = offset - start;
            if local < self.size(previous) {
                self.index = previous;
                let (entry, version, _) = self.files[previous].clone();
                let file = open_source(&self.root, &entry.path, &version)?;
                self.current = Some(Current {
                    file,
                    read: local,
                    hash: Sha256::new(),
                    complete: false,
                });
            }
        }
        Ok(())
    }
    /// Fill `buffer` from the stream; returns 0 only at the stream's end,
    /// after every remaining (including empty) file has been checked.
    pub(crate) fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let mut filled = 0;
        if self.position < self.manifest.len() as u64 {
            let at = self.position as usize;
            let n = (self.manifest.len() - at).min(buffer.len());
            buffer[..n].copy_from_slice(&self.manifest[at..at + n]);
            filled = n;
            self.position += n as u64;
        }
        while filled < buffer.len() && self.index < self.files.len() {
            let size = self.size(self.index);
            if self.current.is_none() {
                let (entry, version, _) = &self.files[self.index];
                self.current = Some(Current {
                    file: open_source(&self.root, &entry.path, version)?,
                    read: 0,
                    hash: Sha256::new(),
                    complete: true,
                });
            }
            let current = self.current.as_mut().expect("folder entry");
            if current.read < size {
                let want = (size - current.read).min((buffer.len() - filled) as u64) as usize;
                #[cfg(test)]
                std::thread::sleep(self.read_delay);
                let n = fs::read_at(
                    &current.file,
                    &mut buffer[filled..filled + want],
                    current.read,
                )
                .map_err(|_| SourceChanged)?;
                ensure!(n > 0, SourceChanged);
                current.hash.update(&buffer[filled..filled + n]);
                current.read += n as u64;
                filled += n;
                self.position += n as u64;
                continue;
            }
            let current = self.current.take().expect("folder entry");
            let (entry, version, _) = &self.files[self.index];
            let mut probe = [0];
            ensure!(
                fs::read_at(&current.file, &mut probe, size).map_err(|_| SourceChanged)? == 0
                    && fs::Version::read_regular(&current.file)
                        .is_ok_and(|v| v.same_content_metadata(version)),
                SourceChanged
            );
            if current.complete {
                let hash = current.hash;
                ensure!(
                    Some(hex::encode(hash.finalize()).as_str())
                        == entry.sha256.as_ref().map(Hash::as_str),
                    SourceChanged
                );
            }
            self.index += 1;
        }
        Ok(filled)
    }
    /// After the declared size is sent, check the last files' ends.
    pub(crate) fn finish(&mut self) -> Result<()> {
        let mut probe = [0];
        ensure!(self.read(&mut probe)? == 0, SourceChanged);
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TreeRecord {
    pub paths: Vec<String>,
    pub identities: Vec<fs::Identity>,
    pub root: fs::Identity,
    pub details: Details,
}
#[derive(Debug)]
pub(crate) struct Tree {
    pub record: TreeRecord,
    pub root: Arc<File>,
    manifest: FolderManifest,
    segments: Vec<Segment>,
    /// The entry currently receiving bytes, identity-checked when opened.
    receiving: std::sync::Mutex<Receiving>,
}
#[derive(Debug, Default)]
struct Receiving {
    writer: Option<(usize, File)>,
    hashes: Vec<Sha256>,
    lengths: Vec<u64>,
    dirty: HashSet<usize>,
}
impl Receiving {
    fn new(entries: usize) -> Self {
        Self {
            hashes: vec![Sha256::new(); entries],
            lengths: vec![0; entries],
            ..Default::default()
        }
    }
}
#[derive(Debug)]
struct Segment {
    index: usize,
    start: u64,
    end: u64,
}
fn segments(manifest: &FolderManifest, begin: &FileBegin) -> Vec<Segment> {
    let mut start = begin.folder.as_ref().expect("folder manifest").manifest_len;
    manifest
        .entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry.size.map(|size| {
                let segment = Segment {
                    index,
                    start,
                    end: start + size,
                };
                start += size;
                segment
            })
        })
        .collect()
}
fn parent<'a>(root: &File, path: &'a str) -> Result<(File, &'a str)> {
    let mut dir = root.try_clone()?;
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((dir, component));
        }
        dir = fs::open_directory_at(&dir, component)?;
    }
    Err(Invalid.into())
}
pub(crate) use crate::storage::numbered_name as numbered;
/// Probe actual destination name equivalence against already-created entries.
fn equivalent_key(root: &File, name: &str) -> Result<bool> {
    match fs::identity_at(root, name) {
        Ok(_) => Ok(true),
        Err(e) if fs::is_missing(&e) => Ok(false),
        Err(e) => Err(mapped_error(e)),
    }
}
fn mapping(
    root: &File,
    manifest: &FolderManifest,
    mapper: &dyn Fn(&str, usize) -> String,
    check: &dyn Fn() -> Result<()>,
) -> Result<(Vec<String>, Vec<fs::Identity>)> {
    let mut result = Vec::new();
    let mut identities = Vec::new();
    let mut next_number: HashMap<(String, String), usize> = HashMap::new();
    let mut directories: HashMap<String, String> = HashMap::new();
    // Probe each sibling set in its actual parent, so case/normalization rules
    // (including per-directory Windows sensitivity) are honored by the OS.
    for entry in &manifest.entries {
        check()?;
        let (wire_parent, base) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
        let mapped_parent = directories.get(wire_parent).cloned().unwrap_or_default();
        let dir = if mapped_parent.is_empty() {
            root.try_clone()?
        } else {
            let (p, n) = parent(root, &mapped_parent)?;
            fs::open_directory_at(&p, n)?
        };
        let directory = entry.entry_type == FolderEntryType::Directory;
        let basename = mapper(base, 255);
        pneu_proto::validate_folder_path(&basename).map_err(|_| Invalid)?;
        let mut name = basename.clone();
        let number = next_number
            .entry((mapped_parent.clone(), basename.clone()))
            .or_insert(2);
        while equivalent_key(&dir, &name)? {
            name = numbered(&basename, *number, directory);
            *number += 1;
        }
        let created = (|| -> Result<File> {
            if directory {
                fs::create_private_directory_at(&dir, &name)?;
                fs::open_directory_at(&dir, &name)
            } else {
                fs::create_private_at(&dir, &name, fs::Access::ReadWrite)
            }
        })()
        .map_err(mapped_error)?;
        crate::platform::private::created(&created)?;
        identities.push(fs::identity(&created)?);
        let path = if mapped_parent.is_empty() {
            name
        } else {
            format!("{mapped_parent}/{name}")
        };
        if directory {
            directories.insert(entry.path.clone(), path.clone());
        }
        pneu_proto::validate_folder_path(&path).map_err(|_| Invalid)?;
        result.push(path);
    }
    Ok((result, identities))
}
fn mapped_error(error: anyhow::Error) -> anyhow::Error {
    if fs::is_unsupported_name(&error) {
        Invalid.into()
    } else {
        error
    }
}
fn manifest(stage: &Stage, begin: &FileBegin) -> Result<FolderManifest> {
    let len = begin.folder.as_ref().ok_or(Invalid)?.manifest_len as usize;
    ensure!(len <= pneu_proto::MAX_MANIFEST_BYTES, Invalid);
    let mut bytes = vec![0; len];
    let mut at = 0;
    while at < len {
        let count = fs::read_at(&stage.0.file, &mut bytes[at..], at as u64)?;
        ensure!(count > 0, Invalid);
        at += count;
    }
    FolderManifest::parse(&bytes, begin).map_err(|_| Invalid.into())
}
struct BuildGuard {
    directory: Folder,
    name: String,
    root: Arc<File>,
    done: bool,
}
impl Drop for BuildGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = crate::storage::wipe(&self.root);
            let _ = fs::remove_directory(
                &self.directory.file,
                format!("{}-tree", self.name),
                &self.root,
            );
            let _ = self.directory.file.sync_all();
        }
    }
}
impl Tree {
    pub(crate) fn create(stage: &Stage, begin: &FileBegin) -> Result<Self> {
        let manifest = manifest(stage, begin)?;
        let tree_name = format!("{}-tree", stage.0.name);
        fs::create_private_directory_at(&stage.0.directory.file, &tree_name)?;
        let root = Arc::new(fs::open_directory_at(&stage.0.directory.file, &tree_name)?);
        crate::platform::private::created(&root)?;
        let mut guard = BuildGuard {
            directory: stage.0.directory.clone(),
            name: stage.0.name.clone(),
            root: root.clone(),
            done: false,
        };
        // Create each real entry once, freezing its mapping and identity.
        // The catalogue persists the complete map before checkpointing bytes.
        let (paths, identities) = mapping(&root, &manifest, &crate::storage::local_name, &|| {
            ensure!(
                !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder cancelled"
            );
            Ok(())
        })?;
        let details = Details {
            items: manifest.entries.iter().filter(|e| e.size.is_some()).count() as u64,
            content_size: Some(manifest.entries.iter().filter_map(|e| e.size).sum()),
            ..Default::default()
        };
        let mut receiving = Receiving::new(paths.len());
        receiving.dirty.extend(
            manifest
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.size.map(|_| i)),
        );
        let tree = Self {
            record: TreeRecord {
                paths,
                identities,
                root: fs::identity(&root)?,
                details,
            },
            root,
            segments: segments(&manifest, begin),
            manifest,
            receiving: std::sync::Mutex::new(receiving),
        };
        tree.sync_directories(stage)?;
        fs::sync_plain(&stage.0.directory.file)?;
        guard.done = true;
        Ok(tree)
    }
    pub(crate) fn reopen(
        stage: &Stage,
        begin: &FileBegin,
        record: TreeRecord,
        deadline: std::time::Instant,
    ) -> Result<Self> {
        let manifest = manifest(stage, begin)?;
        let root = Arc::new(fs::open_directory_at(
            &stage.0.directory.file,
            format!("{}-tree", stage.0.name),
        )?);
        crate::platform::private::check(&root)?;
        ensure!(
            fs::identity(&root)? == record.root
                && record.paths.len() == manifest.entries.len()
                && record.identities.len() == record.paths.len(),
            "folder identity changed"
        );
        for path in &record.paths {
            pneu_proto::validate_folder_path(path).map_err(|_| Invalid)?;
        }
        let receiving = std::sync::Mutex::new(Receiving::new(record.paths.len()));
        let tree = Self {
            root,
            record,
            segments: segments(&manifest, begin),
            manifest,
            receiving,
        };
        tree.exact(stage, Some(deadline))?;
        Ok(tree)
    }
    fn open(&self, index: usize) -> Result<File> {
        let (dir, name) = parent(&self.root, &self.record.paths[index])?;
        let file = if self.manifest.entries[index].entry_type == FolderEntryType::Directory {
            fs::open_directory_at(&dir, name)?
        } else {
            fs::open_read_write_at(&dir, name)?
        };
        crate::platform::private::check(&file)?;
        ensure!(
            fs::identity(&file)? == self.record.identities[index],
            "folder child identity changed"
        );
        if self.manifest.entries[index].entry_type == FolderEntryType::File {
            fs::single_link(&file)?;
            ensure!(file.metadata()?.is_file(), "folder child type changed");
        }
        Ok(file)
    }
    fn exact(&self, stage: &Stage, deadline: Option<std::time::Instant>) -> Result<()> {
        let mut expected: HashMap<String, HashSet<fs::Identity>> = HashMap::new();
        expected.insert(String::new(), HashSet::new());
        for (i, path) in self.record.paths.iter().enumerate() {
            ensure!(
                deadline.is_none_or(|d| std::time::Instant::now() < d)
                    && !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder recovery cancelled or timed out"
            );
            self.open(i)?;
            let (p, _) = path.rsplit_once('/').unwrap_or(("", path));
            expected
                .entry(p.to_owned())
                .or_default()
                .insert(self.record.identities[i]);
            if self.manifest.entries[i].entry_type == FolderEntryType::Directory {
                expected.entry(path.clone()).or_default();
            }
        }
        for (path, expected) in expected {
            ensure!(
                deadline.is_none_or(|d| std::time::Instant::now() < d)
                    && !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder recovery cancelled or timed out"
            );
            let dir = if path.is_empty() {
                self.root.try_clone()?
            } else {
                let (p, n) = parent(&self.root, &path)?;
                fs::open_directory_at(&p, n)?
            };
            let mut actual = HashSet::new();
            let mut count = 0;
            for name in fs::entries_limited(&dir, 10_002)? {
                let name = name?;
                if name == "." || name == ".." {
                    continue;
                }
                count += 1;
                actual.insert(fs::identity_at(&dir, &name)?);
            }
            // Compare identities as well as cardinality: native filesystems may
            // enumerate normalized spellings while retaining name equivalence.
            ensure!(
                actual == expected && count == expected.len(),
                "unexpected folder tree entries"
            );
        }
        Ok(())
    }
    /// Write live content bytes `[from, from+data.len())` (all at or after the
    /// manifest) straight into their entries. Chunks may cross file boundaries.
    fn write(&self, stage: &Stage, from: u64, data: &[u8]) -> Result<()> {
        let to = from + data.len() as u64;
        let first = self.segments.partition_point(|s| s.end <= from);
        let mut receiving = self.receiving.lock().expect("folder receiver");
        for segment in &self.segments[first..] {
            if segment.start >= to {
                break;
            }
            if segment.end == segment.start {
                continue;
            }
            ensure!(
                !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder cancelled"
            );
            let offset = from.max(segment.start);
            let end = to.min(segment.end);
            if receiving
                .writer
                .as_ref()
                .is_none_or(|(i, _)| *i != segment.index)
            {
                receiving.writer = Some((segment.index, self.open(segment.index)?));
            }
            ensure!(
                receiving.lengths[segment.index] == offset - segment.start,
                "noncontiguous folder entry"
            );
            let file = &receiving.writer.as_ref().expect("folder writer").1;
            fs::write_all_at(
                file,
                &data[(offset - from) as usize..(end - from) as usize],
                offset - segment.start,
            )?;
            receiving.hashes[segment.index]
                .update(&data[(offset - from) as usize..(end - from) as usize]);
            receiving.lengths[segment.index] += end - offset;
            receiving.dirty.insert(segment.index);
        }
        Ok(())
    }
    /// Rebuild the whole-stream digest from the private tree: hash every
    /// entry's retained prefix in manifest order, truncating the entry that
    /// intersects `offset` and resetting every later entry to zero bytes.
    fn recover(
        &self,
        stage: &Stage,
        offset: u64,
        hash: &mut Sha256,
        deadline: std::time::Instant,
    ) -> Result<()> {
        self.release();
        let mut receiving = self.receiving.lock().expect("folder receiver");
        let mut buffer = vec![0; pneu_proto::CHUNK_SIZE];
        for segment in &self.segments {
            let file = self.open(segment.index)?;
            let prefix = offset
                .saturating_sub(segment.start)
                .min(segment.end - segment.start);
            ensure!(file.metadata()?.len() >= prefix, "short folder checkpoint");
            let mut at = 0;
            let mut entry_hash = Sha256::new();
            while at < prefix {
                ensure!(
                    std::time::Instant::now() < deadline
                        && !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                    "folder recovery cancelled or timed out"
                );
                let n = (prefix - at).min(buffer.len() as u64) as usize;
                let read = fs::read_at(&file, &mut buffer[..n], at)?;
                ensure!(read > 0, "short folder checkpoint");
                hash.update(&buffer[..read]);
                entry_hash.update(&buffer[..read]);
                at += read as u64;
            }
            if file.metadata()?.len() != prefix {
                file.set_len(prefix)?;
                receiving.dirty.insert(segment.index);
            }
            receiving.hashes[segment.index] = entry_hash;
            receiving.lengths[segment.index] = prefix;
        }
        Ok(())
    }
    pub(crate) fn checkpoint(&self, stage: &Stage) -> Result<()> {
        let mut receiving = self.receiving.lock().expect("folder receiver");
        for &index in &receiving.dirty {
            ensure!(
                !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder cancelled"
            );
            fs::sync_plain(&self.open(index)?)?;
        }
        // On macOS sync_all is the single F_FULLFSYNC barrier, after plain
        // fsyncs of the touched entries and their directory records.
        stage.0.file.sync_all()?;
        receiving.dirty.clear();
        Ok(())
    }
    /// Windows can refuse a tree rename while an entry handle is open.
    pub(crate) fn release(&self) {
        self.receiving
            .lock()
            .expect("folder receiver")
            .writer
            .take();
    }
    fn sync_directories(&self, stage: &Stage) -> Result<()> {
        for i in (0..self.record.paths.len()).rev() {
            ensure!(
                !stage.0.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                "folder cancelled"
            );
            if self.manifest.entries[i].entry_type == FolderEntryType::Directory {
                fs::sync_plain(&self.open(i)?)?;
            }
        }
        fs::sync_plain(&self.root)?;
        Ok(())
    }
    pub(crate) fn verify(&self, stage: &Stage) -> Result<()> {
        self.release();
        self.exact(stage, None)?;
        {
            let receiving = self.receiving.lock().expect("folder receiver");
            for (i, entry) in self.manifest.entries.iter().enumerate() {
                if let Some(size) = entry.size {
                    ensure!(
                        self.open(i)?.metadata()?.len() == size && receiving.lengths[i] == size,
                        HashMismatch
                    );
                    ensure!(
                        hex::encode(receiving.hashes[i].clone().finalize())
                            == entry.sha256.as_ref().ok_or(Invalid)?.as_str(),
                        HashMismatch
                    );
                }
            }
        }
        self.sync_directories(stage)?;
        self.checkpoint(stage)
    }
}
impl Stage {
    /// Store one contiguous stream range. Only manifest bytes go to the stage
    /// file; content goes directly into the private tree, so a folder needs
    /// about its own size in temporary space.
    pub(crate) fn folder_write(&self, begin: &FileBegin, from: u64, data: &[u8]) -> Result<()> {
        let Some(folder) = &begin.folder else {
            return Ok(());
        };
        let manifest_len = folder.manifest_len;
        let to = from + data.len() as u64;
        if from < manifest_len {
            let n = (manifest_len - from).min(data.len() as u64) as usize;
            fs::write_all_at(&self.0.file, &data[..n], from)?;
        }
        if to < manifest_len {
            return Ok(());
        }
        let mut tree = self.0.tree.lock().expect("folder tree");
        if tree.is_none() {
            *tree = Some(Tree::create(self, begin)?);
        }
        let start = from.max(manifest_len);
        if to > start {
            tree.as_ref().expect("folder tree").write(
                self,
                start,
                &data[(start - from) as usize..],
            )?;
        }
        Ok(())
    }
    /// Seed the whole-stream digest after the manifest prefix is hashed.
    pub(crate) fn folder_recover(
        &self,
        offset: u64,
        hash: &mut Sha256,
        deadline: std::time::Instant,
    ) -> Result<()> {
        match self.0.tree.lock().expect("folder tree").as_ref() {
            Some(tree) => tree.recover(self, offset, hash, deadline),
            None => Ok(()),
        }
    }
    pub(crate) fn folder_details(&self) -> Option<Details> {
        self.0
            .tree
            .lock()
            .expect("folder tree")
            .as_ref()
            .map(|t| t.record.details.clone())
    }
    pub(crate) fn published_file(&self) -> Result<File> {
        match self.0.tree.lock().expect("folder tree").as_ref() {
            Some(t) => Ok(t.root.try_clone()?),
            None => Ok(self.0.file.try_clone()?),
        }
    }
}
pub(crate) fn failure(error: &(dyn std::error::Error + 'static)) -> Option<NackCode> {
    if error.is::<Invalid>() {
        Some(NackCode::UnsupportedKind)
    } else if error.is::<HashMismatch>() {
        Some(NackCode::HashMismatch)
    } else {
        None
    }
}

/// Keep published user folders out of every disposable/control namespace,
/// including the pre-resume staging names still reconciled on startup.
pub(crate) fn root_name(name: &str) -> String {
    let name = crate::storage::local_name(name, 255);
    let lower = name.to_ascii_lowercase();
    if lower == crate::storage::STAGING_DIR
        || lower.starts_with(crate::storage::CACHE_PREFIX)
        || lower.starts_with(".pneu-stage-")
    {
        format!("_{}", crate::storage::local_name(&name, 254))
    } else {
        name
    }
}
pub(crate) fn items(count: u64) -> String {
    format!(
        "{} {}",
        grouped(count),
        if count == 1 { "item" } else { "items" }
    )
}
pub(crate) fn grouped(count: u64) -> String {
    let s = count.to_string();
    let mut r = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            r.push(',');
        }
        r.push(c);
    }
    r
}
fn size_unit(size: u64) -> (&'static str, f64) {
    match size {
        0..1_000 => ("B", 1.0),
        1_000..1_000_000 => ("KB", 1e3),
        1_000_000..1_000_000_000 => ("MB", 1e6),
        _ => ("GB", 1e9),
    }
}
fn size_number(bytes: u64, unit: &str, scale: f64) -> String {
    let value = bytes as f64 / scale;
    if value < 10.0 && scale > 1.0 && (unit == "GB" || value.fract() >= 0.05) {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}
pub(crate) fn size_label(size: u64) -> String {
    let (unit, scale) = size_unit(size);
    format!("{} {unit}", size_number(size, unit, scale))
}
pub(crate) fn size_pair(sent: u64, size: u64) -> String {
    let (unit, scale) = size_unit(size);
    format!(
        "{} of {} {unit}",
        size_number(sent.min(size), unit, scale),
        size_number(size, unit, scale)
    )
}

pub(crate) fn discard_orphan_tree(stage: &Stage) -> Result<()> {
    let name = format!("{}-tree", stage.0.name);
    match fs::open_directory_at(&stage.0.directory.file, &name) {
        Ok(tree) => {
            crate::platform::private::check(&tree)?;
            ensure!(fs::is_owned_private(&tree)?, "unsafe folder orphan");
            crate::storage::wipe(&tree)?;
            fs::remove_directory(&stage.0.directory.file, &name, &tree)?;
        }
        Err(e) if fs::is_missing(&e) => {}
        Err(e) => return Err(e),
    }
    stage.0.directory.file.sync_all()?;
    Ok(())
}

impl TreeRecord {
    pub(crate) fn cleanup(&self, directory: &Folder, name: &str) -> Result<()> {
        match fs::open_directory_at(&directory.file, format!("{name}-tree")) {
            Ok(root) => {
                ensure!(
                    fs::identity(&root)? == self.root,
                    "folder root substituted during cleanup"
                );
                for (path, identity) in self.paths.iter().zip(&self.identities).rev() {
                    let (parent, name) = match parent(&root, path) {
                        Ok(p) => p,
                        Err(e) if fs::is_missing(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    let child = match fs::metadata_file(&parent, name) {
                        Ok(child) => child,
                        Err(e) if fs::is_missing(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    ensure!(
                        fs::identity(&child)? == *identity,
                        "folder child substituted during cleanup"
                    );
                    if child.metadata()?.is_dir() {
                        fs::remove_directory(&parent, name, &child)?;
                    } else {
                        fs::remove_file(&parent, name, &child)?;
                    }
                }
                fs::remove_directory(&directory.file, format!("{name}-tree"), &root)?;
            }
            Err(e) if fs::is_missing(&e) => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn folder_capture_skips_links_special_unreadable_changed_and_clutter() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = crate::test_support::scratch()?;
        let root = dir.path().join("Trip");
        std::fs::create_dir_all(root.join("empty/nested"))?;
        std::fs::write(root.join("ok"), b"ok")?;
        std::fs::write(root.join(".hidden"), b"hidden")?;
        for name in [".DS_Store", "Thumbs.db", "desktop.ini", "._data"] {
            std::fs::write(root.join(name), b"ignored")?;
        }
        symlink("ok", root.join("link"))?;
        symlink("ok", root.join("bad\\link"))?;
        symlink(dir.path(), root.join("outside"))?;
        std::fs::write(root.join("hard"), b"hard")?;
        std::fs::hard_link(root.join("hard"), root.join("hard2"))?;
        let _socket = std::os::unix::net::UnixListener::bind(root.join("socket"))?;
        let _bad_socket = std::os::unix::net::UnixListener::bind(root.join("bad\\socket"))?;
        std::fs::write(root.join("denied"), b"secret")?;
        std::fs::set_permissions(root.join("denied"), std::fs::Permissions::from_mode(0o0))?;
        std::fs::create_dir(root.join("no-access"))?;
        std::fs::write(root.join("no-access/unknown-count.txt"), b"hidden subtree")?;
        std::fs::set_permissions(root.join("no-access"), std::fs::Permissions::from_mode(0o0))?;
        let captured = capture(&root, pneu_proto::MAX_FILE_SIZE, &|| Ok(()), &|_, _, _| {});
        std::fs::set_permissions(
            root.join("no-access"),
            std::fs::Permissions::from_mode(0o700),
        )?;
        let captured = captured?;
        assert_eq!(captured.details.diagnostic_count, 1);
        assert!(captured.details.diagnostics[0].starts_with("no-access:"));
        assert_eq!(captured.details.items, 2);
        assert_eq!(captured.details.links, 3);
        assert_eq!(captured.details.special, 4);
        assert_eq!(captured.details.failed, 2);
        let manifest: FolderManifest = serde_json::from_slice(&captured.manifest)?;
        assert!(manifest.entries.iter().any(|e| e.path == "empty/nested"));
        assert!(manifest.entries.iter().any(|e| e.path == ".hidden"));
        std::fs::set_permissions(root.join("denied"), std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(root.join("changing"), vec![0; 512 * 1024])?;
        let once = std::sync::atomic::AtomicBool::new(false);
        let captured = capture(
            &root,
            pneu_proto::MAX_FILE_SIZE,
            &|| Ok(()),
            &|_, bytes, total| {
                if total > 0 && bytes > 6 && !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    std::fs::write(root.join("changing"), b"changed").unwrap();
                }
            },
        )?;
        assert!(captured.details.failed >= 1);
        assert!(captured.details.names.contains(&"changing".into()));
        Ok(())
    }
    #[test]
    fn folder_capture_inspected_entry_limit_has_a_readable_error() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        for i in 0..10_001 {
            File::create(dir.path().join(format!("file-{i}")))?;
        }
        let error = capture(
            dir.path(),
            pneu_proto::MAX_FILE_SIZE,
            &|| Ok(()),
            &|_, _, _| {},
        )
        .err()
        .context("entry limit")?;
        assert_eq!(
            error.to_string(),
            "Folder contains more than 10,000 entries"
        );
        Ok(())
    }
    #[test]
    fn folder_cache_age_starts_at_publication() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = Folder::open_private(&dir.path().join("cache"))?;
        let manifest = br#"{"entries":[]}"#;
        let begin = FileBegin {
            transfer_id: pneu_proto::Id::from_bytes([11; 16]),
            file_name: "Empty".into(),
            mime: "application/vnd.pneu.folder".into(),
            size: manifest.len() as u64,
            sha256: Hash::new(hex::encode(Sha256::digest(manifest)))?,
            kind: pneu_proto::FileKind::File,
            created_at: 0,
            source_media_id: None,
            hlc: None,
            source_device: None,
            sensitive: false,
            resume: Some(true),
            folder: Some(Box::new(pneu_proto::Folder {
                manifest_len: manifest.len() as u64,
            })),
        };
        let name = crate::storage::cache_basename("Empty");
        let stage = Stage::create(
            folder.clone(),
            crate::storage::staging_directory(&folder)?,
            name.clone(),
            begin.size,
        )?;
        stage.folder_write(&begin, 0, manifest)?;
        {
            let tree = stage.0.tree.lock().unwrap();
            let tree = tree.as_ref().unwrap();
            tree.verify(&stage)?;
            tree.root.set_modified(std::time::SystemTime::UNIX_EPOCH)?;
        }
        stage.place()?;
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
        crate::storage::prune_cache(&folder, cutoff)?;
        assert!(folder.path()?.join(name).is_dir());
        Ok(())
    }
    #[test]
    fn folder_capture_empty_zero_files_and_stream_size_limit() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let root = dir.path().join("Trip");
        std::fs::create_dir_all(root.join("empty"))?;
        assert_eq!(
            capture(&root, pneu_proto::MAX_FILE_SIZE, &|| Ok(()), &|_, _, _| {})
                .err()
                .unwrap()
                .to_string(),
            "Trip is empty"
        );
        std::fs::write(root.join("zero"), b"")?;
        let frozen = capture(&root, pneu_proto::MAX_FILE_SIZE, &|| Ok(()), &|_, _, _| {})?;
        assert_eq!(frozen.details.items, 1);
        assert_eq!(
            capture(&root, frozen.size - 1, &|| Ok(()), &|_, _, _| {})
                .err()
                .unwrap()
                .to_string(),
            format!("Trip is over {}", size_label(frozen.size - 1))
        );
        let exact = capture(&root, frozen.size, &|| Ok(()), &|_, _, _| {})?;
        assert_eq!(exact.size, frozen.size);
        std::fs::write(root.join("large"), vec![0; 1024])?;
        assert_eq!(
            capture(&root, 1000, &|| Ok(()), &|_, _, _| {})
                .err()
                .unwrap()
                .to_string(),
            "Trip is over 1 KB"
        );
        Ok(())
    }
    #[test]
    fn folder_capture_refuses_oversized_before_reading_and_skips_invalid_names() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let dir = crate::test_support::scratch()?;
        let root = dir.path().join("Trip");
        std::fs::create_dir(&root)?;
        let file = File::create(root.join("huge"))?;
        file.set_len(1_000_001)?;
        let reading = std::cell::Cell::new(false);
        let error = capture(&root, 1_000_000, &|| Ok(()), &|_, _, total| {
            if total != 0 {
                reading.set(true);
            }
        })
        .err()
        .context("early size refusal")?;
        assert_eq!(error.to_string(), "Trip is over 1 MB");
        assert!(!reading.get(), "no hash pass before size refusal");
        std::fs::remove_file(root.join("huge"))?;
        std::fs::write(root.join("ok"), b"ok")?;
        std::fs::write(root.join("bad\\name"), b"x")?;
        std::fs::write(root.join("X:"), b"x")?;
        // APFS refuses non-Unicode names before enumeration; Linux filesystems
        // allow them, so exercise the capture rejection wherever creation works.
        let non_unicode =
            match std::fs::write(root.join(std::ffi::OsString::from_vec(vec![255])), b"x") {
                Ok(()) => true,
                Err(error)
                    if cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EILSEQ) =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
        std::fs::create_dir(root.join("bad\\subtree"))?;
        std::fs::write(root.join("bad\\subtree/child"), b"x")?;
        let captured = capture(&root, 1000, &|| Ok(()), &|_, _, _| {})?;
        assert_eq!(captured.details.items, 1);
        assert_eq!(captured.details.failed, 3 + u64::from(non_unicode));
        assert_eq!(
            captured
                .details
                .diagnostics
                .iter()
                .any(|n| n.starts_with("Non-Unicode name:")),
            non_unicode
        );
        let parsed: FolderManifest = serde_json::from_slice(&captured.manifest)?;
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].path, "ok");
        Ok(())
    }
    #[test]
    fn folder_capture_allows_metadata_only_changes() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::scratch()?;
        std::fs::write(dir.path().join("a"), b"abc")?;
        let captured = capture(dir.path(), 1000, &|| Ok(()), &|_, _, _| {})?;
        std::fs::set_permissions(dir.path().join("a"), std::fs::Permissions::from_mode(0o640))?;
        let root = Folder::read(dir.path())?;
        assert!(open_source(&root.file, "a", &captured.sources.files[0]).is_ok());
        Ok(())
    }
    #[test]
    fn folder_source_streams_in_place_and_rejects_unverified_content() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let root = dir.path().join("Trip");
        std::fs::create_dir_all(root.join("docs"))?;
        let big: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(root.join("a.bin"), &big)?;
        std::fs::write(root.join("docs/b.txt"), b"bee")?;
        std::fs::write(root.join("zero"), b"")?;
        let captured = capture(&root, pneu_proto::MAX_FILE_SIZE, &|| Ok(()), &|_, _, _| {})?;
        let begin = |manifest: &[u8], sha256: Hash| FileBegin {
            transfer_id: pneu_proto::Id::from_bytes([7; 16]),
            file_name: "Trip".into(),
            mime: "application/vnd.pneu.folder".into(),
            size: captured.size,
            sha256,
            kind: pneu_proto::FileKind::File,
            created_at: 0,
            source_media_id: None,
            hlc: None,
            source_device: None,
            sensitive: false,
            resume: Some(true),
            folder: Some(Box::new(pneu_proto::Folder {
                manifest_len: manifest.len() as u64,
            })),
        };
        let read_all = |source: &mut FolderSource, offset: u64| -> Result<Vec<u8>> {
            source.seek(offset)?;
            let mut out = Vec::new();
            let mut buffer = vec![0; 40_000];
            loop {
                let n = source.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buffer[..n]);
            }
            source.finish()?;
            Ok(out)
        };
        // The in-place stream is exactly what Preparing hashed, from any offset.
        let good = begin(&captured.manifest, captured.sha256.clone());
        let mut stream = captured.manifest.clone();
        stream.extend_from_slice(&big);
        stream.extend_from_slice(b"bee");
        assert_eq!(
            hex::encode(Sha256::digest(&stream)),
            captured.sha256.as_str()
        );
        let m = captured.manifest.len() as u64;
        for offset in [
            0,
            1,
            m,
            m + 1,
            m + 299_999,
            m + 300_000,
            m + 300_002,
            captured.size,
        ] {
            let mut source =
                FolderSource::open(&root, captured.manifest.clone(), &good, &captured.sources)?;
            assert_eq!(read_all(&mut source, offset)?, stream[offset as usize..]);
        }
        // Content the metadata check can't see still fails: a manifest hash
        // that the file doesn't match is caught inline and, for a file a
        // resume enters midway, the receiver checks the retained prefix and suffix.
        let mut manifest: FolderManifest = serde_json::from_slice(&captured.manifest)?;
        for entry in &mut manifest.entries {
            if entry.path == "a.bin" {
                entry.sha256 = Some(Hash::new(hex::encode(Sha256::digest(b"other")))?);
            }
        }
        let tampered = serde_json::to_vec(&manifest)?;
        assert_eq!(tampered.len(), captured.manifest.len());
        let mut sources = captured.sources.clone();
        sources.manifest_sha256 = hex::encode(Sha256::digest(&tampered));
        let bad = begin(&tampered, captured.sha256.clone());
        {
            let offset = 0;
            let mut source = FolderSource::open(&root, tampered.clone(), &bad, &sources)?;
            let error = read_all(&mut source, offset)
                .err()
                .context("tampered hash")?;
            assert!(error.downcast_ref::<SourceChanged>().is_some(), "{error:#}");
        }
        // Resume landing mid-file must not reread a potentially very slow
        // prefix. Even a bad full-file digest cannot block this suffix; the
        // receiver requests offset zero, where the inline check fails above.
        let mut source = FolderSource::open(&root, tampered.clone(), &bad, &sources)?;
        source.seek(m + 10)?;
        source.read_delay = std::time::Duration::from_millis(1);
        let mut suffix = Vec::new();
        let mut buffer = [0; 8192];
        let started = std::time::Instant::now();
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            suffix.extend_from_slice(&buffer[..n]);
        }
        source.finish()?;
        assert_eq!(suffix, stream[(m + 10) as usize..]);
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
        // A changed file fails before BEGIN (metadata), and a stale manifest
        // spool is refused outright.
        std::fs::write(root.join("docs/b.txt"), b"BEE")?;
        let error = FolderSource::open(&root, captured.manifest.clone(), &good, &captured.sources)
            .err()
            .context("changed source")?;
        assert!(error.downcast_ref::<SourceChanged>().is_some(), "{error:#}");
        assert!(FolderSource::open(&root, tampered, &good, &captured.sources).is_err());
        Ok(())
    }
    #[test]
    fn folder_numbering_preserves_extension_and_scalar_boundaries() {
        assert_eq!(numbered("photo.jpg", 2, false), "photo (2).jpg");
        assert_eq!(numbered("Docs.old", 2, true), "Docs.old (2)");
        let long = format!("{}.txt", "é".repeat(125));
        let mapped = numbered(&long, 10_000, false);
        assert!(mapped.len() <= 255);
        assert!(mapped.ends_with(" (10000).txt"));
        assert_eq!(items(1204), "1,204 items");
        assert_eq!(items(1), "1 item");
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    #[test]
    fn folder_unrepresentable_destination_name_is_unsupported_kind() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = Folder::open_private(&dir.path().join("tree"))?;
        let manifest = FolderManifest {
            entries: vec![FolderEntry {
                path: "ok".into(),
                entry_type: FolderEntryType::Directory,
                size: None,
                sha256: None,
            }],
        };
        let error =
            mapping(&folder.file, &manifest, &|_, _| "x".repeat(256), &|| Ok(())).unwrap_err();
        assert_eq!(
            crate::store::save_failure_code(&error),
            NackCode::UnsupportedKind
        );
        Ok(())
    }
    #[test]
    fn folder_mapped_collisions_suffix_once_and_descendants_follow_parent() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = Folder::open_private(&dir.path().join("tree"))?;
        let empty = Hash::new(hex::encode(Sha256::digest(b"")))?;
        let manifest = FolderManifest {
            entries: vec![
                FolderEntry {
                    path: "a:b.txt".into(),
                    entry_type: FolderEntryType::File,
                    size: Some(0),
                    sha256: Some(empty.clone()),
                },
                FolderEntry {
                    path: "a_b.txt".into(),
                    entry_type: FolderEntryType::File,
                    size: Some(0),
                    sha256: Some(empty.clone()),
                },
                FolderEntry {
                    path: "a_b (2).txt".into(),
                    entry_type: FolderEntryType::File,
                    size: Some(0),
                    sha256: Some(empty),
                },
                FolderEntry {
                    path: "Docs?".into(),
                    entry_type: FolderEntryType::Directory,
                    size: None,
                    sha256: None,
                },
                FolderEntry {
                    path: "Docs_".into(),
                    entry_type: FolderEntryType::Directory,
                    size: None,
                    sha256: None,
                },
                FolderEntry {
                    path: "Docs_/empty".into(),
                    entry_type: FolderEntryType::Directory,
                    size: None,
                    sha256: None,
                },
            ],
        };
        let (paths, _) = mapping(
            &folder.file,
            &manifest,
            &crate::storage::windows_names::map,
            &|| Ok(()),
        )?;
        assert_eq!(
            paths,
            vec![
                "a_b.txt",
                "a_b (2).txt",
                "a_b (2) (2).txt",
                "Docs_",
                "Docs_ (2)",
                "Docs_ (2)/empty"
            ]
        );
        Ok(())
    }
    #[cfg(windows)]
    #[test]
    fn folder_case_insensitive_destination_reserves_names_without_merging() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let folder = Folder::open_private(&dir.path().join("tree"))?;
        let manifest = FolderManifest {
            entries: ["A", "A/empty", "a", "a/empty"]
                .iter()
                .map(|p| FolderEntry {
                    path: (*p).into(),
                    entry_type: FolderEntryType::Directory,
                    size: None,
                    sha256: None,
                })
                .collect(),
        };
        assert_eq!(
            mapping(
                &folder.file,
                &manifest,
                &crate::storage::local_name,
                &|| Ok(())
            )?
            .0,
            vec!["A", "A/empty", "a (2)", "a (2)/empty"]
        );
        Ok(())
    }
}
