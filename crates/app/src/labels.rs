//! Categories and tags: how torrents are organized.
//!
//! A torrent has at most one category and any number of tags. Both also
//! exist on their own, so they can be made before any torrent uses them.
//! They are saved in a small text file of their own, beside the session:
//!
//! ```text
//! EmpireBT labels 1
//! category<TAB>Linux<TAB><save folder, optional>
//! tag<TAB>favorite
//! torrent<TAB><info hash in hex><TAB><category, or empty><TAB><tag>...
//! ```
//!
//! Names never contain tabs, line breaks, or other control characters,
//! so the file needs no escaping.

use crate::InfoHash;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const HEADER: &str = "EmpireBT labels 1";
/// Longest category or tag name, in characters.
pub const MAX_NAME: usize = 64;

/// One torrent's category and tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Assigned {
    pub category: Option<String>,
    pub tags: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Labels {
    pub categories: BTreeSet<String>,
    pub tags: BTreeSet<String>,
    /// The folder each category saves new torrents in, when it has one.
    pub folders: BTreeMap<String, PathBuf>,
    torrents: HashMap<InfoHash, Assigned>,
}

impl Labels {
    pub fn of(&self, hash: &InfoHash) -> Option<&Assigned> {
        self.torrents.get(hash)
    }

    /// Sets a torrent's category; `None` removes it. A new category is
    /// created.
    pub fn set_category(&mut self, hash: InfoHash, category: Option<String>) {
        if let Some(category) = &category {
            self.categories.insert(category.clone());
        }
        self.torrents.entry(hash).or_default().category = category;
        self.forget_empty(&hash);
    }

    /// Replaces a torrent's tags. New tags are created.
    pub fn set_tags(&mut self, hash: InfoHash, tags: BTreeSet<String>) {
        self.tags.extend(tags.iter().cloned());
        self.torrents.entry(hash).or_default().tags = tags;
        self.forget_empty(&hash);
    }

    /// Gives a category, made if new, the folder its new torrents are
    /// saved in; `None` takes the folder away.
    pub fn set_folder(&mut self, category: String, folder: Option<PathBuf>) {
        match folder {
            Some(folder) => self.folders.insert(category.clone(), folder),
            None => self.folders.remove(&category),
        };
        self.categories.insert(category);
    }

    /// Removes a category; torrents in it are left without one.
    pub fn remove_category(&mut self, name: &str) -> bool {
        let removed = self.categories.remove(name);
        self.folders.remove(name);
        for assigned in self.torrents.values_mut() {
            if assigned.category.as_deref() == Some(name) {
                assigned.category = None;
            }
        }
        self.torrents
            .retain(|_, assigned| *assigned != Assigned::default());
        removed
    }

    /// Removes a tag from the list and from every torrent.
    pub fn remove_tag(&mut self, name: &str) -> bool {
        let removed = self.tags.remove(name);
        for assigned in self.torrents.values_mut() {
            assigned.tags.remove(name);
        }
        self.torrents
            .retain(|_, assigned| *assigned != Assigned::default());
        removed
    }

    /// Forgets a removed torrent.
    pub fn forget(&mut self, hash: &InfoHash) -> bool {
        self.torrents.remove(hash).is_some()
    }

    /// Follows a torrent whose identity changed, such as a v2 magnet link
    /// once its metadata arrived.
    pub fn rename(&mut self, from: &InfoHash, to: InfoHash) -> bool {
        match self.torrents.remove(from) {
            Some(assigned) => {
                self.torrents.insert(to, assigned);
                true
            }
            None => false,
        }
    }

