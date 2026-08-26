//! Tiered call-graph resolution.
//!
//! Tiers (a name NEVER fans out to multiple definition sites as edges):
//! - T0: callee defined in the same file → `Exact`
//! - T1: callee defined in exactly one file the caller imports → `Exact`
//! - T2: callee name defined in exactly one file repo-wide → `Probable`
//! - otherwise → `unresolved_calls` row (feeds the epistemic envelope)

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::store::{EdgeKind, EdgeRow, GraphStore, StoreError, SymbolKind, Tier};

#[derive(Debug, Default, Clone)]
pub struct ResolveStats {
    pub exact: u64,
    pub probable: u64,
    pub unresolved: u64,
}

/// One extracted call site awaiting resolution (symbol ids already assigned).
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub callee_name: String,
    pub enclosing_symbol_id: Option<i64>,
    pub site_line: u32,
}

/// All pending calls of one file.
#[derive(Debug, Clone)]
pub struct FileCalls {
    pub file_id: i64,
    pub calls: Vec<PendingCall>,
}

/// Per-call resolution decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Exact(i64),
    Probable(i64),
    Unresolved,
}

#[derive(Clone, Copy)]
struct Candidate {
    file_id: i64,
    symbol_id: i64,
    kind: SymbolKind,
    start_line: u32,
}

/// Symbol-name index + import graph snapshot used for tier decisions.
pub struct ResolveIndex {
    by_name: HashMap<String, Vec<Candidate>>,
    imports_of: HashMap<i64, HashSet<i64>>,
}

fn callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Class | SymbolKind::Struct
    )
}

fn kind_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Struct => 3,
        _ => 9,
    }
}

fn best(cands: &[Candidate]) -> Option<i64> {
    cands
        .iter()
        .min_by_key(|c| (kind_priority(c.kind), c.start_line, c.symbol_id))
        .map(|c| c.symbol_id)
}

impl ResolveIndex {
    pub fn build(store: &GraphStore) -> Result<Self, StoreError> {
        let conn = store.conn();
        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT name, file_id, id, kind, start_line FROM symbols")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Candidate {
                        file_id: r.get(1)?,
                        symbol_id: r.get(2)?,
                        kind: SymbolKind::parse(&r.get::<_, String>(3)?),
                        start_line: r.get(4)?,
                    },
                ))
            })?;
            for row in rows {
                let (name, cand) = row?;
                if callable(cand.kind) {
                    by_name.entry(name).or_default().push(cand);
                }
            }
        }
        let mut imports_of: HashMap<i64, HashSet<i64>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT file_id, resolved_file_id FROM imports WHERE resolved_file_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (fid, dst) = row?;
                imports_of.entry(fid).or_default().insert(dst);
            }
        }
        Ok(Self { by_name, imports_of })
    }

    /// The tier decision for one call from `caller_file_id` to `name`.
    pub fn decide(&self, caller_file_id: i64, name: &str) -> Decision {
        let Some(cands) = self.by_name.get(name) else {
            return Decision::Unresolved;
        };
        // T0: same file.
        let same_file: Vec<Candidate> =
            cands.iter().copied().filter(|c| c.file_id == caller_file_id).collect();
        if let Some(id) = best(&same_file) {
            return Decision::Exact(id);
        }
        // T1: defined in exactly one imported file.
        if let Some(imported) = self.imports_of.get(&caller_file_id) {
            let hits: Vec<Candidate> =
                cands.iter().copied().filter(|c| imported.contains(&c.file_id)).collect();
            let files: HashSet<i64> = hits.iter().map(|c| c.file_id).collect();
            if files.len() == 1 {
                if let Some(id) = best(&hits) {
                    return Decision::Exact(id);
                }
            }
            if files.len() > 1 {
                return Decision::Unresolved; // ambiguous — never fan out
            }
        }
        // T2: unique definition file repo-wide.
        let files: HashSet<i64> = cands.iter().map(|c| c.file_id).collect();
        if files.len() == 1 {
            if let Some(id) = best(cands) {
                return Decision::Probable(id);
            }
        }
        Decision::Unresolved
    }
}

/// Resolve the given in-memory pending calls, writing edges / unresolved
/// rows into the store. Used by `build::build_graph` after extraction.
pub fn resolve_calls(
    store: &GraphStore,
    pending: &[FileCalls],
) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for fc in pending {
        for call in &fc.calls {
            let Some(src_id) = call.enclosing_symbol_id else {
                // Top-level call site: no source symbol to hang an edge on.
                store.insert_unresolved_call(fc.file_id, &call.callee_name, None, call.site_line)?;
                stats.unresolved += 1;
                continue;
            };
            match idx.decide(fc.file_id, &call.callee_name) {
                Decision::Exact(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Exact,
                        site_line: call.site_line,
                    })?;
                    stats.exact += 1;
                }
                Decision::Probable(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Probable,
                        site_line: call.site_line,
                    })?;
                    stats.probable += 1;
                }
                Decision::Unresolved => {
                    store.insert_unresolved_call(
                        fc.file_id,
                        &call.callee_name,
                        Some(src_id),
                        call.site_line,
                    )?;
                    stats.unresolved += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Re-attempt resolution of every stored `unresolved_calls` row against the
/// current index. Rows that resolve become edges and are deleted; the rest
/// stay (keeping the epistemic envelope honest). Used after incremental
/// updates so callers into a rebuilt file re-link.
pub fn resolve_all(store: &mut GraphStore) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    struct Row {
        id: i64,
        file_id: i64,
        name: String,
        enclosing: i64,
        site_line: u32,
    }
    let rows: Vec<Row> = {
        let mut stmt = store.conn().prepare(
            "SELECT u.id, u.file_id, u.name, u.enclosing_symbol_id, u.site_line
               FROM unresolved_calls u
               JOIN symbols s ON s.id = u.enclosing_symbol_id
              WHERE u.enclosing_symbol_id IS NOT NULL",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                file_id: r.get(1)?,
                name: r.get(2)?,
                enclosing: r.get(3)?,
                site_line: r.get(4)?,
            })
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    let mut stats = ResolveStats::default();
    for row in &rows {
        let decision = idx.decide(row.file_id, &row.name);
        let (dst, tier) = match decision {
            Decision::Exact(d) => (d, Tier::Exact),
            Decision::Probable(d) => (d, Tier::Probable),
            Decision::Unresolved => {
                stats.unresolved += 1;
                continue;
            }
        };
        store.insert_edge(&EdgeRow {
            src_id: row.enclosing,
            dst_id: dst,
            kind: EdgeKind::Calls,
            tier,
            site_line: row.site_line,
        })?;
        store
            .conn()
            .execute("DELETE FROM unresolved_calls WHERE id = ?1", params![row.id])?;
        match tier {
            Tier::Exact => stats.exact += 1,
            Tier::Probable => stats.probable += 1,
        }
    }
    Ok(stats)
}
