//! Whole-repo build / single-file update orchestration.
//!
//! `build_graph`: walk → parallel extract (rayon) → single-writer store
//! phase (files+symbols, then imports, then tiered call resolution).
//! `update_file`: transactional per-file replacement + re-resolution.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use ignore::WalkBuilder;
use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::extract::{extract_file, lang_of, FileExtraction};
use crate::imports::resolve_import;
use crate::resolve::{resolve_all, resolve_calls, FileCalls, PendingCall};
use crate::store::{EdgeKind, GraphStore};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub struct GraphStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub unresolved: u64,
    pub elapsed_ms: u128,
}

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

fn is_binary(content: &[u8]) -> bool {
    let end = content.len().min(BINARY_SNIFF_BYTES);
    content[..end].contains(&0)
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

/// Walk `root` collecting supported source files (skips .gitpixel, hidden
/// files, gitignored paths, binaries, oversized files).
fn collect_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .filter_entry(|e| e.file_name().to_string_lossy() != ".gitpixel")
        .build();
    for entry in walker.flatten() {
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let Some(rel) = rel_path(root, entry.path()) else { continue };
        if lang_of(&rel).is_none() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
        }
        let Ok(content) = std::fs::read(entry.path()) else { continue };
        if is_binary(&content) {
            continue;
        }
        out.push((rel, content));
    }
    out
}

struct Extracted {
    rel: String,
    blob_oid: String,
    fx: FileExtraction,
}

/// Full graph build: parse everything in parallel, then write files,
/// symbols, imports, and resolved call edges.
pub fn build_graph(root: &Path, db_path: &Path) -> Result<GraphStats, BoxErr> {
    let t0 = Instant::now();

    let inputs = collect_files(root);
    let extracted: Vec<Extracted> = inputs
        .par_iter()
        .filter_map(|(rel, content)| {
            let fx = extract_file(rel, content)?;
            Some(Extracted {
                rel: rel.clone(),
                blob_oid: format!("{:016x}", xxh3_64(content)),
                fx,
            })
        })
        .collect();

    let all_paths: Vec<String> = extracted.iter().map(|e| e.rel.clone()).collect();

    let mut store = GraphStore::open(db_path)?;

    // Drop files that vanished since the last build.
    let known: std::collections::HashSet<&str> = all_paths.iter().map(|s| s.as_str()).collect();
    let stale: Vec<String> = store
        .files()?
        .into_iter()
        .filter(|f| !known.contains(f.path.as_str()))
        .map(|f| f.path)
        .collect();
    for path in &stale {
        store.remove_file(path)?;
    }

    // Pass 1: files + symbols (need every file id before import resolution).
    let mut path_to_id: HashMap<String, i64> = HashMap::new();
    let mut sym_ids: Vec<Vec<i64>> = Vec::with_capacity(extracted.len());
    for e in &extracted {
        let file_id = store.replace_file(&e.rel, &e.blob_oid, e.fx.lang)?;
        path_to_id.insert(e.rel.clone(), file_id);
        let mut ids = Vec::with_capacity(e.fx.symbols.len());
        for s in &e.fx.symbols {
            let uid = format!("{}#{}#{}", e.rel, s.qualified, s.kind.as_str());
            let id = store.insert_symbol(
                file_id,
                &uid,
                &s.name,
                &s.qualified,
                s.kind,
                s.start_line,
                s.end_line,
                &s.sig,
            )?;
            ids.push(id);
        }
        sym_ids.push(ids);
    }

    // Pass 2: imports (resolved against the full file list) + pending calls.
    let mut pending: Vec<FileCalls> = Vec::with_capacity(extracted.len());
    for (i, e) in extracted.iter().enumerate() {
        let file_id = path_to_id[&e.rel];
        for imp in &e.fx.imports {
            let resolved = resolve_import(&imp.spec, &e.rel, &all_paths)
                .and_then(|p| path_to_id.get(&p).copied());
            store.insert_import(file_id, &imp.spec, resolved)?;
        }
        let calls = e
            .fx
            .calls
            .iter()
            .map(|c| PendingCall {
                callee_name: c.callee_name.clone(),
                enclosing_symbol_id: c.enclosing_index.map(|ix| sym_ids[i][ix]),
                site_line: c.site_line,
            })
            .collect();
        pending.push(FileCalls { file_id, calls });
    }

    resolve_calls(&store, &pending)?;

    let (files, symbols, edges, unresolved) = store.counts()?;
    Ok(GraphStats { files, symbols, edges, unresolved, elapsed_ms: t0.elapsed().as_millis() })
}

