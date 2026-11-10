//! Git: status, numstat, diffs, commits, and drafting commit messages.

use std::{
    collections::HashMap,
    env,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    thread,
};

const MAX_DIFF_FOR_PROMPT: usize = 60_000;

pub fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    // The pane owns the terminal, so a password prompt would wreck the screen.
    if env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_cmd(dir, args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        Err(if err.is_empty() { format!("git {} failed", args[0]) } else { err.to_string() })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Sec {
    Staged,
    Changed,
    Untracked,
}

impl Sec {
    pub fn title(self) -> &'static str {
        match self {
            Sec::Staged => "staged",
            Sec::Changed => "changes",
            Sec::Untracked => "untracked",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub sec: Sec,
    pub code: char,
    pub path: String,
}

#[derive(Default)]
pub struct Repo {
    pub is_repo: bool,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<Entry>,
    /// Lines added and removed, per section and path. Binary files are absent.
    pub counts: HashMap<(Sec, String), (u32, u32)>,
    pub commits: Vec<(String, String, String)>,
}

impl Repo {
    pub fn load(dir: &Path) -> Repo {
        let Ok(raw) = git(dir, &["status", "--porcelain=v1", "-b", "-z", "-uall"]) else {
            return Repo::default();
        };
        let mut repo = Repo { is_repo: true, ..Repo::default() };
        let mut toks = raw.split('\0');
        if let Some(head) = toks.next().and_then(|h| h.strip_prefix("## ")) {
            repo.parse_head(head, dir);
        }
        while let Some(tok) = toks.next() {
            if tok.len() < 4 {
                continue;
            }
            let mut chars = tok.chars();
            let (x, y) = (chars.next().unwrap(), chars.next().unwrap());
            let path = tok[3..].to_string();
            if x == 'R' || x == 'C' {
                toks.next(); // the original path of a rename or copy
            }
            match (x, y) {
                ('?', _) => repo.entries.push(Entry { sec: Sec::Untracked, code: '?', path }),
                ('!', _) => {}
                _ => {
                    if x != ' ' {
                        repo.entries.push(Entry { sec: Sec::Staged, code: x, path: path.clone() });
                    }
                    if y != ' ' {
                        repo.entries.push(Entry { sec: Sec::Changed, code: y, path });
                    }
                }
            }
        }
        repo.entries.sort_by(|a, b| a.sec.cmp(&b.sec));
        repo.load_counts(dir);
        if let Ok(log) = git(dir, &["log", "-5", "--format=%h%x1f%s%x1f%cr"]) {
            repo.commits = log
                .lines()
                .filter_map(|l| {
                    let mut p = l.split('\x1f');
                    Some((p.next()?.into(), p.next()?.into(), short_age(p.next()?)))
                })
                .collect();
        }
        repo
    }

    fn load_counts(&mut self, dir: &Path) {
        for (sec, args) in [
            (Sec::Staged, &["diff", "--cached", "--numstat", "-z", "--no-renames"][..]),
            (Sec::Changed, &["diff", "--numstat", "-z", "--no-renames"][..]),
        ] {
            let Ok(out) = git(dir, args) else { continue };
            for rec in out.split('\0') {
                let mut f = rec.splitn(3, '\t');
                let (Some(a), Some(d), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
                if let (Ok(a), Ok(d)) = (a.parse(), d.parse()) {
                    self.counts.insert((sec, path.to_string()), (a, d));
                }
            }
        }
        // Untracked files count as all added.
        for e in self.entries.iter().filter(|e| e.sec == Sec::Untracked) {
            if let Ok(bytes) = std::fs::read(dir.join(&e.path)) {
                if bytes.len() < 2_000_000 && !bytes.contains(&0) {
                    let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u32
                        + (!bytes.is_empty() && !bytes.ends_with(b"\n")) as u32;
                    self.counts.insert((Sec::Untracked, e.path.clone()), (lines, 0));
                }
            }
        }
    }

    fn parse_head(&mut self, head: &str, dir: &Path) {
        let head = head.strip_prefix("No commits yet on ").unwrap_or(head);
        let (refs, counts) = head.split_once(" [").unwrap_or((head, ""));
        if refs.starts_with("HEAD (no branch)") {
            let sha = git(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
            self.branch = format!("detached {}", sha.trim());
        } else if let Some((b, up)) = refs.split_once("...") {
            self.branch = b.to_string();
            self.upstream = Some(up.to_string());
        } else {
            self.branch = refs.to_string();
        }
        if counts.starts_with("gone") {
            self.upstream = None;
        }
        for part in counts.trim_end_matches(']').split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                self.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                self.behind = n.parse().unwrap_or(0);
            }
        }
    }

    pub fn staged(&self) -> usize {
        self.entries.iter().filter(|e| e.sec == Sec::Staged).count()
    }
}

pub fn short_age(s: &str) -> String {
    let s = s.trim_end_matches(" ago");
    let Some((n, unit)) = s.split_once(' ') else { return s.to_string() };
    let u = match unit.trim_end_matches('s') {
        "second" => "s",
        "minute" => "m",
        "hour" => "h",
        "day" => "d",
        "week" => "w",
        "month" => "mo",
        "year" => "y",
        _ => return s.to_string(),
    };
    format!("{n}{u}")
}

/// The unified diff of one entry, with `context` lines around each change.
pub fn diff_text(dir: &Path, e: &Entry, context: u32) -> String {
    let u = format!("-U{context}");
    let out = match e.sec {
        Sec::Staged => git(dir, &["diff", "--cached", "--no-color", &u, "--", &e.path]),
        Sec::Changed => git(dir, &["diff", "--no-color", &u, "--", &e.path]),
        // --no-index exits 1 when the files differ, which they always do here.
        Sec::Untracked => git_cmd(dir, &["diff", "--no-index", "--no-color", &u, "--", "/dev/null", &e.path])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .map_err(|e| e.to_string()),
    };
    out.unwrap_or_else(|e| e)
}

/// Ask Claude for a commit message for the staged changes, or for all
/// changes when nothing is staged. `start` is what the user has typed so far,
/// to complete; `intent` is the Axel thread's goal, for the why.
pub fn draft_message(dir: &Path, start: &str, intent: &str) -> Result<String, String> {
    let mut diff = git(dir, &["diff", "--cached", "--no-color"])?;
    if diff.trim().is_empty() {
        diff = git(dir, &["diff", "--no-color"])?;
    }
    if diff.trim().is_empty() {
        return Err("nothing to describe: no changes to tracked files".into());
    }
    if diff.len() > MAX_DIFF_FOR_PROMPT {
        let mut cut = MAX_DIFF_FOR_PROMPT;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str("\n[diff truncated]\n");
    }
    let recent = git(dir, &["log", "-10", "--format=%s"]).unwrap_or_default();
    let mut prompt = String::from(
        "Write a git commit message for the diff on stdin.\n\
         Use Conventional Commits: `type(scope): summary`, with type one of feat, fix, chore, \
         refactor, docs, test, perf, ci, build, style, revert. Write the summary in the \
         imperative mood, lowercase, under 72 characters. Add a short body after a blank line \
         only when the change needs explaining, and keep it to what the change does and why.\n",
    );
    if !intent.trim().is_empty() {
        prompt.push_str(&format!(
            "The change was made for this task, which tells you why: {}\n",
            intent.trim()
        ));
    }
    if !recent.trim().is_empty() {
        prompt.push_str(&format!("Match the style of these recent subjects:\n{recent}"));
    }
    if !start.trim().is_empty() {
        prompt.push_str(&format!(
            "The author has started the message. Keep their words and complete it:\n{start}\n"
        ));
    }
    prompt.push_str("Output only the commit message: no code fences, no quotes, no commentary.");

    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--model", "haiku", "--tools", "", "--no-session-persistence"])
        .args(["--strict-mcp-config", "--setting-sources", "", &prompt])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Don't let the helper look like an agent running in this pane.
    for (key, _) in env::vars() {
        if key.starts_with("AXEL_") || key.starts_with("REX_") || key.starts_with("CLAUDE") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("claude: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || stdin.write_all(diff.as_bytes()));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(if err.is_empty() { "claude failed".into() } else { err.into() });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim().trim_start_matches("```").trim_end_matches("```").trim();
    if text.is_empty() {
        return Err("claude returned an empty message".into());
    }
    Ok(text.to_string())
}

pub fn commit(dir: &Path, msg: &str) -> Result<String, String> {
    let mut child = git_cmd(dir, &["commit", "-q", "-F", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child.stdin.take().unwrap().write_all(msg.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout);
        return Err(err.trim().to_string());
    }
    let sha = git(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    Ok(format!("committed {}", sha.trim()))
}
