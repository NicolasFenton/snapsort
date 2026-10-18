//! Moving/copying originals into category folders. All methods block, so call
//! them from `spawn_blocking`.

use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context, Result};

/// Give up looking for a free `name (N).ext` after this many collisions.
const MAX_SUFFIX: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Move,
    Copy,
}

pub struct Sorter {
    root: PathBuf,
    transfer: Transfer,
    dry_run: bool,
    /// Destinations picked by a dry run, which creates nothing on disk; real
    /// runs reserve names on disk instead (see `claim`).
    planned: Mutex<HashSet<PathBuf>>,
    created_dirs: Mutex<HashSet<PathBuf>>,
}

impl Sorter {
    pub fn new(root: PathBuf, transfer: Transfer, dry_run: bool) -> Self {
        Self {
            root,
            transfer,
            dry_run,
            planned: Mutex::default(),
            created_dirs: Mutex::default(),
        }
    }

    /// Put `source` into `<root>/<folder>/`, never overwriting an existing
    /// file: collisions become `name (1).ext`, `name (2).ext`, ... Returns the
    /// final destination. In dry-run mode only the destination is computed.
    pub fn place(&self, source: &Path, folder: &str) -> Result<PathBuf> {
        let dir = self.root.join(folder);
        let file_name = source.file_name().context("source path has no file name")?;
        if self.dry_run {
            return self.plan(&dir, file_name);
        }
        self.ensure_dir(&dir)?;
        let destination = claim(&dir, file_name)?;
        let result = match self.transfer {
            Transfer::Move => move_file(source, &destination),
            Transfer::Copy => copy_file(source, &destination),
        };
        if result.is_err() {
            release(&destination);
        }
        result.map(|()| destination)
    }

    fn ensure_dir(&self, dir: &Path) -> Result<()> {
        let mut created = lock(&self.created_dirs);
        if !created.contains(dir) {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            created.insert(dir.to_path_buf());
        }
        Ok(())
    }

    fn plan(&self, dir: &Path, file_name: &OsStr) -> Result<PathBuf> {
        let mut planned = lock(&self.planned);
        for n in 0..MAX_SUFFIX {
            let candidate = dir.join(numbered(file_name, n));
            if planned.contains(&candidate) {
                continue;
            }
            // symlink_metadata also catches dangling symlinks.
            match candidate.symlink_metadata() {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    planned.insert(candidate.clone());
                    return Ok(candidate);
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("cannot check {}", candidate.display()))
                }
            }
        }
        Err(no_free_name(dir, file_name))
    }
}

/// Reserve a free name in `dir` by creating an empty placeholder there. The
/// exclusive create is atomic, so other tasks, and other processes sorting
/// into the same folder, can never be handed the same name; the transfer then
/// renames over the placeholder, which is ours to replace.
fn claim(dir: &Path, file_name: &OsStr) -> Result<PathBuf> {
    for n in 0..MAX_SUFFIX {
        let candidate = dir.join(numbered(file_name, n));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            // Exclusive create also fails on dangling symlinks, and Windows
            // reports some existing entries (directories) as access denied.
            Err(err)
                if err.kind() == io::ErrorKind::AlreadyExists
                    || candidate.symlink_metadata().is_ok() => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to create {}", candidate.display()))
            }
        }
    }
    Err(no_free_name(dir, file_name))
}

/// Undo a claim after a failed transfer. Only an empty placeholder is
/// removed: a cross-device move that copied the image but couldn't delete the
/// original leaves a real file there, which must stay.
fn release(destination: &Path) {
    if destination
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() == 0)
    {
        let _ = fs::remove_file(destination);
    }
}

fn no_free_name(dir: &Path, file_name: &OsStr) -> anyhow::Error {
    anyhow::anyhow!(
        "no free file name for {} in {}",
        Path::new(file_name).display(),
        dir.display()
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The guarded sets stay consistent even if a holder panicked.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `photo.jpg` -> `photo (n).jpg` (n = 0 keeps the original name).
fn numbered(file_name: &OsStr, n: u32) -> OsString {
    if n == 0 {
        return file_name.to_owned();
    }
    let path = Path::new(file_name);
    let mut name = path.file_stem().unwrap_or(file_name).to_owned();
    name.push(format!(" ({n})"));
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    name
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        // Different filesystem: rename can't work, so copy then delete.
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_file(source, destination)?;
            fs::remove_file(source).with_context(|| {
                format!(
                    "copied to {} but could not remove the original",
                    destination.display()
                )
            })
        }
        Err(err) => {
            Err(err).with_context(|| format!("failed to move to {}", destination.display()))
        }
    }
}

