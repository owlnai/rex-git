//! Git: status, numstat, diffs, commits, worktrees, and drafting commit messages.

use std::{
    collections::HashMap,
    env,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
};

const MAX_DIFF_FOR_PROMPT: usize = 60_000;

/// The value to give `GIT_SSH_COMMAND`, or None to leave git's choice alone.
/// The variable outranks `core.sshCommand` and `GIT_SSH`, so it's set only
/// when it carries the user's own command, or when they have none.
fn ssh_command(env_command: bool, env_ssh: bool, configured: Option<&str>) -> Option<String> {
    if env_command || env_ssh {
        return None;
    }
    let Some(configured) = configured.map(str::trim).filter(|c| !c.is_empty()) else {
        return Some("ssh -o BatchMode=yes".into());
    };
    // Only OpenSSH is known to take the option; another program keeps its own arguments.
    let program = configured.split_whitespace().next().unwrap_or("");
    let is_ssh = Path::new(program).file_name().is_some_and(|n| n == "ssh");
    is_ssh.then(|| format!("{configured} -o BatchMode=yes"))
}

/// `ssh_command` for `dir`, looked up once: `git_cmd` runs many times a second.
fn ssh_command_for(dir: &Path) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<PathBuf, Option<String>>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(found) = cache.get(dir) {
        return found.clone();
    }
    let configured = Command::new("git")
        .args(["config", "--get", "core.sshCommand"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let command =
        ssh_command(env::var_os("GIT_SSH_COMMAND").is_some(), env::var_os("GIT_SSH").is_some(), configured.as_deref());
    cache.insert(dir.to_path_buf(), command.clone());
    command
}

pub fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0").stdin(Stdio::null());
    // The pane owns the terminal, so a password prompt would wreck the screen.
    if let Some(ssh) = ssh_command_for(dir) {
        cmd.env("GIT_SSH_COMMAND", ssh);
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
    /// Left unmerged by a merge, rebase or cherry-pick.
    Conflict,
    Staged,
    Changed,
    Untracked,
}

impl Sec {
    pub fn title(self) -> &'static str {
        match self {
            Sec::Conflict => "conflicts",
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
    /// Every checkout of the repo, the main one first.
    pub worktrees: Vec<Worktree>,
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
        repo.entries = parse_entries(toks);
        repo.load_counts(dir);
        repo.worktrees = worktrees(dir);
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
            if let Ok(bytes) = std::fs::read(dir.join(&e.path))
                && bytes.len() < 2_000_000
                && !bytes.contains(&0)
            {
                let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u32
                    + (!bytes.is_empty() && !bytes.ends_with(b"\n")) as u32;
                self.counts.insert((Sec::Untracked, e.path.clone()), (lines, 0));
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

    /// The main checkout's folder name when `dir` is a linked worktree.
    pub fn main_worktree(&self, dir: &Path) -> Option<String> {
        let main = self.worktrees.first()?;
        (main.path != dir).then(|| main.name())
    }
}

/// The entries in `git status --porcelain=v1 -z` records, staged first.
fn parse_entries<'a>(mut toks: impl Iterator<Item = &'a str>) -> Vec<Entry> {
    let mut entries = Vec::new();
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
            ('?', _) => entries.push(Entry { sec: Sec::Untracked, code: '?', path }),
            ('!', _) => {}
            // Unmerged: one entry, not a staged one and a changed one.
            ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U') => {
                entries.push(Entry { sec: Sec::Conflict, code: 'U', path })
            }
            _ => {
                if x != ' ' {
                    entries.push(Entry { sec: Sec::Staged, code: x, path: path.clone() });
                }
                if y != ' ' {
                    entries.push(Entry { sec: Sec::Changed, code: y, path });
                }
            }
        }
    }
    entries.sort_by_key(|e| e.sec);
    entries
}

/// One checkout of the repo: the main one or a linked worktree.
pub struct Worktree {
    pub path: PathBuf,
    /// The branch checked out, or the short hash when detached.
    pub branch: String,
    /// A bare repo has nothing checked out, so there's nothing to switch to.
    pub bare: bool,
}

