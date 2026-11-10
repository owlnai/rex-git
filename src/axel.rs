//! What Axel knows about the workspace: the thread working in it, its agents
//! and intent, and the files its agents' turns changed.

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use serde_json::Value;

const POLL: Duration = Duration::from_secs(10);

#[derive(Clone, Default, Debug)]
pub struct Info {
    pub thread_id: String,
    pub intent: String,
    pub agents: Vec<String>,
    /// `axel thread diff --stat` output for every turn that changed files.
    pub all_turns: String,
    /// The same for the latest such turn.
    pub last_turn: String,
}

fn axel_bin() -> PathBuf {
    let local = env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/axel"));
    match local {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("axel"),
    }
}

fn axel(args: &[&str]) -> Option<String> {
    let out = Command::new(axel_bin()).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn same_dir(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    a == b || a.canonicalize().is_ok_and(|a| a == b)
}

/// The thread working in `dir`: the one named by `wanted`, or else the most
/// recently updated thread with a live session there.
fn find_thread(dir: &Path, wanted: Option<&str>) -> Option<(String, String, Vec<String>)> {
    let threads: Value = serde_json::from_str(&axel(&["thread", "ls", "--json"])?).ok()?;
    let names: HashMap<String, String> = axel(&["agent", "ls", "--json"])
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|a| Some((a["pubkey"].as_str()?.to_string(), a["name"].as_str()?.to_string())))
        .collect();

    let mut best: Option<(i64, &Value)> = None;
    for t in threads.as_array()? {
        let id = t["thread"]["id"].as_str().unwrap_or("");
        let matches = match wanted {
            Some(w) => id.starts_with(w),
            None => {
                let live = t["sessions_live"].as_array().is_some_and(|s| !s.is_empty());
                live && t["projects"].as_array().into_iter().flatten().any(|p| {
                    let path = p["workspace"]["worktree_path"].as_str().or(p["path_hint"].as_str());
                    path.is_some_and(|path| same_dir(path, dir))
                })
            }
        };
        let updated = t["thread"]["updated_at"].as_i64().unwrap_or(0);
        if matches && best.is_none_or(|(u, _)| updated > u) {
            best = Some((updated, t));
        }
    }
    let t = best?.1;
    let mut agents: Vec<String> = t["sessions_live"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["agent_pubkey"].as_str())
        .map(|k| names.get(k).cloned().unwrap_or_else(|| k.chars().take(6).collect()))
        .collect();
    agents.dedup();
    Some((t["thread"]["id"].as_str()?.to_string(), t["thread"]["intent"].as_str().unwrap_or("").to_string(), agents))
}

/// The thread's turns that changed files, oldest first, from Axel's database.
fn edited_turns(thread_id: &str) -> Option<Vec<(String, i64)>> {
    if !thread_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let db = env::var_os("AXEL_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".axel")))?
        .join("axel.db");
    let query = format!(
        "select session_id, ordinal from turns where thread_id = '{thread_id}' and files_edited > 0 order by created_at"
    );
    let out = Command::new("sqlite3").arg("-readonly").arg(&db).arg(query).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (s, n) = l.split_once('|')?;
                Some((s.to_string(), n.parse().ok()?))
            })
            .collect(),
    )
}

/// Watch Axel in the background and send what it knows whenever it changes.
pub fn watch(dir: PathBuf, wanted: Option<String>, tx: Sender<Option<Info>>) {
    thread::spawn(move || {
        let mut cache: HashMap<(String, i64), String> = HashMap::new();
        let mut last_sent: Option<(String, String, String)> = None;
        loop {
            let info = find_thread(&dir, wanted.as_deref()).map(|(thread_id, intent, agents)| {
                let mut info = Info { thread_id, intent, agents, ..Info::default() };
                match edited_turns(&info.thread_id) {
                    Some(turns) => {
                        let newest = turns.last().cloned();
                        for (session, ordinal) in &turns {
                            let key = (session.clone(), *ordinal);
                            // A turn's checkpoints don't change once it's over, except the newest.
                            if !cache.contains_key(&key) || Some(&key) == newest.as_ref() {
                                let n = ordinal.to_string();
                                let stat = axel(&["thread", "diff", &info.thread_id, "--session", session, "--turn", &n, "--stat"]);
                                cache.insert(key.clone(), stat.unwrap_or_default());
                            }
                            info.all_turns.push_str(&cache[&key]);
                            info.all_turns.push('\n');
                        }
                        if let Some(key) = newest {
                            info.last_turn = cache[&key].clone();
                        }
                    }
                    None => {
                        let stat = axel(&["thread", "diff", &info.thread_id, "--stat"]).unwrap_or_default();
                        info.all_turns = stat.clone();
                        info.last_turn = stat;
                    }
                }
                info
            });
            let key = info.as_ref().map(|i| (i.thread_id.clone(), i.all_turns.clone(), i.last_turn.clone() + &i.agents.join(",")));
            if key != last_sent {
                last_sent = key;
                if tx.send(info).is_err() {
                    return;
                }
            }
            thread::sleep(POLL);
        }
    });
}

/// Whether `text` mentions `path` as a whole path, not as the tail of a longer one.
pub fn mentions(text: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let boundary = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || "\"'(),:|".contains(c));
    let mut from = 0;
    while let Some(i) = text[from..].find(path) {
        let start = from + i;
        let end = start + path.len();
        if boundary(text[..start].chars().next_back()) && boundary(text[end..].chars().next()) {
            return true;
        }
        from = start + 1;
        while !text.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn whole_paths_only() {
        let stat = "  src/lib.rs   +3 −1\n  README.md\n";
        assert!(mentions(stat, "src/lib.rs"));
        assert!(mentions(stat, "README.md"));
        assert!(!mentions(stat, "lib.rs"));
        assert!(!mentions(stat, "src/lib"));
    }
}
