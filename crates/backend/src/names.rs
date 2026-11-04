//! File and folder names from torrents, made safe to use as paths, exactly
//! as libtorrent makes them (RC_2_1 `torrent_info.cpp`,
//! `resolve_duplicate_filenames.cpp`, `utf8.cpp`).
//!
//! Torrents name their files in whatever encoding the maker's system used:
//! old ones often in a legacy code page, not UTF-8. None is refused for it.
//! Each name is cleaned one path element at a time: bytes that are not
//! UTF-8 and control characters become `_`; separators, NUL, and invisible
//! or direction-changing characters (a right-to-left override, U+202E, can
//! make a program's name look like a video's) are dropped; `.`, `..`, and
//! empty elements become `_`; an element longer than 240 bytes is cut,
//! keeping its extension. Files
//! whose cleaned paths then collide (ignoring ASCII case) are renamed
//! `name.1.ext`, `name.2.ext`, and so on.
//!
//! Doing it exactly as libtorrent means the files have the names qBittorrent
//! gave them on disk, which the qBittorrent importer relies on, and the
//! info hash is untouched: it is taken from the original bytes.
//!
//! Ported from libtorrent, Copyright (c) 2003-2022 Arvid Norberg and its
//! contributors, under the BSD 3-Clause license: see THIRD_PARTY_NOTICES.md.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// More folder levels than this in one file's path is refused, as
/// libtorrent's `max_directory_depth`.
pub(crate) const MAX_DIRECTORY_DEPTH: usize = 100;
/// More name collisions than this in one torrent is refused as malicious,
/// as libtorrent's `max_duplicate_filenames`.
const MAX_DUPLICATE_FILENAMES: usize = 500;

/// One code point from the start of `bytes`, `None` for an invalid
/// sequence, and how many bytes it took, as libtorrent's
/// `parse_utf8_codepoint`.
fn parse_utf8_codepoint(bytes: &[u8]) -> (Option<u32>, usize) {
    let first = bytes[0];
    let length = if first < 0b1000_0000 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else if first >> 2 == 0b11_1110 {
        // Not a valid prefix, but libtorrent skips that many bytes.
        5
    } else {
        0
    };
    match length {
        1 => return (Some(u32::from(first)), 1),
        0 => return (None, 1),
        length if length > bytes.len() => return (None, bytes.len()),
        5 => return (None, 5),
        _ => {}
    }
    let mut code_point = u32::from(match length {
        2 => first & 0b0001_1111,
        3 => first & 0b0000_1111,
        _ => first & 0b0000_0111,
    });
    for &byte in &bytes[1..length] {
        if !(0b1000_0000..=0b1011_1111).contains(&byte) {
            return (None, length);
        }
        code_point = (code_point << 6) + u32::from(byte & 0b11_1111);
    }
    let overlong = match length {
        2 => code_point < 0x80,
        3 => code_point < 0x800,
        _ => code_point < 0x1_0000,
    };
    if overlong || code_point > 0x10_ffff || (0xd800..=0xdfff).contains(&code_point) {
        return (None, length);
    }
    (Some(code_point), length)
}

/// Characters a file name may hold, as libtorrent's
/// `valid_path_character`; the rest become `_`.
fn valid_path_character(c: u32) -> bool {
    // On Windows, both the file system and the system itself refuse these.
    let invalid: &[u8] = if cfg!(windows) { b"?<>\"|\x08*:" } else { b"" };
    // C0 controls and DEL; C1 controls.
    if c < 32 || c == 0x7f || (0x80..=0x9f).contains(&c) {
        return false;
    }
    c > 127 || !invalid.contains(&(c as u8))
}

/// Characters dropped from file names, as libtorrent's
/// `filter_path_character`: separators and NUL, and zero-width or
/// direction-changing formatting characters, which can disguise a name
/// (U+061C, U+200B-200F, U+202A-202E, U+2060-2064, U+2066-2069, U+FEFF).
fn filter_path_character(c: u32) -> bool {
    matches!(c, 0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xfeff)
        || (c <= 127 && matches!(c as u8, b'/' | b'\\' | 0))
}

