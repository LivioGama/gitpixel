//! Immutable on-disk gram shard.
//!
//! Single-file format, mmapped read side, atomic publish (tmp + fsync +
//! rename). Layout (all integers little-endian):
//!
//! ```text
//! header (fixed 192 bytes):
//!   magic  b"GPXSHARD"                            8
//!   version u32                                   4
//!   reserved u32                                  4
//!   commit_oid [u8; 40]  (hex, zero-padded)      40
//!   extractor_id [u8; 64] (utf8, zero-padded)    64
//!   file_count u32                                4
//!   gram_count u64                                8
//!   files_off u64, files_len u64                 16
//!   lookup_off u64, lookup_len u64               16
//!   postings_off u64, postings_len u64           16
//!   padding to 192
//! files:    per file: u32 path_len, path bytes
//! lookup:   gram_count records of 20 bytes: u64 hash, u64 postings_off(rel), u32 postings_len
//!           sorted by hash — binary-searched via mmap
//! postings: delta-varint (LEB128) encoded sorted file-id lists
//! ```
//!
//! Only the whole file is mmapped; queries touch the lookup section via
//! binary search and decode one posting run. Storing gram *hashes* (never
//! gram bytes) is safe: a collision can only widen the candidate set, and
//! regex verification is authoritative.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

pub const MAGIC: &[u8; 8] = b"GPXSHARD";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 192;
const LOOKUP_RECORD: usize = 20;

#[derive(Debug)]
pub enum ShardError {
    Io(io::Error),
    Corrupt(&'static str),
    VersionMismatch { found: u32 },
}

impl From<io::Error> for ShardError {
    fn from(e: io::Error) -> Self {
        ShardError::Io(e)
    }
}

impl std::fmt::Display for ShardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardError::Io(e) => write!(f, "shard io error: {e}"),
            ShardError::Corrupt(what) => write!(f, "corrupt shard: {what}"),
            ShardError::VersionMismatch { found } => {
                write!(f, "shard version {found} != supported {VERSION}")
            }
        }
    }
}

impl std::error::Error for ShardError {}

// --- varint (LEB128, u32) ---

#[inline]
pub fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

#[inline]
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let mut v: u32 = 0;
    let mut shift = 0;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        v |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
}

// --- builder ---

pub struct ShardBuilder {
    /// Sorted-on-finish inverted map gram_hash -> sorted file ids.
    postings: HashMap<u64, Vec<u32>>,
    files: Vec<String>,
    extractor_id: String,
    commit_oid: Option<String>,
}

impl ShardBuilder {
    pub fn new(extractor_id: &str) -> Self {
        Self {
            postings: HashMap::new(),
            files: Vec::new(),
            extractor_id: extractor_id.to_string(),
            commit_oid: None,
        }
    }

    pub fn set_commit_oid(&mut self, oid: &str) {
        self.commit_oid = Some(oid.to_string());
    }

    /// Register a file and its (pre-deduped or not) gram hashes.
    /// Returns the assigned file id.
    pub fn add_file(&mut self, path: &str, mut hashes: Vec<u64>) -> u32 {
        let id = self.files.len() as u32;
        self.files.push(path.to_string());
        hashes.sort_unstable();
        hashes.dedup();
        for h in hashes {
            self.postings.entry(h).or_default().push(id);
        }
        id
    }

