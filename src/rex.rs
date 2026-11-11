//! The Rex pane rex-git runs in: retitling it, zooming it, and opening an
//! editor beside it or in a tab.

use std::{
    env,
    path::Path,
    process::{Command, Stdio},
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
