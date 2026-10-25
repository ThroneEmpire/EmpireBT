use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A file contained in a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    path: PathBuf,
    length: u64,
    /// A BEP 47 padding file (`attr` contains `p`): zeros that align the
    /// next file to a piece boundary. Never stored on disk.
    padding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentFormat {
    V1,
    V2,
    Hybrid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2File {
    path: PathBuf,
    length: u64,
    pieces_root: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2PieceLayer {
    root: [u8; 32],
    pieces: Vec<[u8; 32]>,
}

impl V2PieceLayer {
    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }

    pub fn pieces(&self) -> &[[u8; 32]] {
        &self.pieces
    }
}

impl V2File {
    pub fn from_parts(path: impl Into<PathBuf>, length: u64, pieces_root: [u8; 32]) -> Self {
        Self {
            path: path.into(),
            length,
            pieces_root,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn length(&self) -> u64 {
        self.length
    }
    pub fn pieces_root(&self) -> &[u8; 32] {
        &self.pieces_root
    }

    /// Pieces this file occupies in v2's file-aligned piece numbering. An
    /// empty file occupies none (BEP 52).
    pub fn piece_count(&self, piece_length: u32) -> usize {
        self.length.div_ceil(u64::from(piece_length)) as usize
    }
}

impl TorrentFile {
    pub fn new(path: impl Into<PathBuf>, length: u64) -> Self {
        Self {
            path: path.into(),
            length,
            padding: false,
        }
    }

    /// Whether this is a padding file, which reads as zeros and is never
    /// written to disk.
    pub fn is_padding(&self) -> bool {
        self.padding
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn length(&self) -> u64 {
        self.length
    }
}

/// Metadata required to identify and download a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
///
/// The bulky parts (raw bytes, piece hashes, file lists) are shared between
/// clones, so copying metadata into transfers and seeders is cheap even with
/// thousands of torrents.
pub struct TorrentInfo {
    name: String,
    piece_length: u32,
    pieces: Arc<[[u8; 20]]>,
    files: Arc<[TorrentFile]>,
    announce: Option<String>,
    tracker_urls: Vec<String>,
    info_hash: [u8; 20],
    source_bytes: Arc<[u8]>,
    /// Byte range of the raw info dictionary within `source_bytes`.
    info_range: std::ops::Range<usize>,
    /// BEP 27: peers may only come from the torrent's trackers.
    private: bool,
    format: TorrentFormat,
    v2_files: Arc<[V2File]>,
    v2_info_hash: Option<[u8; 32]>,
    v2_piece_layers: Arc<[V2PieceLayer]>,
}

impl TorrentInfo {
    pub fn new(
        name: impl Into<String>,
        piece_length: u32,
        pieces: Vec<[u8; 20]>,
        files: Vec<TorrentFile>,
    ) -> Result<Self, crate::BackendError> {
        let name = name.into();
        validate_fields(&name, piece_length, &files)?;
        let info_hash = v1_info_hash(&name, piece_length, &pieces, &files);
        Ok(Self {
            name,
            piece_length,
            pieces: pieces.into(),
            files: files.into(),
            announce: None,
            tracker_urls: Vec::new(),
            info_hash,
            source_bytes: Arc::from([]),
            info_range: 0..0,
            private: false,
            format: TorrentFormat::V1,
            v2_files: Arc::from([]),
            v2_info_hash: None,
            v2_piece_layers: Arc::from([]),
        })
    }

    /// Parses a bencoded `.torrent` file from memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, crate::BackendError> {
        Self::parse(bytes, true)
    }

    /// The v2 files described by a raw info dictionary, before its piece
    /// layers (which live outside the info dictionary) are known. Used to
    /// decide which layers to request from peers for a magnet link.
    pub(crate) fn v2_files_of_info(info: &[u8]) -> Result<Vec<V2File>, crate::BackendError> {
        let mut bytes = b"d4:info".to_vec();
        bytes.extend(info);
        bytes.push(b'e');
        Ok(Self::parse(&bytes, false)?.v2_files.to_vec())
    }

    fn parse(bytes: &[u8], require_layers: bool) -> Result<Self, crate::BackendError> {
        let mut parser = Parser { bytes, position: 0 };
        let root = parser.parse_value()?;
        if parser.position != bytes.len() {
            return Err(invalid("trailing data after torrent dictionary"));
        }
        let root_dict = root.as_dict()?;
        let info = required_dict(root_dict, b"info")?;
        let info_start = info.start;
        let info_end = info.end;
        let info_dict = info.as_dict()?;

        let name = text(required(info_dict, b"name")?, "name")?;
        let piece_length = integer(required(info_dict, b"piece length")?, "piece length")?;
        let meta_version = find(info_dict, b"meta version")
            .map(|value| integer_u64(value, "meta version"))
            .transpose()?;
        let private = find(info_dict, b"private")
            .map(|value| integer_u64(value, "private"))
            .transpose()?
            == Some(1);
        let v1_pieces = find(info_dict, b"pieces");
        let pieces = if let Some(value) = v1_pieces {
            let pieces_bytes = bytes_value(value, "pieces")?;
            if pieces_bytes.len() % 20 != 0 {
                return Err(invalid("'pieces' length must be a multiple of 20"));
            }
            pieces_bytes
                .chunks_exact(20)
                .map(|piece| piece.try_into().unwrap())
                .collect()
        } else {
            Vec::new()
        };
        let v2_files = if let Some(tree) = find(info_dict, b"file tree") {
            let mut files = Self::parse_v2_tree(tree, Path::new(""))?;
            // Paths are relative to the torrent's directory, `name`, unless
            // the torrent is a single file of that name.
            let single = files.len() == 1 && files[0].path == Path::new(&name);
            if !single {
                for file in &mut files {
                    file.path = Path::new(&name).join(&file.path);
                }
            }
            files
        } else {
            Vec::new()
        };
        let v2_piece_layers = if let Some(layers) = find(root_dict, b"piece layers") {
            Self::parse_piece_layers(layers)?
        } else {
            Vec::new()
        };
        if require_layers {
            validate_piece_layers(&v2_files, piece_length, &v2_piece_layers)?;
        }
        let files = if !v2_files.is_empty() && v1_pieces.is_none() {
            v2_files
                .iter()
                .map(|file| TorrentFile::new(file.path.clone(), file.length))
                .collect()
        } else if let Some(files_value) = find(info_dict, b"files") {
            parse_multi_file(files_value, &name)?
        } else if let Some(length) = find(info_dict, b"length") {
            let length = integer_u64(length, "length")?;
            vec![TorrentFile::new(&name, length)]
        } else if !v2_files.is_empty() {
            v2_files
                .iter()
                .map(|file| TorrentFile::new(file.path.clone(), file.length))
                .collect()
        } else {
            return Err(invalid("torrent has no v1 files or v2 file tree"));
        };
        validate_fields(&name, piece_length, &files)?;

        let announce = find(root_dict, b"announce")
            .map(|value| text(value, "announce"))
            .transpose()?;
        let mut tracker_urls = Vec::new();
        if let Some(url) = &announce {
            tracker_urls.push(url.clone());
        }
        if let Some(announce_list) = find(root_dict, b"announce-list") {
            for tier in announce_list.as_list()? {
                for tracker in tier.as_list()? {
                    let url = text(tracker, "announce-list tracker")?;
                    if !tracker_urls.contains(&url) {
                        tracker_urls.push(url);
                    }
                }
            }
        }
        let mut hash = Sha1::new();
        hash.update(&bytes[info_start..info_end]);
        let info_hash = hash.finalize().into();
        let v2_info_hash = if meta_version == Some(2) {
            Some(Sha256::digest(&bytes[info_start..info_end]).into())
        } else {
            None
        };
        let format = match (meta_version == Some(2), v1_pieces.is_some()) {
            (true, true) => TorrentFormat::Hybrid,
            (true, false) => TorrentFormat::V2,
            _ => TorrentFormat::V1,
        };
        if format != TorrentFormat::V1 {
            validate_v2_piece_length(piece_length)?;
        }

        Ok(Self {
            name,
            piece_length,
            pieces: pieces.into(),
            files: files.into(),
            announce,
            tracker_urls,
            info_hash,
            source_bytes: Arc::from(bytes),
            info_range: info_start..info_end,
            private,
            format,
            v2_files: v2_files.into(),
            v2_info_hash,
            v2_piece_layers: v2_piece_layers.into(),
        })
    }

    /// Builds torrent metadata from a raw info dictionary fetched from peers
    /// (BEP 9), adding `trackers` as the announce list. The info dictionary
    /// is embedded byte-for-byte so its hash is preserved.
    pub fn from_metadata(info: &[u8], trackers: &[String]) -> Result<Self, crate::BackendError> {
        Self::from_metadata_with_layers(info, &[], trackers)
    }

    /// Like [`TorrentInfo::from_metadata`], also adding v2 piece layers
    /// (pieces root and its piece hashes) fetched from peers.
    pub fn from_metadata_with_layers(
        info: &[u8],
        layers: &[([u8; 32], Vec<[u8; 32]>)],
        trackers: &[String],
    ) -> Result<Self, crate::BackendError> {
        let text = |value: &[u8]| {
            let mut out = format!("{}:", value.len()).into_bytes();
            out.extend(value);
            out
        };
        // Keys in sorted order: announce, announce-list, info.
        let mut bytes = b"d".to_vec();
        if let Some(first) = trackers.first() {
            bytes.extend(text(b"announce"));
            bytes.extend(text(first.as_bytes()));
            bytes.extend(text(b"announce-list"));
            bytes.push(b'l');
            for tracker in trackers {
                bytes.push(b'l');
                bytes.extend(text(tracker.as_bytes()));
                bytes.push(b'e');
            }
            bytes.push(b'e');
        }
        bytes.extend(text(b"info"));
        bytes.extend(info);
        // "piece layers" sorts after "info"; its keys (roots) must be sorted.
        if !layers.is_empty() {
            let mut sorted: Vec<_> = layers.iter().collect();
            sorted.sort_by_key(|(root, _)| *root);
            bytes.extend(text(b"piece layers"));
            bytes.push(b'd');
            for (root, hashes) in sorted {
                bytes.extend(text(root));
                bytes.extend(text(&hashes.concat()));
            }
            bytes.push(b'e');
        }
        bytes.push(b'e');
        Self::from_bytes(&bytes)
    }

    /// Whether this is a private torrent (BEP 27). Private torrents must
    /// only use their trackers for peers: never the DHT.
    pub fn is_private(&self) -> bool {
        self.private
    }

    /// The raw bencoded info dictionary, when this metadata was parsed from
    /// bytes. Peers fetching a magnet link are served exactly these bytes.
    pub fn info_bytes(&self) -> Option<&[u8]> {
        self.source_bytes
            .get(self.info_range.clone())
            .filter(|bytes| !bytes.is_empty())
    }

    /// Parses a `.torrent` file from disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, crate::BackendError> {
        let bytes =
            std::fs::read(path).map_err(|error| crate::BackendError::Io(error.to_string()))?;
        Self::from_bytes(&bytes)
    }

    /// Whether `source` looks like a `.torrent` URL for [`Self::from_url`].
    pub fn is_url(source: &str) -> bool {
        let source = source.trim();
        source.starts_with("http://") || source.starts_with("https://")
    }

    /// Downloads a `.torrent` file from an `http://` or `https://` URL,
    /// following redirects. This blocks for up to `timeout` per request.
    pub fn from_url(url: &str, timeout: std::time::Duration) -> Result<Self, crate::BackendError> {
        Self::from_url_with(url, timeout, &crate::Binding::any())
    }

    /// Like [`Self::from_url`], through one network interface only.
    pub fn from_url_with(
        url: &str,
        timeout: std::time::Duration,
        binding: &crate::Binding,
    ) -> Result<Self, crate::BackendError> {
        /// Large torrents run to tens of MB of piece hashes; anything much
        /// bigger is not a torrent.
        const LIMIT: u64 = 64 * 1024 * 1024;
        let bytes = crate::http::get(url.trim(), timeout, LIMIT, binding).map_err(|error| {
            crate::BackendError::Io(format!("cannot download {url}: {}", error.into_message()))
        })?;
        // Sites often answer with a login or error page instead.
        Self::from_bytes(&bytes).map_err(|error| {
            crate::BackendError::InvalidMetainfo(format!(
                "{url} did not return a .torrent file ({error})"
            ))
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    pub fn pieces(&self) -> &[[u8; 20]] {
        &self.pieces
    }

    pub fn files(&self) -> &[TorrentFile] {
        &self.files
    }

    pub fn total_length(&self) -> u64 {
        self.files.iter().map(TorrentFile::length).sum()
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn piece_size(&self, index: usize) -> Option<u64> {
        if index >= self.piece_count() {
            return None;
        }
        let start = index as u64 * self.piece_length as u64;
        Some((self.total_length() - start).min(self.piece_length as u64))
    }

    pub fn announce(&self) -> Option<&str> {
        self.announce.as_deref()
    }

    /// Lists these trackers, for tests that build torrents in code.
    #[cfg(test)]
    pub(crate) fn with_tracker_urls(mut self, urls: Vec<String>) -> Self {
        self.tracker_urls = urls;
        self
    }

    /// Returns tracker URLs in announce priority order, without duplicates.
    pub fn tracker_urls(&self) -> &[String] {
        &self.tracker_urls
    }

    /// Returns the SHA-1 hash used in peer wire handshakes.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// Returns the 20-byte hash used in the peer handshake for this format.
    ///
    /// BitTorrent v2 uses the first 20 bytes of its SHA-256 info hash, while
    /// v1 and the v1 side of hybrid torrents use the SHA-1 info hash.
    pub fn peer_info_hash(&self) -> [u8; 20] {
        if self.format == TorrentFormat::V2 {
            let mut hash = [0; 20];
            hash.copy_from_slice(
                &self
                    .v2_info_hash
                    .expect("v2 format always has a v2 info hash")[..20],
            );
            hash
        } else {
            self.info_hash
        }
    }

    /// Every hash this torrent's swarm is announced under: the v1 hash, the
    /// truncated v2 hash, or both for a hybrid torrent, whose peers may know
    /// it by either.
    pub fn swarm_hashes(&self) -> Vec<[u8; 20]> {
        match (self.format, self.v2_info_hash) {
            (TorrentFormat::Hybrid, Some(v2)) => {
                vec![
                    self.info_hash,
                    v2[..20].try_into().expect("v2 hash is 32 bytes"),
                ]
            }
            _ => vec![self.peer_info_hash()],
        }
    }

    pub fn format(&self) -> TorrentFormat {
        self.format
    }
    pub fn v2_files(&self) -> &[V2File] {
        &self.v2_files
    }
    pub fn v2_info_hash(&self) -> Option<&[u8; 32]> {
        self.v2_info_hash.as_ref()
    }

    pub fn v2_piece_layers(&self) -> &[V2PieceLayer] {
        &self.v2_piece_layers
    }

    pub fn v2_piece_count(&self) -> usize {
        self.v2_files
            .iter()
            .map(|file| file.piece_count(self.piece_length))
            .sum()
    }

    pub fn v2_piece_location(&self, index: usize) -> Option<(&V2File, usize)> {
        let mut offset = index;
        for file in self.v2_files.iter() {
            let count = file.piece_count(self.piece_length);
            if offset < count {
                return Some((file, offset));
            }
            offset -= count;
        }
        None
    }

    /// Returns the original bencoded metainfo, when parsed from bytes or file.
    pub(crate) fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }

    fn parse_piece_layers(value: &ValueSpan<'_>) -> Result<Vec<V2PieceLayer>, crate::BackendError> {
        value
            .as_dict()?
            .iter()
            .map(|(root, hashes)| {
                let root: [u8; 32] = (*root)
                    .try_into()
                    .map_err(|_| invalid("piece-layer key must be 32 bytes"))?;
                let bytes = bytes_value(hashes, "piece layer")?;
                if bytes.len() % 32 != 0 {
                    return Err(invalid("piece layer length must be a multiple of 32"));
                }
                let pieces = bytes
                    .chunks_exact(32)
                    .map(|piece| piece.try_into().expect("chunk size is 32"))
                    .collect();
                Ok(V2PieceLayer { root, pieces })
            })
            .collect()
    }

    fn parse_v2_tree(
        value: &ValueSpan<'_>,
        prefix: &Path,
    ) -> Result<Vec<V2File>, crate::BackendError> {
        let dict = value.as_dict()?;
        let mut files = Vec::new();
        for (name, child) in dict {
            if *name == b"" {
                let leaf = child.as_dict()?;
                let length = integer_u64(required(leaf, b"length")?, "v2 file length")?;
                // Empty files have no pieces root (BEP 52).
                let pieces_root: [u8; 32] = if length == 0 {
                    [0; 32]
                } else {
                    bytes_value(required(leaf, b"pieces root")?, "pieces root")?
                        .try_into()
                        .map_err(|_| invalid("v2 pieces root must be 32 bytes"))?
                };
                files.push(V2File {
                    path: prefix.to_path_buf(),
                    length,
                    pieces_root,
                });
            } else {
                let component = String::from_utf8(name.to_vec())
                    .map_err(|_| invalid("v2 file-tree path is not UTF-8"))?;
                if component.is_empty()
                    || component == "."
                    || component == ".."
                    || Path::new(&component).is_absolute()
                {
                    return Err(invalid("torrent contains an unsafe v2 file-tree path"));
                }
                let mut next = prefix.to_path_buf();
                next.push(component);
                files.extend(Self::parse_v2_tree(child, &next)?);
            }
        }
        Ok(files)
    }
}

/// The info hash a v1 `.torrent` with these fields would have: the SHA-1
/// of its canonical BEP 3 info dictionary. Torrents built in code then
/// have distinct hashes, as real ones do.
fn v1_info_hash(
    name: &str,
    piece_length: u32,
    pieces: &[[u8; 20]],
    files: &[TorrentFile],
) -> [u8; 20] {
    use crate::bencode::Value;
    let text = |text: &str| Value::bytes(text.as_bytes());
    let mut info = std::collections::BTreeMap::new();
    info.insert(b"name".to_vec(), text(name));
    info.insert(
        b"piece length".to_vec(),
        Value::Int(i64::from(piece_length)),
    );
    info.insert(b"pieces".to_vec(), Value::bytes(pieces.concat()));
    if let [file] = files
        && file.path == Path::new(name)
    {
        info.insert(b"length".to_vec(), Value::Int(file.length as i64));
    } else {
        let list = files
            .iter()
            .map(|file| {
                // Multi-file paths sit under the torrent's directory.
                let relative = file.path.strip_prefix(name).unwrap_or(&file.path);
                let path = relative
                    .components()
                    .map(|part| text(&part.as_os_str().to_string_lossy()))
                    .collect();
                let mut entry = std::collections::BTreeMap::new();
                entry.insert(b"length".to_vec(), Value::Int(file.length as i64));
                entry.insert(b"path".to_vec(), Value::List(path));
                if file.padding {
                    entry.insert(b"attr".to_vec(), text("p"));
                }
                Value::Dict(entry)
            })
            .collect();
        info.insert(b"files".to_vec(), Value::List(list));
    }
    Sha1::digest(Value::Dict(info).encode()).into()
}

fn parse_multi_file(
    value: &ValueSpan<'_>,
    root_name: &str,
) -> Result<Vec<TorrentFile>, crate::BackendError> {
    let files = value.as_list()?;
    files
        .iter()
        .map(|file| {
            let dict = file.as_dict()?;
            let length = integer_u64(required(dict, b"length")?, "file length")?;
            let path_list = required(dict, b"path")?.as_list()?;
            let mut path = PathBuf::from(root_name);
            for component in path_list {
                let component = text(component, "file path")?;
                let component_path = Path::new(&component);
                if component.is_empty()
                    || component_path.is_absolute()
                    || component_path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(invalid("torrent contains an unsafe file path"));
                }

                path.push(component);
            }
            let padding = find(dict, b"attr")
                .and_then(|attr| bytes_value(attr, "attr").ok())
                .is_some_and(|attr| attr.contains(&b'p'));
            Ok(TorrentFile {
                path,
                length,
                padding,
            })
        })
        .collect()
}

fn validate_piece_layers(
    files: &[V2File],
    piece_length: u32,
    layers: &[V2PieceLayer],
) -> Result<(), crate::BackendError> {
    for file in files {
        let piece_count = file.length.div_ceil(u64::from(piece_length)).max(1);
        let layer = layers.iter().find(|layer| layer.root == file.pieces_root);
        if piece_count > 1 {
            let Some(layer) = layer else {
                return Err(invalid("v2 file is missing its piece layer"));
            };
            if layer.pieces.len() != piece_count as usize {
                return Err(invalid("v2 piece layer has the wrong number of pieces"));
            }
        } else if layer.is_some() {
            return Err(invalid("v2 piece layer is present for a single-piece file"));
        }
    }
    if layers
        .iter()
        .any(|layer| !files.iter().any(|file| file.pieces_root == layer.root))
    {
        return Err(invalid("v2 piece layer has no matching file"));
    }
    Ok(())
}

fn validate_fields(
    name: &str,
    piece_length: u32,
    files: &[TorrentFile],
) -> Result<(), crate::BackendError> {
    if name.trim().is_empty() {
        return Err(invalid("torrent name cannot be empty"));
    }

    if piece_length == 0 {
        return Err(invalid("piece length must be greater than zero"));
    }
    if files.is_empty() {
        return Err(invalid("torrent must contain at least one file"));
    }
    Ok(())
}

fn validate_v2_piece_length(piece_length: u32) -> Result<(), crate::BackendError> {
    if piece_length < 16 * 1024 || !piece_length.is_power_of_two() {
        return Err(invalid(
            "v2 piece length must be a power of two of at least 16 KiB",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> crate::BackendError {
    crate::BackendError::InvalidMetainfo(message.into())
}

struct ValueSpan<'a> {
    value: Value<'a>,
    start: usize,
    end: usize,
}

enum Value<'a> {
    Bytes(&'a [u8]),
    Integer(i64),
    List(Vec<ValueSpan<'a>>),
    Dict(Vec<(&'a [u8], ValueSpan<'a>)>),
}

impl<'a> ValueSpan<'a> {
    fn as_dict(&self) -> Result<&Vec<(&'a [u8], ValueSpan<'a>)>, crate::BackendError> {
        match &self.value {
            Value::Dict(value) => Ok(value),
            _ => Err(invalid("expected dictionary")),
        }
    }

    fn as_list(&self) -> Result<&Vec<ValueSpan<'a>>, crate::BackendError> {
        match &self.value {
            Value::List(value) => Ok(value),
            _ => Err(invalid("expected list")),
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self) -> Result<ValueSpan<'a>, crate::BackendError> {
        let start = self.position;
        let value = match self.bytes.get(self.position) {
            Some(b'i') => {
                self.position += 1;
                let end = self.find_byte(b'e')?;
                let number = std::str::from_utf8(&self.bytes[self.position..end])
                    .map_err(|_| invalid("integer is not valid UTF-8"))?
                    .parse()
                    .map_err(|_| invalid("invalid integer"))?;
                self.position = end + 1;
                Value::Integer(number)
            }
            Some(b'l') => {
                self.position += 1;
                let mut values = Vec::new();
                while self.bytes.get(self.position) != Some(&b'e') {
                    values.push(self.parse_value()?);
                }
                self.position += 1;
                Value::List(values)
            }
            Some(b'd') => {
                self.position += 1;
                let mut values = Vec::new();
                while self.bytes.get(self.position) != Some(&b'e') {
                    let key = self.parse_bytes()?;
                    let value = self.parse_value()?;
                    values.push((key, value));
                }
                self.position += 1;
                Value::Dict(values)
            }
            Some(b'0'..=b'9') => Value::Bytes(self.parse_bytes()?),
            _ => return Err(invalid("unexpected end or invalid bencode value")),
        };
        Ok(ValueSpan {
            value,
            start,
            end: self.position,
        })
    }

    fn parse_bytes(&mut self) -> Result<&'a [u8], crate::BackendError> {
        let separator = self
            .bytes
            .get(self.position..)
            .and_then(|remaining| remaining.iter().position(|byte| *byte == b':'))
            .ok_or_else(|| invalid("byte string is missing its length separator"))?;
        let end = self.position + separator;
        let length = std::str::from_utf8(&self.bytes[self.position..end])
            .map_err(|_| invalid("byte string length is invalid"))?
            .parse::<usize>()
            .map_err(|_| invalid("byte string length is invalid"))?;
        let start = end + 1;
        let finish = start
            .checked_add(length)
            .ok_or_else(|| invalid("byte string is too large"))?;
        let value = self
            .bytes
            .get(start..finish)
            .ok_or_else(|| invalid("byte string is truncated"))?;
        self.position = finish;
        Ok(value)
    }

    fn find_byte(&self, byte: u8) -> Result<usize, crate::BackendError> {
        self.bytes
            .get(self.position..)
            .and_then(|remaining| remaining.iter().position(|candidate| *candidate == byte))
            .map(|offset| self.position + offset)
            .ok_or_else(|| invalid("unterminated integer"))
    }
}

fn find<'a>(dict: &'a [(&'a [u8], ValueSpan<'a>)], key: &[u8]) -> Option<&'a ValueSpan<'a>> {
    dict.iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, value)| value)
}

fn required<'a>(
    dict: &'a [(&'a [u8], ValueSpan<'a>)],
    key: &[u8],
) -> Result<&'a ValueSpan<'a>, crate::BackendError> {
    find(dict, key).ok_or_else(|| invalid(format!("missing '{}'", String::from_utf8_lossy(key))))
}

fn required_dict<'a>(
    dict: &'a [(&'a [u8], ValueSpan<'a>)],
    key: &[u8],
) -> Result<&'a ValueSpan<'a>, crate::BackendError> {
    required(dict, key)
}

fn bytes_value<'a>(value: &'a ValueSpan<'a>, field: &str) -> Result<&'a [u8], crate::BackendError> {
    match &value.value {
        Value::Bytes(bytes) => Ok(bytes),
        _ => Err(invalid(format!("'{field}' must be a byte string"))),
    }
}

fn text(value: &ValueSpan<'_>, field: &str) -> Result<String, crate::BackendError> {
    String::from_utf8(bytes_value(value, field)?.to_vec())
        .map_err(|_| invalid(format!("'{field}' must be valid UTF-8")))
}

fn integer(value: &ValueSpan<'_>, field: &str) -> Result<u32, crate::BackendError> {
    match value.value {
        Value::Integer(number) if number >= 0 && number <= u32::MAX as i64 => Ok(number as u32),
        _ => Err(invalid(format!(
            "'{field}' must be a non-negative 32-bit integer"
        ))),
    }
}

fn integer_u64(value: &ValueSpan<'_>, field: &str) -> Result<u64, crate::BackendError> {
    match value.value {
        Value::Integer(number) if number >= 0 => Ok(number as u64),
        _ => Err(invalid(format!("'{field}' must be a non-negative integer"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrents_built_in_code_hash_like_their_torrent_files() {
        let mut single =
            b"d4:infod6:lengthi5e4:name9:hello.txt12:piece lengthi16384e6:pieces20:".to_vec();
        single.extend([7; 20]);
        single.extend(b"ee");
        let built = TorrentInfo::new(
            "hello.txt",
            16384,
            vec![[7; 20]],
            vec![TorrentFile::new("hello.txt", 5)],
        )
        .unwrap();
        assert_eq!(
            built.info_hash(),
            TorrentInfo::from_bytes(&single).unwrap().info_hash()
        );

        let mut multi = b"d4:infod5:filesld6:lengthi3e4:pathl1:aeed6:lengthi4e4:pathl3:sub1:beee\
4:name5:album12:piece lengthi16384e6:pieces20:"
            .to_vec();
        multi.extend([9; 20]);
        multi.extend(b"ee");
        let built = TorrentInfo::new(
            "album",
            16384,
            vec![[9; 20]],
            vec![
                TorrentFile::new("album/a", 3),
                TorrentFile::new("album/sub/b", 4),
            ],
        )
        .unwrap();
        assert_eq!(
            built.info_hash(),
            TorrentInfo::from_bytes(&multi).unwrap().info_hash()
        );
        assert_ne!(built.info_hash(), &[0; 20]);
    }

    /// Needs network access: downloads a real `.torrent` over HTTPS and
    /// plain HTTP. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn downloads_a_torrent_file_from_a_url() {
        let path = "releases.ubuntu.com/24.04/ubuntu-24.04.3-desktop-amd64.iso.torrent";
        for scheme in ["https", "http"] {
            let url = format!("{scheme}://{path}");
            assert!(TorrentInfo::is_url(&url));
            let info = TorrentInfo::from_url(&url, std::time::Duration::from_secs(20)).unwrap();
            assert_eq!(info.name(), "ubuntu-24.04.3-desktop-amd64.iso");
        }
        // A web page is not a torrent.
        let error = TorrentInfo::from_url(
            "https://releases.ubuntu.com/24.04/",
            std::time::Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("did not return a .torrent file"),
            "{error}"
        );
    }

    #[test]
    fn parses_single_file_torrent() {
        let bytes = b"d8:announce14:http://tracker4:infod4:name3:foo6:lengthi4e6:pieces20:1234567890123456789012:piece lengthi16384eee";

        let info = TorrentInfo::from_bytes(bytes).expect("valid torrent metadata");

        assert_eq!(info.name(), "foo");
        assert_eq!(info.piece_length(), 16_384);
        assert_eq!(info.pieces().len(), 1);
        assert_eq!(info.files(), &[TorrentFile::new("foo", 4)]);
        assert_eq!(info.announce(), Some("http://tracker"));
        assert_ne!(info.info_hash(), &[0; 20]);
    }

    #[test]
    fn rejects_unsafe_multi_file_path() {
        let bytes = b"d4:infod5:filesld6:lengthi1e4:pathl2:..eee4:name4:test6:pieces012:piece lengthi16384eee";

        let result = TorrentInfo::from_bytes(bytes);

        assert!(matches!(
            result,
            Err(crate::BackendError::InvalidMetainfo(_))
        ));
    }

    #[test]
    fn parses_tracker_tiers_without_duplicates() {
        let bytes = b"d13:announce-listll27:http://tracker.example/main29:http://tracker.example/backupee4:infod4:name3:foo6:lengthi4e6:pieces0:12:piece lengthi16384eee";

        let info = TorrentInfo::from_bytes(bytes).expect("valid tracker metadata");

        assert_eq!(
            info.tracker_urls(),
            &[
                "http://tracker.example/main".to_owned(),
                "http://tracker.example/backup".to_owned()
            ]
        );
    }

    #[test]
    fn parses_v2_file_tree_and_identifies_v2_format() {
        let root = [7_u8; 32];
        let mut bytes = b"d4:infod4:name4:test12:piece lengthi16384e9:file treed3:foo\
d0:d6:lengthi4e11:pieces root32:"
            .to_vec();
        bytes.extend(root);
        bytes.extend_from_slice(b"eee12:meta versioni2eee");

        let info = TorrentInfo::from_bytes(&bytes).expect("valid v2 torrent");

        assert_eq!(info.format(), TorrentFormat::V2);
        assert_eq!(info.v2_files().len(), 1);
        assert_eq!(info.v2_files()[0].path(), Path::new("test/foo"));
        assert_eq!(info.v2_files()[0].length(), 4);
        assert_eq!(info.v2_files()[0].pieces_root(), &root);
        assert_eq!(info.v2_info_hash().map(|hash| hash.len()), Some(32));
        assert_eq!(info.v2_piece_count(), 1);
        assert_eq!(info.v2_piece_location(0).map(|(_, index)| index), Some(0));
        assert!(info.v2_piece_location(1).is_none());
        let expected: [u8; 20] = info.v2_info_hash().unwrap()[..20].try_into().unwrap();
        assert_eq!(info.peer_info_hash(), expected);
    }

    #[test]
    fn rejects_v2_piece_lengths_that_cannot_define_merkle_layers() {
        let bytes = b"d4:infod4:name4:test12:piece lengthi20000e9:file treed3:foo".to_vec();
        let mut bytes = bytes;
        bytes.extend_from_slice(b"d6:lengthi1e pieces root32:");
        bytes.extend(std::iter::repeat(b'a').take(32));
        bytes.extend_from_slice(b"eeeee");
        assert!(TorrentInfo::from_bytes(&bytes).is_err());
    }
}