/// Incrementally re-index one file: preserve incoming call knowledge as
/// unresolved rows, replace the file, re-extract, re-resolve its own calls,
/// then retry every unresolved call repo-wide.
pub fn update_file(root: &Path, db_path: &Path, rel: &str) -> Result<(), BoxErr> {
    let mut store = GraphStore::open(db_path)?;
    let abs = root.join(rel);

    // Demote incoming call edges (from OTHER files) into unresolved rows so
    // they can re-link after the rebuild instead of being silently dropped.
    if let Some(old) = store.file_by_path(rel)? {
        let old_syms = store.symbols_in_file(old.id)?;
        let mut demoted: Vec<(i64, String, i64, u32)> = Vec::new();
        for sym in &old_syms {
            for edge in store.edges_to(sym.id, Some(EdgeKind::Calls))? {
                let src_file: Option<i64> = store
                    .conn()
                    .query_row(
                        "SELECT file_id FROM symbols WHERE id = ?1",
                        rusqlite::params![edge.src_id],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(src_file) = src_file {
                    if src_file != old.id {
                        demoted.push((src_file, sym.name.clone(), edge.src_id, edge.site_line));
                    }
                }
            }
        }
        for (src_file, name, src_id, site_line) in demoted {
            store.insert_unresolved_call(src_file, &name, Some(src_id), site_line)?;
        }
    }

    let Ok(content) = std::fs::read(&abs) else {
        // File deleted: drop it, then let survivors re-resolve.
        store.remove_file(rel)?;
        resolve_all(&mut store)?;
        return Ok(());
    };

    let Some(fx) = extract_file(rel, &content) else {
        store.remove_file(rel)?;
        resolve_all(&mut store)?;
        return Ok(());
    };

    let blob_oid = format!("{:016x}", xxh3_64(&content));
    let file_id = store.replace_file(rel, &blob_oid, fx.lang)?;

    let mut ids = Vec::with_capacity(fx.symbols.len());
    for s in &fx.symbols {
        let uid = format!("{rel}#{}#{}", s.qualified, s.kind.as_str());
        let id = store.insert_symbol(
            file_id,
            &uid,
            &s.name,
            &s.qualified,
            s.kind,
            s.start_line,
            s.end_line,
            &s.sig,
        )?;
        ids.push(id);
    }

    let all_paths: Vec<String> = store.files()?.into_iter().map(|f| f.path).collect();
    let path_to_id: HashMap<String, i64> = {
        let mut m = HashMap::new();
        for f in store.files()? {
            m.insert(f.path, f.id);
        }
        m
    };
    for imp in &fx.imports {
        let resolved =
            resolve_import(&imp.spec, rel, &all_paths).and_then(|p| path_to_id.get(&p).copied());
        store.insert_import(file_id, &imp.spec, resolved)?;
    }

    let calls = fx
        .calls
        .iter()
        .map(|c| PendingCall {
            callee_name: c.callee_name.clone(),
            enclosing_symbol_id: c.enclosing_index.map(|ix| ids[ix]),
            site_line: c.site_line,
        })
        .collect();
    resolve_calls(&store, &[FileCalls { file_id, calls }])?;

    // Retry everything unresolved (including the demoted incoming calls).
    resolve_all(&mut store)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Tier;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gitpixel-graph-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn smoke_build_graph_ts_and_rust() {
        let root = tmpdir("smoke");
        std::fs::write(
            root.join("a.ts"),
            "export function greet(name: string): string {\n  return \"hi \" + name;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() {\n  return greet(\"x\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c.rs"),
            "fn helper() -> u32 { 1 }\nfn run() -> u32 { helper() }\n",
        )
        .unwrap();

        let db = root.join(".gitpixel").join("graph.db");
        let stats = build_graph(&root, &db).unwrap();
        assert_eq!(stats.files, 3, "all three files indexed");
        assert!(stats.symbols >= 4, "greet, main, helper, run: {}", stats.symbols);
        assert!(stats.edges >= 2, "cross-file + same-file call edges: {}", stats.edges);

        let store = GraphStore::open(&db).unwrap();
        // Cross-file: main -> greet must be an Exact (T1 import-resolved) edge.
        let greet = &store.symbols_by_name("greet", 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 1, "exactly one caller of greet");
        assert_eq!(callers[0].tier, Tier::Exact);
        let main_sym = &store.symbols_by_name("main", 10).unwrap()[0];
        assert_eq!(callers[0].src_id, main_sym.id, "caller is b.ts main");
        // Same-file Rust: run -> helper Exact (T0).
        let helper = &store.symbols_by_name("helper", 10).unwrap()[0];
        let hcallers = store.edges_to(helper.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(hcallers.len(), 1);
        assert_eq!(hcallers[0].tier, Tier::Exact);
        // Sanity: counts agree with stats.
        let (f, s, e, _u) = store.counts().unwrap();
        assert_eq!((f, s, e), (stats.files, stats.symbols, stats.edges));
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smoke_update_file_relinks_callers() {
        let root = tmpdir("update");
        std::fs::write(root.join("a.ts"), "export function greet() { return 1 }\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() { return greet() }\n",
        )
        .unwrap();
        let db = root.join(".gitpixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        // Edit a.ts (same symbol, new body) and update just that file.
        std::fs::write(root.join("a.ts"), "export function greet() { return 2 }\n").unwrap();
        update_file(&root, &db, "a.ts").unwrap();

        let store = GraphStore::open(&db).unwrap();
        let greet = &store.symbols_by_name("greet", 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 1, "caller edge survives an incremental update");
        assert_eq!(callers[0].tier, Tier::Exact);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }
}