/// One path element from a torrent, made safe, as libtorrent's
/// `sanitize_append_path_element`. `None` means the element adds nothing
/// (it was `.` or `..`, or nothing of it was usable) and is left out; with
/// `force`, used for every element of a file's path, it becomes `_`
/// instead, so a path always keeps its depth.
pub(crate) fn sanitize_element(element: &[u8], force: bool) -> Option<String> {
    if element == b"." && !force {
        return None;
    }
    if element.is_empty() {
        return Some("_".to_owned());
    }
    let mut out: Vec<u8> = Vec::with_capacity(element.len());
    let mut dots = 0;
    let mut found_extension = false;
    let mut at = 0;
    while at < element.len() {
        let (code_point, length) = parse_utf8_codepoint(&element[at..]);
        let start = at;
        at += length;
        if code_point.is_some_and(filter_path_character) {
            continue;
        }
        let Some(code_point) = code_point.filter(|c| valid_path_character(*c)) else {
            // An invalid sequence or a forbidden character.
            out.push(b'_');
            continue;
        };
        out.extend(&element[start..at]);
        if code_point == u32::from(b'.') {
            dots += 1;
        }
        // At most 255 bytes an element: past 240, keep only the extension
        // (a dot within the last 10 bytes), if there is one.
        if out.len() >= 240 && !found_extension {
            let floor = element.len().saturating_sub(10).max(start);
            let dot = (floor + 1..element.len())
                .rev()
                .find(|j| element[*j] == b'.');
            match dot {
                None => break,
                Some(dot) => {
                    found_extension = true;
                    at = dot;
                }
            }
        }
    }
    if out.len() == dots && out.len() <= 2 {
        // `.`, `..`, or nothing left.
        return force.then(|| "_".to_owned());
    }
    if cfg!(windows) {
        // Windows refuses names ending in spaces or dots.
        while out.last().is_some_and(|byte| matches!(byte, b' ' | b'.')) {
            out.pop();
        }
        if out.is_empty() {
            return force.then(|| "_".to_owned());
        }
    }
    // Every byte copied came from a complete, valid sequence.
    Some(String::from_utf8(out).unwrap_or_else(|_| "_".to_owned()))
}

/// A path's elements from a torrent, each made safe with `force`.
pub(crate) fn sanitize_path<'a>(
    elements: impl IntoIterator<Item = &'a [u8]>,
) -> Result<PathBuf, crate::BackendError> {
    let mut path = PathBuf::new();
    for (depth, element) in elements.into_iter().enumerate() {
        if depth >= MAX_DIRECTORY_DEPTH {
            return Err(crate::BackendError::InvalidMetainfo(format!(
                "torrent has a file more than {MAX_DIRECTORY_DEPTH} folders deep"
            )));
        }
        if let Some(element) = sanitize_element(element, true) {
            path.push(element);
        }
    }
    Ok(path)
}

