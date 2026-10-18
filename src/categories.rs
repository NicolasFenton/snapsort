use std::{collections::HashSet, fmt, fs, path::Path};

use anyhow::{bail, Context, Result};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

/// Folder that receives images the model was not confident about.
pub const NEEDS_REVIEW: &str = "needs_review";

/// Folder that receives blurry, badly exposed or accidental photos when the
/// unusable check is on.
pub const UNUSABLE: &str = "unusable";

/// SubQ choice questions accept between 2 and 255 options.
pub const MIN_CATEGORIES: usize = 2;
pub const MAX_CATEGORIES: usize = 255;

/// Most filesystems cap a single path component at 255 bytes.
const MAX_NAME_BYTES: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub name: String,
    pub description: Option<String>,
}

/// Validated, ordered list of target folders.
#[derive(Debug, Clone)]
pub struct Categories {
    items: Vec<Category>,
}

impl Categories {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read categories file {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("invalid categories file {}", path.display()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let RawEntries(entries) = serde_json::from_str(raw)?;

        if !(MIN_CATEGORIES..=MAX_CATEGORIES).contains(&entries.len()) {
            bail!(
                "found {} categories; SubQ needs between {MIN_CATEGORIES} and {MAX_CATEGORIES}",
                entries.len()
            );
        }

        let mut seen = HashSet::new();
        let mut items = Vec::with_capacity(entries.len());
        for (name, description) in entries {
            validate_name(&name)
                .map_err(|reason| anyhow::anyhow!("category {name:?}: {reason}"))?;
            // Compare case-insensitively: macOS and Windows filesystems would
            // merge "Cats" and "cats" into one folder.
            let folded = name.to_lowercase();
            if folded == NEEDS_REVIEW {
                bail!("category {name:?} is reserved for low-confidence images");
            }
            if !seen.insert(folded) {
                bail!("category {name:?} is listed more than once (names are case-insensitive)");
            }
            let description = description
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.is_empty());
            items.push(Category { name, description });
        }
        Ok(Self { items })
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Category> {
        self.items.iter()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.items.iter().any(|c| c.name == name)
    }

    /// The category whose folder would be `name`, ignoring case (as macOS
    /// and Windows filesystems do).
    pub fn find_folder(&self, name: &str) -> Option<&Category> {
        let folded = name.to_lowercase();
        self.items.iter().find(|c| c.name.to_lowercase() == folded)
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("folder name is empty".into());
    }
    if name.trim() != name {
        return Err("folder name has leading or trailing whitespace".into());
    }
    if name == "." || name == ".." {
        return Err("folder name cannot be `.` or `..`".into());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("folder name is longer than {MAX_NAME_BYTES} bytes"));
    }
    if let Some(c) = name
        .chars()
        .find(|&c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(format!("folder name contains invalid character {c:?}"));
    }
    if cfg!(windows) {
        validate_windows_name(name)?;
    }
    Ok(())
}

fn validate_windows_name(name: &str) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if let Some(c) = name.chars().find(|c| r#"<>:"|?*"#.contains(*c)) {
        return Err(format!(
            "folder name contains character {c:?}, which Windows does not allow"
        ));
    }
    if name.ends_with('.') {
        return Err("folder name cannot end with `.` on Windows".into());
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Err(format!("{stem} is a reserved device name on Windows"));
    }
    Ok(())
}

/// JSON object entries in file order. A plain map would silently drop
/// duplicate keys and lose the user's ordering.
struct RawEntries(Vec<(String, Option<String>)>);

impl<'de> Deserialize<'de> for RawEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = RawEntries;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object mapping folder names to descriptions (string or null)")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawEntries, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some(entry) = map.next_entry::<String, Option<String>>()? {
                    entries.push(entry);
                }
                Ok(RawEntries(entries))
            }
        }

        deserializer.deserialize_map(EntriesVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_in_file_order_with_null_descriptions() {
        let cats =
            Categories::parse(r#"{"zebra": "Striped", "apple": null, "mango": "  "}"#).unwrap();
        let names: Vec<_> = cats.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["zebra", "apple", "mango"]);
        assert_eq!(
            cats.iter().next().unwrap().description.as_deref(),
            Some("Striped")
        );
        assert!(cats.iter().skip(1).all(|c| c.description.is_none()));
    }

    #[test]
    fn enforces_category_count() {
        assert!(Categories::parse(r#"{"only": null}"#).is_err());
        let many: Vec<String> = (0..256).map(|i| format!("\"c{i}\": null")).collect();
        assert!(Categories::parse(&format!("{{{}}}", many.join(","))).is_err());
        let max: Vec<String> = (0..255).map(|i| format!("\"c{i}\": null")).collect();
        assert_eq!(
            Categories::parse(&format!("{{{}}}", max.join(",")))
                .unwrap()
                .len(),
            255
        );
    }

    #[test]
    fn rejects_duplicates_and_reserved_names() {
        assert!(Categories::parse(r#"{"cats": null, "cats": "again"}"#).is_err());
        assert!(Categories::parse(r#"{"Cats": null, "cats": null}"#).is_err());
        assert!(Categories::parse(r#"{"needs_review": null, "cats": null}"#).is_err());
    }

    #[test]
    fn finds_folders_case_insensitively() {
        let cats = Categories::parse(r#"{"Unusable": null, "cats": null}"#).unwrap();
        assert_eq!(cats.find_folder(UNUSABLE).unwrap().name, "Unusable");
        assert!(cats.find_folder("dogs").is_none());
    }

    #[test]
    fn rejects_unsafe_folder_names() {
        for bad in ["", " padded", "a/b", "a\\b", "..", ".", "tab\there"] {
            let json = serde_json::json!({ bad: null, "ok": null }).to_string();
            assert!(
                Categories::parse(&json).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_non_string_descriptions() {
        assert!(Categories::parse(r#"{"a": 1, "b": null}"#).is_err());
        assert!(Categories::parse(r#"["a", "b"]"#).is_err());
    }

    #[test]
    fn windows_rules() {
        assert!(validate_windows_name("CON").is_err());
        assert!(validate_windows_name("com1.txt").is_err());
        assert!(validate_windows_name("a:b").is_err());
        assert!(validate_windows_name("trailing.").is_err());
        assert!(validate_windows_name("console").is_ok());
    }
}