    /// Write the shard to `dest` atomically (tmp + fsync + rename).
    pub fn write(self, dest: &Path) -> Result<(), ShardError> {
        let tmp: PathBuf = dest.with_extension("tmp");
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        {
            let file = File::create(&tmp)?;
            let mut w = BufWriter::new(file);

            // Sections are assembled in memory first (v1 simplicity; the
            // external-sort streaming builder is a later optimization).
            let mut files_buf: Vec<u8> = Vec::new();
            for path in &self.files {
                files_buf.extend_from_slice(&(path.len() as u32).to_le_bytes());
                files_buf.extend_from_slice(path.as_bytes());
            }

            let mut hashes: Vec<&u64> = self.postings.keys().collect();
            hashes.sort_unstable();

            let mut lookup_buf: Vec<u8> = Vec::with_capacity(hashes.len() * LOOKUP_RECORD);
            let mut postings_buf: Vec<u8> = Vec::new();
            for h in &hashes {
                let ids = &self.postings[h];
                let start = postings_buf.len() as u64;
                let mut prev = 0u32;
                for (i, &id) in ids.iter().enumerate() {
                    let delta = if i == 0 { id } else { id - prev };
                    write_varint(&mut postings_buf, delta);
                    prev = id;
                }
                let len = postings_buf.len() as u64 - start;
                lookup_buf.extend_from_slice(&h.to_le_bytes());
                lookup_buf.extend_from_slice(&start.to_le_bytes());
                lookup_buf.extend_from_slice(&(len as u32).to_le_bytes());
            }

            let files_off = HEADER_LEN as u64;
            let lookup_off = files_off + files_buf.len() as u64;
            let postings_off = lookup_off + lookup_buf.len() as u64;

            let mut header = [0u8; HEADER_LEN];
            header[0..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&VERSION.to_le_bytes());
            // 12..16 reserved
            let oid = self.commit_oid.as_deref().unwrap_or("");
            let oid_bytes = oid.as_bytes();
            header[16..16 + oid_bytes.len().min(40)]
                .copy_from_slice(&oid_bytes[..oid_bytes.len().min(40)]);
            let ex = self.extractor_id.as_bytes();
            header[56..56 + ex.len().min(64)].copy_from_slice(&ex[..ex.len().min(64)]);
            header[120..124].copy_from_slice(&(self.files.len() as u32).to_le_bytes());
            header[124..132].copy_from_slice(&(hashes.len() as u64).to_le_bytes());
            header[132..140].copy_from_slice(&files_off.to_le_bytes());
            header[140..148].copy_from_slice(&(files_buf.len() as u64).to_le_bytes());
            header[148..156].copy_from_slice(&lookup_off.to_le_bytes());
            header[156..164].copy_from_slice(&(lookup_buf.len() as u64).to_le_bytes());
            header[164..172].copy_from_slice(&postings_off.to_le_bytes());
            header[172..180].copy_from_slice(&(postings_buf.len() as u64).to_le_bytes());

            w.write_all(&header)?;
            w.write_all(&files_buf)?;
            w.write_all(&lookup_buf)?;
            w.write_all(&postings_buf)?;
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        fs::rename(&tmp, dest)?;
        Ok(())
    }
}

// --- reader ---

pub struct Shard {
    mmap: Mmap,
    files: Vec<String>,
    gram_count: u64,
    lookup_off: usize,
    postings_off: usize,
    postings_len: usize,
    extractor_id: String,
    commit_oid: Option<String>,
}

impl Shard {
    pub fn open(path: &Path) -> Result<Self, ShardError> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_LEN {
            return Err(ShardError::Corrupt("file shorter than header"));
        }
        if &mmap[0..8] != MAGIC {
            return Err(ShardError::Corrupt("bad magic"));
        }
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(ShardError::VersionMismatch { found: version });
        }
        let read_u64 =
            |off: usize| u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap()) as usize;
        let oid_raw = &mmap[16..56];
        let oid_end = oid_raw.iter().position(|&b| b == 0).unwrap_or(40);
        let commit_oid = if oid_end == 0 {
            None
        } else {
            Some(
                std::str::from_utf8(&oid_raw[..oid_end])
                    .map_err(|_| ShardError::Corrupt("commit oid not utf8"))?
                    .to_string(),
            )
        };
        let ex_raw = &mmap[56..120];
        let ex_end = ex_raw.iter().position(|&b| b == 0).unwrap_or(64);
        let extractor_id = std::str::from_utf8(&ex_raw[..ex_end])
            .map_err(|_| ShardError::Corrupt("extractor id not utf8"))?
            .to_string();
        let file_count = u32::from_le_bytes(mmap[120..124].try_into().unwrap());
        let gram_count = u64::from_le_bytes(mmap[124..132].try_into().unwrap());
        let files_off = read_u64(132);
        let files_len = read_u64(140);
        let lookup_off = read_u64(148);
        let lookup_len = read_u64(156);
        let postings_off = read_u64(164);
        let postings_len = read_u64(172);
        if postings_off + postings_len > mmap.len()
            || lookup_off + lookup_len > mmap.len()
            || files_off + files_len > mmap.len()
            || lookup_len != gram_count as usize * LOOKUP_RECORD
        {
            return Err(ShardError::Corrupt("section bounds exceed file"));
        }

        // Path table is small; materialize it.
        let mut files = Vec::with_capacity(file_count as usize);
        let fbuf = &mmap[files_off..files_off + files_len];
        let mut pos = 0usize;
        for _ in 0..file_count {
            if pos + 4 > fbuf.len() {
                return Err(ShardError::Corrupt("truncated file table"));
            }
            let len = u32::from_le_bytes(fbuf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + len > fbuf.len() {
                return Err(ShardError::Corrupt("truncated file path"));
            }
            let path = std::str::from_utf8(&fbuf[pos..pos + len])
                .map_err(|_| ShardError::Corrupt("path not utf8"))?;
            files.push(path.to_string());
            pos += len;
        }

        Ok(Self {
            mmap,
            files,
            gram_count,
            lookup_off,
            postings_off,
            postings_len,
            extractor_id,
            commit_oid,
        })
    }

    pub fn file_count(&self) -> u32 {
        self.files.len() as u32
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    pub fn path_of(&self, file_id: u32) -> Option<&str> {
        self.files.get(file_id as usize).map(String::as_str)
    }

    pub fn extractor_id(&self) -> &str {
        &self.extractor_id
    }

    pub fn commit_oid(&self) -> Option<&str> {
        self.commit_oid.as_deref()
    }

    pub fn gram_count(&self) -> u64 {
        self.gram_count
    }

    /// Sorted file ids containing `hash` (empty if absent).
    pub fn postings(&self, hash: u64) -> Vec<u32> {
        let n = self.gram_count as usize;
        let lookup = &self.mmap[self.lookup_off..self.lookup_off + n * LOOKUP_RECORD];
        let record_hash = |i: usize| -> u64 {
            u64::from_le_bytes(lookup[i * LOOKUP_RECORD..i * LOOKUP_RECORD + 8].try_into().unwrap())
        };
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if record_hash(mid) < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= n || record_hash(lo) != hash {
            return Vec::new();
        }
        let rec = &lookup[lo * LOOKUP_RECORD..(lo + 1) * LOOKUP_RECORD];
        let off = u64::from_le_bytes(rec[8..16].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(rec[16..20].try_into().unwrap()) as usize;
        if off + len > self.postings_len {
            return Vec::new(); // corrupt record; fail open (no candidates)
        }
        let run = &self.mmap[self.postings_off + off..self.postings_off + off + len];
        let mut ids = Vec::new();
        let mut pos = 0usize;
        let mut prev = 0u32;
        while pos < run.len() {
            match read_varint(run, &mut pos) {
                Some(delta) => {
                    let id = if ids.is_empty() { delta } else { prev + delta };
                    ids.push(id);
                    prev = id;
                }
                None => break,
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gram::{GramExtractor, SparseGramExtractor};
    use crate::weights::Crc32Weigher;
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};

    fn build_and_open(files: &[(&str, &[u8])], dir: &Path) -> (Shard, HashMap<u64, Vec<u32>>) {
        let ex = SparseGramExtractor::new(Crc32Weigher);
        let mut builder = ShardBuilder::new(&ex.id());
        builder.set_commit_oid("0123456789abcdef0123456789abcdef01234567");
        let mut expected: HashMap<u64, Vec<u32>> = HashMap::new();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut hits = Vec::new();
            ex.grams(content, &mut hits);
            let hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
            for h in hashes.iter().copied().collect::<HashSet<_>>() {
                expected.entry(h).or_default().push(i as u32);
            }
            builder.add_file(path, hashes);
        }
        for ids in expected.values_mut() {
            ids.sort_unstable();
        }
        let dest = dir.join("shard.bin");
        builder.write(&dest).unwrap();
        (Shard::open(&dest).unwrap(), expected)
    }

    #[test]
    fn roundtrip_metadata_and_postings() {
        let dir = std::env::temp_dir().join(format!("gpx-shard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<(&str, &[u8])> = vec![
            ("src/a.rs", b"fn handleClick() { openMenu(); }" as &[u8]),
            ("src/b.rs", b"fn openMenu() { let x = MAX_FILE_SIZE; }"),
            ("README.md", b"gitpixel handleClick docs"),
        ];
        let (shard, expected) = build_and_open(&files, &dir);

        assert_eq!(shard.file_count(), 3);
        assert_eq!(shard.path_of(0), Some("src/a.rs"));
        assert_eq!(
            shard.commit_oid(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert!(shard.extractor_id().starts_with("sparse-crc32"));
        assert_eq!(shard.gram_count(), expected.len() as u64);

        for (h, ids) in &expected {
            assert_eq!(&shard.postings(*h), ids, "postings mismatch for {h:#x}");
        }
        assert!(shard.postings(0xdead_beef_dead_beef).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn covering_query_finds_containing_files() {
        let dir = std::env::temp_dir().join(format!("gpx-shard-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<(&str, &[u8])> = vec![
            ("a", b"the quick brown fox jumps over handleClick" as &[u8]),
            ("b", b"nothing interesting here at all"),
            ("c", b"more handleClick usage in this one"),
        ];
        let (shard, _) = build_and_open(&files, &dir);
        let ex = SparseGramExtractor::new(Crc32Weigher);

        let covering = ex.covering(b"handleClick");
        let q = crate::posting::GramQuery::And(
            covering.into_iter().map(crate::posting::GramQuery::Literal).collect(),
        );
        let candidates =
            crate::posting::resolve_query(&q, shard.file_count(), &|h| shard.postings(h));
        assert_eq!(candidates, vec![0, 2]);
        std::fs::remove_dir_all(&dir).ok();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn prop_shard_roundtrip(
            contents in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..300), 1..12),
        ) {
            let dir = std::env::temp_dir().join(format!(
                "gpx-shard-prop-{}-{:x}", std::process::id(),
                xxhash_rust::xxh3::xxh3_64(&contents.concat())));
            std::fs::create_dir_all(&dir).unwrap();
            let names: Vec<String> = (0..contents.len()).map(|i| format!("f{i}")).collect();
            let files: Vec<(&str, &[u8])> = names
                .iter()
                .zip(&contents)
                .map(|(n, c)| (n.as_str(), c.as_slice()))
                .collect();
            let (shard, expected) = build_and_open(&files, &dir);
            for (h, ids) in &expected {
                prop_assert_eq!(&shard.postings(*h), ids);
            }
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