/// A path as `/`-separated text, lowercased in ASCII: how libtorrent
/// compares paths for collisions.
fn key(path: &Path) -> String {
    path.iter()
        .map(|part| part.to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

/// A file to check for name collisions: its path, and, for padding files
/// (which may share a name if they have the same size), its size.
pub(crate) struct Named<'a> {
    pub path: &'a Path,
    pub padding: Option<u64>,
}

/// New paths for files whose paths collide with an earlier file's or with
/// a folder's, ignoring ASCII case, as libtorrent's
/// `resolve_duplicate_filenames`: `name.ext` becomes `name.1.ext`, or the
/// next number free. Returns the index and new path of each file renamed.
pub(crate) fn resolve_duplicates(
    files: &[Named<'_>],
) -> Result<Vec<(usize, PathBuf)>, crate::BackendError> {
    // Folders first, so no file takes a folder's name.
    let mut taken: HashSet<String> = HashSet::new();
    for file in files {
        let mut folder = PathBuf::new();
        let mut parts = file.path.iter().peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            folder.push(part);
            taken.insert(key(&folder));
        }
    }
    let mut padding_sizes: HashMap<String, u64> = HashMap::new();
    let mut renamed = Vec::new();
    let mut collisions = 0;
    for (index, file) in files.iter().enumerate() {
        let name = key(file.path);
        if taken.insert(name.clone()) {
            if let Some(size) = file.padding {
                padding_sizes.insert(name, size);
            }
            continue;
        }
        // Padding files of the same size may share a name.
        if file.padding.is_some() && padding_sizes.get(&name) == file.padding.as_ref() {
            continue;
        }
        let file_name = file
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (base, extension) = match file_name.rfind('.') {
            Some(dot) => file_name.split_at(dot),
            None => (file_name.as_str(), ""),
        };
        let mut count = 0;
        let path = loop {
            count += 1;
            let candidate = file
                .path
                .with_file_name(format!("{base}.{count}{extension}"));
            if taken.insert(key(&candidate)) {
                break candidate;
            }
            collisions += 1;
            if collisions > MAX_DUPLICATE_FILENAMES {
                return Err(crate::BackendError::InvalidMetainfo(
                    "torrent has too many files of the same name".to_owned(),
                ));
            }
        };
        renamed.push((index, path));
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_good_names_as_they_are() {
        for name in [
            "Ubuntu 26.04.iso",
            "日本語のファイル.mkv",
            "Ünïcödé",
            "a.b.c",
            "..a",
            "x.",
        ] {
            assert_eq!(
                sanitize_element(name.as_bytes(), true).as_deref(),
                Some(name)
            );
        }
    }

    #[test]
    fn replaces_what_is_not_utf_8_as_libtorrent_does() {
        // Latin-1 "Café", and GBK for "中文" (0xd6 0xd0 0xce 0xc4).
        assert_eq!(sanitize_element(b"Caf\xe9", true).as_deref(), Some("Caf_"));
        assert_eq!(
            sanitize_element(b"\xd6\xd0\xce\xc4.txt", true).as_deref(),
            Some("__.txt")
        );
        // A lone continuation byte is one `_`; a cut-off sequence, one for
        // the rest; an overlong encoding of '/', one for its two bytes.
        assert_eq!(sanitize_element(b"a\x80b", true).as_deref(), Some("a_b"));
        assert_eq!(
            sanitize_element(b"ab\xe6\x97", true).as_deref(),
            Some("ab_")
        );
        assert_eq!(
            sanitize_element(b"a\xc0\xafb", true).as_deref(),
            Some("a_b")
        );
        // Surrogates are not UTF-8.
        assert_eq!(
            sanitize_element(b"\xed\xa0\x80x", true).as_deref(),
            Some("_x")
        );
    }

    #[test]
    fn drops_separators_and_disguises_and_replaces_controls() {
        assert_eq!(
            sanitize_element(b"a/b\\c\0d", true).as_deref(),
            Some("abcd")
        );
        assert_eq!(
            sanitize_element(b"a\nb\x7fc\xc2\x85d", true).as_deref(),
            Some("a_b_c_d")
        );
        // A right-to-left override that would show "evilexe.avi".
        assert_eq!(
            sanitize_element("evil\u{202e}iva.exe".as_bytes(), true).as_deref(),
            Some("eviliva.exe")
        );
        assert_eq!(
            sanitize_element("\u{200b}".as_bytes(), true).as_deref(),
            Some("_")
        );
    }

    #[test]
    fn turns_dots_and_empty_elements_into_underscores() {
        for name in [&b"."[..], b"..", b"", b"/"] {
            assert_eq!(
                sanitize_element(name, true).as_deref(),
                Some("_"),
                "{name:?}"
            );
        }
        // A torrent's own name of `.` or `..` adds nothing.
        assert_eq!(sanitize_element(b".", false), None);
        assert_eq!(sanitize_element(b"..", false), None);
        assert_eq!(sanitize_element(b"", false).as_deref(), Some("_"));
    }

    #[test]
    fn cuts_long_names_but_keeps_the_extension() {
        let long = format!("{}.mkv", "a".repeat(300));
        let cut = sanitize_element(long.as_bytes(), true).unwrap();
        assert_eq!(cut, format!("{}.mkv", "a".repeat(240)));
        let no_extension = "b".repeat(300);
        assert_eq!(
            sanitize_element(no_extension.as_bytes(), true)
                .unwrap()
                .len(),
            240
        );
    }

    #[test]
    fn refuses_absurdly_deep_paths() {
        let deep = vec![&b"d"[..]; MAX_DIRECTORY_DEPTH + 1];
        assert!(sanitize_path(deep).is_err());
        let fine = vec![&b"d"[..]; MAX_DIRECTORY_DEPTH];
        assert_eq!(
            sanitize_path(fine).unwrap().iter().count(),
            MAX_DIRECTORY_DEPTH
        );
    }

    #[test]
    fn renames_files_whose_names_collide_as_libtorrent_does() {
        let named = |path: &'static str| Named {
            path: Path::new(path),
            padding: None,
        };
        let files = [
            named("t/movie.mkv"),
            named("t/MOVIE.mkv"),
            named("t/movie.mkv"),
            named("t/movie.1.mkv"),
            // A file named like a folder.
            named("t/sub"),
            named("t/sub/a.txt"),
            named("t/archive.tar.gz"),
            named("t/archive.tar.gz"),
        ];
        let renamed = resolve_duplicates(&files).unwrap();
        assert_eq!(
            renamed,
            [
                (1, PathBuf::from("t/MOVIE.1.mkv")),
                (2, PathBuf::from("t/movie.2.mkv")),
                (3, PathBuf::from("t/movie.1.1.mkv")),
                (4, PathBuf::from("t/sub.1")),
                (7, PathBuf::from("t/archive.tar.1.gz")),
            ]
        );
        // Padding files of one size may share a name.
        let pad = |size| Named {
            path: Path::new("t/.pad/16384"),
            padding: Some(size),
        };
        assert!(
            resolve_duplicates(&[pad(16384), pad(16384)])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn refuses_torrents_made_of_colliding_names() {
        let files: Vec<Named> = (0..1000)
            .map(|_| Named {
                path: Path::new("t/same"),
                padding: None,
            })
            .collect();
        assert!(resolve_duplicates(&files).is_err());
    }
}