impl Worktree {
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// How many files a checkout has changed, or None when it can't be read.
/// Whether the file still has a conflict's opening marker in it.
pub fn has_conflict_markers(dir: &Path, path: &str) -> bool {
    std::fs::read(dir.join(path)).is_ok_and(|bytes| bytes.split(|&b| b == b'\n').any(|l| l.starts_with(b"<<<<<<< ")))
}

pub fn changed_count(dir: &Path) -> Option<usize> {
    git(dir, &["status", "--porcelain"]).ok().map(|s| s.lines().count())
}

/// The repo's checkouts as `git worktree list` gives them, the main one first.
pub fn worktrees(dir: &Path) -> Vec<Worktree> {
    git(dir, &["worktree", "list", "--porcelain"]).map(|out| parse_worktrees(&out)).unwrap_or_default()
}

fn parse_worktrees(out: &str) -> Vec<Worktree> {
    out.split("\n\n")
        .filter_map(|block| {
            let mut wt = Worktree { path: PathBuf::new(), branch: String::new(), bare: false };
            for line in block.lines() {
                if let Some(path) = line.strip_prefix("worktree ") {
                    // Compare like the canonical dir rex-git runs in.
                    let path = PathBuf::from(path);
                    wt.path = path.canonicalize().unwrap_or(path);
                } else if let Some(head) = line.strip_prefix("HEAD ") {
                    wt.branch = head.chars().take(7).collect();
                } else if let Some(branch) = line.strip_prefix("branch ") {
                    wt.branch = branch.trim_start_matches("refs/heads/").to_string();
                } else if line == "bare" {
                    wt.bare = true;
                }
            }
            (!wt.path.as_os_str().is_empty()).then_some(wt)
        })
        .collect()
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
        Sec::Changed | Sec::Conflict => git(dir, &["diff", "--no-color", &u, "--", &e.path]),
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
/// Bring the branch level with its upstream: publish it if it has none,
/// otherwise fetch, pull (fast-forward only) what's new, and push what's ours.
pub fn sync(dir: &Path) -> Result<String, String> {
    if git(dir, &["rev-parse", "--abbrev-ref", "@{u}"]).is_err() {
        git(dir, &["push", "--quiet", "-u", "origin", "HEAD"])?;
        return Ok("published".into());
    }
    git(dir, &["fetch", "--quiet", "--prune"])?;
    let count = |range: &str| {
        git(dir, &["rev-list", "--count", range]).ok().and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
    };
    let (behind, ahead) = (count("HEAD..@{u}"), count("@{u}..HEAD"));
    let commits = |n: u32| if n == 1 { "1 commit".to_string() } else { format!("{n} commits") };
    let mut done = Vec::new();
    if behind > 0 {
        git(dir, &["pull", "--ff-only", "--quiet"])?;
        done.push(format!("pulled {}", commits(behind)));
    }
    if ahead > 0 {
        git(dir, &["push", "--quiet"])?;
        done.push(format!("pushed {}", commits(ahead)));
    }
    Ok(if done.is_empty() { "up to date".into() } else { done.join(", ") })
}

/// One row of `git log --graph`: the lane drawing, and the commit if the row
/// has one (rows that only bend lanes don't).
pub struct GraphRow {
    pub lanes: String,
    pub commit: Option<GraphCommit>,
}

pub struct GraphCommit {
    pub hash: String,
    pub refs: Vec<String>,
    pub subject: String,
    pub when: String,
}

/// The commit graph for HEAD, or for every branch and tag with `all`.
pub fn graph(dir: &Path, all: bool) -> Result<Vec<GraphRow>, String> {
    let mut args = vec!["log", "--graph", "--color=never", "-n", "500", "--format=%x1f%h%x1f%D%x1f%s%x1f%cr"];
    if all {
        args.push("--all");
    }
    Ok(parse_graph(&git(dir, &args)?))
}

fn parse_graph(out: &str) -> Vec<GraphRow> {
    out.lines()
        .map(|l| {
            let mut p = l.split('\x1f');
            let lanes = p.next().unwrap_or("").trim_end().to_string();
            let commit = match (p.next(), p.next(), p.next(), p.next()) {
                (Some(hash), Some(refs), Some(subject), Some(when)) => Some(GraphCommit {
                    hash: hash.into(),
                    refs: refs.split(", ").filter(|r| !r.is_empty()).map(String::from).collect(),
                    subject: subject.into(),
                    when: short_age(when),
                }),
                _ => None,
            };
            GraphRow { lanes, commit }
        })
        .collect()
}

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
        prompt.push_str(&format!("The change was made for this task, which tells you why: {}\n", intent.trim()));
    }
    if !recent.trim().is_empty() {
        prompt.push_str(&format!("Match the style of these recent subjects:\n{recent}"));
    }
    if !start.trim().is_empty() {
        prompt.push_str(&format!("The author has started the message. Keep their words and complete it:\n{start}\n"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    #[test]
    fn unmerged_codes_are_one_conflict_entry() {
        let entries = parse_entries("UU a\0AA b\0DU c\0M  d\0".split('\0'));
        let got: Vec<(Sec, char, &str)> = entries.iter().map(|e| (e.sec, e.code, e.path.as_str())).collect();
        assert_eq!(
            got,
            [(Sec::Conflict, 'U', "a"), (Sec::Conflict, 'U', "b"), (Sec::Conflict, 'U', "c"), (Sec::Staged, 'M', "d")]
        );
    }

    #[test]
    fn finds_conflict_markers() {
        let t = TempRepo::new("conflict-markers");
        t.write("x", "a\n<<<<<<< HEAD\nb\n");
        assert!(has_conflict_markers(&t.dir, "x"));
        t.write("y", "a <<<<<<< b\n");
        assert!(!has_conflict_markers(&t.dir, "y"));
        assert!(!has_conflict_markers(&t.dir, "missing"));
    }

    #[test]
    fn entries_by_section() {
        let entries = parse_entries("M  a.rs\0 M b.rs\0MM c.rs\0?? d.rs\0R  new.rs\0old.rs\0".split('\0'));
        let got: Vec<(Sec, char, &str)> = entries.iter().map(|e| (e.sec, e.code, e.path.as_str())).collect();
        assert_eq!(
            got,
            [
                (Sec::Staged, 'M', "a.rs"),
                (Sec::Staged, 'M', "c.rs"),
                (Sec::Staged, 'R', "new.rs"),
                (Sec::Changed, 'M', "b.rs"),
                (Sec::Changed, 'M', "c.rs"),
                (Sec::Untracked, '?', "d.rs"),
            ]
        );
    }

    #[test]
    fn ignored_and_short_records_are_skipped() {
        assert!(parse_entries("!! target/x\0\0ab\0".split('\0')).is_empty());
    }

    fn head(h: &str) -> Repo {
        let mut repo = Repo::default();
        repo.parse_head(h, Path::new("."));
        repo
    }

    #[test]
    fn head_with_upstream_and_counts() {
        let r = head("main...origin/main [ahead 1, behind 2]");
        assert_eq!(r.branch, "main");
        assert_eq!(r.upstream, Some("origin/main".into()));
        assert_eq!((r.ahead, r.behind), (1, 2));
    }

    #[test]
    fn head_without_upstream() {
        let r = head("main");
        assert_eq!(r.branch, "main");
        assert_eq!(r.upstream, None);
        assert_eq!((r.ahead, r.behind), (0, 0));
    }

    #[test]
    fn head_before_the_first_commit() {
        assert_eq!(head("No commits yet on main").branch, "main");
    }

    #[test]
    fn head_with_a_gone_upstream() {
        let r = head("main...origin/main [gone]");
        assert_eq!(r.branch, "main");
        assert_eq!(r.upstream, None);
    }

    #[test]
    fn worktree_blocks() {
        let wts = parse_worktrees(
            "worktree /nonexistent/main\nHEAD abcdef1234567890\nbranch refs/heads/main\n\n\
             worktree /nonexistent/wt\nHEAD 1234567890abcdef\ndetached\n\n\
             worktree /nonexistent/repo.git\nbare\n\n",
        );
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].path, PathBuf::from("/nonexistent/main"));
        assert_eq!(wts[0].branch, "main");
        assert!(!wts[0].bare);
        assert_eq!(wts[1].branch, "1234567");
        assert!(wts[2].bare);
    }

    #[test]
    fn graph_rows() {
        let rows = parse_graph("* \x1fabc1234\x1fHEAD -> main, tag: v1\x1ffix: thing\x1f2 hours ago\n|\\  \n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].lanes, "*");
        let c = rows[0].commit.as_ref().unwrap();
        assert_eq!(c.hash, "abc1234");
        assert_eq!(c.refs, ["HEAD -> main", "tag: v1"]);
        assert_eq!(c.subject, "fix: thing");
        assert_eq!(c.when, "2h");
        assert_eq!(rows[1].lanes, "|\\");
        assert!(rows[1].commit.is_none());
    }

    #[test]
    fn ages() {
        assert_eq!(short_age("3 hours ago"), "3h");
        assert_eq!(short_age("1 month ago"), "1mo");
        assert_eq!(short_age("2 weeks ago"), "2w");
        assert_eq!(short_age("5 minutes ago"), "5m");
        assert_eq!(short_age("now"), "now");
    }

    #[test]
    fn loads_a_real_repo() {
        let t = TempRepo::new("loads-a-real-repo");
        t.write("a.txt", "one\n");
        t.write("b.txt", "one\n");
        t.commit_all("base");
        t.write("a.txt", "one\ntwo\n");
        t.git(&["add", "a.txt"]);
        t.write("b.txt", "changed\n");
        t.write("c.txt", "x\ny\n");
        let repo = Repo::load(&t.dir);

        assert!(repo.is_repo);
        assert_eq!(repo.branch, "main");
        assert!(repo.upstream.is_none());
        assert_eq!(repo.staged(), 1);
        let got: Vec<(Sec, &str)> = repo.entries.iter().map(|e| (e.sec, e.path.as_str())).collect();
        assert_eq!(got, [(Sec::Staged, "a.txt"), (Sec::Changed, "b.txt"), (Sec::Untracked, "c.txt")]);
        assert_eq!(repo.counts[&(Sec::Staged, "a.txt".to_string())], (1, 0));
        assert_eq!(repo.counts[&(Sec::Changed, "b.txt".to_string())], (1, 1));
        assert_eq!(repo.counts[&(Sec::Untracked, "c.txt".to_string())], (2, 0));
        assert_eq!(repo.commits.len(), 1);
        assert_eq!(repo.commits[0].1, "base");
    }

    #[test]
    fn not_a_repo() {
        let dir = std::env::temp_dir().join(format!("rex-git-test-{}-not-a-repo", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let is_repo = Repo::load(&dir).is_repo;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!is_repo);
    }

    #[test]
    fn ssh_command_rules() {
        assert_eq!(ssh_command(false, false, None).as_deref(), Some("ssh -o BatchMode=yes"));
        assert_eq!(ssh_command(false, false, Some("  \n")).as_deref(), Some("ssh -o BatchMode=yes"));
        assert_eq!(
            ssh_command(false, false, Some("ssh -i ~/.ssh/work\n")).as_deref(),
            Some("ssh -i ~/.ssh/work -o BatchMode=yes")
        );
        assert_eq!(
            ssh_command(false, false, Some("/usr/bin/ssh -F cfg")).as_deref(),
            Some("/usr/bin/ssh -F cfg -o BatchMode=yes")
        );
        assert_eq!(ssh_command(false, false, Some("plink -batch")), None);
        assert_eq!(ssh_command(true, false, Some("ssh -i key")), None);
        assert_eq!(ssh_command(false, true, None), None);
    }

    #[test]
    fn configured_ssh_command_reaches_git() {
        // The developer's own environment decides the result when these are set.
        if env::var_os("GIT_SSH_COMMAND").is_some() || env::var_os("GIT_SSH").is_some() {
            return;
        }
        let t = TempRepo::new("configured-ssh-command");
        t.git(&["config", "core.sshCommand", "ssh -i /nonexistent/key"]);
        let cmd = git_cmd(&t.dir, &["status"]);
        let ssh = cmd.get_envs().find(|(k, _)| *k == "GIT_SSH_COMMAND").and_then(|(_, v)| v);
        assert_eq!(ssh.and_then(|v| v.to_str()), Some("ssh -i /nonexistent/key -o BatchMode=yes"));
    }
}
