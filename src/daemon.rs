//! One background process per repository that owns the watcher, the warm resolver cache and
//! all writes; MCP servers (one per agent session) forward tool calls to it, one JSON line per
//! request and reply.
//!
//! Transport: a Unix socket in a private per-user directory (see [`socket_path`]). Windows has no
//! Unix sockets, so there the daemon listens on a random loopback TCP port and writes the port
//! and a fresh 256-bit token to a file in the per-user `%LOCALAPPDATA%\nexusweave`. Every TCP
//! connection starts with a mutual proof of that token over fresh nonces ([`tcp`]), so another
//! local user can neither use the daemon nor stand in for it on a stale or squatted port.
//! `NEXUSWEAVE_DAEMON_TCP=1` selects TCP on Unix too (the tests run that path everywhere).
//!
//! Election is an exclusive lock on `.nexusweave/daemon.lock` (`flock`; on Windows a handle
//! opened without sharing), and the daemon is recorded in `.nexusweave/daemon.json`. It exits after
//! `NEXUSWEAVE_DAEMON_IDLE` seconds (default 900) without connected clients, or once its executable
//! has been replaced (`cargo install`, `nexusweave upgrade`): clients then respawn the new build.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Longest request or reply line accepted (tool output is capped far below this).
const MAX_LINE: u64 = 16 << 20;

fn use_tcp() -> bool { cfg!(not(unix)) || std::env::var_os("NEXUSWEAVE_DAEMON_TCP").is_some_and(|v| v != "0") }

fn key(root: &Path) -> String { format!("{:016x}", xxhash_rust::xxh3::xxh3_64(root.to_string_lossy().as_bytes())) }

