//! Unix-socket NDJSON daemon: one JSON `Request` per line, one JSON
//! `Response` line back. Single-threaded request handling (requests are
//! fast); an accept thread and a notify watcher feed one mpsc channel.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::api::{Request, Response, ServeError, Service};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEBOUNCE: Duration = Duration::from_millis(500);
const IGNORED_DIRS: &[&str] = [".gitpixel", ".git", "target", "node_modules"].as_slice();

/// $TMPDIR/gitpixel-<xxh3-of-canonical-root>.sock
pub fn socket_path(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = xxhash_rust::xxh3::xxh3_64(canon.to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("gitpixel-{h:016x}.sock"))
}

pub fn pid_path(root: &Path) -> PathBuf {
    socket_path(root).with_extension("pid")
}

enum Msg {
    Conn(UnixStream),
    Fs(notify::Event),
}

/// Run the daemon in the foreground until Shutdown, idle timeout, or error.
pub fn run(root: &Path) -> Result<(), ServeError> {
    let mut service = Service::open(root)?;
    let root = service.root().to_path_buf();
    let sock = socket_path(&root);

    // A live socket means another daemon owns this root.
    if UnixStream::connect(&sock).is_ok() {
        return Err(ServeError::Msg(format!(
            "daemon already running for {} ({})",
            root.display(),
            sock.display()
        )));
    }
    let _ = std::fs::remove_file(&sock); // stale leftover

    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(pid_path(&root), std::process::id().to_string())?;

    let (tx, rx) = mpsc::channel::<Msg>();

    // Accept thread: forwards connections into the single-threaded loop.
    let tx_conn = tx.clone();
    let accept_listener = listener.try_clone()?;
    std::thread::spawn(move || {
        for stream in accept_listener.incoming() {
            match stream {
                Ok(s) => {
                    if tx_conn.send(Msg::Conn(s)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Watcher: raw notify events into the channel; debounced below.
    let tx_fs = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx_fs.send(Msg::Fs(ev));
        }
    })
    .map_err(|e| ServeError::Msg(format!("watcher init: {e}")))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| ServeError::Msg(format!("watch {}: {e}", root.display())))?;

    eprintln!("gitpixel daemon: root={} socket={}", root.display(), sock.display());

    // rel path -> removed?
    let mut pending: BTreeMap<String, bool> = BTreeMap::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_activity = Instant::now();
    let mut shutdown = false;

    while !shutdown {
        let now = Instant::now();
        let idle_left = IDLE_TIMEOUT
            .checked_sub(now.duration_since(last_activity))
            .unwrap_or(Duration::ZERO);
        let timeout = match flush_at {
            Some(at) => at.saturating_duration_since(now).min(idle_left),
            None => idle_left,
        };

        match rx.recv_timeout(timeout.max(Duration::from_millis(10))) {
            Ok(Msg::Conn(stream)) => {
                last_activity = Instant::now();
                handle_conn(&mut service, stream, &mut shutdown);
            }
            Ok(Msg::Fs(ev)) => {
                record_event(&root, &ev, &mut pending);
                if !pending.is_empty() {
                    flush_at = Some(Instant::now() + DEBOUNCE);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(at) = flush_at {
            if Instant::now() >= at {
                for (rel, removed) in std::mem::take(&mut pending) {
                    if removed {
                        service.remove_file(&rel);
                    } else {
                        service.refresh_file(&rel);
                    }
                }
                flush_at = None;
            }
        }

        if last_activity.elapsed() >= IDLE_TIMEOUT {
            eprintln!("gitpixel daemon: idle timeout, exiting");
            break;
        }
    }

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_path(&root));
    Ok(())
}

fn record_event(root: &Path, ev: &notify::Event, pending: &mut BTreeMap<String, bool>) {
    for path in &ev.paths {
        let Ok(rel) = path.strip_prefix(root) else { continue };
        if rel.components().any(|c| match c {
            Component::Normal(s) => IGNORED_DIRS.iter().any(|d| s == *d),
            _ => false,
        }) {
            continue;
        }
        if path.is_dir() {
            continue;
        }
        let removed =
            matches!(ev.kind, notify::EventKind::Remove(_)) || !path.exists();
        let rel = rel.to_string_lossy().into_owned();
        if rel.is_empty() {
            continue;
        }
        // A later create/modify wins over an earlier remove and vice versa.
        pending.insert(rel, removed);
    }
}

fn handle_conn(service: &mut Service, stream: UnixStream, shutdown: &mut bool) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, is_shutdown) = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => {
                let is_shutdown = matches!(req, Request::Shutdown);
                (service.handle(req), is_shutdown)
            }
            Err(e) => (Response::err(format!("bad request: {e}")), false),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| {
            r#"{"ok":false,"error":"serialize failure","data":null}"#.to_string()
        });
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() {
            break;
        }
        if is_shutdown {
            *shutdown = true;
            break;
        }
    }
}
