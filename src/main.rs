//! rex-git: a git side pane for Rex, styled after Axel.
//!
//! Shows the branch and its sync state, the changed files (marking the ones
//! an Axel thread's agents changed), and recent commits. Click a file or
//! press Enter for its diff, Space to stage it, `c` to write a commit message
//! (Tab asks Claude to draft or complete it), `g` for the commit graph, and
//! `p`/`P`/`f` to push, pull and fetch.

mod axel;
mod diff;
mod git;

use std::{
    collections::HashMap,
    env,
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use ratatui::{
    DefaultTerminal, Frame,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
            KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    diff::{Item, Kind, Parsed},
    git::{Entry, Repo, Sec, git},
};

const REFRESH: Duration = Duration::from_secs(2);
const AUTO_FETCH: Duration = Duration::from_secs(180);
const FLASH: Duration = Duration::from_secs(6);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const CONTEXTS: [u32; 3] = [3, 10, 30];

// Lanes in the commit graph take these in turn.
const LANES: [Color; 6] = [ACCENT, Color::Blue, GREEN, Color::Yellow, Color::Magenta, Color::Cyan];

// Axel's palette: coral accent, orange agent names, gray labels.
const ACCENT: Color = Color::Rgb(255, 117, 101);
const AGENT: Color = Color::Indexed(208);
const GRAY: Color = Color::Indexed(8);
const GREEN: Color = Color::Rgb(120, 205, 140);
const RED: Color = Color::Rgb(240, 115, 115);
const ADD_BG: Color = Color::Rgb(20, 46, 31);
const ADD_FG: Color = Color::Rgb(190, 238, 200);
const ADD_EMPH: Color = Color::Rgb(38, 98, 60);
const DEL_BG: Color = Color::Rgb(56, 22, 26);
const DEL_FG: Color = Color::Rgb(246, 186, 186);
const DEL_EMPH: Color = Color::Rgb(122, 40, 46);

fn fg(s: impl Into<String>, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(c))
}

fn gray(s: impl Into<String>) -> Span<'static> {
    fg(s, GRAY)
}

fn accent(s: impl Into<String>) -> Span<'static> {
    fg(s, ACCENT)
}

// ---------------------------------------------------------------- app

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Files,
    Diff,
    Commit,
    Graph,
    Worktrees,
    Help,
}

enum Job {
    Drafted(Result<String, String>),
    Synced { what: &'static str, quiet: bool, result: Result<String, String> },
}

struct DiffView {
    entry: Entry,
    parsed: Parsed,
    scroll: usize,
    /// Rendered rows for the width they were wrapped at.
    rows: Option<(u16, Vec<Line<'static>>)>,
}

struct GraphView {
    all: bool,
    rows: Vec<git::GraphRow>,
    scroll: usize,
}

struct WorktreeView {
    /// Each checkout with how many files it has changed.
    items: Vec<(git::Worktree, Option<usize>)>,
    sel: usize,
}

enum Row {
    Head(Sec, usize),
    Item(usize),
    Blank,
}

struct App {
    dir: PathBuf,
    name: String,
    repo: Repo,
    axel: Option<axel::Info>,
    sel: usize,
    scroll: usize,
    mode: Mode,
    diff: Option<DiffView>,
    graph: Option<GraphView>,
    worktrees: Option<WorktreeView>,
    context: usize,
    msg: String,
    /// Messages being written in the other worktrees, kept across switches.
    msgs: HashMap<PathBuf, String>,
    /// Enter was pressed with nothing staged: ask whether to stage everything.
    confirm_stage: bool,
    /// The message box's first visible row, and whether it follows the cursor.
    msg_scroll: usize,
    msg_follow: bool,
    msg_page: usize,
    /// Ctrl+E asked for $EDITOR, which the run loop opens.
    edit: bool,
    /// The footer message, whether it's an error, and whether it stays until
    /// the next key or click instead of fading after `FLASH`.
    flash: Option<(String, bool, bool, Instant)>,
    /// The last error in full, which `!` shows.
    last_error: Option<String>,
    show_error: bool,
    drafting: Option<Instant>,
    syncing: Option<(&'static str, Instant)>,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
    axel_rx: mpsc::Receiver<Option<axel::Info>>,
    axel_dir: mpsc::Sender<PathBuf>,
    last_refresh: Instant,
    last_fetch: Option<Instant>,
    /// The last fetch that worked, which the header shows.
    fetched: Option<Instant>,
    title: String,
    // Screen geometry from the last draw, for mouse clicks.
    click_rows: Vec<(u16, usize)>,
    marker_x: u16,
    commit_rect: Rect,
    sync_rect: Rect,
    /// Footer hints on screen, as (row, from, to, key), so a click presses the key.
    hint_hits: Vec<(u16, u16, u16, KeyEvent)>,
    diff_header_y: u16,
    diff_page: usize,
    quit: bool,
}

impl App {
    fn new(dir: PathBuf, thread: Option<String>) -> App {
        let (tx, rx) = mpsc::channel();
        let (axel_tx, axel_rx) = mpsc::channel();
        let (axel_dir, dirs) = mpsc::channel();
        axel::watch(dir.clone(), thread, axel_tx, dirs);
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let repo = Repo::load(&dir);
        App {
            dir,
            name,
            repo,
            axel: None,
            sel: 0,
            scroll: 0,
            mode: Mode::Files,
            diff: None,
            graph: None,
            worktrees: None,
            context: 0,
            msg: String::new(),
            msgs: HashMap::new(),
            confirm_stage: false,
            msg_scroll: 0,
            msg_follow: true,
            msg_page: 8,
            edit: false,
            flash: None,
            last_error: None,
            show_error: false,
            drafting: None,
            syncing: None,
            tx,
            rx,
            axel_rx,
            axel_dir,
            last_refresh: Instant::now(),
            last_fetch: None,
            fetched: None,
            title: String::new(),
            click_rows: Vec::new(),
            marker_x: 0,
            commit_rect: Rect::default(),
            sync_rect: Rect::default(),
            hint_hits: Vec::new(),
            diff_header_y: 0,
            diff_page: 20,
            quit: false,
        }
    }

    fn refresh(&mut self) {
        let keep = self.repo.entries.get(self.sel).cloned();
        self.repo = Repo::load(&self.dir);
        if let Some(keep) = keep
            && let Some(i) = self.repo.entries.iter().position(|e| *e == keep)
        {
            self.sel = i;
        }
        self.sel = self.sel.min(self.repo.entries.len().saturating_sub(1));
        self.last_refresh = Instant::now();
    }

    /// Show a message; an error stays until the next key or click.
    fn flash(&mut self, text: impl Into<String>, err: bool) {
        self.flash_for(text, err, err);
    }

    fn flash_for(&mut self, text: impl Into<String>, err: bool, sticky: bool) {
        let text = text.into();
        // Git ends with advice, so an error leads with the line that says what failed.
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let cause = |l: &&str| l.contains("fatal:") || l.contains("error:");
        let line = if err { lines.clone().find(cause) } else { None };
        let line = line.or_else(|| lines.next_back()).unwrap_or("");
        self.flash = Some((line.to_string(), err, sticky, Instant::now()));
        if err {
            self.last_error = Some(text.trim().to_string());
        }
    }

    /// Whether an agent on the thread changed this file: `Some(true)` in its
    /// latest turn that changed files, `Some(false)` in an earlier one.
    fn touched(&self, e: &Entry) -> Option<bool> {
        let info = self.axel.as_ref()?;
        if axel::mentions(&info.last_turn, &e.path) {
            Some(true)
        } else if axel::mentions(&info.all_turns, &e.path) {
            Some(false)
        } else {
            None
        }
    }

