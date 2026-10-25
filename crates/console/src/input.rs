//! A single-line path input with shell-style Tab completion.

use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct PathInput {
    text: String,
    /// Cursor position as a character index.
    cursor: usize,
    /// Candidates shown after an ambiguous Tab completion.
    candidates: Vec<String>,
}

impl PathInput {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    pub fn insert(&mut self, character: char) {
        let at = self.byte_index(self.cursor);
        self.text.insert(at, character);
        self.cursor += 1;
        self.candidates.clear();
    }

    /// Inserts pasted text, dropping newlines and the quotes terminals add
    /// around dragged-in file paths.
    pub fn paste(&mut self, text: &str) {
        let cleaned = text.trim().trim_matches(['\'', '"']);
        for character in cleaned.chars().filter(|character| !character.is_control()) {
            self.insert(character);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_index(self.cursor);
        self.text.remove(at);
        self.candidates.clear();
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let at = self.byte_index(self.cursor);
            self.text.remove(at);
            self.candidates.clear();
        }
    }

    /// Deletes the word before the cursor, stopping at path separators.
    pub fn delete_word(&mut self) {
        let before: Vec<char> = self.text.chars().take(self.cursor).collect();
        let mut start = before.len();
        while start > 0 && is_separator(before[start - 1]) {
            start -= 1;
        }
        while start > 0 && !is_separator(before[start - 1]) {
            start -= 1;
        }
        let (from, to) = (self.byte_index(start), self.byte_index(self.cursor));
        self.text.replace_range(from..to, "");
        self.cursor = start;
        self.candidates.clear();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    /// Completes the path to the longest unambiguous prefix among matching
    /// directories and `.torrent` files, listing candidates when several
    /// remain.
    pub fn complete(&mut self) {
        self.end();
        let typed = self.text.clone();
        let (directory_text, prefix) = match typed.rfind(is_separator) {
            Some(at) => (&typed[..=at], &typed[at + 1..]),
            None => ("", typed.as_str()),
        };
        let directory = if directory_text.is_empty() {
            PathBuf::from(".")
        } else {
            expand_home(directory_text)
        };
        let Ok(entries) = std::fs::read_dir(&directory) else {
            self.candidates.clear();
            return;
        };
        let mut matches: Vec<(String, bool)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let is_dir = entry.path().is_dir();
                let wanted = is_dir
                    || Path::new(&name)
                        .extension()
                        .is_some_and(|ext| ext == "torrent");
                let hidden = name.starts_with('.') && !prefix.starts_with('.');
                (name.starts_with(prefix) && wanted && !hidden).then_some((name, is_dir))
            })
            .collect();
        matches.sort();

        match matches.as_slice() {
            [] => self.candidates.clear(),
            [(name, is_dir)] => {
                let suffix = if *is_dir {
                    std::path::MAIN_SEPARATOR_STR
                } else {
                    ""
                };
                self.text = format!("{directory_text}{name}{suffix}");
                self.candidates.clear();
            }
            _ => {
                let common = matches
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .reduce(common_prefix)
                    .unwrap_or(prefix);
                self.text = format!("{directory_text}{common}");
                self.candidates = matches
                    .into_iter()
                    .map(|(name, is_dir)| if is_dir { format!("{name}/") } else { name })
                    .collect();
            }
        }
        self.end();
    }

    fn byte_index(&self, character_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(character_index)
            .map_or(self.text.len(), |(index, _)| index)
    }
}

fn is_separator(character: char) -> bool {
    character == '/' || (cfg!(windows) && character == '\\')
}

fn common_prefix<'a>(left: &'a str, right: &str) -> &'a str {
    let length = left
        .char_indices()
        .zip(right.chars())
        .take_while(|((_, a), b)| a == b)
        .last()
        .map_or(0, |((index, character), _)| index + character.len_utf8());
    &left[..length]
}

fn expand_home(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with(is_separator))
        && let Some(home) = empirebt_app::home_dir()
    {
        return home.join(rest.trim_start_matches(is_separator));
    }
    PathBuf::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> PathInput {
        let mut input = PathInput::default();
        input.paste(text);
        input
    }

    #[test]
    fn edits_at_the_cursor() {
        let mut input = typed("abd");
        input.left();
        input.insert('c');
        assert_eq!(input.text(), "abcd");
        input.home();
        input.delete();
        assert_eq!(input.text(), "bcd");
        input.end();
        input.backspace();
        assert_eq!(input.text(), "bc");
    }

    #[test]
    fn deletes_the_last_path_segment() {
        let mut input = typed("/home/user/file.torrent");
        input.delete_word();
        assert_eq!(input.text(), "/home/user/");
        input.delete_word();
        assert_eq!(input.text(), "/home/");
    }

    #[test]
    fn paste_strips_quotes_and_newlines() {
        assert_eq!(typed("'/tmp/a b.torrent'\n").text(), "/tmp/a b.torrent");
    }

    #[test]
    fn completes_unique_and_shared_prefixes() {
        let root = std::env::temp_dir().join(format!("empirebt-input-{}", std::process::id()));
        std::fs::create_dir_all(root.join("movies")).unwrap();
        std::fs::write(root.join("linux-a.torrent"), b"").unwrap();
        std::fs::write(root.join("linux-b.torrent"), b"").unwrap();
        std::fs::write(root.join("linux-notes.txt"), b"").unwrap();
        let base = format!("{}/", root.display());

        let mut input = typed(&format!("{base}mo"));
        input.complete();
        assert_eq!(input.text(), format!("{base}movies/"));

        let mut input = typed(&format!("{base}li"));
        input.complete();
        assert_eq!(input.text(), format!("{base}linux-"));
        assert_eq!(input.candidates(), ["linux-a.torrent", "linux-b.torrent"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn handles_multibyte_text() {
        let mut input = typed("été");
        input.backspace();
        input.left();
        input.insert('x');
        assert_eq!(input.text(), "éxt");
        assert_eq!(common_prefix("ébc", "ébd"), "éb");
    }
}
