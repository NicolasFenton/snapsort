use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// Camera raws are classified from the JPEG they embed; see `raw`.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "cr2", "cr3", "nef", "arw", "raf", "dng", "rw2",
];

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Collect supported image files under `root`, in a stable (sorted) order.
///
/// Hidden files and directories are skipped (this also skips macOS `._*`
/// AppleDouble files, which carry image extensions but are not images), as are
/// symlinks and any directory listed in `exclude`, so that destination folders
/// nested inside the input are never re-scanned.
pub fn scan(root: &Path, recursive: bool, exclude: &[PathBuf]) -> ScanResult {
    let max_depth = if recursive { usize::MAX } else { 1 };
    let walker = WalkDir::new(root)
        .min_depth(1)
        .max_depth(max_depth)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| !is_hidden(entry) && !exclude.iter().any(|x| x == entry.path()));

    let mut result = ScanResult::default();
    for entry in walker {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_supported(entry.path()) => {
                result.files.push(entry.into_path());
            }
            Ok(_) => {}
            Err(err) => result.warnings.push(err.to_string()),
        }
    }
    result
}

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|s| ext.eq_ignore_ascii_case(s))
        })
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn names(result: &ScanResult, root: &Path) -> Vec<String> {
        result
            .files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn filters_extensions_hidden_files_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for f in [
            "b.JPG",
            "a.png",
            "c.webp",
            "d.jpeg",
            "e.CR3",
            "f.nef",
            "notes.txt",
            "photo.xmp",
            ".hidden.jpg",
            "._a.png",
            "sub/e.jpg",
        ] {
            touch(&root.join(f));
        }
        assert_eq!(
            names(&scan(root, false, &[]), root),
            ["a.png", "b.JPG", "c.webp", "d.jpeg", "e.CR3", "f.nef"]
        );
        assert_eq!(
            names(&scan(root, true, &[]), root),
            [
                "a.png",
                "b.JPG",
                "c.webp",
                "d.jpeg",
                "e.CR3",
                "f.nef",
                "sub/e.jpg"
            ]
        );
    }

    #[test]
    fn skips_excluded_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("new.jpg"));
        touch(&root.join("sorted/cats/old.jpg"));
        touch(&root.join("sorted/unrelated/keep.jpg"));
        let result = scan(root, true, &[root.join("sorted/cats")]);
        assert_eq!(
            names(&result, root),
            ["new.jpg", "sorted/unrelated/keep.jpg"]
        );
    }
}