    fn tick(&mut self) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Drafted(result) => {
                    self.drafting = None;
                    match result {
                        Ok(text) => {
                            self.msg = text;
                            self.mode = Mode::Commit;
                            (self.msg_scroll, self.msg_follow) = (0, false);
                        }
                        Err(err) => self.flash(err, true),
                    }
                }
                Job::Synced { what, quiet, result } => {
                    self.syncing = None;
                    if what == "fetch" || what == "sync" {
                        self.last_fetch = Some(Instant::now());
                        if result.is_ok() {
                            self.fetched = self.last_fetch;
                        }
                    }
                    match result {
                        Ok(text) if what == "commit" => {
                            self.msg.clear();
                            (self.msg_scroll, self.msg_follow) = (0, true);
                            self.mode = Mode::Files;
                            self.flash(text, false);
                        }
                        Ok(text) if what == "sync" => self.flash(text, false),
                        Ok(_) if quiet => {}
                        Ok(_) => self.flash(format!("{what} done"), false),
                        // The background fetch fails quietly, as when offline.
                        Err(err) if quiet => self.flash_for(format!("fetch: {err}"), true, false),
                        Err(err) => self.flash(err, true),
                    }
                    self.refresh();
                }
            }
        }
        while let Ok(info) = self.axel_rx.try_recv() {
            self.axel = info;
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
        let fetch_due = self.last_fetch.is_none_or(|t| t.elapsed() >= AUTO_FETCH);
        if fetch_due && self.syncing.is_none() && self.repo.upstream.is_some() {
            self.sync("fetch", true);
        }
        if self.flash.as_ref().is_some_and(|f| !f.2 && f.3.elapsed() >= FLASH) {
            self.flash = None;
        }
        self.update_title();
    }

    /// Keep the Rex pane's title showing the branch and its sync state.
    fn update_title(&mut self) {
        let Ok(block) = env::var("REX_BLOCK") else { return };
        if !self.repo.is_repo {
            return;
        }
        let mut title = format!("git · {}", self.repo.branch);
        if self.repo.ahead > 0 {
            title.push_str(&format!(" ↑{}", self.repo.ahead));
        }
        if self.repo.behind > 0 {
            title.push_str(&format!(" ↓{}", self.repo.behind));
        }
        if title == self.title {
            return;
        }
        self.title = title.clone();
        thread::spawn(move || {
            let mut cmd = Command::new("rex");
            cmd.args(["block", "rename"]);
            if let Ok(session) = env::var("REX_SESSION") {
                cmd.args(["-s", &session]);
            }
            let _ = cmd.args([&block, &title]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        });
    }

    fn sync(&mut self, what: &'static str, quiet: bool) {
        if self.syncing.is_some() {
            self.flash("busy, one moment", true);
            return;
        }
        if what == "sync" {
            self.syncing = Some((what, Instant::now()));
            let (dir, tx) = (self.dir.clone(), self.tx.clone());
            thread::spawn(move || {
                let result = git::sync(&dir);
                let _ = tx.send(Job::Synced { what, quiet, result });
            });
            return;
        }
        let args: Vec<&'static str> = match what {
            "fetch" => vec!["fetch", "--quiet", "--prune"],
            "pull" => vec!["pull", "--ff-only", "--quiet"],
            "push" if self.repo.upstream.is_none() => vec!["push", "--quiet", "-u", "origin", "HEAD"],
            "push" => vec!["push", "--quiet"],
            _ => return,
        };
        self.syncing = Some((what, Instant::now()));
        let (dir, tx) = (self.dir.clone(), self.tx.clone());
        thread::spawn(move || {
            let result = git(&dir, &args);
            let _ = tx.send(Job::Synced { what, quiet, result });
        });
    }

    /// Draft a message, or complete the one being written, and open the commit box.
    fn draft(&mut self) {
        self.mode = Mode::Commit;
        if self.drafting.is_some() {
            return;
        }
        self.drafting = Some(Instant::now());
        let (dir, tx) = (self.dir.clone(), self.tx.clone());
        let start = self.msg.clone();
        let intent = self.axel.as_ref().map(|i| i.intent.clone()).unwrap_or_default();
        thread::spawn(move || {
            let _ = tx.send(Job::Drafted(git::draft_message(&dir, &start, &intent)));
        });
    }

    fn commit(&mut self) {
        let msg = self.msg.trim().to_string();
        if msg.is_empty() {
            self.flash("write a message first, or Tab to draft one", true);
            return;
        }
        if self.repo.staged() == 0 {
            if self.repo.entries.is_empty() {
                self.flash("nothing to commit", true);
            } else {
                self.confirm_stage = true;
            }
            return;
        }
        if self.syncing.is_some() {
            self.flash("busy, one moment", true);
            return;
        }
        self.syncing = Some(("commit", Instant::now()));
        let (dir, tx) = (self.dir.clone(), self.tx.clone());
        thread::spawn(move || {
            let result = git::commit(&dir, &msg);
            let _ = tx.send(Job::Synced { what: "commit", quiet: false, result });
        });
    }

    fn toggle(&mut self, i: usize) {
        let Some(e) = self.repo.entries.get(i).cloned() else { return };
        let result = match e.sec {
            Sec::Staged => git(&self.dir, &["restore", "--staged", "--", &e.path])
                .or_else(|_| git(&self.dir, &["rm", "--cached", "--quiet", "--", &e.path])),
            _ => git(&self.dir, &["add", "--", &e.path]),
        };
        if let Err(err) = result {
            self.flash(err, true);
        }
        self.refresh();
        // Follow the file to its new section, so Space twice undoes itself.
        let sec = if e.sec == Sec::Staged { Sec::Changed } else { Sec::Staged };
        if let Some(j) = self.repo.entries.iter().position(|x| x.path == e.path && x.sec == sec) {
            self.sel = j;
        }
    }

    /// Stage the files the agents changed in their latest turn.
    fn stage_turn(&mut self) {
        let Some(info) = self.axel.as_ref() else {
            self.flash("no Axel thread is working here", true);
            return;
        };
        let paths: Vec<String> = self
            .repo
            .entries
            .iter()
            .filter(|e| e.sec != Sec::Staged && axel::mentions(&info.last_turn, &e.path))
            .map(|e| e.path.clone())
            .collect();
        if paths.is_empty() {
            self.flash("nothing left to stage from the last turn", true);
            return;
        }
        let mut args = vec!["add", "--"];
        args.extend(paths.iter().map(String::as_str));
        match git(&self.dir, &args) {
            Ok(_) => self.flash(format!("staged {} files from the last turn", paths.len()), false),
            Err(err) => self.flash(err, true),
        }
        self.refresh();
    }

    fn open_diff(&mut self, i: usize) {
        let Some(e) = self.repo.entries.get(i).cloned() else { return };
        self.sel = i;
        let parsed = diff::parse(&git::diff_text(&self.dir, &e, CONTEXTS[self.context]));
        self.diff = Some(DiffView { entry: e, parsed, scroll: 0, rows: None });
        self.mode = Mode::Diff;
    }

    fn open_graph(&mut self, all: bool) {
        match git::graph(&self.dir, all) {
            Ok(rows) => {
                self.graph = Some(GraphView { all, rows, scroll: 0 });
                self.mode = Mode::Graph;
            }
            Err(err) => self.flash(err, true),
        }
    }

    fn open_worktrees(&mut self) {
        let items: Vec<_> = git::worktrees(&self.dir)
            .into_iter()
            .filter(|w| !w.bare)
            .map(|w| {
                let changed = git::changed_count(&w.path);
                (w, changed)
            })
            .collect();
        if items.len() < 2 {
            self.flash("no other worktrees; git worktree add makes one", false);
            return;
        }
        let sel = items.iter().position(|(w, _)| w.path == self.dir).unwrap_or(0);
        self.worktrees = Some(WorktreeView { items, sel });
        self.mode = Mode::Worktrees;
    }

