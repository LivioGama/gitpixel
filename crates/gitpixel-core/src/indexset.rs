//! Git-anchored 3-layer index: base shard + delta shard + dirty overlay.
//!
//! Layering (freshest wins, by path):
//! 1. **base.shard** — all tracked files at a pinned commit OID.
//! 2. **delta.shard** — files changed between base OID and current HEAD;
//!    modified/deleted base paths are tombstoned via `state.json`.
//! 3. **overlay** — in-memory gram sets for working-tree-dirty files (from
//!    `git status --porcelain`, updated live by the watcher); overlay paths
//!    tombstone their base/delta entries.
//!
//! Outside a git repo the set degrades to a plain single-shard index.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::delta::{delta_shard_path, DeltaState};
use crate::gram::GramExtractor;
use crate::index::{SearchStats, SHARD_DIR, SHARD_FILE};
use crate::overlay::Overlay;
use crate::plan::plan_pattern;
use crate::posting::{resolve_query, GramQuery};
use crate::shard::{Shard, ShardBuilder, ShardError};
use crate::verify::{MatchLine, Verifier, VerifyError};
use crate::{gitsync, index};

#[derive(Debug)]
pub enum IndexSetError {
    Shard(ShardError),
    Verify(VerifyError),
    Pattern(String),
    Io(std::io::Error),
    Index(index::IndexError),
}

impl From<ShardError> for IndexSetError {
    fn from(e: ShardError) -> Self {
        IndexSetError::Shard(e)
    }
}
impl From<VerifyError> for IndexSetError {
    fn from(e: VerifyError) -> Self {
        IndexSetError::Verify(e)
    }
}
impl From<std::io::Error> for IndexSetError {
    fn from(e: std::io::Error) -> Self {
        IndexSetError::Io(e)
    }
}
impl From<index::IndexError> for IndexSetError {
    fn from(e: index::IndexError) -> Self {
        IndexSetError::Index(e)
    }
}

