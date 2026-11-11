//! The Rex pane rex-git runs in: retitling it, zooming it, opening an editor
//! beside it or in a tab, and hearing where the other shells in its window go.

use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::Sender,
    thread,
};

use serde_json::Value;

/// The pane rex-git runs in, when it runs in Rex.
pub fn block() -> Option<String> {
    env::var("REX_BLOCK").ok().filter(|b| !b.is_empty())
}

/// `rex` with `args`, scoped to this session. The session goes before any
/// `--`, after which come the command's own arguments.
fn rex(args: &[&str]) -> Command {
    let split = args.iter().position(|a| *a == "--").unwrap_or(args.len());
    let mut cmd = Command::new("rex");
    cmd.args(&args[..split]);
    if let Ok(session) = env::var("REX_SESSION") {
        cmd.args(["-s", &session]);
    }
    cmd.args(&args[split..]);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}

/// Run `rex` in the background, ignoring how it went.
fn spawn(args: Vec<String>) {
    thread::spawn(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = rex(&args).status();
    });
}

pub fn rename(block: &str, title: &str) {
    spawn(vec!["block".into(), "rename".into(), block.into(), title.into()]);
}

/// Zoom the pane to fill its window, or put the layout back. It waits, so
/// that the layout is back before rex-git exits.
pub fn zoom(block: &str, on: bool) {
    let _ = rex(&["--timeout", "2s", "zoom", block, if on { "--on" } else { "--off" }]).status();
}

/// Where an editor opens.
#[derive(Clone, Copy)]
pub enum Place {
    /// A pane in rex-git's window, beside its biggest other pane.
    Pane,
    /// A tab of its own.
    Tab,
}

/// Run `command` in a new Rex pane or tab named `label`, in `dir`, and wait
/// for it to exit. Rex then shows rex-git's pane again. Says whether it
/// succeeded.
pub fn run_editor(block: &str, place: Place, label: &str, dir: &Path, command: &str) -> Result<(), String> {
    let dir = dir.to_string_lossy();
    let mut args = vec!["run", "--label", label, "-c", &dir, "--wait"];
    let anchor;
    if let Place::Pane = place {
        // Beside the biggest other pane, so rex-git keeps its width; alone in
        // its window, rex-git gives up most of its own.
        anchor = biggest_neighbor(block);
        match &anchor {
            Some(other) => args.extend(["--split", "right", "-b", other]),
            None => args.extend(["--split", "left", "--ratio", "70", "-b", block]),
        }
    }
    args.extend(["--", "sh", "-c", command]);
    let out = rex(&args).stderr(Stdio::piped()).output().map_err(|e| format!("rex: {e}"))?;
    let _ = rex(&["focus", block]).status();
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("the editor exited with {}", out.status) } else { err })
    }
}

/// The biggest pane in `block`'s window other than `block`.
fn biggest_neighbor(block: &str) -> Option<String> {
    let session = env::var("REX_SESSION").ok()?;
    let out = Command::new("rex")
        .args(["session", "inspect", &session, "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut panes = Vec::new();
    for w in v["windows"].as_array()? {
        let mut leaves = Vec::new();
        for layer in w["layers"].as_array().into_iter().flatten().filter(|l| l["kind"] == "tiled") {
            collect_panes(&layer["layout"], &mut leaves);
        }
        if leaves.iter().any(|(b, _)| b == block) {
            panes = leaves;
            break;
        }
    }
    panes.into_iter().filter(|(b, _)| b != block).max_by(|a, b| a.1.total_cmp(&b.1)).map(|(b, _)| b)
}

/// Every pane in a layout tree, with its share of the window's area.
fn collect_panes(node: &Value, out: &mut Vec<(String, f64)>) {
    if let Some(id) = node["block_id"].as_str() {
        let rect = |i: usize| node["rect"][i].as_f64().unwrap_or(0.0);
        out.push((id.to_string(), rect(2) * rect(3)));
    } else if node["split"].is_object() {
        collect_panes(&node["split"]["before"], out);
        collect_panes(&node["split"]["after"], out);
    }
}

/// Send the directory each of the window's other shells moves to, as they
/// report it. Stops when `rex events` ends or the receiver is gone.
pub fn follow(block: String, tx: Sender<PathBuf>) {
    thread::spawn(move || {
        let Ok(mut child) = rex(&["events", "--json"]).stdout(Stdio::piped()).spawn() else { return };
        let Some(out) = child.stdout.take() else { return };
        // Where each shell is, so that reporting it again at a prompt isn't a move.
        let mut at: HashMap<String, String> = window_blocks(&block)
            .into_iter()
            .filter(|b| *b != block)
            .filter_map(|b| Some((b.clone(), pwd_of(&b)?)))
            .collect();
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            let Some((from, pwd)) = pwd_change(&line) else { continue };
            if from == block || at.get(&from) == Some(&pwd) {
                continue;
            }
            at.insert(from.clone(), pwd.clone());
            if !window_blocks(&block).contains(&from) {
                continue;
            }
            if let Some(path) = file_url_path(&pwd)
                && tx.send(path).is_err()
            {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });
}

/// The block and directory of a `pwd_changed` event from `rex events --json`.
fn pwd_change(line: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ev = &v["payload"]["event"];
    if ev["type"] != "block_event" || ev["payload"]["name"] != "pwd_changed" {
        return None;
    }
    let block = ev["payload"]["block_id"].as_str()?.to_string();
    let pwd = ev["payload"]["payload"]["pwd"].as_str()?.to_string();
    Some((block, pwd))
}

/// The blocks in `block`'s window now, itself included. Blocks move between
/// windows, so this asks Rex every time.
fn window_blocks(block: &str) -> Vec<String> {
    let Ok(out) = rex(&["block", "ls", "--json"]).stdout(Stdio::piped()).output() else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return Vec::new() };
    let blocks = v["blocks"].as_array().cloned().unwrap_or_default();
    let window = blocks.iter().find(|b| b["block_id"] == block).and_then(|b| b["window_id"].as_str());
    let Some(window) = window else { return Vec::new() };
    blocks
        .iter()
        .filter(|b| b["window_id"] == window)
        .filter_map(|b| Some(b["block_id"].as_str()?.to_string()))
        .collect()
}

/// The directory a terminal last reported, as a `file://` URL.
fn pwd_of(block: &str) -> Option<String> {
    let out = rex(&["block", "call", "-b", block, "pwd"]).stdout(Stdio::piped()).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(v["pwd"].as_str()?.to_string())
}

/// The local path in a `file://host/path` URL, percent-decoded.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls() {
        assert_eq!(file_url_path("file://mac.local/Users/me/src"), Some(PathBuf::from("/Users/me/src")));
        assert_eq!(
            file_url_path("file://mac.local/Users/me/Application%20Support"),
            Some(PathBuf::from("/Users/me/Application Support"))
        );
        assert_eq!(file_url_path("file:///tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(file_url_path("https://example.com/x"), None);
    }

    #[test]
    fn pwd_events() {
        let line = r#"{"type":"session_event","payload":{"session_id":"s","event":{"type":"block_event","payload":{"block_id":"block:1","creator_name":"com.superlogical.terminal","name":"pwd_changed","payload":{"pwd":"file://h/tmp","owner":{"pid":1,"name":"fish"}}}}}}"#;
        assert_eq!(pwd_change(line), Some(("block:1".into(), "file://h/tmp".into())));
        let other = line.replace("pwd_changed", "title_changed");
        assert_eq!(pwd_change(&other), None);
    }
}