    /// Point everything at another checkout, keeping each one's message.
    fn switch_worktree(&mut self, i: usize) {
        let Some(path) = self.worktrees.as_ref().and_then(|v| v.items.get(i)).map(|(w, _)| w.path.clone()) else {
            return;
        };
        if path == self.dir {
            self.mode = Mode::Files;
            return;
        }
        if self.syncing.is_some() || self.drafting.is_some() {
            self.flash("busy, one moment", true);
            return;
        }
        if !path.is_dir() {
            self.flash(format!("{} is gone; git worktree prune cleans it up", path.display()), true);
            return;
        }
        let old = std::mem::replace(&mut self.dir, path.clone());
        let msg = self.msgs.remove(&path).unwrap_or_default();
        self.msgs.insert(old, std::mem::replace(&mut self.msg, msg));
        self.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.repo = Repo::load(&path);
        self.axel = None;
        let _ = self.axel_dir.send(path);
        (self.sel, self.scroll) = (0, 0);
        (self.diff, self.graph, self.worktrees) = (None, None, None);
        (self.msg_scroll, self.msg_follow, self.confirm_stage) = (0, true, false);
        (self.last_fetch, self.fetched) = (None, None);
        self.mode = Mode::Files;
        self.last_refresh = Instant::now();
    }

    // ------------------------------------------------------------ input

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.dismiss_error();
        if self.show_error {
            self.show_error = false;
            return;
        }
        if k.code == KeyCode::Char('!') && self.mode != Mode::Commit && self.last_error.is_some() {
            self.show_error = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Files,
            Mode::Commit => self.on_commit_key(k),
            Mode::Diff => self.on_diff_key(k),
            Mode::Graph => self.on_graph_key(k),
            Mode::Worktrees => self.on_worktrees_key(k),
            Mode::Files => self.on_files_key(k),
        }
    }

    /// An error stays on screen until the next key or click, which still acts.
    fn dismiss_error(&mut self) {
        if self.flash.as_ref().is_some_and(|f| f.2) {
            self.flash = None;
        }
    }

    fn on_files_key(&mut self, k: KeyEvent) {
        let n = self.repo.entries.len();
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.sel = (self.sel + 1).min(n - 1),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Home => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('d') => self.open_diff(self.sel),
            KeyCode::Char(' ') | KeyCode::Char('s') => self.toggle(self.sel),
            KeyCode::Char('a') => {
                if let Err(err) = git(&self.dir, &["add", "-A"]) {
                    self.flash(err, true);
                }
                self.refresh();
            }
            KeyCode::Char('u') => {
                let r = git(&self.dir, &["reset", "-q"]).or_else(|_| git(&self.dir, &["rm", "-r", "--cached", "-q", "."]));
                if let Err(err) = r {
                    self.flash(err, true);
                }
                self.refresh();
            }
            KeyCode::Char('t') => self.stage_turn(),
            KeyCode::Char('c') | KeyCode::Char('i') => self.enter_commit(),
            KeyCode::Tab => self.draft(),
            KeyCode::Char('g') => self.open_graph(false),
            KeyCode::Char('w') => self.open_worktrees(),
            KeyCode::Char('p') => self.sync("push", false),
            KeyCode::Char('P') => self.sync("pull", false),
            KeyCode::Char('f') => self.sync("fetch", false),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn enter_commit(&mut self) {
        self.mode = Mode::Commit;
        self.confirm_stage = false;
        // Pre-draft a message when there's something staged and nothing written.
        if self.msg.is_empty() && self.repo.staged() > 0 {
            self.draft();
        }
    }

    fn on_commit_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let newline = k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        // The draft replaces the message when it arrives, so don't let typing get lost.
        if self.drafting.is_some() && k.code != KeyCode::Esc {
            return;
        }
        if self.confirm_stage {
            self.confirm_stage = false;
            match k.code {
                KeyCode::Enter => {
                    if let Err(err) = git(&self.dir, &["add", "-A"]) {
                        self.flash(err, true);
                        return;
                    }
                    self.refresh();
                    self.commit();
                }
                KeyCode::Char('s') => {
                    self.mode = Mode::Files;
                    self.flash("space stages a file, then c ⏎ commits", false);
                }
                _ => {}
            }
            return;
        }
        let page = self.msg_page.max(1);
        match k.code {
            KeyCode::Esc => self.mode = Mode::Files,
            KeyCode::Tab => self.draft(),
            KeyCode::Char('e') if ctrl => self.edit = true,
            KeyCode::Up => (self.msg_scroll, self.msg_follow) = (self.msg_scroll.saturating_sub(1), false),
            KeyCode::Down => (self.msg_scroll, self.msg_follow) = (self.msg_scroll + 1, false),
            KeyCode::PageUp => (self.msg_scroll, self.msg_follow) = (self.msg_scroll.saturating_sub(page), false),
            KeyCode::PageDown => (self.msg_scroll, self.msg_follow) = (self.msg_scroll + page, false),
            _ => self.on_commit_edit(k, ctrl, newline),
        }
    }

    fn on_commit_edit(&mut self, k: KeyEvent, ctrl: bool, newline: bool) {
        // Typing brings the cursor back into view.
        self.msg_follow = true;
        match k.code {
            KeyCode::Enter if newline => self.msg.push('\n'),
            KeyCode::Char('j') if ctrl => self.msg.push('\n'),
            KeyCode::Enter => self.commit(),
            KeyCode::Backspace => {
                self.msg.pop();
            }
            KeyCode::Char('u') if ctrl => self.msg.clear(),
            KeyCode::Char('w') if ctrl => {
                let t = self.msg.trim_end().len();
                self.msg.truncate(t);
                let cut = self.msg.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.msg.truncate(cut);
            }
            KeyCode::Char(c) if !ctrl => self.msg.push(c),
            _ => {}
        }
    }

    fn on_diff_key(&mut self, k: KeyEvent) {
        let page = self.diff_page.max(1);
        let Some(d) = self.diff.as_mut() else {
            self.mode = Mode::Files;
            return;
        };
        let max = d.rows.as_ref().map_or(0, |(_, r)| r.len().saturating_sub(1));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h') => self.mode = Mode::Files,
            KeyCode::Down | KeyCode::Char('j') => d.scroll = (d.scroll + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') => d.scroll = d.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => d.scroll = (d.scroll + page).min(max),
            KeyCode::PageUp | KeyCode::Char('b') => d.scroll = d.scroll.saturating_sub(page),
            KeyCode::Home => d.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => d.scroll = max,
            KeyCode::Char('e') => {
                self.context = (self.context + 1) % CONTEXTS.len();
                self.open_diff(self.sel);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let n = self.repo.entries.len();
                if n > 0 {
                    let next = if k.code == KeyCode::Char('n') { (self.sel + 1) % n } else { (self.sel + n - 1) % n };
                    self.open_diff(next);
                }
            }
            KeyCode::Char('s') => {
                let (path, scroll) = (d.entry.path.clone(), d.scroll);
                self.toggle(self.sel);
                if let Some(i) = self.repo.entries.iter().position(|e| e.path == path) {
                    self.open_diff(i);
                    if let Some(d) = self.diff.as_mut() {
                        d.scroll = scroll;
                    }
                } else {
                    self.mode = Mode::Files;
                }
            }
            _ => {}
        }
    }