/// A directory only this user can enter, for sockets and endpoint files; `None` when it can't be
/// made private (callers then answer in-process).
#[cfg(unix)]
fn runtime_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir()) {
        Some(run) => run.join("nexusweave"),
        None => std::env::temp_dir().join(format!("nexusweave-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let md = std::fs::symlink_metadata(&dir).ok()?;
    (md.is_dir() && md.uid() == uid && md.mode() & 0o077 == 0).then_some(dir)
}

/// `%LOCALAPPDATA%\nexusweave`: inside the user profile, whose ACL admits only this user (plus
/// SYSTEM and administrators).
#[cfg(not(unix))]
fn runtime_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).filter(|p| p.is_absolute())?;
    let dir = base.join("nexusweave");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The Unix socket for `root` (also where TCP mode keeps its endpoint file, with another suffix).
pub fn socket_path(root: &Path) -> Option<PathBuf> { Some(runtime_dir()?.join(format!("{}.sock", key(root)))) }

fn endpoint_path(root: &Path) -> Option<PathBuf> { Some(runtime_dir()?.join(format!("{}.tcp.json", key(root)))) }

fn lock_path(root: &Path) -> PathBuf { root.join(".nexusweave").join("daemon.lock") }

/// One connection, either side: buffered line reads, unbuffered writes.
struct Conn {
    r: BufReader<Box<dyn Read + Send>>,
    w: Box<dyn Write + Send>,
}

impl Conn {
    fn tcp(s: TcpStream) -> std::io::Result<Conn> {
        Ok(Conn { r: BufReader::new(Box::new(s.try_clone()?)), w: Box::new(s) })
    }

    #[cfg(unix)]
    fn unix(s: std::os::unix::net::UnixStream) -> std::io::Result<Conn> {
        Ok(Conn { r: BufReader::new(Box::new(s.try_clone()?)), w: Box::new(s) })
    }

    fn send(&mut self, v: &Value) -> std::io::Result<()> {
        writeln!(self.w, "{v}")?;
        self.w.flush()
    }

    /// The next JSON line; `None` at end of stream. Lines over `limit` bytes or not JSON are errors.
    fn recv(&mut self, limit: u64) -> std::io::Result<Option<Value>> {
        let mut line = String::new();
        if (&mut self.r).take(limit).read_line(&mut line)? == 0 { return Ok(None); }
        if !line.ends_with('\n') { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line too long")); }
        serde_json::from_str(&line).map(Some).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Token-authenticated loopback connections (Windows, or `NEXUSWEAVE_DAEMON_TCP`).
pub mod tcp {
    use super::Conn;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::io::{Error, ErrorKind, Result};
    use std::path::Path;

    /// Handshake lines are tiny; anything bigger is not a client.
    const HANDSHAKE_LINE: u64 = 4096;

    pub type Token = [u8; 32];

    /// 32 bytes from the operating system's CSPRNG.
    pub fn random() -> Result<Token> {
        let mut b = [0u8; 32];
        #[cfg(unix)]
        { use std::io::Read; std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?; }
        #[cfg(windows)]
        {
            #[link(name = "bcrypt")]
            unsafe extern "system" {
                fn BCryptGenRandom(alg: *mut std::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
            }
            const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
            if unsafe { BCryptGenRandom(std::ptr::null_mut(), b.as_mut_ptr(), b.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) } != 0 {
                return Err(Error::other("BCryptGenRandom failed"));
            }
        }
        #[cfg(not(any(unix, windows)))]
        { return Err(Error::other("no system random source on this platform")); }
        #[allow(unreachable_code)]
        Ok(b)
    }

    pub fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

    pub fn unhex(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) { return None; }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    }

    /// What `role` sends to prove it holds `token`, bound to the peer's fresh nonce.
    fn proof(token: &Token, role: &str, nonce: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(token);
        h.update(role.as_bytes());
        h.update(nonce);
        hex(&h.finalize())
    }

    /// Equality whose time doesn't depend on where the inputs first differ.
    fn same(a: &str, b: &str) -> bool { a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0 }

    fn denied(why: &str) -> Error { Error::new(ErrorKind::PermissionDenied, why.to_string()) }

    fn field(v: Option<Value>, k: &str) -> Result<String> {
        v.and_then(|v| v[k].as_str().map(str::to_string)).ok_or_else(|| denied("malformed handshake"))
    }

    /// Client side: the daemon must prove the token first, then the client does.
    pub(super) fn client(c: &mut Conn, token: &Token) -> Result<()> {
        let mine = random()?;
        c.send(&json!({"hello": hex(&mine)}))?;
        let r = c.recv(HANDSHAKE_LINE)?;
        let theirs = unhex(&field(r.clone(), "nonce")?).filter(|n| n.len() == 32).ok_or_else(|| denied("malformed handshake"))?;
        if !same(&field(r, "proof")?, &proof(token, "daemon", &mine)) { return Err(denied("the process on this port is not this project's daemon")); }
        c.send(&json!({"proof": proof(token, "client", &theirs)}))?;
        match c.recv(HANDSHAKE_LINE)? {
            Some(v) if v["ok"] == true => Ok(()),
            _ => Err(denied("the daemon refused the connection")),
        }
    }

    /// Daemon side; on error the connection is dropped without serving anything.
    pub(super) fn server(c: &mut Conn, token: &Token) -> Result<()> {
        let theirs = unhex(&field(c.recv(HANDSHAKE_LINE)?, "hello")?).filter(|n| n.len() == 32).ok_or_else(|| denied("malformed handshake"))?;
        let mine = random()?;
        c.send(&json!({"nonce": hex(&mine), "proof": proof(token, "daemon", &theirs)}))?;
        if !same(&field(c.recv(HANDSHAKE_LINE)?, "proof")?, &proof(token, "client", &mine)) {
            let _ = c.send(&json!({"ok": false}));
            return Err(denied("wrong token"));
        }
        c.send(&json!({"ok": true}))
    }

    /// Publish `port` and `token` for clients: written beside, then renamed into place (owner-only
    /// on Unix; the per-user directory protects it on Windows).
    pub(super) fn write_endpoint(path: &Path, port: u16, token: &Token) -> Result<()> {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; o.mode(0o600); }
        std::io::Write::write_all(&mut o.open(&tmp)?, json!({"port": port, "token": hex(token), "pid": std::process::id()}).to_string().as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    pub(super) fn read_endpoint(path: &Path) -> Option<(u16, Token)> {
        let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let port = u16::try_from(v["port"].as_u64()?).ok()?;
        let token: Token = unhex(v["token"].as_str()?)?.try_into().ok()?;
        Some((port, token))
    }
}

/// Connected client (one per MCP server / agent session).
pub struct Client {
    conn: Conn,
}

impl Client {
    fn request(&mut self, req: &Value) -> Result<Value> {
        self.conn.send(req)?;
        self.conn.recv(MAX_LINE)?.context("daemon closed the connection")
    }

    /// Forward a tool call: (text, is_error).
    pub fn call(&mut self, tool: &str, args: &Value) -> Result<(String, bool)> {
        let r = self.request(&json!({"tool": tool, "args": args}))?;
        Ok((r["text"].as_str().unwrap_or("").to_string(), r["error"].as_bool().unwrap_or(false)))
    }

    pub fn status(&mut self) -> Result<Value> { self.request(&json!({"cmd": "status"})) }
    pub fn stop(&mut self) -> Result<Value> { self.request(&json!({"cmd": "stop"})) }
}

fn open_conn(root: &Path) -> Option<Conn> {
    if use_tcp() {
        let (port, token) = tcp::read_endpoint(&endpoint_path(root)?)?;
        let s = TcpStream::connect_timeout(&(std::net::Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
        let mut c = Conn::tcp(s.try_clone().ok()?).ok()?;
        tcp::client(&mut c, &token).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(600))).ok()?;
        return Some(c);
    }
    #[cfg(unix)]
    {
        let s = std::os::unix::net::UnixStream::connect(socket_path(root)?).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(600))).ok()?;
        Conn::unix(s).ok()
    }
    #[cfg(not(unix))]
    None
}

pub fn connect(root: &Path) -> Option<Client> {
    let mut c = Client { conn: open_conn(root)? };
    c.request(&json!({"cmd": "ping"})).ok()?;
    Some(c)
}

/// Start `nexusweave daemon run <root>` detached from this process, so it outlives the MCP server.
fn spawn_daemon(exe: &Path, root: &Path) -> bool {
    let cmd = || {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["daemon", "run"]).arg(root)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        cmd
    };
    let reap = |c: std::io::Result<std::process::Child>| c.map(|mut c| { std::thread::spawn(move || c.wait()); }).is_ok();
    #[cfg(unix)]
    {
        let mut c = cmd();
        // a new session: no controlling terminal, not killed with the MCP server's process group
        unsafe {
            use std::os::unix::process::CommandExt;
            c.pre_exec(|| { libc::setsid(); Ok(()) });
        }
        reap(c.spawn())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // leave the agent's job object when it allows that, so closing the agent's session
        // doesn't take the shared daemon down with it; a job that forbids breakaway fails the
        // spawn, so retry inside it
        reap(cmd().creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).spawn())
            || reap(cmd().creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).spawn())
    }
    #[cfg(not(any(unix, windows)))]
    reap(cmd().spawn())
}

