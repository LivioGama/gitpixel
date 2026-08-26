//! Transport-agnostic service: one `Request` in, one `Response` out.
//!
//! All cross-crate contract calls (graph analyses, context rendering) are
//! centralized in the `bridge` module at the bottom so integration drift is
//! a one-line fix per call site.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use gitpixel_core::TrigramExtractor;
use gitpixel_core::indexset::{IndexSet, IndexSetError};
use gitpixel_graph::{EdgeKind, EdgeRow, GraphStore, SymbolKind, SymbolRow};

pub const GRAPH_DB_FILE: &str = "graph.db";

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ServeError {
    Index(IndexSetError),
    Io(std::io::Error),
    Msg(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Index(e) => write!(f, "{e}"),
            ServeError::Io(e) => write!(f, "io error: {e}"),
            ServeError::Msg(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for ServeError {}

impl From<IndexSetError> for ServeError {
    fn from(e: IndexSetError) -> Self {
        ServeError::Index(e)
    }
}
impl From<std::io::Error> for ServeError {
    fn from(e: std::io::Error) -> Self {
        ServeError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Search {
        pattern: String,
        #[serde(default)]
        json: bool,
        #[serde(default)]
        limit: Option<usize>,
    },
    Symbol {
        name: String,
    },
    Context {
        uid: String,
        #[serde(default)]
        budget_tokens: Option<usize>,
    },
    Impact {
        uid_or_name: String,
        direction: String,
        #[serde(default)]
        depth: Option<u32>,
    },
    Uses {
        uid_or_name: String,
        /// "callers" | "callees"
        role: String,
    },
    Trace {
        from: String,
        to: String,
    },
    Processes {},
    Clusters {},
    Changes {
        #[serde(default)]
        base: Option<String>,
    },
    Status {},
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub error: Option<String>,
    pub data: Value,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Response { ok: true, error: None, data }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, error: Some(msg.into()), data: Value::Null }
    }
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

pub struct Service {
    root: PathBuf,
    index: IndexSet,
    graph: Option<GraphStore>,
}

impl Service {
    /// Open (building layers if needed) the text index; graph db is lazy.
    pub fn open(root: &Path) -> Result<Self, ServeError> {
        let root = root
            .canonicalize()
            .map_err(|e| ServeError::Msg(format!("bad root {}: {e}", root.display())))?;
        let index = IndexSet::open_or_build(&root, Box::new(TrigramExtractor))?;
        Ok(Service { root, index, graph: None })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.root.join(gitpixel_core::index::SHARD_DIR).join(GRAPH_DB_FILE)
    }

    /// Watcher hook: refresh one file in index + graph (best effort).
    pub fn refresh_file(&mut self, rel: &str) {
        self.index.refresh_file(rel);
        let db = self.graph_db_path();
        if db.exists() {
            bridge::update_file(&self.root, &db, rel);
            // Drop the cached handle so the next read sees the update.
            self.graph = None;
        }
    }

    /// Watcher hook: file deleted.
    pub fn remove_file(&mut self, rel: &str) {
        self.index.remove_file(rel);
        let db = self.graph_db_path();
        if db.exists() {
            if let Ok(mut store) = GraphStore::open(&db) {
                let _ = store.remove_file(rel);
            }
            self.graph = None;
        }
    }

    /// Make sure `self.graph` is populated; builds graph.db on first use.
    /// Returns build info (stats + timing) when a build happened.
    fn ensure_graph(&mut self) -> Result<Option<Value>, String> {
        if self.graph.is_some() {
            return Ok(None);
        }
        let db = self.graph_db_path();
        let built = if db.exists() {
            None
        } else {
            let t = Instant::now();
            let stats = bridge::build_graph(&self.root, &db)?;
            Some(json!({
                "graph_built": true,
                "build_ms": t.elapsed().as_millis() as u64,
                "stats": stats,
            }))
        };
        let store = GraphStore::open(&db).map_err(|e| e.to_string())?;
        self.graph = Some(store);
        Ok(built)
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match self.dispatch(req) {
            Ok(v) => Response::ok(v),
            Err(e) => Response::err(e),
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(json!({"pong": true, "root": self.root.display().to_string()})),
            Request::Shutdown => Ok(json!({"shutting_down": true})),
            Request::Search { pattern, json: _, limit } => self.op_search(&pattern, limit),
            Request::Symbol { name } => self.op_symbol(&name),
            Request::Context { uid, budget_tokens } => self.op_context(&uid, budget_tokens),
            Request::Impact { uid_or_name, direction, depth } => {
                self.op_impact(&uid_or_name, &direction, depth)
            }
            Request::Uses { uid_or_name, role } => self.op_uses(&uid_or_name, &role),
            Request::Trace { from, to } => self.op_trace(&from, &to),
            Request::Processes {} => self.op_processes(),
            Request::Clusters {} => self.op_clusters(),
            Request::Changes { base } => self.op_changes(base.as_deref()),
            Request::Status {} => self.op_status(),
        }
    }

    // -- ops ---------------------------------------------------------------

    fn op_search(&self, pattern: &str, limit: Option<usize>) -> Result<Value, String> {
        let (matches, stats) = self.index.search(pattern).map_err(|e| e.to_string())?;
        let shown = limit.unwrap_or(usize::MAX);
        let arr: Vec<Value> = matches
            .iter()
            .take(shown)
            .map(|m| json!({"path": m.path, "line": m.line_number, "text": m.line}))
            .collect();
        Ok(json!({
            "matches": arr,
            "truncated": matches.len() > arr.len(),
            "stats": {
                "candidates": stats.candidates,
                "scanned_all": stats.scanned_all,
                "matches": stats.matches,
                "elapsed_us": stats.elapsed_us as u64,
            }
        }))
    }

    fn op_symbol(&mut self, name: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let syms = store.symbols_by_name(name, 50).map_err(|e| e.to_string())?;
        let envelope = store.envelope_for_name(name).map_err(|e| e.to_string())?;
        let mut out = json!({
            "symbols": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_context(&mut self, uid: &str, budget_tokens: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let sym = store
            .symbol_by_uid(uid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no symbol with uid {uid:?}"))?;
        let envelope = store.envelope_for_name(&sym.name).map_err(|e| e.to_string())?;

        let incoming = store.edges_to(sym.id, None).map_err(|e| e.to_string())?;
        let outgoing = store.edges_from(sym.id, None).map_err(|e| e.to_string())?;
        let incoming_by_kind = edges_by_kind(store, &incoming, &files, false)?;
        let outgoing_by_kind = edges_by_kind(store, &outgoing, &files, true)?;

        // Budget-fitted text rendering (gitpixel-context) — best effort.
        let mut items = Vec::new();
        items.push(context_item(&self.root, &sym, &files));
        for e in incoming.iter().take(20) {
            if let Some(other) = symbol_by_id(store, e.src_id) {
                items.push(context_item(&self.root, &other, &files));
            }
        }
        for e in outgoing.iter().take(20) {
            if let Some(other) = symbol_by_id(store, e.dst_id) {
                items.push(context_item(&self.root, &other, &files));
            }
        }
        let text = bridge::render_context(&items, budget_tokens.unwrap_or(2000));

        let mut out = json!({
            "symbol": symbol_json(&sym, &files),
            "incoming": incoming_by_kind,
            "outgoing": outgoing_by_kind,
            "envelope": envelope,
            "text": text,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_impact(
        &mut self,
        uid_or_name: &str,
        direction: &str,
        depth: Option<u32>,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return Ok(candidates_value(store, &v)?),
        };
        let mut out = bridge::impact(store, &sym.uid, direction, depth.unwrap_or(3))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_uses(&mut self, uid_or_name: &str, role: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return Ok(candidates_value(store, &v)?),
        };
        let files = file_map(store)?;
        let (edges, other_is_src) = match role {
            "callees" => (store.edges_from(sym.id, Some(EdgeKind::Calls)), false),
            _ => (store.edges_to(sym.id, Some(EdgeKind::Calls)), true),
        };
        let edges = edges.map_err(|e| e.to_string())?;
        let mut arr = Vec::new();
        for e in &edges {
            let other_id = if other_is_src { e.src_id } else { e.dst_id };
            let other = symbol_by_id(store, other_id);
            arr.push(json!({
                "symbol": other.as_ref().map(|s| symbol_json(s, &files)),
                "tier": e.tier.as_str(),
                "site_line": e.site_line,
            }));
        }
        let envelope = store.envelope_for_name(&sym.name).map_err(|e| e.to_string())?;
        let mut out = json!({
            "symbol": symbol_json(&sym, &files),
            "role": if role == "callees" { "callees" } else { "callers" },
            "edges": arr,
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_trace(&mut self, from: &str, to: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let from_sym = match resolve_symbol(store, from)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return Ok(candidates_value(store, &v)?),
        };
        let to_sym = match resolve_symbol(store, to)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return Ok(candidates_value(store, &v)?),
        };
        let mut out = bridge::trace(store, &from_sym.uid, &to_sym.uid)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_processes(&mut self) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::processes(store)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_clusters(&mut self) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::clusters(store)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_changes(&mut self, base: Option<&str>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let root = self.root.clone();
        let store = self.graph.as_ref().unwrap();
        let mut out = bridge::changes(store, &root, base)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_status(&mut self) -> Result<Value, String> {
        let s = self.index.status();
        let db = self.graph_db_path();
        let graph = if db.exists() {
            match GraphStore::open(&db) {
                Ok(store) => {
                    let (files, symbols, edges, unresolved) =
                        store.counts().map_err(|e| e.to_string())?;
                    json!({
                        "present": true,
                        "files": files,
                        "symbols": symbols,
                        "edges": edges,
                        "unresolved_calls": unresolved,
                    })
                }
                Err(e) => json!({"present": true, "error": e.to_string()}),
            }
        } else {
            json!({"present": false})
        };
        Ok(json!({
            "root": self.root.display().to_string(),
            "index": {
                "commit_oid": s.commit_oid,
                "base_files": s.base_files,
                "delta_files": s.delta_files,
                "overlay_files": s.overlay_files,
                "tombstones": s.tombstones,
            },
            "graph": graph,
        }))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

enum Resolved {
    One(SymbolRow),
    Many(Vec<SymbolRow>),
}

/// `uid_or_name` protocol: '#' means uid; otherwise a name, with the
/// disambiguation protocol (`{candidates: [...], hint}`) on ambiguity.
fn resolve_symbol(store: &GraphStore, uid_or_name: &str) -> Result<Resolved, String> {
    if uid_or_name.contains('#') {
        return store
            .symbol_by_uid(uid_or_name)
            .map_err(|e| e.to_string())?
            .map(Resolved::One)
            .ok_or_else(|| format!("no symbol with uid {uid_or_name:?}"));
    }
    let syms = store.symbols_by_name(uid_or_name, 50).map_err(|e| e.to_string())?;
    match syms.len() {
        0 => Err(format!("no symbol named {uid_or_name:?}")),
        1 => Ok(Resolved::One(syms.into_iter().next().unwrap())),
        _ => Ok(Resolved::Many(syms)),
    }
}

fn candidates_value(store: &GraphStore, syms: &[SymbolRow]) -> Result<Value, String> {
    let files = file_map(store)?;
    Ok(json!({
        "candidates": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
        "hint": "ambiguous name; re-call with uid",
    }))
}

fn file_map(store: &GraphStore) -> Result<HashMap<i64, String>, String> {
    Ok(store
        .files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
}

fn symbol_json(s: &SymbolRow, files: &HashMap<i64, String>) -> Value {
    json!({
        "uid": s.uid,
        "name": s.name,
        "qualified": s.qualified,
        "kind": s.kind.as_str(),
        "path": files.get(&s.file_id).cloned().unwrap_or_default(),
        "start_line": s.start_line,
        "end_line": s.end_line,
        "sig": s.sig,
    })
}

/// Public-API `GraphStore` has uid/name lookups only; edge rows carry raw
/// ids, so resolve them through the sanctioned `conn()` escape hatch.
fn symbol_by_id(store: &GraphStore, id: i64) -> Option<SymbolRow> {
    store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .ok()
}

fn edges_by_kind(
    store: &GraphStore,
    edges: &[EdgeRow],
    files: &HashMap<i64, String>,
    other_is_dst: bool,
) -> Result<Value, String> {
    let mut grouped: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for e in edges {
        let other_id = if other_is_dst { e.dst_id } else { e.src_id };
        let other = symbol_by_id(store, other_id);
        grouped.entry(e.kind.as_str()).or_default().push(json!({
            "symbol": other.as_ref().map(|s| symbol_json(s, files)),
            "tier": e.tier.as_str(),
            "site_line": e.site_line,
        }));
    }
    Ok(serde_json::to_value(grouped).unwrap_or(Value::Null))
}

fn context_item(root: &Path, s: &SymbolRow, files: &HashMap<i64, String>) -> bridge::Item {
    let path = files.get(&s.file_id).cloned().unwrap_or_default();
    let snippet = read_snippet(&root.join(&path), s.start_line, s.end_line, 60);
    bridge::Item {
        name: s.name.clone(),
        kind: s.kind.as_str().to_string(),
        path,
        start_line: s.start_line,
        end_line: s.end_line,
        sig: s.sig.clone(),
        snippet,
    }
}

fn read_snippet(abs: &Path, start_line: u32, end_line: u32, max_lines: usize) -> String {
    let Ok(content) = std::fs::read_to_string(abs) else {
        return String::new();
    };
    let start = start_line.saturating_sub(1) as usize;
    content
        .lines()
        .skip(start)
        .take(((end_line as usize).saturating_sub(start)).min(max_lines).max(1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn merge_build_info(out: &mut Value, built: Option<Value>) {
    if let (Some(info), Some(obj)) = (built, out.as_object_mut()) {
        obj.insert("graph_build".into(), info);
    }
}

// ---------------------------------------------------------------------------
// bridge — the ONLY place that calls concurrently-developed crate APIs.
// Each fn is one call deep so contract drift is a one-line fix.
// ---------------------------------------------------------------------------

mod bridge {
    use super::{Value, es, to_val};
    use gitpixel_graph::GraphStore;
    use std::path::Path;

    /// Neutral mirror of `gitpixel_context::ContextItem`.
    pub struct Item {
        pub name: String,
        pub kind: String,
        pub path: String,
        pub start_line: u32,
        pub end_line: u32,
        pub sig: String,
        pub snippet: String,
    }

    pub fn build_graph(root: &Path, db: &Path) -> Result<Value, String> {
        let s = gitpixel_graph::build::build_graph(root, db).map_err(es)?;
        Ok(serde_json::json!({
            "files": s.files,
            "symbols": s.symbols,
            "edges": s.edges,
            "unresolved": s.unresolved,
            "elapsed_ms": s.elapsed_ms as u64,
        }))
    }

    pub fn update_file(root: &Path, db: &Path, rel: &str) {
        let _ = gitpixel_graph::build::update_file(root, db, rel);
    }

    pub fn impact(
        store: &GraphStore,
        uid: &str,
        direction: &str,
        depth: u32,
    ) -> Result<Value, String> {
        use gitpixel_graph::impact::{Direction, impact};
        let dir = if direction == "downstream" {
            Direction::Downstream
        } else {
            Direction::Upstream
        };
        impact(store, uid, dir, depth, 50).map(to_val).map_err(es)
    }

    pub fn trace(store: &GraphStore, from_uid: &str, to_uid: &str) -> Result<Value, String> {
        gitpixel_graph::trace::trace(store, from_uid, to_uid, 8).map(to_val).map_err(es)
    }

    pub fn processes(store: &mut GraphStore) -> Result<Value, String> {
        use gitpixel_graph::process;
        let listed = process::list(store).map_err(es)?;
        let v = if listed.is_empty() {
            process::discover(store, 10, 4, 3, 75).map_err(es)?
        } else {
            listed
        };
        Ok(serde_json::json!({ "processes": to_val(v) }))
    }

    pub fn clusters(store: &mut GraphStore) -> Result<Value, String> {
        use gitpixel_graph::cluster;
        let listed = cluster::list(store).map_err(es)?;
        let v = if listed.is_empty() { cluster::compute(store).map_err(es)? } else { listed };
        Ok(serde_json::json!({ "clusters": to_val(v) }))
    }

    pub fn changes(store: &GraphStore, root: &Path, base: Option<&str>) -> Result<Value, String> {
        gitpixel_graph::changes::detect(store, root, base).map(to_val).map_err(es)
    }

    /// Budget-fitted text rendering via gitpixel-context; empty on any miss.
    pub fn render_context(items: &[Item], budget_tokens: usize) -> String {
        use gitpixel_context::{ContextItem, Layer, fit_to_budget};
        let mapped: Vec<ContextItem> = items
            .iter()
            .map(|i| ContextItem {
                name: i.name.clone(),
                kind: i.kind.clone(),
                path: i.path.clone(),
                start_line: i.start_line,
                end_line: i.end_line,
                sig: i.sig.clone(),
                snippet: i.snippet.clone(),
            })
            .collect();
        fit_to_budget(&mapped, budget_tokens, Layer::L1)
    }
}

fn to_val<T: Serialize>(t: T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}
