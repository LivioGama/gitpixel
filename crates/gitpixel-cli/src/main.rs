//! gitpixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use gitpixel_core::index::{build, shard_path};
use gitpixel_core::shard::Shard;
use gitpixel_core::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};
use gitpixel_serve::api::{Request, Response, Service};
use gitpixel_serve::daemon;
use serde_json::Value;

#[derive(Parser)]
#[command(name = "gitpixel", version, about = "Fast, fresh code retrieval for agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, ValueEnum)]
enum ExtractorKind {
    Sparse,
    Trigram,
}

#[derive(Copy, Clone, ValueEnum)]
enum DirectionArg {
    Upstream,
    Downstream,
}

#[derive(Copy, Clone, ValueEnum)]
enum RoleArg {
    Callers,
    Callees,
}

#[derive(Subcommand)]
enum Command {
    /// Build (or rebuild) the text index for a directory tree.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "trigram")]
        extractor: ExtractorKind,
        /// Maximum sparse gram length (ignored for trigram).
        #[arg(long, default_value_t = gitpixel_core::gram::DEFAULT_MAX_GRAM)]
        max_gram: usize,
    },
    /// Search the indexed tree with a regex pattern.
    Search {
        pattern: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit ndjson matches instead of text lines.
        #[arg(long)]
        json: bool,
        /// Print candidate/timing stats to stderr.
        #[arg(long)]
        stats: bool,
        /// Skip the daemon even if one is running.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Look up symbols by name in the code graph.
    Symbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Budget-fitted context for a symbol uid.
    Context {
        uid: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        budget: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Blast radius of a symbol (callers upstream / callees downstream).
    Impact {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "upstream")]
        direction: DirectionArg,
        #[arg(long)]
        depth: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Direct callers or callees of a symbol.
    Uses {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "callers")]
        role: RoleArg,
        #[arg(long)]
        json: bool,
    },
    /// Call path between two symbols.
    Trace {
        from: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Discovered execution flows.
    Processes {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Functional-area clusters.
    Clusters {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Symbols/flows affected by working-tree changes.
    Changes {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Force (re)build of the code graph db.
    Graph {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Index + graph freshness status.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show raw shard metadata (legacy).
    Stats {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage the per-root background daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon (background unless --foreground).
    Start {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        foreground: bool,
    },
    /// Stop a running daemon.
    Stop {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check whether a daemon is running.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// daemon client / execution
// ---------------------------------------------------------------------------

/// One NDJSON round trip on an open stream.
fn roundtrip(stream: &mut UnixStream, req: &Request) -> Option<Response> {
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    serde_json::from_str(&buf).ok()
}

/// Daemon path: only if the socket answers Ping within ~100ms.
fn try_daemon(root: &Path, req: &Request) -> Option<Response> {
    let sock = daemon::socket_path(root);
    let mut stream = UnixStream::connect(&sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(100))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(100))).ok()?;
    let ping = roundtrip(&mut stream, &Request::Ping)?;
    if !ping.ok {
        return None;
    }
    // Real request may legitimately take a while (lazy graph build).
    stream.set_read_timeout(Some(Duration::from_secs(600))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok()?;
    roundtrip(&mut stream, req)
}

/// Prefer the daemon; fall back to an in-process Service.
fn execute(root: &Path, req: Request, no_daemon: bool) -> Result<Value, String> {
    if !no_daemon {
        if let Some(resp) = try_daemon(root, &req) {
            return unwrap_response(resp);
        }
    }
    let mut svc = Service::open(root).map_err(|e| e.to_string())?;
    unwrap_response(svc.handle(req))
}

fn unwrap_response(resp: Response) -> Result<Value, String> {
    if resp.ok {
        Ok(resp.data)
    } else {
        Err(resp.error.unwrap_or_else(|| "unknown error".into()))
    }
}

fn announce_graph_build(data: &Value) {
    if let Some(info) = data.get("graph_build") {
        let ms = info.get("build_ms").and_then(Value::as_u64).unwrap_or(0);
        eprintln!("gitpixel: built graph.db on first use ({ms} ms)");
    }
}

fn print_data(data: &Value, raw_json: bool) {
    if raw_json {
        println!("{}", serde_json::to_string(data).unwrap_or_default());
    } else {
        println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
    }
}

/// Shared graph-command epilogue: candidates protocol + build announcement.
fn finish_graph_cmd(data: Value, raw_json: bool, pretty: impl Fn(&Value) -> Option<()>) {
    announce_graph_build(&data);
    if raw_json {
        print_data(&data, true);
        return;
    }
    if let Some(cands) = data.get("candidates").and_then(Value::as_array) {
        eprintln!("ambiguous name — re-run with one of these uids:");
        for c in cands {
            println!(
                "  {}  ({} {}:{})",
                c.get("uid").and_then(Value::as_str).unwrap_or("?"),
                c.get("kind").and_then(Value::as_str).unwrap_or("?"),
                c.get("path").and_then(Value::as_str).unwrap_or("?"),
                c.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            );
        }
        return;
    }
    if pretty(&data).is_none() {
        print_data(&data, false);
    }
}

fn symbol_line(s: &Value) -> String {
    format!(
        "{:<9} {}  {}:{}-{}  {}",
        s.get("kind").and_then(Value::as_str).unwrap_or("?"),
        s.get("name").and_then(Value::as_str).unwrap_or("?"),
        s.get("path").and_then(Value::as_str).unwrap_or("?"),
        s.get("start_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("end_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("uid").and_then(Value::as_str).unwrap_or("?"),
    )
}

fn envelope_note(data: &Value) {
    if let Some(env) = data.get("envelope") {
        if env.get("lower_bound").and_then(Value::as_bool).unwrap_or(false) {
            let n = env.get("unresolved_same_name").and_then(Value::as_u64).unwrap_or(0);
            eprintln!("note: lower bound — {n} same-name call site(s) unresolved");
        }
    }
}

// ---------------------------------------------------------------------------
// legacy index/search helpers (kept behavior)
// ---------------------------------------------------------------------------

fn make_extractor(kind: ExtractorKind, max_gram: usize) -> Box<dyn GramExtractor> {
    match kind {
        ExtractorKind::Sparse => Box::new(SparseGramExtractor::with_lengths(
            Crc32Weigher,
            gitpixel_core::gram::DEFAULT_MIN_GRAM,
            max_gram,
        )),
        ExtractorKind::Trigram => Box::new(TrigramExtractor),
    }
}

fn extractor_for_shard(shard: &Shard) -> Result<Box<dyn GramExtractor>, String> {
    let id = shard.extractor_id();
    if id == "trigram" {
        return Ok(Box::new(TrigramExtractor));
    }
    if let Some(rest) = id.strip_prefix("sparse-crc32-") {
        if let Some((min, max)) = rest.split_once('-') {
            if let (Ok(min), Ok(max)) = (min.parse::<usize>(), max.parse::<usize>()) {
                return Ok(Box::new(SparseGramExtractor::with_lengths(Crc32Weigher, min, max)));
            }
        }
    }
    Err(format!("index built with unsupported extractor {id:?}; re-run `gitpixel index`"))
}

fn print_search_matches(matches: &[Value], json: bool) {
    let mut stdout = String::with_capacity(matches.len() * 80);
    for m in matches {
        let path = m.get("path").and_then(Value::as_str).unwrap_or("");
        let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
        let text = m.get("text").and_then(Value::as_str).unwrap_or("");
        if json {
            stdout.push_str(
                &serde_json::json!({"path": path, "line": line, "text": text}).to_string(),
            );
            stdout.push('\n');
        } else {
            stdout.push_str(&format!("{path}:{line}:{text}\n"));
        }
    }
    print!("{stdout}");
}

fn run_search(
    pattern: String,
    path: PathBuf,
    json: bool,
    stats: bool,
    no_daemon: bool,
) -> Result<(), String> {
    // Fast path via daemon/service (index auto-built if missing).
    let data = execute(
        &path,
        Request::Search { pattern: pattern.clone(), json, limit: None },
        no_daemon,
    )?;
    let empty = Vec::new();
    let matches = data.get("matches").and_then(Value::as_array).unwrap_or(&empty);
    print_search_matches(matches, json);
    if stats {
        if let Some(s) = data.get("stats") {
            eprintln!(
                "candidates={}{} matches={} elapsed_us={}",
                s.get("candidates").and_then(Value::as_u64).unwrap_or(0),
                if s.get("scanned_all").and_then(Value::as_bool).unwrap_or(false) {
                    " (full scan)"
                } else {
                    ""
                },
                s.get("matches").and_then(Value::as_u64).unwrap_or(0),
                s.get("elapsed_us").and_then(Value::as_u64).unwrap_or(0),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon management
// ---------------------------------------------------------------------------

fn daemon_ping(root: &Path) -> bool {
    try_daemon(root, &Request::Ping).map(|r| r.ok).unwrap_or(false)
}

fn daemon_start(path: PathBuf, foreground: bool) -> Result<(), String> {
    if foreground {
        return daemon::run(&path).map_err(|e| e.to_string());
    }
    if daemon_ping(&path) {
        println!("daemon already running ({})", daemon::socket_path(&path).display());
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let abs = path.canonicalize().map_err(|e| format!("bad path {}: {e}", path.display()))?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("start")
        .arg(&abs)
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn daemon: {e}"))?;
    // Wait for the socket to come up (index build can take a moment).
    for _ in 0..100 {
        if daemon_ping(&abs) {
            println!("daemon started ({})", daemon::socket_path(&abs).display());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(
        "daemon spawned; socket not answering yet ({})",
        daemon::socket_path(&abs).display()
    );
    Ok(())
}

fn daemon_stop(path: PathBuf) -> Result<(), String> {
    match try_daemon(&path, &Request::Shutdown) {
        Some(r) if r.ok => {
            println!("daemon stopped");
            Ok(())
        }
        _ => {
            println!("no daemon running for {}", path.display());
            Ok(())
        }
    }
}

fn daemon_status(path: PathBuf) -> Result<(), String> {
    if daemon_ping(&path) {
        println!("daemon running ({})", daemon::socket_path(&path).display());
    } else {
        println!("daemon not running for {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn run() -> Result<(), String> {
    match Cli::parse().command {
        Command::Index { path, extractor, max_gram } => {
            let ex = make_extractor(extractor, max_gram);
            let stats = build(&path, ex.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "indexed {} files ({} bytes) -> {} grams, shard {} bytes, {} ms",
                stats.files, stats.bytes, stats.grams, stats.shard_bytes, stats.elapsed_ms
            );
            Ok(())
        }
        Command::Search { pattern, path, json, stats, no_daemon } => {
            run_search(pattern, path, json, stats, no_daemon)
        }
        Command::Symbol { name, path, json } => {
            let data = execute(&path, Request::Symbol { name }, false)?;
            finish_graph_cmd(data, json, |d| {
                let syms = d.get("symbols")?.as_array()?;
                if syms.is_empty() {
                    println!("no symbols found");
                } else {
                    for s in syms {
                        println!("{}", symbol_line(s));
                    }
                }
                envelope_note(d);
                Some(())
            });
            Ok(())
        }
        Command::Context { uid, path, budget, json } => {
            let data =
                execute(&path, Request::Context { uid, budget_tokens: budget }, false)?;
            finish_graph_cmd(data, json, |d| {
                if let Some(s) = d.get("symbol") {
                    println!("{}", symbol_line(s));
                }
                let text = d.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    println!("\n{text}");
                } else {
                    println!(
                        "\nincoming: {}",
                        serde_json::to_string_pretty(d.get("incoming").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    );
                    println!(
                        "outgoing: {}",
                        serde_json::to_string_pretty(d.get("outgoing").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    );
                }
                envelope_note(d);
                Some(())
            });
            Ok(())
        }
        Command::Impact { uid_or_name, path, direction, depth, json } => {
            let dir = match direction {
                DirectionArg::Upstream => "upstream",
                DirectionArg::Downstream => "downstream",
            };
            let data = execute(
                &path,
                Request::Impact { uid_or_name, direction: dir.to_string(), depth },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None);
            Ok(())
        }
        Command::Uses { uid_or_name, path, role, json } => {
            let role_s = match role {
                RoleArg::Callers => "callers",
                RoleArg::Callees => "callees",
            };
            let data = execute(
                &path,
                Request::Uses { uid_or_name, role: role_s.to_string() },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let edges = d.get("edges")?.as_array()?;
                let role = d.get("role").and_then(Value::as_str).unwrap_or("?");
                if let Some(s) = d.get("symbol") {
                    println!("{}", symbol_line(s));
                }
                println!("{role}: {}", edges.len());
                for e in edges {
                    let tier = e.get("tier").and_then(Value::as_str).unwrap_or("?");
                    let line = e.get("site_line").and_then(Value::as_u64).unwrap_or(0);
                    match e.get("symbol").filter(|s| !s.is_null()) {
                        Some(s) => println!("  [{tier}] line {line}  {}", symbol_line(s)),
                        None => println!("  [{tier}] line {line}  <unknown symbol>"),
                    }
                }
                envelope_note(d);
                Some(())
            });
            Ok(())
        }
        Command::Trace { from, to, path, json } => {
            let data = execute(&path, Request::Trace { from, to }, false)?;
            finish_graph_cmd(data, json, |_| None);
            Ok(())
        }
        Command::Processes { path, json } => {
            let data = execute(&path, Request::Processes {}, false)?;
            finish_graph_cmd(data, json, |_| None);
            Ok(())
        }
        Command::Clusters { path, json } => {
            let data = execute(&path, Request::Clusters {}, false)?;
            finish_graph_cmd(data, json, |_| None);
            Ok(())
        }
        Command::Changes { path, base, json } => {
            let data = execute(&path, Request::Changes { base }, false)?;
            finish_graph_cmd(data, json, |_| None);
            Ok(())
        }
        Command::Graph { path, json } => {
            let root = path
                .canonicalize()
                .map_err(|e| format!("bad path {}: {e}", path.display()))?;
            let db = root.join(gitpixel_core::index::SHARD_DIR).join("graph.db");
            let _ = std::fs::remove_file(&db);
            let started = std::time::Instant::now();
            let stats = gitpixel_graph::build::build_graph(&root, &db)
                .map_err(|e| e.to_string())?;
            let v = serde_json::json!({
                "files": stats.files,
                "symbols": stats.symbols,
                "edges": stats.edges,
                "unresolved": stats.unresolved,
                "elapsed_ms": stats.elapsed_ms as u64,
            });
            eprintln!("graph built in {} ms -> {}", started.elapsed().as_millis(), db.display());
            print_data(&v, json);
            Ok(())
        }
        Command::Status { path, json } => {
            let data = execute(&path, Request::Status {}, false)?;
            if json {
                print_data(&data, true);
            } else {
                println!("root: {}", data.get("root").and_then(Value::as_str).unwrap_or("?"));
                if let Some(i) = data.get("index") {
                    println!(
                        "index: commit={} base_files={} delta_files={} overlay_files={} tombstones={}",
                        i.get("commit_oid").and_then(Value::as_str).unwrap_or("-"),
                        i.get("base_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("delta_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("overlay_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("tombstones").and_then(Value::as_u64).unwrap_or(0),
                    );
                }
                match data.get("graph") {
                    Some(g) if g.get("present").and_then(Value::as_bool).unwrap_or(false) => {
                        println!(
                            "graph: files={} symbols={} edges={} unresolved_calls={}",
                            g.get("files").and_then(Value::as_u64).unwrap_or(0),
                            g.get("symbols").and_then(Value::as_u64).unwrap_or(0),
                            g.get("edges").and_then(Value::as_u64).unwrap_or(0),
                            g.get("unresolved_calls").and_then(Value::as_u64).unwrap_or(0),
                        );
                    }
                    _ => println!("graph: not built (runs on first graph command)"),
                }
                println!(
                    "daemon: {}",
                    if daemon_ping(&path) { "running" } else { "not running" }
                );
            }
            Ok(())
        }
        Command::Stats { path } => {
            let shard = Shard::open(&shard_path(&path)).map_err(|e| e.to_string())?;
            let _ = extractor_for_shard(&shard); // validates extractor id
            println!(
                "files={} grams={} extractor={} commit={}",
                shard.file_count(),
                shard.gram_count(),
                shard.extractor_id(),
                shard.commit_oid().unwrap_or("-")
            );
            Ok(())
        }
        Command::Daemon { cmd } => match cmd {
            DaemonCmd::Start { path, foreground } => daemon_start(path, foreground),
            DaemonCmd::Stop { path } => daemon_stop(path),
            DaemonCmd::Status { path } => daemon_status(path),
        },
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gitpixel: {e}");
            ExitCode::FAILURE
        }
    }
}