/// Connect to the project's daemon, starting one if none is running.
pub fn connect_or_spawn(root: &Path) -> Option<Client> {
    if let Some(c) = connect(root) { return Some(c); }
    let mut exe = std::env::current_exe().ok()?;
    // this binary was replaced on disk since it started (Linux reports `path (deleted)`): start
    // the new build at that path
    if !exe.exists() && let Some(p) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) { exe = p.into(); }
    if !spawn_daemon(&exe, root) { return None; }
    let started = Instant::now();
    let mut last_spawn = started;
    while started.elapsed() < Duration::from_secs(20) {  // first start may run a sync
        std::thread::sleep(Duration::from_millis(100));
        if let Some(c) = connect(root) { return Some(c); }
        // a daemon that is still shutting down (an older build, unreachable at this address) holds
        // the election lock and the spawn above exits at once: try again (a loser just exits)
        if last_spawn.elapsed() >= Duration::from_secs(3) {
            last_spawn = Instant::now();
            spawn_daemon(&exe, root);
        }
    }
    None
}

/// The election lock: held for the daemon's lifetime, released by the OS when it exits. `None`
/// when another daemon holds it.
fn elect(root: &Path) -> Result<Option<std::fs::File>> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        return match o.share_mode(0).open(lock_path(root)) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
            Err(e) => Err(e).context("opening the daemon lock"),
        };
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let f = o.open(lock_path(root))?;
        Ok((unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f))
    }
    #[cfg(not(any(unix, windows)))]
    Ok(Some(o.open(lock_path(root))?))
}

/// Where clients find this daemon; `cleanup` removes it on the way out.
enum Listener {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, PathBuf),
    Tcp(TcpListener, tcp::Token, PathBuf),
}