impl std::fmt::Display for IndexSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexSetError::Shard(e) => write!(f, "{e}"),
            IndexSetError::Verify(e) => write!(f, "{e}"),
            IndexSetError::Pattern(e) => write!(f, "bad pattern: {e}"),
            IndexSetError::Io(e) => write!(f, "io error: {e}"),
            IndexSetError::Index(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for IndexSetError {}

#[derive(Debug, Clone)]
pub struct FreshnessStatus {
    pub commit_oid: Option<String>,
    pub base_files: u32,
    pub delta_files: u32,
    pub overlay_files: usize,
    pub tombstones: usize,
}

pub struct IndexSet {
    root: PathBuf,
    extractor: Box<dyn GramExtractor>,
    base: Shard,
    delta: Option<Shard>,
    /// Paths superseded between base OID and HEAD (from state.json).
    delta_tombstones: HashSet<String>,
    overlay: Overlay,
}

/// Paths inside our own sidecar dir are never indexed or tombstoned.
fn is_internal(rel: &str) -> bool {
    rel == SHARD_DIR || rel.starts_with(&format!("{SHARD_DIR}/"))
}

/// Read + extract one file for shard building. `None` = unreadable or binary.
fn extract_file(
    root: &Path,
    rel: &str,
    extractor: &dyn GramExtractor,
) -> Option<(String, Vec<u64>)> {
    let content = std::fs::read(root.join(rel)).ok()?;
    if content[..content.len().min(8192)].contains(&0) {
        return None;
    }
    let mut hits = Vec::new();
    extractor.grams(&content, &mut hits);
    let mut hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    Some((rel.to_string(), hashes))
}

/// Build a shard from an explicit repo-relative file list (parallel extract).
fn build_shard_from(
    root: &Path,
    rel_paths: &[String],
    extractor: &dyn GramExtractor,
    commit_oid: &str,
    dest: &Path,
) -> Result<Shard, IndexSetError> {
    let mut extracted: Vec<(String, Vec<u64>)> = rel_paths
        .par_iter()
        .filter(|rel| !is_internal(rel))
        .filter_map(|rel| extract_file(root, rel, extractor))
        .collect();
    extracted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut builder = ShardBuilder::new(&extractor.id());
    builder.set_commit_oid(commit_oid);
    for (rel, hashes) in extracted {
        builder.add_file(&rel, hashes);
    }
    builder.write(dest)?;
    Ok(Shard::open(dest)?)
}

impl IndexSet {
    /// Open the index at `root/.gitpixel`, (re)building layers as needed.
    pub fn open_or_build(
        root: &Path,
        extractor: Box<dyn GramExtractor>,
    ) -> Result<Self, IndexSetError> {
        let gpx_dir = root.join(SHARD_DIR);
        let base_path = gpx_dir.join(SHARD_FILE);
        let head = gitsync::rev_parse_head(root);

        // --- base layer ---
        let mut base = match Shard::open(&base_path) {
            Ok(s) if s.extractor_id() == extractor.id() => Some(s),
            _ => None,
        };
        // A git repo demands a git-anchored base; a plain-walk base (no OID)
        // cannot be delta'd against and is rebuilt.
        if head.is_some() && base.as_ref().is_some_and(|s| s.commit_oid().is_none()) {
            base = None;
        }
        let base = match base {
            Some(s) => s,
            None => {
                // Invalidate stale delta state alongside a base rebuild.
                std::fs::remove_file(delta_shard_path(&gpx_dir)).ok();
                std::fs::remove_file(crate::delta::state_path(&gpx_dir)).ok();
                match &head {
                    Some(oid) => {
                        let tracked = gitsync::ls_files(root);
                        let shard = build_shard_from(
                            root,
                            &tracked,
                            extractor.as_ref(),
                            oid,
                            &base_path,
                        )?;
                        DeltaState {
                            base_oid: oid.clone(),
                            delta_oid: None,
                            tombstones: Vec::new(),
                        }
                        .save(&gpx_dir)?;
                        shard
                    }
                    None => {
                        index::build(root, extractor.as_ref())?;
                        Shard::open(&base_path)?
                    }
                }
            }
        };

        let mut set = Self {
            root: root.to_path_buf(),
            extractor,
            base,
            delta: None,
            delta_tombstones: HashSet::new(),
            overlay: Overlay::new(),
        };

        // --- delta + overlay (git repos only) ---
        if let Some(head_oid) = head {
            let base_oid = set
                .base
                .commit_oid()
                .expect("git-anchored base has an oid")
                .to_string();
            if head_oid != base_oid {
                set.reconcile_delta(&gpx_dir, &base_oid, &head_oid)?;
            }
            // Dirty working tree -> overlay.
            for (xy, path) in gitsync::status_porcelain(root) {
                if is_internal(&path) {
                    continue;
                }
                if xy.contains('D') {
                    set.overlay.remove_file(&path);
                } else {
                    set.overlay
                        .refresh_file(root, &path, set.extractor.as_ref());
                }
            }
        }
        Ok(set)
    }

    /// Build or reuse the delta layer covering `base_oid..head_oid`.
    fn reconcile_delta(
        &mut self,
        gpx_dir: &Path,
        base_oid: &str,
        head_oid: &str,
    ) -> Result<(), IndexSetError> {
        let delta_path = delta_shard_path(gpx_dir);
        // Reuse a delta already pinned to this exact HEAD.
        if let Some(state) = DeltaState::load(gpx_dir) {
            if state.base_oid == base_oid && state.delta_oid.as_deref() == Some(head_oid) {
                if let Ok(s) = Shard::open(&delta_path) {
                    if s.extractor_id() == self.extractor.id() {
                        self.delta_tombstones = state.tombstones.into_iter().collect();
                        self.delta = Some(s);
                        return Ok(());
                    }
                }
            }
        }
        // Cumulative diff base..HEAD — simple and correct however HEAD moved.
        let diff = gitsync::diff_name_status(&self.root, base_oid, head_oid);
        let mut changed: Vec<String> = Vec::new();
        let mut tombstones: Vec<String> = Vec::new();
        for (status, path) in diff {
            if is_internal(&path) {
                continue;
            }
            match status {
                'A' => changed.push(path),
                'D' => tombstones.push(path),
                // M, T, and anything else: superseded in base + re-indexed.
                _ => {
                    tombstones.push(path.clone());
                    changed.push(path);
                }
            }
        }
        self.delta = if changed.is_empty() {
            std::fs::remove_file(&delta_path).ok();
            None
        } else {
            Some(build_shard_from(
                &self.root,
                &changed,
                self.extractor.as_ref(),
                head_oid,
                &delta_path,
            )?)
        };
        DeltaState {
            base_oid: base_oid.to_string(),
            delta_oid: Some(head_oid.to_string()),
            tombstones: tombstones.clone(),
        }
        .save(gpx_dir)?;
        self.delta_tombstones = tombstones.into_iter().collect();
        Ok(())
    }

    /// Re-extract one file from disk into the in-memory overlay (tombstoning
    /// its base/delta entry). Called by the daemon watcher on file change.
    pub fn refresh_file(&mut self, rel_path: &str) {
        let root = self.root.clone();
        self.overlay
            .refresh_file(&root, rel_path, self.extractor.as_ref());
    }

    /// Tombstone a deleted file everywhere.
    pub fn remove_file(&mut self, rel_path: &str) {
        self.overlay.remove_file(rel_path);
    }

    /// Merge-order query: base ∪ delta candidates − tombstones + overlay
    /// matches → verify every survivor with the real regex.
    pub fn search(&self, pattern: &str) -> Result<(Vec<MatchLine>, SearchStats), IndexSetError> {
        let started = std::time::Instant::now();
        let query = plan_pattern(pattern, self.extractor.as_ref())
            .map_err(|e| IndexSetError::Pattern(e.to_string()))?;
        let scanned_all = matches!(query, GramQuery::All);

        let mut paths: BTreeSet<String> = BTreeSet::new();
        // Base candidates, minus everything superseded by newer layers.
        for id in resolve_query(&query, self.base.file_count(), &|h| self.base.postings(h)) {
            if let Some(p) = self.base.path_of(id) {
                if !self.delta_tombstones.contains(p) && !self.overlay.tombstones.contains(p) {
                    paths.insert(p.to_string());
                }
            }
        }
        // Delta candidates, minus dirty-overlay supersessions.
        if let Some(delta) = &self.delta {
            for id in resolve_query(&query, delta.file_count(), &|h| delta.postings(h)) {
                if let Some(p) = delta.path_of(id) {
                    if !self.overlay.tombstones.contains(p) {
                        paths.insert(p.to_string());
                    }
                }
            }
        }
        // Overlay files whose in-memory gram set satisfies the plan.
        for p in self.overlay.matching_files(&query) {
            paths.insert(p.to_string());
        }

        let candidates: Vec<String> = paths.into_iter().collect();
        let verifier = Verifier::new(pattern)?;
        let results: Vec<Vec<MatchLine>> = candidates
            .par_iter()
            .filter_map(|rel| {
                let abs = self.root.join(rel);
                if !abs.is_file() {
                    return None; // deleted since indexing; skip.
                }
                let mut out = Vec::new();
                verifier.search_file(&abs, rel, &mut out).ok()?;
                (!out.is_empty()).then_some(out)
            })
            .collect();

        let mut matches: Vec<MatchLine> = results.into_iter().flatten().collect();
        matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
        let stats = SearchStats {
            candidates: candidates.len(),
            scanned_all,
            matches: matches.len(),
            elapsed_us: started.elapsed().as_micros(),
        };
        Ok((matches, stats))
    }

    pub fn status(&self) -> FreshnessStatus {
        FreshnessStatus {
            commit_oid: self.base.commit_oid().map(str::to_string),
            base_files: self.base.file_count(),
            delta_files: self.delta.as_ref().map_or(0, Shard::file_count),
            overlay_files: self.overlay.files.len(),
            tombstones: self.delta_tombstones.len() + self.overlay.tombstones.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gram::SparseGramExtractor;
    use crate::weights::Crc32Weigher;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    }

    fn ex() -> Box<dyn GramExtractor> {
        Box::new(SparseGramExtractor::new(Crc32Weigher))
    }

    #[test]
    fn git_anchored_layers_end_to_end() {
        let dir = std::env::temp_dir().join(format!("gpx-indexset-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("alpha.rs"), "fn handleClick() {}\n").unwrap();
        std::fs::write(dir.join("beta.rs"), "fn openMenuWidget() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);

        // Base layer finds committed content.
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let st = set.status();
        assert!(st.commit_oid.is_some());
        assert_eq!(st.base_files, 2);
        let (m, _) = set.search("handleClick").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "alpha.rs");

        // Commit a change + a new file -> reopened set builds a delta layer.
        std::fs::write(dir.join("alpha.rs"), "fn renamedEntryPoint() {}\n").unwrap();
        std::fs::write(dir.join("gamma.rs"), "fn freshDeltaSymbol() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "two"]);
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let st = set.status();
        assert_eq!(st.delta_files, 2, "alpha (modified) + gamma (added)");
        let (m, _) = set.search("freshDeltaSymbol").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "gamma.rs");
        let (m, _) = set.search("handleClick").unwrap();
        assert!(m.is_empty(), "old base content is tombstoned by the delta");

        // Overlay: uncommitted edit is visible without a rebuild.
        let mut set = set;
        std::fs::write(dir.join("beta.rs"), "fn overlayOnlySymbol() {}\n").unwrap();
        set.refresh_file("beta.rs");
        let (m, _) = set.search("overlayOnlySymbol").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "beta.rs");
        let (m, _) = set.search("openMenuWidget").unwrap();
        assert!(m.is_empty(), "overlay tombstones the stale base entry");

        // remove_file tombstones everywhere.
        std::fs::remove_file(dir.join("gamma.rs")).unwrap();
        set.remove_file("gamma.rs");
        let (m, _) = set.search("freshDeltaSymbol").unwrap();
        assert!(m.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_git_plain_build() {
        let dir = std::env::temp_dir().join(format!("gpx-indexset-nogit-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.txt"), "plainWalkNeedle here\n").unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert!(set.status().commit_oid.is_none());
        let (m, _) = set.search("plainWalkNeedle").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "solo.txt");
        std::fs::remove_dir_all(&dir).ok();
    }
}