    fn forget_empty(&mut self, hash: &InfoHash) {
        if self.torrents.get(hash) == Some(&Assigned::default()) {
            self.torrents.remove(hash);
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!("{} is not a labels file", path.display()));
        }
        let mut labels = Self::default();
        for line in lines {
            let mut fields = line.split('\t');
            let valid = |name: &str| name_error(name).is_none().then(|| name.to_owned());
            match fields.next() {
                Some("category") => {
                    let Some(name) = fields.next().and_then(valid) else {
                        continue;
                    };
                    if let Some(folder) = fields.next().and_then(folder_from_text) {
                        labels.folders.insert(name.clone(), folder);
                    }
                    labels.categories.insert(name);
                }
                Some("tag") => labels.tags.extend(fields.next().and_then(valid)),
                Some("torrent") => {
                    let Some(hash) = fields.next().and_then(unhex) else {
                        continue;
                    };
                    let category = fields.next().filter(|name| !name.is_empty());
                    let assigned = Assigned {
                        category: category.and_then(valid),
                        tags: fields.filter_map(valid).collect(),
                    };
                    labels.categories.extend(assigned.category.clone());
                    labels.tags.extend(assigned.tags.iter().cloned());
                    if assigned != Assigned::default() {
                        labels.torrents.insert(hash, assigned);
                    }
                }
                // Lines from a later version are skipped.
                _ => {}
            }
        }
        Ok(labels)
    }

    /// Writes the file atomically, so a crash never leaves it truncated.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = format!("{HEADER}\n");
        for category in &self.categories {
            match self
                .folders
                .get(category)
                .and_then(|folder| folder.to_str())
            {
                Some(folder) => text.push_str(&format!("category\t{category}\t{folder}\n")),
                None => text.push_str(&format!("category\t{category}\n")),
            }
        }
        for tag in &self.tags {
            text.push_str(&format!("tag\t{tag}\n"));
        }
        let mut torrents: Vec<_> = self.torrents.iter().collect();
        torrents.sort_by_key(|(hash, _)| **hash);
        for (hash, assigned) in torrents {
            text.push_str(&format!(
                "torrent\t{}\t{}",
                hex(hash),
                assigned.category.as_deref().unwrap_or("")
            ));
            for tag in &assigned.tags {
                text.push_str(&format!("\t{tag}"));
            }
            text.push('\n');
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| format!("cannot save {}: {error}", path.display()))
    }
}

/// A category or tag name as typed, cleaned up: surrounding spaces
/// trimmed. An error when it is empty, too long, or contains control
/// characters.
pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    match name_error(name) {
        Some(error) => Err(error),
        None => Ok(name.to_owned()),
    }
}

/// A category folder as saved: a full path without control characters.
fn folder_from_text(text: &str) -> Option<PathBuf> {
    let folder = PathBuf::from(text);
    (folder.is_absolute() && !text.chars().any(char::is_control)).then_some(folder)
}

/// Checks a category folder before it is saved: a full path whose name is
/// text without control characters (the file separates fields by tabs).
pub fn check_folder(folder: &Path) -> Result<(), String> {
    match folder.to_str() {
        Some(text) if folder_from_text(text).is_some() => Ok(()),
        Some(_) if !folder.is_absolute() => Err(format!(
            "a category's folder must be a full path, such as /media/films, not {}",
            folder.display()
        )),
        _ => Err("a category's folder cannot contain tabs or control characters".to_owned()),
    }
}