impl Listener {
    fn bind(root: &Path) -> Result<Listener> {
        if use_tcp() {
            let ep = endpoint_path(root).context("no private per-user directory for the daemon endpoint (is %LOCALAPPDATA% set?)")?;
            let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).context("binding a loopback port")?;
            let token = tcp::random().context("generating the daemon token")?;
            tcp::write_endpoint(&ep, l.local_addr()?.port(), &token).with_context(|| format!("writing {}", ep.display()))?;
            return Ok(Listener::Tcp(l, token, ep));
        }
        #[cfg(unix)]
        {
            let sock = socket_path(root).context("no private directory for the daemon socket (check the permissions of $XDG_RUNTIME_DIR or the temp dir)")?;
            let _ = std::fs::remove_file(&sock);
            let l = std::os::unix::net::UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
            Ok(Listener::Unix(l, sock))
        }
        #[cfg(not(unix))]
        unreachable!("TCP is the only transport here")
    }

    fn address(&self) -> Value {
        match self {
            #[cfg(unix)]
            Listener::Unix(_, p) => json!({"transport": "unix", "socket": p.display().to_string()}),
            // the token stays in the private endpoint file
            Listener::Tcp(l, _, p) => json!({"transport": "tcp", "port": l.local_addr().map(|a| a.port()).unwrap_or(0), "endpoint": p.display().to_string()}),
        }
    }

    fn cleanup_path(&self) -> PathBuf {
        match self {
            #[cfg(unix)]
            Listener::Unix(_, p) => p.clone(),
            Listener::Tcp(_, _, p) => p.clone(),
        }
    }

    /// Next authenticated connection (blocking); spawns nothing itself.
    fn accept(&self) -> std::io::Result<(Conn, Option<(TcpStream, tcp::Token)>)> {
        match self {
            #[cfg(unix)]
            Listener::Unix(l, _) => Ok((Conn::unix(l.accept()?.0)?, None)),
            Listener::Tcp(l, token, _) => {
                let (s, _) = l.accept()?;
                Ok((Conn::tcp(s.try_clone()?)?, Some((s, *token))))
            }
        }
    }
}

enum Job {
    Call { session: u64, tool: String, args: Value, reply: mpsc::Sender<Value> },
    EndSession(u64),
}

