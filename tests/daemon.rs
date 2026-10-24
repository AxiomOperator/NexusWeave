//! The shared daemon end to end, through the real binary: MCP sessions spawn it, share it, and
//! stop it. Runs once per transport; the TCP one is what Windows uses.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_nexusweave");

fn project(name: &str) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dst = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() { copy(&e.path(), &dst); } else { std::fs::copy(e.path(), dst).unwrap(); }
        }
    }
    let d = std::env::temp_dir().join(format!("nexusweave-daemon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    copy(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ts"), &d);
    let st = Command::new(BIN).args(["init", "-q"]).current_dir(&d).status().unwrap();
    assert!(st.success());
    nexusweave::canonical(&d).unwrap()
}

fn cmd(dir: &Path, tcp: bool) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(dir).env_remove("NEXUSWEAVE_NO_DAEMON").env("NEXUSWEAVE_DAEMON_IDLE", "60");
    if tcp { c.env("NEXUSWEAVE_DAEMON_TCP", "1"); } else { c.env_remove("NEXUSWEAVE_DAEMON_TCP"); }
    c
}

/// One MCP session: initialize, then one explore call; returns the explore text.
fn session(dir: &Path, tcp: bool) -> String {
    let mut child = cmd(dir, tcp).arg("serve").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"nexusweave_explore","arguments":{{"query":"UserService.getUser"}}}}}}"#).unwrap();
    let mut lines = Vec::new();
    for _ in 0..2 {
        let mut l = String::new();
        out.read_line(&mut l).unwrap();
        lines.push(serde_json::from_str::<Value>(&l).unwrap());
    }
    drop(stdin);
    child.wait().unwrap();
    assert_eq!(lines[1]["result"]["isError"], false, "{}", lines[1]);
    lines[1]["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

fn daemon_info(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join(".nexusweave/daemon.json")).expect("daemon.json written")).unwrap()
}

fn status(dir: &Path, tcp: bool) -> Option<Value> {
    let o = cmd(dir, tcp).args(["daemon", "status"]).output().unwrap();
    serde_json::from_slice(&o.stdout).ok()
}

fn shared_daemon(tcp: bool) {
    let dir = project(if tcp { "tcp" } else { "unix" });
    assert!(session(&dir, tcp).contains("getUser"));
    let info = daemon_info(&dir);
    assert_eq!(info["transport"], if tcp { "tcp" } else { "unix" }, "{info}");
    assert!(info.get("token").is_none(), "the token never leaves the private endpoint file");
    let first = status(&dir, tcp).expect("daemon answers status");
    // a second session reuses the same daemon
    assert!(session(&dir, tcp).contains("getUser"));
    let second = status(&dir, tcp).unwrap();
    assert_eq!(first["pid"], second["pid"]);
    assert!(second["served"].as_u64().unwrap() >= 2, "{second}");
    let stop = cmd(&dir, tcp).args(["daemon", "stop"]).output().unwrap();
    assert!(String::from_utf8_lossy(&stop.stdout).contains("stopped"));
    for _ in 0..50 {
        if status(&dir, tcp).is_none() { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(status(&dir, tcp).is_none(), "daemon stopped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn shared_daemon_unix_socket() { shared_daemon(false); }

#[test]
fn shared_daemon_tcp() { shared_daemon(true); }