fn name_error(name: &str) -> Option<String> {
    if name.is_empty() {
        Some("a name cannot be empty".to_owned())
    } else if name.chars().count() > MAX_NAME {
        Some(format!("names are at most {MAX_NAME} characters"))
    } else if name.chars().any(char::is_control) {
        Some("names cannot contain tabs, line breaks, or control characters".to_owned())
    } else if name != name.trim() {
        Some("names cannot start or end with spaces".to_owned())
    } else {
        None
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<InfoHash> {
    if text.len() != 40 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0; 20];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn assigns_categories_and_tags() {
        let mut labels = Labels::default();
        labels.set_category([1; 20], Some("Linux".to_owned()));
        labels.set_tags([1; 20], tags(&["iso", "favorite"]));
        labels.set_tags([2; 20], tags(&["iso"]));
        assert_eq!(
            labels.of(&[1; 20]).unwrap().category.as_deref(),
            Some("Linux")
        );
        assert_eq!(labels.categories, tags(&["Linux"]));
        assert_eq!(labels.tags, tags(&["favorite", "iso"]));

        // Removing a tag takes it off every torrent; a torrent left with
        // nothing is forgotten.
        assert!(labels.remove_tag("iso"));
        assert_eq!(labels.of(&[1; 20]).unwrap().tags, tags(&["favorite"]));
        assert!(labels.of(&[2; 20]).is_none());
        assert!(labels.remove_category("Linux"));
        assert_eq!(labels.of(&[1; 20]).unwrap().category, None);

        assert!(labels.rename(&[1; 20], [3; 20]));
        assert!(labels.of(&[3; 20]).is_some());
        assert!(labels.forget(&[3; 20]));
        assert!(labels.of(&[3; 20]).is_none());
    }

    /// Category folders are saved with the category, and a file without
    /// them (from before) still loads.
    #[test]
    fn categories_keep_their_folders() {
        let dir = std::env::temp_dir().join(format!("empirebt-folders-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("labels");
        let mut labels = Labels::default();
        labels.set_folder("Films".into(), Some("/media/films".into()));
        labels.categories.insert("Linux".into());
        labels.save(&path).unwrap();
        let loaded = Labels::load(&path).unwrap();
        assert_eq!(loaded, labels);
        assert_eq!(
            loaded.folders.get("Films"),
            Some(&PathBuf::from("/media/films"))
        );
        assert!(!loaded.folders.contains_key("Linux"));

        std::fs::write(
            &path,
            "EmpireBT labels 1\ncategory\tOld\ncategory\tBad\trelative\n",
        )
        .unwrap();
        let loaded = Labels::load(&path).unwrap();
        assert!(loaded.categories.contains("Old") && loaded.categories.contains("Bad"));
        assert!(loaded.folders.is_empty());

        labels.remove_category("Films");
        assert!(labels.folders.is_empty());
        assert!(check_folder(Path::new("relative")).is_err());
        assert!(check_folder(Path::new("/media/a\tb")).is_err());
        assert!(check_folder(Path::new("/media/films")).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saves_and_loads_labels() {
        let path = std::env::temp_dir().join(format!("empirebt-labels-{}.txt", std::process::id()));
        let mut labels = Labels::default();
        labels.categories.insert("Empty category".to_owned());
        labels.tags.insert("unused".to_owned());
        labels.set_category([1; 20], Some("Linux ISOs".to_owned()));
        labels.set_tags([1; 20], tags(&["4k", "favorite ★"]));
        labels.set_tags([2; 20], tags(&["4k"]));
        labels.save(&path).unwrap();
        assert_eq!(Labels::load(&path).unwrap(), labels);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            Labels::load(&path).unwrap(),
            Labels::default(),
            "missing file"
        );
    }

    #[test]
    fn skips_what_it_cannot_read_in_a_labels_file() {
        let path =
            std::env::temp_dir().join(format!("empirebt-labels-bad-{}.txt", std::process::id()));
        let hash = "ab".repeat(20);
        std::fs::write(
            &path,
            format!("{HEADER}\ntorrent\tnot-a-hash\tX\nfuture\tthing\ntorrent\t{hash}\t\tok\n"),
        )
        .unwrap();
        let labels = Labels::load(&path).unwrap();
        assert_eq!(labels.of(&[0xab; 20]).unwrap().tags, tags(&["ok"]));
        assert_eq!(labels.categories, BTreeSet::new());
        std::fs::write(&path, "something else\n").unwrap();
        assert!(Labels::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn names_are_trimmed_and_checked() {
        assert_eq!(clean_name("  Movies ").unwrap(), "Movies");
        assert!(clean_name("   ").is_err());
        assert!(clean_name("a\tb").is_err());
        assert!(clean_name("line\nbreak").is_err());
        assert!(clean_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert_eq!(
            clean_name(&"é".repeat(MAX_NAME)).unwrap().chars().count(),
            MAX_NAME
        );
    }
}