/// Copy via a hidden temp file in the destination folder, then rename it into
/// place, so an interrupted copy never leaves a truncated image behind under
/// the real name. The modification time is preserved because photo libraries
/// often sort by it.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    let temp = temp_sibling(destination);
    let result = (|| -> io::Result<()> {
        let metadata = fs::metadata(source)?;
        let copied = fs::copy(source, &temp)?;
        if copied != metadata.len() {
            return Err(io::Error::other(format!(
                "short copy ({copied} of {} bytes)",
                metadata.len()
            )));
        }
        if let Ok(modified) = metadata.modified() {
            let file = fs::OpenOptions::new().write(true).open(&temp)?;
            // Best effort: some filesystems refuse timestamp changes.
            let _ = file.set_modified(modified);
        }
        fs::rename(&temp, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("failed to copy to {}", destination.display()))
}

fn temp_sibling(destination: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(destination.file_name().unwrap_or_default());
    name.push(format!(".snapsort-{}.tmp", std::process::id()));
    destination.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Barrier,
        time::{Duration, SystemTime},
    };

    #[test]
    fn numbering_keeps_extension() {
        assert_eq!(numbered(OsStr::new("a.jpg"), 0), "a.jpg");
        assert_eq!(numbered(OsStr::new("a.jpg"), 2), "a (2).jpg");
        assert_eq!(
            numbered(OsStr::new("archive.tar.png"), 1),
            "archive.tar (1).png"
        );
        assert_eq!(numbered(OsStr::new("noext"), 1), "noext (1)");
    }

    #[test]
    fn moves_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("cats")).unwrap();
        fs::write(out.join("cats/a.jpg"), b"existing").unwrap();
        let src = dir.path().join("a.jpg");
        fs::write(&src, b"new").unwrap();

        let sorter = Sorter::new(out.clone(), Transfer::Move, false);
        let dest = sorter.place(&src, "cats").unwrap();
        assert_eq!(dest, out.join("cats/a (1).jpg"));
        assert!(!src.exists());
        assert_eq!(fs::read(out.join("cats/a.jpg")).unwrap(), b"existing");
        assert_eq!(fs::read(dest).unwrap(), b"new");
    }

    #[test]
    fn copies_and_preserves_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.png");
        fs::write(&src, b"pixels").unwrap();
        let past = SystemTime::now() - Duration::from_secs(86_400 * 30);
        fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let sorter = Sorter::new(dir.path().join("out"), Transfer::Copy, false);
        let dest = sorter.place(&src, "dogs").unwrap();
        assert!(src.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"pixels");
        let mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        let drift = mtime.duration_since(past).unwrap_or_else(|e| e.duration());
        assert!(drift < Duration::from_secs(2), "mtime drifted by {drift:?}");
        let leftovers: Vec<_> = fs::read_dir(dest.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind");
    }

    #[test]
    fn concurrent_runs_into_one_folder_never_overwrite_each_other() {
        // Two Sorters stand in for two snapsort processes: neither sees the
        // other's in-memory state, so only the filesystem can stop them from
        // picking the same name. The barrier lines up each same-named pair.
        const FILES: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        for run in ["a", "b"] {
            fs::create_dir_all(dir.path().join(run)).unwrap();
            for i in 0..FILES {
                fs::write(
                    dir.path().join(run).join(format!("{i}.jpg")),
                    format!("{run}{i}"),
                )
                .unwrap();
            }
        }
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            for run in ["a", "b"] {
                let (input, out, barrier) = (dir.path().join(run), &out, &barrier);
                scope.spawn(move || {
                    let sorter = Sorter::new(out.clone(), Transfer::Move, false);
                    for i in 0..FILES {
                        barrier.wait();
                        // A failure shows up below as a missing file.
                        sorter.place(&input.join(format!("{i}.jpg")), "cats").ok();
                    }
                });
            }
        });

        let found: HashSet<String> = fs::read_dir(out.join("cats"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        let lost: Vec<String> = ["a", "b"]
            .iter()
            .flat_map(|run| (0..FILES).map(move |i| format!("{run}{i}")))
            .filter(|content| !found.contains(content))
            .collect();
        assert!(lost.is_empty(), "overwritten or not placed: {lost:?}");
    }

    #[test]
    fn failed_transfer_leaves_no_placeholder_behind() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let sorter = Sorter::new(out.clone(), Transfer::Move, false);
        sorter
            .place(&dir.path().join("vanished.jpg"), "cats")
            .unwrap_err();
        assert_eq!(fs::read_dir(out.join("cats")).unwrap().count(), 0);
    }

    #[test]
    fn dry_run_touches_nothing_but_still_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let sorter = Sorter::new(out.clone(), Transfer::Move, true);
        let a = sorter.place(Path::new("/in/x/same.jpg"), "cats").unwrap();
        let b = sorter.place(Path::new("/in/y/same.jpg"), "cats").unwrap();
        assert_eq!(a, out.join("cats/same.jpg"));
        assert_eq!(b, out.join("cats/same (1).jpg"));
        assert!(!out.exists());
    }
}