    fn on_worktrees_key(&mut self, k: KeyEvent) {
        let Some(v) = self.worktrees.as_mut() else {
            self.mode = Mode::Files;
            return;
        };
        let max = v.items.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('w') | KeyCode::Left | KeyCode::Char('h') => {
                self.mode = Mode::Files
            }
            KeyCode::Down | KeyCode::Char('j') => v.sel = (v.sel + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') => v.sel = v.sel.saturating_sub(1),
            KeyCode::Home => v.sel = 0,
            KeyCode::End | KeyCode::Char('G') => v.sel = max,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let i = v.sel;
                self.switch_worktree(i);
            }
            KeyCode::Char('r') => self.open_worktrees(),
            _ => {}
        }
    }

    fn on_graph_key(&mut self, k: KeyEvent) {
        let page = self.diff_page.max(1);
        let Some(g) = self.graph.as_mut() else {
            self.mode = Mode::Files;
            return;
        };
        let max = g.rows.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('g') | KeyCode::Left | KeyCode::Char('h') => {
                self.mode = Mode::Files
            }
            KeyCode::Down | KeyCode::Char('j') => g.scroll = (g.scroll + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') => g.scroll = g.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => g.scroll = (g.scroll + page).min(max),
            KeyCode::PageUp | KeyCode::Char('b') => g.scroll = g.scroll.saturating_sub(page),
            KeyCode::Home => g.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => g.scroll = max,
            KeyCode::Char('a') => {
                let all = !g.all;
                self.open_graph(all);
            }
            KeyCode::Char('r') => {
                let (all, scroll) = (g.all, g.scroll);
                self.open_graph(all);
                if let Some(g) = self.graph.as_mut() {
                    g.scroll = scroll.min(g.rows.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if let MouseEventKind::Down(_) = m.kind {
            self.dismiss_error();
            if self.show_error {
                self.show_error = false;
                return;
            }
        }
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            let hit = self.hint_hits.iter().find(|(y, x0, x1, _)| m.row == *y && (*x0..*x1).contains(&m.column));
            if let Some(&(.., key)) = hit {
                self.on_key(key);
                return;
            }
            let header = matches!(self.mode, Mode::Files | Mode::Commit);
            if header && contains(self.sync_rect, m.column, m.row) {
                self.sync("sync", false);
                return;
            }
        }
        match (self.mode, m.kind) {
            (Mode::Diff, MouseEventKind::ScrollDown) => {
                if let Some(d) = self.diff.as_mut() {
                    let max = d.rows.as_ref().map_or(0, |(_, r)| r.len().saturating_sub(1));
                    d.scroll = (d.scroll + 3).min(max);
                }
            }
            (Mode::Diff, MouseEventKind::ScrollUp) => {
                if let Some(d) = self.diff.as_mut() {
                    d.scroll = d.scroll.saturating_sub(3);
                }
            }
            (Mode::Diff | Mode::Graph | Mode::Worktrees, MouseEventKind::Down(MouseButton::Left))
                if m.row == self.diff_header_y =>
            {
                self.mode = Mode::Files;
            }
            (Mode::Worktrees, MouseEventKind::Down(MouseButton::Left)) => {
                if let Some(&(_, i)) = self.click_rows.iter().find(|(y, _)| *y == m.row) {
                    self.switch_worktree(i);
                }
            }
            (Mode::Worktrees, MouseEventKind::ScrollDown) => {
                if let Some(v) = self.worktrees.as_mut() {
                    v.sel = (v.sel + 1).min(v.items.len().saturating_sub(1));
                }
            }
            (Mode::Worktrees, MouseEventKind::ScrollUp) => {
                if let Some(v) = self.worktrees.as_mut() {
                    v.sel = v.sel.saturating_sub(1);
                }
            }
            (Mode::Graph, MouseEventKind::ScrollDown) => {
                if let Some(g) = self.graph.as_mut() {
                    g.scroll = (g.scroll + 3).min(g.rows.len().saturating_sub(1));
                }
            }
            (Mode::Graph, MouseEventKind::ScrollUp) => {
                if let Some(g) = self.graph.as_mut() {
                    g.scroll = g.scroll.saturating_sub(3);
                }
            }
            (Mode::Commit, MouseEventKind::ScrollDown) => (self.msg_scroll, self.msg_follow) = (self.msg_scroll + 3, false),
            (Mode::Commit, MouseEventKind::ScrollUp) => {
                (self.msg_scroll, self.msg_follow) = (self.msg_scroll.saturating_sub(3), false);
            }
            (Mode::Files, MouseEventKind::ScrollDown) => {
                self.sel = (self.sel + 1).min(self.repo.entries.len().saturating_sub(1));
            }
            (Mode::Files, MouseEventKind::ScrollUp) => self.sel = self.sel.saturating_sub(1),
            (Mode::Files | Mode::Commit, MouseEventKind::Down(MouseButton::Left)) => {
                if let Some(&(_, i)) = self.click_rows.iter().find(|(y, _)| *y == m.row) {
                    if m.column <= self.marker_x + 1 {
                        self.sel = i;
                        self.toggle(i);
                    } else {
                        self.open_diff(i);
                    }
                } else if contains(self.commit_rect, m.column, m.row) {
                    if self.mode != Mode::Commit {
                        self.enter_commit();
                    }
                } else if self.mode == Mode::Commit {
                    self.mode = Mode::Files;
                }
            }
            (Mode::Help, MouseEventKind::Down(_)) => self.mode = Mode::Files,
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    fn spinner(&self, since: Instant) -> &'static str {
        SPINNER[(since.elapsed().as_millis() / 80) as usize % SPINNER.len()]
    }

    fn draw(&mut self, f: &mut Frame) {
        self.hint_hits.clear();
        self.sync_rect = Rect::default();
        let area = f.area().inner(Margin { horizontal: 1, vertical: 0 });
        if !self.repo.is_repo {
            let text = vec![
                Line::raw(""),
                Line::from(gray("not a git repository")),
                Line::from(gray(self.dir.display().to_string())),
            ];
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
            return;
        }
        if matches!(self.mode, Mode::Diff | Mode::Graph | Mode::Worktrees) {
            match self.mode {
                Mode::Diff => self.draw_diff(f, area),
                Mode::Graph => self.draw_graph(f, area),
                _ => self.draw_worktrees(f, area),
            }
            if self.show_error {
                self.draw_error(f, area);
            }
            return;
        }

        let header_h = if self.axel.is_some() { 4 } else { 3 };
        if self.mode == Mode::Commit {
            // The message takes the file list's place while it's being written.
            let [header, input, _, footer] = Layout::vertical([
                Constraint::Length(header_h),
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(area);
            self.click_rows.clear();
            self.draw_header(f, header);
            self.draw_input(f, input);
            self.draw_footer(f, footer);
            return;
        }
        let recent_h = if area.height > 24 && !self.repo.commits.is_empty() {
            self.repo.commits.len().min(3) as u16 + 2
        } else {
            0
        };
        // A blank row above the commit box; the keys sit right under it.
        let [header, list, recent, _, input, footer] = Layout::vertical([
            Constraint::Length(header_h),
            Constraint::Min(3),
            Constraint::Length(recent_h),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(area);

        self.draw_header(f, header);
        self.draw_list(f, list);
        self.draw_recent(f, recent);
        self.draw_input(f, input);
        self.draw_footer(f, footer);
        if self.mode == Mode::Help {
            self.draw_help(f, area);
        }
        if self.show_error {
            self.draw_error(f, area);
        }
    }

    /// The repo and branch with the sync state (click it to sync), then the
    /// upstream when it isn't the usual one and when it was last fetched.
    fn draw_header(&mut self, f: &mut Frame, area: Rect) {
        let r = &self.repo;
        let mut sync: Vec<Span> = Vec::new();
        if let Some((what, since)) = self.syncing {
            let what = if what == "sync" { "syncing" } else { what };
            sync.push(accent(format!("{} {what}", self.spinner(since))));
        } else if r.upstream.is_none() {
            sync.push(accent("↑ publish"));
        } else if r.ahead == 0 && r.behind == 0 {
            sync.push(gray("✓ synced"));
        } else {
            if r.ahead > 0 {
                sync.push(format!("↑{}", r.ahead).bold());
            }
            if r.behind > 0 {
                if r.ahead > 0 {
                    sync.push(gray(" "));
                }
                sync.push(format!("↓{}", r.behind).bold());
            }
            sync.push(gray(if r.behind > 0 { " to pull" } else { " to push" }));
        }
        let rows: [Rect; 3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
        let sync_w: u16 = sync.iter().map(|s| s.width() as u16).sum();
        let [left, right] = Layout::horizontal([Constraint::Min(1), Constraint::Length(sync_w + 1)]).areas(rows[0]);
        let room = (left.width as usize).saturating_sub(self.name.chars().count() + 3);
        let branch = truncate_end(&r.branch, room);
        f.render_widget(Line::from(vec![self.name.clone().bold(), gray(" ⎇ "), accent(branch)]), left);
        f.render_widget(Line::from(sync).right_aligned(), right);
        self.sync_rect = Rect { x: right.right().saturating_sub(sync_w), width: sync_w, ..right };

        let mut parts = Vec::new();
        if let Some(main) = r.main_worktree(&self.dir) {
            parts.push(format!("worktree of {main}"));
        }
        match &r.upstream {
            None => parts.push("not published yet".to_string()),
            Some(up) => {
                if *up != format!("origin/{}", r.branch) {
                    parts.push(format!("→ {up}"));
                }
                if let Some(t) = self.fetched {
                    parts.push(format!("fetched {}", ago(t.elapsed())));
                } else if !parts.iter().any(|p| p.starts_with('→')) {
                    parts.push(format!("→ {up}"));
                }
            }
        }
        f.render_widget(Line::from(clip(vec![gray(parts.join(" · "))], rows[1].width as usize)), rows[1]);

        if let Some(info) = &self.axel {
            let n = r.entries.iter().filter(|e| self.touched(e).is_some()).count();
            let mut line = vec![fg("◆ ", AGENT)];
            if info.agents.is_empty() {
                line.push(gray("thread"));
            }
            for (i, a) in info.agents.iter().enumerate() {
                if i > 0 {
                    line.push(gray(" · "));
                }
                line.push(fg(a.clone(), AGENT));
            }
            line.push(gray(match n {
                0 => " · no changes here yet".to_string(),
                1 => " · changed 1 file".to_string(),
                n => format!(" · changed {n} files"),
            }));
            f.render_widget(Line::from(clip(line, rows[2].width as usize)), rows[2]);
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let entries = &self.repo.entries;
        let mut rows: Vec<Row> = Vec::new();
        for sec in [Sec::Staged, Sec::Changed, Sec::Untracked] {
            let idx: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].sec == sec).collect();
            if idx.is_empty() {
                continue;
            }
            if !rows.is_empty() {
                rows.push(Row::Blank);
            }
            rows.push(Row::Head(sec, idx.len()));
            rows.extend(idx.into_iter().map(Row::Item));
        }
        self.click_rows.clear();
        self.marker_x = area.x + 1;
        if rows.is_empty() {
            f.render_widget(Line::from(vec![fg("✓ ", GREEN), gray("working tree clean")]), area);
            return;
        }

        let h = area.height as usize;
        let sel_row = rows.iter().position(|r| matches!(r, Row::Item(i) if *i == self.sel)).unwrap_or(0);
        if sel_row <= 1 {
            self.scroll = 0;
        } else if sel_row < self.scroll + 1 {
            self.scroll = sel_row - 1;
        } else if sel_row + 1 >= self.scroll + h {
            self.scroll = (sel_row + 2).saturating_sub(h);
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(h));

        let focus = self.mode == Mode::Files;
        let with_agent = self.axel.is_some();
        let mut lines = Vec::new();
        for (k, row) in rows.iter().skip(self.scroll).take(h).enumerate() {
            let line = match row {
                Row::Blank => Line::raw(""),
                Row::Head(sec, n) => Line::from(vec![accent(sec.title()).bold(), gray(format!("  {n}"))]),
                Row::Item(i) => {
                    let e = &entries[*i];
                    self.click_rows.push((area.y + k as u16, *i));
                    self.file_row(e, *i == self.sel, focus, with_agent, area.width as usize)
                }
            };
            lines.push(line);
        }
        if rows.len() > self.scroll + h {
            let more = rows.len() - self.scroll - h;
            if let Some(last) = lines.last_mut() {
                *last = Line::from(gray(format!("  ↓ {more} more")));
                self.click_rows.retain(|(y, _)| *y != area.y + h as u16 - 1);
            }
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn file_row(&self, e: &Entry, selected: bool, focus: bool, with_agent: bool, width: usize) -> Line<'static> {
        let mut left = vec![
            if selected && focus { accent("›").bold() } else { " ".into() },
            if e.sec == Sec::Staged { fg("●", GREEN) } else { gray("○") },
            " ".into(),
            fg(e.code.to_string(), code_color(e.code)),
            " ".into(),
        ];
        if with_agent {
            left.push(match self.touched(e) {
                Some(true) => fg("◆ ", AGENT),
                Some(false) => fg("◇ ", AGENT),
                None => "  ".into(),
            });
        }
        let mut right: Vec<Span> = Vec::new();
        if let Some(&(a, d)) = self.repo.counts.get(&(e.sec, e.path.clone())) {
            if a > 0 {
                right.push(fg(format!("+{a}"), GREEN));
            }
            if d > 0 {
                if a > 0 {
                    right.push(" ".into());
                }
                right.push(fg(format!("−{d}"), RED));
            }
        }
        let left_w: usize = left.iter().map(|s| s.width()).sum();
        let right_w: usize = right.iter().map(|s| s.width()).sum();
        let path_w = width.saturating_sub(left_w + right_w + 1);
        // Keep the counts only when the file name still fits beside them.
        let (right, right_w, path_w) = if path_w < 12 && right_w > 0 {
            (Vec::new(), 0, width.saturating_sub(left_w))
        } else {
            (right, right_w, path_w)
        };
        let path = truncate_start(&e.path, path_w);
        let (dir, file) = match path.rfind('/') {
            Some(p) => (path[..=p].to_string(), path[p + 1..].to_string()),
            None => (String::new(), path),
        };
        let used = left_w + dir.chars().count() + file.chars().count();
        left.push(gray(dir));
        left.push(if selected { file.bold() } else { file.into() });
        if right_w > 0 {
            left.push(" ".repeat(width.saturating_sub(used + right_w)).into());
            left.extend(right);
        }
        Line::from(left)
    }

    fn draw_recent(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = vec![Line::raw(""), Line::from(accent("recent").bold())];
        for (i, (h, s, when)) in self.repo.commits.iter().take(area.height as usize - 2).enumerate() {
            let room = (area.width as usize).saturating_sub(h.len() + when.len() + 3);
            // Older commits fade out, like the end of a list.
            let fade = match i {
                0 => Style::new(),
                1 => Style::new().add_modifier(Modifier::DIM),
                _ => Style::new().fg(GRAY).add_modifier(Modifier::DIM),
            };
            let line = Line::from(vec![
                gray(h.clone()),
                " ".into(),
                truncate_end(s, room).into(),
                " ".into(),
                gray(when.clone()),
            ]);
            lines.push(line.patch_style(fade));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The commit message box: one line while the files have focus, the whole
    /// list's space while writing.
    fn draw_input(&mut self, f: &mut Frame, area: Rect) {
        self.commit_rect = area;
        let active = self.mode == Mode::Commit;
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(GRAY))
            .padding(Padding::horizontal(1));
        if active {
            let subject = self.msg.split('\n').next().unwrap_or("").chars().count();
            let color = match subject {
                0..=50 => GRAY,
                51..=72 => Color::Yellow,
                _ => RED,
            };
            let staged = match self.repo.staged() {
                0 => "nothing staged".to_string(),
                n => format!("{n} staged"),
            };
            block = block
                .border_style(Style::new().fg(ACCENT))
                .title(Line::from(accent(" commit message ")))
                .title(Line::from(vec![gray(format!(" {staged} · ")), fg(format!("{subject} "), color)]).right_aligned());
        }
        let body = block.inner(area);
        f.render_widget(block, area);
        let width = body.width as usize;

        let text: Vec<Line> = if let Some(since) = self.drafting {
            let what = if self.msg.is_empty() { "drafting with claude…" } else { "completing with claude…" };
            vec![Line::from(vec![accent(format!("{} ", self.spinner(since))), gray(what)])]
        } else if self.msg.is_empty() {
            let hint = if active { "write a message, or Tab to draft one" } else { "c to commit · Tab drafts a message" };
            let mut line = Vec::new();
            if active {
                line.push(accent("▏"));
            }
            line.push(gray(hint));
            vec![Line::from(clip(line, width))]
        } else if !active {
            let mut lines = self.msg.split('\n');
            let subject = lines.next().unwrap_or("").to_string();
            let more = lines.filter(|l| !l.trim().is_empty()).count();
            let more = match more {
                0 => String::new(),
                1 => "  +1 line".into(),
                n => format!("  +{n} lines"),
            };
            let room = width.saturating_sub(more.chars().count()).max(8);
            vec![Line::from(clip(vec![truncate_end(&subject, room).dim(), gray(more)], width))]
        } else {
            let mut lines: Vec<Line> = wrap_message(&self.msg, width)
                .into_iter()
                .map(|(subject, text)| Line::from(if subject { text.bold() } else { text.into() }))
                .collect();
            if let Some(last) = lines.last_mut() {
                last.push_span(accent("▏"));
            }
            let keep = body.height as usize;
            let max = lines.len().saturating_sub(keep);
            self.msg_page = keep;
            self.msg_scroll = if self.msg_follow { max } else { self.msg_scroll.min(max) };
            lines.drain(..self.msg_scroll);
            lines
        };
        f.render_widget(Paragraph::new(text), body);
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        // The question comes before any flash, so Enter never answers it unseen.
        if self.mode == Mode::Commit && self.confirm_stage {
            let all = format!("stage all {} & commit", self.unstaged_count());
            let keys = [("⏎", all.as_str()), ("s", "pick"), ("esc", "cancel")];
            // A narrow pane keeps the choices and drops the label.
            let lead = if hints_width(&keys) + 16 > area.width as usize {
                vec![]
            } else {
                vec![fg("nothing staged", RED), "  ".into()]
            };
            self.put_hints(f, area, lead, &keys, None);
        } else if let Some(line) = self.flash_line(area.width as usize) {
            f.render_widget(line, area);
        } else if self.mode == Mode::Commit {
            let keys = [("⏎", "commit"), ("⇧⏎", "line"), ("Tab", "draft"), ("^E", "editor"), ("esc", "back")];
            self.put_hints(f, area, vec![], &keys, None);
        } else {
            let keys = self.file_hints();
            self.put_hints(f, area, vec![], &keys, Some(("?", "keys")));
        }
    }

    /// The file list's hints, most useful first for the state the repo is in.
    fn file_hints(&self) -> Vec<(&'static str, &'static str)> {
        let r = &self.repo;
        let mut keys = Vec::new();
        if let Some(e) = r.entries.get(self.sel) {
            keys.push(("spc", if e.sec == Sec::Staged { "unstage" } else { "stage" }));
        }
        if r.staged() > 0 {
            keys.push(("c", "commit"));
        } else if !r.entries.is_empty() {
            keys.push(("a", "stage all"));
        }
        if r.upstream.is_none() {
            keys.push(("p", "publish"));
        } else if r.behind > 0 {
            keys.push(("P", "pull"));
        } else if r.ahead > 0 {
            keys.push(("p", "push"));
        }
        if !r.entries.is_empty() {
            keys.push(("⏎", "diff"));
            keys.push(("Tab", "draft"));
        }
        if self.axel.is_some() {
            keys.push(("t", "stage turn"));
        }
        keys.push(("g", "graph"));
        if r.worktrees.iter().filter(|w| !w.bare).count() > 1 {
            keys.push(("w", "worktrees"));
        }
        keys
    }

    /// Draw `lead`, then as many hints as fit, then `pinned` at the right edge,
    /// and remember where each hint is so clicking it presses its key.
    fn put_hints(
        &mut self,
        f: &mut Frame,
        area: Rect,
        lead: Vec<Span<'static>>,
        keys: &[(&str, &str)],
        pinned: Option<(&str, &str)>,
    ) {
        let width = area.width as usize;
        let pin_w = pinned.map_or(0, |p| hints_width(&[p]) + 2);
        let mut spans = lead;
        let mut x: usize = spans.iter().map(|s| s.width()).sum();
        let mut add = |spans: &mut Vec<Span<'static>>, x: &mut usize, (k, v): (&str, &str)| {
            let w = k.chars().count() + 1 + v.chars().count();
            if let Some(key) = hint_key(k) {
                let x0 = area.x + *x as u16;
                self.hint_hits.push((area.y, x0, x0 + w as u16, key));
            }
            spans.push(accent(format!("{k} ")));
            spans.push(gray(v.to_string()));
            *x += w;
        };
        let mut first = true;
        for &key in keys {
            let gap = if first { 0 } else { 2 };
            if x + gap + hints_width(&[key]) + pin_w > width {
                break;
            }
            if !first {
                spans.push("  ".into());
                x += 2;
            }
            add(&mut spans, &mut x, key);
            first = false;
        }
        if let Some(p) = pinned {
            let pad = width.saturating_sub(x + hints_width(&[p]));
            spans.push(" ".repeat(pad).into());
            x += pad;
            add(&mut spans, &mut x, p);
        }
        f.render_widget(Line::from(spans), area);
    }

    /// Files that `git add -A` would stage.
    fn unstaged_count(&self) -> usize {
        let mut paths: Vec<&str> = self.repo.entries.iter().filter(|e| e.sec != Sec::Staged).map(|e| e.path.as_str()).collect();
        paths.dedup();
        paths.len()
    }

    fn flash_line(&self, width: usize) -> Option<Line<'static>> {
        let (text, err, ..) = self.flash.as_ref()?;
        if !*err {
            return Some(Line::from(fg(truncate_end(text, width), GREEN)));
        }
        // Point at the rest when the line shows only part of the error.
        let more = "  ! more";
        let cut = text.chars().count() > width || self.last_error.as_deref() != Some(text.as_str());
        if !cut || width < 24 {
            return Some(Line::from(fg(truncate_end(text, width), RED)));
        }
        let room = width - more.chars().count();
        Some(Line::from(vec![fg(truncate_end(text, room), RED), accent("  !"), gray(" more")]))
    }

    /// The last error in full, over whatever is on screen.
    fn draw_error(&self, f: &mut Frame, area: Rect) {
        let Some(text) = self.last_error.as_deref() else { return };
        let width = area.width.saturating_sub(4).max(1) as usize;
        let mut lines = vec![Line::from(fg("last error", RED).bold())];
        lines.extend(text.lines().map(|l| Line::from(l.to_string())));
        lines.push(Line::raw(""));
        lines.push(Line::from(gray("any key to close")));
        // Count wrapped rows so the box fits the text.
        let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(width)).sum();
        let h = (rows as u16 + 2).min(area.height);
        let rect = Rect { x: area.x, y: area.y + area.height.saturating_sub(h) / 2, width: area.width, height: h };
        f.render_widget(Clear, rect);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(RED)).padding(Padding::horizontal(1));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), rect);
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let rows: &[(&str, &str)] = &[
            ("↑↓ j k", "move"),
            ("⏎ click", "show the diff"),
            ("spc  ●", "stage / unstage"),
            ("a  u", "stage all / unstage all"),
            ("t", "stage the agents' last turn"),
            ("c", "write the commit message"),
            ("Tab", "draft or complete it with claude"),
            ("^E", "edit it in $EDITOR"),
            ("g", "commit graph"),
            ("w", "switch worktree"),
            ("p  P", "push / pull --ff-only"),
            ("f", "fetch (also every 3m)"),
            ("click ✓", "sync: fetch, pull, push"),
            ("!", "the last error in full"),
            ("q", "quit"),
            ("", ""),
            ("◆  ◇", "changed in the last / an earlier turn"),
            ("", ""),
            ("in a diff", ""),
            ("n  N", "next / previous file"),
            ("s", "stage / unstage the file"),
            ("e", "more context"),
            ("esc", "back"),
            ("", ""),
            ("in the graph", ""),
            ("a", "all branches / this one"),
        ];
        let h = (rows.len() as u16 + 2).min(area.height);
        let rect = Rect { x: area.x, y: area.y + area.height.saturating_sub(h) / 2, width: area.width, height: h };
        f.render_widget(Clear, rect);
        let mut lines = vec![Line::from(accent("keys").bold())];
        lines.extend(rows.iter().map(|(k, v)| Line::from(vec![accent(format!("{k:<10}")), gray(v.to_string())])));
        f.render_widget(Paragraph::new(lines), rect);
    }

    fn draw_diff(&mut self, f: &mut Frame, area: Rect) {
        let [header, _, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.diff_header_y = header.y;
        self.diff_page = body.height.saturating_sub(2) as usize;
        let flash = self.flash_line(footer.width as usize);
        let touched = self.diff.as_ref().and_then(|d| self.touched(&d.entry));
        let Some(d) = self.diff.as_mut() else { return };

        let tag = match d.entry.sec {
            Sec::Staged => fg("staged", GREEN),
            Sec::Changed => gray("unstaged"),
            Sec::Untracked => accent("new"),
        };
        let mut right = vec![];
        if d.parsed.adds > 0 {
            right.push(fg(format!("+{}", d.parsed.adds), GREEN));
        }
        if d.parsed.dels > 0 {
            right.push(fg(format!(" −{}", d.parsed.dels), RED));
        }
        let right_w: usize = right.iter().map(|s| s.width()).sum();
        let mark = match touched {
            Some(true) => fg(" ◆", AGENT),
            Some(false) => fg(" ◇", AGENT),
            None => "".into(),
        };
        let room = (header.width as usize).saturating_sub(right_w + tag.width() + mark.width() + 4);
        let mut line = vec![accent("← "), truncate_start(&d.entry.path, room).bold(), " ".into(), tag, mark];
        let used: usize = line.iter().map(|s| s.width()).sum();
        line.push(" ".repeat((header.width as usize).saturating_sub(used + right_w)).into());
        line.extend(right);
        f.render_widget(Line::from(line), header);

        if d.rows.as_ref().is_none_or(|(w, _)| *w != body.width) {
            d.rows = Some((body.width, render_diff(&d.parsed, body.width as usize)));
        }
        let rows = &d.rows.as_ref().unwrap().1;
        d.scroll = d.scroll.min(rows.len().saturating_sub(1));
        let visible: Vec<Line> = rows.iter().skip(d.scroll).take(body.height as usize).cloned().collect();
        f.render_widget(Paragraph::new(visible), body);

        match flash {
            Some(line) => f.render_widget(line, footer),
            None => {
                // The context hint carries its line count, so `e` needs no message.
                let context = format!("context {}", CONTEXTS[self.context]);
                let keys = [("esc", "back"), ("s", "stage"), ("n", "next file"), ("e", context.as_str())];
                self.put_hints(f, footer, vec![], &keys, None);
            }
        }
    }

    fn draw_graph(&mut self, f: &mut Frame, area: Rect) {
        let [header, _, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.diff_header_y = header.y;
        self.diff_page = body.height.saturating_sub(2) as usize;
        let flash = self.flash_line(footer.width as usize);
        let Some(g) = self.graph.as_mut() else { return };

        let scope = if g.all { "all branches".to_string() } else { self.repo.branch.clone() };
        let count = g.rows.iter().filter(|r| r.commit.is_some()).count();
        let count = gray(format!("{count} commits"));
        let room = (header.width as usize).saturating_sub(count.width() + 12);
        let mut line = vec![accent("← "), "graph".bold(), gray(" · "), truncate_end(&scope, room).into()];
        let used: usize = line.iter().map(|s| s.width()).sum();
        line.push(" ".repeat((header.width as usize).saturating_sub(used + count.width())).into());
        line.push(count);
        f.render_widget(Line::from(line), header);

        g.scroll = g.scroll.min(g.rows.len().saturating_sub(1));
        let width = body.width as usize;
        let visible: Vec<Line> = g
            .rows
            .iter()
            .skip(g.scroll)
            .take(body.height as usize)
            .map(|r| graph_line(r, width))
            .collect();
        f.render_widget(Paragraph::new(visible), body);

        let other = if g.all { "this branch" } else { "all branches" };
        match flash {
            Some(line) => f.render_widget(line, footer),
            None => self.put_hints(f, footer, vec![], &[("esc", "back"), ("a", other), ("r", "refresh")], None),
        }
    }
}

impl App {
    /// The repo's checkouts: folder, branch, and how many files each changed.
    fn draw_worktrees(&mut self, f: &mut Frame, area: Rect) {
        let [header, _, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.diff_header_y = header.y;
        self.click_rows.clear();
        let flash = self.flash_line(footer.width as usize);
        let Some(v) = self.worktrees.as_mut() else { return };

        let count = gray(format!("{} checkouts", v.items.len()));
        let mut line = vec![accent("← "), "worktrees".bold()];
        let used: usize = line.iter().map(|s| s.width()).sum();
        line.push(" ".repeat((header.width as usize).saturating_sub(used + count.width())).into());
        line.push(count);
        f.render_widget(Line::from(line), header);

        let name_w = v.items.iter().map(|(w, _)| w.name().chars().count()).max().unwrap_or(0);
        let branch_w = v.items.iter().map(|(w, _)| w.branch.chars().count()).max().unwrap_or(0);
        v.sel = v.sel.min(v.items.len().saturating_sub(1));
        let mut lines = Vec::new();
        for (i, (w, changed)) in v.items.iter().enumerate().take(body.height as usize) {
            let mut spans = vec![
                if i == v.sel { accent("› ") } else { "  ".into() },
                format!("{:<name_w$}  ", w.name()).bold(),
                accent(format!("{:<branch_w$}  ", w.branch)),
                match changed {
                    Some(0) => gray("✓ clean"),
                    Some(n) => fg(format!("{n} changed"), Color::Yellow),
                    None => fg("missing", RED),
                },
            ];
            if w.path == self.dir {
                spans.push(gray("  here"));
            }
            lines.push(Line::from(clip(spans, body.width as usize)));
            self.click_rows.push((body.y + i as u16, i));
        }
        f.render_widget(Paragraph::new(lines), body);

        match flash {
            Some(line) => f.render_widget(line, footer),
            None => self.put_hints(f, footer, vec![], &[("⏎", "switch"), ("esc", "back"), ("r", "refresh")], None),
        }
    }
}

/// One graph row: the lanes in their colors, then the commit's short hash,
/// branches and tags, subject, and age, with the subject giving way first.
fn graph_line(row: &git::GraphRow, width: usize) -> Line<'static> {
    let mut spans: Vec<Span> = row
        .lanes
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let glyph = match c {
                '*' => '●',
                '|' => '│',
                '/' => '╱',
                '\\' => '╲',
                c => c,
            };
            // Diagonals sit between two lanes; color them as the one they lead to.
            let lane = if matches!(c, '/' | '\\' | '_') { i.div_ceil(2) } else { i / 2 };
            fg(glyph.to_string(), LANES[lane % LANES.len()])
        })
        .collect();
    let Some(c) = &row.commit else { return Line::from(clip(spans, width)) };
    spans.push(" ".into());
    spans.push(gray(c.hash.clone()));
    for r in &c.refs {
        let span = if let Some(branch) = r.strip_prefix("HEAD -> ") {
            accent(branch.to_string()).bold()
        } else if let Some(tag) = r.strip_prefix("tag: ") {
            fg(tag.to_string(), Color::Yellow)
        } else if r == "HEAD" {
            accent("HEAD").bold()
        } else if r.ends_with("/HEAD") {
            continue;
        } else if r.contains('/') {
            gray(r.clone())
        } else {
            fg(r.clone(), Color::Blue)
        };
        spans.push(" ".into());
        spans.push(span);
    }
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let room = width.saturating_sub(used + c.when.chars().count() + 2);
    if room >= 4 {
        spans.push(" ".into());
        spans.push(truncate_end(&c.subject, room).into());
    }
    spans.push(" ".into());
    spans.push(gray(c.when.clone()));
    Line::from(clip(spans, width))
}

/// Lay a parsed diff out as rows `width` wide: a line-number gutter, then the
/// text on a green or red background, wrapped, with edited words brighter.
fn render_diff(p: &Parsed, width: usize) -> Vec<Line<'static>> {
    let gw = p.max_num().to_string().len().max(2);
    let cw = width.saturating_sub(gw + 2).max(1);
    let mut rows = Vec::new();
    for item in &p.items {
        match item {
            Item::Note(text) => rows.push(Line::from(gray(format!("{:gw$}  {text}", "")))),
            Item::Gap { hidden, context } => {
                let lines = if *hidden == 1 { "1 unchanged line".to_string() } else { format!("{hidden} unchanged lines") };
                let mut text = format!("{:>gw$}  ⋯ {lines}", "");
                if !context.is_empty() {
                    text.push_str(" · ");
                    text.push_str(context.trim());
                }
                rows.push(Line::from(gray(truncate_end(&text, width))));
            }
            Item::Line(l) => {
                let (bg, text_fg, emph_bg, num_fg) = match l.kind {
                    Kind::Add => (Some(ADD_BG), Some(ADD_FG), Some(ADD_EMPH), GREEN),
                    Kind::Del => (Some(DEL_BG), Some(DEL_FG), Some(DEL_EMPH), RED),
                    Kind::Ctx => (None, None, None, GRAY),
                };
                let base = {
                    let mut s = Style::new();
                    if let Some(c) = bg {
                        s = s.bg(c);
                    }
                    if let Some(c) = text_fg {
                        s = s.fg(c);
                    }
                    s
                };
                let emph = emph_bg.map_or(base, |c| base.bg(c));
                let chars: Vec<char> = l.text.chars().collect();
                let chunks = chars.len().div_ceil(cw).max(1);
                for c in 0..chunks {
                    let (from, to) = (c * cw, ((c + 1) * cw).min(chars.len()));
                    let num = if c == 0 { format!(" {:>gw$} ", l.num) } else { " ".repeat(gw + 2) };
                    let mut spans = vec![Span::styled(num, base.fg(num_fg))];
                    // Split the chunk where the highlighted part starts and ends.
                    let (es, ee) = l.emph.unwrap_or((0, 0));
                    let mut cuts = vec![from, to];
                    for x in [es, ee] {
                        if x > from && x < to {
                            cuts.push(x);
                        }
                    }
                    cuts.sort_unstable();
                    cuts.dedup();
                    for w in cuts.windows(2) {
                        let s: String = chars[w[0]..w[1]].iter().collect();
                        let style = if l.emph.is_some() && w[0] >= es && w[1] <= ee { emph } else { base };
                        spans.push(Span::styled(s, style));
                    }
                    if bg.is_some() {
                        spans.push(Span::styled(" ".repeat(cw - (to - from)), base));
                    }
                    rows.push(Line::from(spans));
                }
            }
        }
    }
    rows
}

/// Word-wrap a commit message for the box, leaving a column for the cursor.
/// Each row says whether it's in the subject.
fn wrap_message(msg: &str, width: usize) -> Vec<(bool, String)> {
    let w = width.saturating_sub(1).max(8);
    let mut rows = Vec::new();
    for (i, line) in msg.split('\n').enumerate() {
        let mut row = String::new();
        for word in line.split(' ') {
            let len = row.chars().count();
            if len > 0 && len + 1 + word.chars().count() > w {
                rows.push((i == 0, std::mem::take(&mut row)));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
            // A word longer than the row breaks wherever it has to.
            while row.chars().count() > w {
                let head: String = row.chars().take(w).collect();
                row = row.chars().skip(w).collect();
                rows.push((i == 0, head));
            }
        }
        rows.push((i == 0, row));
    }
    rows
}

/// How wide hints are when laid out two spaces apart.
fn hints_width(keys: &[(&str, &str)]) -> usize {
    let w: usize = keys.iter().map(|(k, v)| k.chars().count() + 1 + v.chars().count()).sum();
    w + 2 * keys.len().saturating_sub(1)
}

/// The key a hint's label stands for.
fn hint_key(label: &str) -> Option<KeyEvent> {
    let none = KeyModifiers::NONE;
    let (code, mods) = match label {
        "spc" => (KeyCode::Char(' '), none),
        "⏎" => (KeyCode::Enter, none),
        "⇧⏎" => (KeyCode::Enter, KeyModifiers::SHIFT),
        "Tab" => (KeyCode::Tab, none),
        "esc" => (KeyCode::Esc, none),
        "^E" => (KeyCode::Char('e'), KeyModifiers::CONTROL),
        k if k.chars().count() == 1 => (KeyCode::Char(k.chars().next()?), none),
        _ => return None,
    };
    Some(KeyEvent::new(code, mods))
}

fn clip(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let w = s.width();
        if used + w <= width {
            used += w;
            out.push(s);
        } else {
            let room = width.saturating_sub(used);
            if room > 0 {
                out.push(Span::styled(truncate_end(&s.content, room), s.style));
            }
            break;
        }
    }
    out
}

fn code_color(c: char) -> Color {
    match c {
        'M' => Color::Yellow,
        'A' => GREEN,
        'D' => RED,
        'R' | 'C' => Color::Blue,
        '?' => ACCENT,
        'U' => Color::LightRed,
        _ => GRAY,
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn ago(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
    }
}

fn truncate_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

fn truncate_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{keep}")
}

// ---------------------------------------------------------------- main

fn run(term: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    while !app.quit {
        app.tick();
        term.draw(|f| app.draw(f))?;
        let wait = if app.drafting.is_some() || app.syncing.is_some() { 80 } else { 250 };
        if event::poll(Duration::from_millis(wait))? {
            let ev = event::read()?;
            if let Some(path) = env::var_os("REX_GIT_DEBUG")
                && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
            {
                let _ = writeln!(f, "{ev:?}");
            }
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
        }
        if app.edit {
            app.edit = false;
            edit_message(term, app)?;
        }
    }
    Ok(())
}

/// Hand the terminal to $VISUAL or $EDITOR to edit the message, then take it back.
fn edit_message(term: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let path = env::temp_dir().join(format!("rex-git-{}.txt", std::process::id()));
    std::fs::write(&path, &app.msg)?;
    let editor = env::var("VISUAL").or_else(|_| env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    // Through sh, so an editor with arguments like `code -w` works.
    let status = Command::new("sh").args(["-c", &format!("{editor} \"$1\""), "sh"]).arg(&path).status();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    term.clear()?;
    match status {
        Ok(s) if s.success() => {
            app.msg = std::fs::read_to_string(&path)?.trim_end().to_string();
            (app.msg_scroll, app.msg_follow) = (0, false);
        }
        Ok(s) => app.flash(format!("{editor} exited with {s}"), true),
        Err(err) => app.flash(format!("{editor}: {err}"), true),
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn main() -> io::Result<()> {
    let mut dir = None;
    let mut thread = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: rex-git [DIR] [--thread ID]\n\n\
                     A git side pane: status, diffs, staging, commits drafted by Claude, and sync.\n\
                     --thread names the Axel thread whose agents' changes to mark; without it,\n\
                     rex-git looks for a live thread working in DIR. Press ? inside for keys."
                );
                return Ok(());
            }
            "--thread" => thread = args.next().filter(|t| !t.is_empty()),
            _ => dir = Some(PathBuf::from(arg)),
        }
    }
    let dir = dir.unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let dir = dir.canonicalize().unwrap_or(dir);
    let root = git(&dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim())).unwrap_or(dir);

    let mut app = App::new(root, thread);
    let mut term = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = run(&mut term, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
