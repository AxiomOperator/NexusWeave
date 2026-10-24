//! One background process per repository that owns the watcher, the warm resolver cache and
//! all writes; MCP servers (one per agent session) forward tool calls to it over a Unix socket.
//!
//! Election is an exclusive `flock` on `.xgraph/daemon.lock`; the socket lives in the temp dir
//! (short path) and is recorded in `.xgraph/daemon.json`. The daemon exits after
//! `XGRAPH_DAEMON_IDLE` seconds (default 900) without connected clients.

#![cfg(unix)]

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub fn socket_path(root: &Path) -> PathBuf {
    let h = xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("xgraph-{h:016x}.sock"))
}

fn lock_path(root: &Path) -> PathBuf { root.join(".xgraph").join("daemon.lock") }

/// Connected client (one per MCP server / agent session).
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn request(&mut self, req: &Value) -> Result<Value> {
        writeln!(self.stream, "{req}")?;
        self.stream.flush()?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 { anyhow::bail!("daemon closed the connection"); }
        Ok(serde_json::from_str(&line)?)
    }

    /// Forward a tool call: (text, is_error).
    pub fn call(&mut self, tool: &str, args: &Value) -> Result<(String, bool)> {
        let r = self.request(&json!({"tool": tool, "args": args}))?;
        Ok((r["text"].as_str().unwrap_or("").to_string(), r["error"].as_bool().unwrap_or(false)))
    }

    pub fn status(&mut self) -> Result<Value> { self.request(&json!({"cmd": "status"})) }
    pub fn stop(&mut self) -> Result<Value> { self.request(&json!({"cmd": "stop"})) }
}

pub fn connect(root: &Path) -> Option<Client> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(600))).ok()?;
    let reader = BufReader::new(stream.try_clone().ok()?);
    let mut c = Client { stream, reader };
    c.request(&json!({"cmd": "ping"})).ok()?;
    Some(c)
}

/// Connect to the project's daemon, starting one if none is running.
pub fn connect_or_spawn(root: &PathBuf) -> Option<Client> {
    if let Some(c) = connect(root) { return Some(c); }
    let exe = std::env::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run"]).arg(root)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // detach from the MCP server's session so the daemon outlives it
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| { libc::setsid(); Ok(()) });
    }
    cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);  // first start may run a sync
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(c) = connect(root) { return Some(c); }
    }
    None
}

enum Job {
    Call { session: u64, tool: String, args: Value, reply: mpsc::Sender<Value> },
    EndSession(u64),
}

/// Run the daemon in the foreground (normally started by `connect_or_spawn`).
pub fn run(root: &Path) -> Result<()> {
    crate::index::ensure_dir(root)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path(root))?;
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            eprintln!("xgraph daemon already running for {}", root.display());
            return Ok(());
        }
    }
    let sock = socket_path(root);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::write(root.join(".xgraph").join("daemon.json"),
        json!({"pid": std::process::id(), "socket": sock.display().to_string(), "version": env!("CARGO_PKG_VERSION")}).to_string())?;

    // catch up on changes made while nothing was watching, then watch
    let _ = crate::index::sync(root, None);
    let watcher = crate::watch::start(root, |_| {}).ok();
    let (tx, rx) = mpsc::channel::<Job>();
    let clients = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    // one worker owns the graph connections and the watcher (rusqlite connections aren't Sync)
    let root_w = root.to_path_buf();
    let served_w = served.clone();
    std::thread::spawn(move || {
        let mut srv = crate::mcp::Server::new(Some(root_w), watcher);
        for job in rx {
            match job {
                Job::Call { session, tool, args, reply } => {
                    let (text, error) = srv.handle(session, &tool, &args);
                    served_w.fetch_add(1, Ordering::Relaxed);
                    let _ = reply.send(json!({"text": text, "error": error}));
                }
                Job::EndSession(s) => srv.end_session(s),
            }
        }
    });

    // idle shutdown
    let idle = Duration::from_secs(std::env::var("XGRAPH_DAEMON_IDLE").ok().and_then(|v| v.parse().ok()).unwrap_or(900));
    let clients_i = clients.clone();
    let sock_i = sock.clone();
    std::thread::spawn(move || {
        let mut quiet_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if clients_i.load(Ordering::Relaxed) > 0 { quiet_since = Instant::now(); continue; }
            if quiet_since.elapsed() >= idle {
                let _ = std::fs::remove_file(&sock_i);
                std::process::exit(0);
            }
        }
    });

    let next_session = AtomicU64::new(1);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let session = next_session.fetch_add(1, Ordering::Relaxed);
        let (tx, clients, served, sock) = (tx.clone(), clients.clone(), served.clone(), sock.clone());
        let root = root.to_path_buf();
        clients.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let reader = BufReader::new(match stream.try_clone() { Ok(s) => s, Err(_) => return });
            let mut out = stream;
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
                let resp = match req.get("cmd").and_then(Value::as_str) {
                    Some("ping") => json!({"ok": true}),
                    Some("status") => json!({"pid": std::process::id(), "root": root.display().to_string(), "clients": clients.load(Ordering::Relaxed),
                        "served": served.load(Ordering::Relaxed), "uptime_s": started.elapsed().as_secs()}),
                    Some("stop") => {
                        let _ = writeln!(out, "{}", json!({"ok": true}));
                        let _ = std::fs::remove_file(&sock);
                        std::process::exit(0);
                    }
                    _ => {
                        let (rtx, rrx) = mpsc::channel();
                        let tool = req["tool"].as_str().unwrap_or("").to_string();
                        if tx.send(Job::Call { session, tool, args: req["args"].clone(), reply: rtx }).is_err() { break; }
                        rrx.recv().unwrap_or_else(|_| json!({"text": "daemon worker stopped", "error": true}))
                    }
                };
                if writeln!(out, "{resp}").and_then(|_| out.flush()).is_err() { break; }
            }
            let _ = tx.send(Job::EndSession(session));
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}