/// Run the daemon in the foreground (normally started by `connect_or_spawn`).
pub fn run(root: &Path) -> Result<()> {
    crate::index::ensure_dir(root)?;
    let Some(_lock) = elect(root)? else {
        eprintln!("nexusweave daemon already running for {}", root.display());
        return Ok(());
    };
    let listener = Listener::bind(root)?;
    let mut info = json!({"pid": std::process::id(), "version": env!("CARGO_PKG_VERSION")});
    if let (Some(i), Some(a)) = (info.as_object_mut(), listener.address().as_object()) { i.extend(a.clone()); }
    std::fs::write(root.join(".nexusweave").join("daemon.json"), info.to_string())?;
    let sock = listener.cleanup_path();

    // catch up on changes made while nothing was watching, then watch
    let _ = crate::index::sync(root, None);
    let watcher = crate::watch::start(root, |_| {}).ok();
    let (tx, rx) = mpsc::channel::<Job>();
    let clients = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let busy = Arc::new(AtomicUsize::new(0));
    let exe = std::env::current_exe().ok();
    let exe_id = exe.as_deref().and_then(crate::db::file_id);
    let replaced = move || exe.as_deref().is_some_and(|e| crate::db::file_id(e) != exe_id);

    // one worker owns the graph connections and the watcher (rusqlite connections aren't Sync)
    let root_w = root.to_path_buf();
    let served_w = served.clone();
    let busy_w = busy.clone();
    std::thread::spawn(move || {
        let mut srv = crate::mcp::Server::new(Some(root_w), watcher);
        for job in rx {
            match job {
                Job::Call { session, tool, args, reply } => {
                    busy_w.fetch_add(1, Ordering::SeqCst);
                    let (text, error) = srv.handle(session, &tool, &args);
                    busy_w.fetch_sub(1, Ordering::SeqCst);
                    served_w.fetch_add(1, Ordering::Relaxed);
                    let _ = reply.send(json!({"text": text, "error": error}));
                }
                Job::EndSession(s) => srv.end_session(s),
            }
        }
    });

    // idle shutdown, and handing over to a replaced executable between calls (clients reconnect
    // on a closed connection and spawn the new build)
    let idle = Duration::from_secs(std::env::var("NEXUSWEAVE_DAEMON_IDLE").ok().and_then(|v| v.parse().ok()).unwrap_or(900));
    let clients_i = clients.clone();
    let sock_i = sock.clone();
    std::thread::spawn(move || {
        let mut quiet_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if busy.load(Ordering::SeqCst) == 0 && replaced() {
                let _ = std::fs::remove_file(&sock_i);
                std::process::exit(0);
            }
            if clients_i.load(Ordering::Relaxed) > 0 { quiet_since = Instant::now(); continue; }
            if quiet_since.elapsed() >= idle {
                let _ = std::fs::remove_file(&sock_i);
                std::process::exit(0);
            }
        }
    });

    let next_session = AtomicU64::new(1);
    loop {
        let Ok((mut conn, tcp_auth)) = listener.accept() else { continue };
        let session = next_session.fetch_add(1, Ordering::Relaxed);
        let (tx, clients, served, sock) = (tx.clone(), clients.clone(), served.clone(), sock.clone());
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            if let Some((s, token)) = tcp_auth {
                // an unauthenticated peer gets a few seconds and nothing else
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                if tcp::server(&mut conn, &token).is_err() { return; }
                let _ = s.set_read_timeout(None);
            }
            clients.fetch_add(1, Ordering::Relaxed);
            loop {
                let req = match conn.recv(MAX_LINE) {
                    Ok(Some(v)) => v,
                    Ok(None) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                    Err(_) => break,
                };
                let resp = match req.get("cmd").and_then(Value::as_str) {
                    Some("ping") => json!({"ok": true}),
                    // `clients` excludes the connection asking
                    Some("status") => json!({"pid": std::process::id(), "root": root.display().to_string(), "clients": clients.load(Ordering::Relaxed).saturating_sub(1),
                        "served": served.load(Ordering::Relaxed), "uptime_s": started.elapsed().as_secs(), "version": env!("CARGO_PKG_VERSION")}),
                    Some("stop") => {
                        let _ = conn.send(&json!({"ok": true}));
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
                if conn.send(&resp).is_err() { break; }
            }
            let _ = tx.send(Job::EndSession(session));
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Conn, tcp};
    use std::net::{TcpListener, TcpStream};

    #[cfg(unix)]
    #[test]
    fn socket_dir_is_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let p = super::socket_path(std::path::Path::new("/some/project")).expect("private socket dir");
        let md = std::fs::metadata(p.parent().unwrap()).unwrap();
        assert_eq!(md.uid(), unsafe { libc::getuid() });
        assert_eq!(md.permissions().mode() & 0o077, 0);
        assert_ne!(p, super::socket_path(std::path::Path::new("/other/project")).unwrap());
    }

    /// Run one handshake over a real loopback connection: (client result, server result).
    fn handshake(client_token: tcp::Token, server_token: tcp::Token) -> (bool, bool) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            tcp::server(&mut Conn::tcp(s).unwrap(), &server_token).is_ok()
        });
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let client = tcp::client(&mut Conn::tcp(s).unwrap(), &client_token).is_ok();
        (client, server.join().unwrap())
    }

    #[test]
    fn tcp_handshake_is_mutual() {
        let (a, b) = (tcp::random().unwrap(), tcp::random().unwrap());
        assert_ne!(a, b, "tokens are random");
        assert_eq!(handshake(a, a), (true, true));
        // the server can't prove a token it doesn't have: the client never sends its proof
        assert_eq!(handshake(a, b), (false, false));
    }

    #[test]
    fn tcp_server_rejects_garbage_and_replays() {
        use std::io::{BufRead, BufReader, Write};
        let token = tcp::random().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut results = Vec::new();
            for _ in 0..2 {
                let (s, _) = l.accept().unwrap();
                s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                results.push(tcp::server(&mut Conn::tcp(s).unwrap(), &token).is_ok());
            }
            results
        });
        // a request without any handshake
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(s, "{{\"cmd\": \"stop\"}}").unwrap();
        // a client that answers the challenge with a made-up proof
        let s2 = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut r = BufReader::new(s2.try_clone().unwrap());
        let mut w = s2;
        writeln!(w, "{{\"hello\": \"{}\"}}", tcp::hex(&[7u8; 32])).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        writeln!(w, "{{\"proof\": \"{}\"}}", tcp::hex(&[0u8; 32])).unwrap();
        assert_eq!(server.join().unwrap(), vec![false, false]);
    }

    #[test]
    fn endpoint_round_trip() {
        let dir = std::env::temp_dir().join(format!("nexusweave-ep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.tcp.json");
        let t = tcp::random().unwrap();
        tcp::write_endpoint(&p, 4242, &t).unwrap();
        assert_eq!(tcp::read_endpoint(&p), Some((4242, t)));
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o077, 0); }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
