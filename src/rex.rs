//! The Rex pane rex-git runs in: retitling it and zooming it.

use std::{
    env,
    process::{Command, Stdio},
    thread,
};

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
