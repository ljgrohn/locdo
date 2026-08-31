use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{Local, NaiveDateTime};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap};
use ratatui_textarea::TextArea;

const STARTER: &str = "# Todo\n\n- [ ] Add your first item\n\n## Later\n\n## Done\n";

/// Runs git in `dir`; Ok(stdout) on success, Err(last stderr line) on
/// failure. Never prompts for credentials (fails instead).
fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        // the diagnostic line, not git's trailing "hint:" advice
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("fatal:") || l.starts_with("error:") || l.starts_with('!'))
            .or_else(|| {
                err.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with("hint:"))
            })
            .unwrap_or("git failed");
        Err(line.to_string())
    }
}

/// The directory to sync, if the todo file lives in a git repo that has a
/// remote. Sync is entirely opt-in by construction: no repo, no sync.
fn sync_repo(path: &Path) -> Option<PathBuf> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if run_git(dir, &["rev-parse", "--is-inside-work-tree"]).ok()? != "true" {
        return None;
    }
    if run_git(dir, &["remote"]).ok()?.is_empty() {
        return None;
    }
    Some(dir.to_path_buf())
}

/// Stages the todo file and its sidecars, commits them (and only them) if
/// changed, pulls --rebase so a remote another machine moved ahead doesn't
/// leave the push permanently rejected, then pushes. Best-effort: callers
/// surface the Err as a status line.
fn sync_commit_push(path: &Path) -> Result<String, String> {
    let dir = sync_repo(path).ok_or("todo file is not in a git repo with a remote")?;
    // pathspecs must be relative to `git -C <dir>`, not to our cwd
    let mut specs: Vec<String> = Vec::new();
    for f in [
        path.to_path_buf(),
        path.with_extension("archive.md"),
        path.with_extension("done.md"),
    ] {
        if f.exists() {
            let spec = f.strip_prefix(&dir).unwrap_or(&f).to_string_lossy().into_owned();
            run_git(&dir, &["add", &spec])?;
            specs.push(spec);
        }
    }
    // diff --cached --quiet exits 1 when the staged todo files changed
    let mut args = vec!["diff", "--cached", "--quiet", "--"];
    args.extend(specs.iter().map(String::as_str));
    let dirty = run_git(&dir, &args).is_err();
    if dirty {
        let msg = format!("locdo sync {}", Local::now().format("%Y-%m-%d %H:%M"));
        let mut args = vec!["commit", "--quiet", "-m", &msg, "--"];
        args.extend(specs.iter().map(String::as_str));
        run_git(&dir, &args)?;
    }
    git_pull_rebase(&dir)?;
    run_git(&dir, &["push", "--quiet", "-u", "origin", "HEAD"])?;
    Ok(if dirty {
        "pushed".to_string()
    } else {
        "nothing to push".to_string()
    })
}

fn sync_pull(path: &Path) -> Result<(), String> {
    let dir = sync_repo(path).ok_or("no repo")?;
    git_pull_rebase(&dir)
}

/// Fetch + rebase onto upstream. A failed rebase (conflict) is aborted so
/// the repo isn't left mid-rebase, which would make every later sync fail.
fn git_pull_rebase(dir: &Path) -> Result<(), String> {
    run_git(dir, &["fetch", "--quiet", "origin"])?;
    // no upstream ref yet (brand-new or empty remote): nothing to rebase
    // onto, and the later push -u will create it
    if run_git(dir, &["rev-parse", "--verify", "--quiet", "@{u}"]).is_err() {
        return Ok(());
    }
    if let Err(e) = run_git(dir, &["rebase", "--autostash", "--quiet", "@{u}"]) {
        let _ = run_git(dir, &["rebase", "--abort"]);
        return Err(e);
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| env::var_os("LOCDO_FILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("todo.md"));

    let synced = sync_repo(&path).is_some();
    let mut startup_status = None;
    if synced {
        if let Err(e) = sync_pull(&path) {
            startup_status = Some(format!("sync pull failed: {e}"));
        }
    }

    if !path.exists() {
        fs::write(&path, STARTER)?;
    }

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let result = run(&path, startup_status);
    io::stdout().execute(LeaveAlternateScreen)?;
    disable_raw_mode()?;

    if synced {
        match sync_commit_push(&path) {
            Ok(msg) => println!("locdo: {msg}"),
            Err(e) => eprintln!("locdo: sync push failed: {e}"),
        }
    }
    result
}

/// A top-level task and the indented block (notes, sub-todos) beneath it.
#[derive(Clone, Copy)]
struct Item {
    start: usize,
    len: usize,
    done: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SectionKind {
    Todo,
    Later,
    Done,
}

struct NotesState {
    start: usize,
    child_len: usize,
    indent: usize,
    title: String,
    textarea: TextArea<'static>,
}

#[derive(Clone, Copy, PartialEq)]
enum InputKind {
    NewTask,
    NewSub,
    NewSection,
    Edit,
}

/// A selectable position in the main list: a task, or a collapsed
/// section's heading line.
#[derive(Clone, Copy)]
enum Entry {
    Task(Item),
    Section(usize),
}

#[derive(Clone, PartialEq)]
enum MoveDest {
    Todo,
    Section(String),
    Later,
    NewSection,
}

impl MoveDest {
    fn label(&self) -> &str {
        match self {
            MoveDest::Todo => "todo",
            MoveDest::Section(name) => name,
            MoveDest::Later => "later",
            MoveDest::NewSection => "new section…",
        }
    }
}

struct MoveMenu {
    options: Vec<MoveDest>,
    sel: usize,
}

/// Read-only view of the todo.done.md sidecar.
struct HistoryView {
    lines: Vec<String>,
    scroll: u16,
}

/// Single-line prompt for adding a new todo/subtask or editing a title.
struct InputState {
    kind: InputKind,
    textarea: TextArea<'static>,
}

struct App {
    path: PathBuf,
    lines: Vec<String>,
    crlf: bool,
    mtime: Option<SystemTime>,
    cursor: usize,
    /// Selected subtask index within the cursor item's sub lines, if any.
    sub: Option<usize>,
    expand_all: bool,
    show_help: bool,
    hide_done: bool,
    status: String,
    notes: Option<NotesState>,
    input: Option<InputState>,
    move_menu: Option<MoveMenu>,
    history: Option<HistoryView>,
    /// Quit-confirmation popup; the bool is whether "yes" is highlighted.
    quit_confirm: Option<bool>,
    /// Collapsed section names, in-memory only.
    collapsed: HashSet<String>,
    undo: Vec<Vec<String>>,
}

/// A renderable row in the main list.
enum Row {
    Header(String),
    Task {
        text: String,
        done: bool,
        stamp: Option<String>,
        subs: Option<(usize, usize)>,
        has_notes: bool,
    },
    Sub {
        text: String,
        done: bool,
    },
    CollapsedSection {
        name: String,
        done: usize,
        total: usize,
    },
}

fn leading_ws(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_heading(line: &str) -> bool {
    line.starts_with('#')
}

fn section_kind(heading_text: &str) -> SectionKind {
    match heading_text.to_lowercase().as_str() {
        "later" => SectionKind::Later,
        "done" => SectionKind::Done,
        _ => SectionKind::Todo,
    }
}

/// If `line` is a task item, returns (done, byte index of the status char inside `[ ]`).
fn task_info(line: &str) -> Option<(bool, usize)> {
    let indent = leading_ws(line);
    let rest = &line[indent..];
    let rest = rest
        .strip_prefix("- [")
        .or_else(|| rest.strip_prefix("* ["))?;
    let mut chars = rest.chars();
    let status = chars.next()?;
    if chars.next() != Some(']') {
        return None;
    }
    let done = matches!(status, 'x' | 'X');
    if !done && status != ' ' {
        return None;
    }
    Some((done, indent + 3))
}

/// Splits a task's display text from its `@done(...)` stamp, if any.
fn split_stamp(text: &str) -> (String, Option<String>) {
    if let Some(pos) = text.find("@done(") {
        if let Some(end) = text[pos..].find(')') {
            let stamp = text[pos + 6..pos + end].to_string();
            let mut rest = text[..pos].to_string();
            rest.push_str(&text[pos + end + 1..]);
            return (rest.trim().to_string(), Some(stamp));
        }
    }
    (text.trim().to_string(), None)
}

fn fmt_stamp(stamp: &str) -> String {
    NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M")
        .map(|dt| dt.format("%b %-d, %H:%M").to_string())
        .unwrap_or_else(|_| stamp.to_string())
}

/// Normalizes blank lines: no runs of blanks, one blank around headings,
/// no leading/trailing blanks.
fn tidy_lines(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let blank = line.trim().is_empty();
        let prev_blank = out.last().is_none_or(|p: &String| p.trim().is_empty());
        if blank && prev_blank {
            continue;
        }
        if is_heading(line) && !prev_blank {
            out.push(String::new());
        }
        out.push(line.clone());
    }
    let mut i = 0;
    while i < out.len() {
        if is_heading(&out[i]) && i + 1 < out.len() && !out[i + 1].trim().is_empty() {
            out.insert(i + 1, String::new());
        }
        i += 1;
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

/// Top-level tasks with their child blocks. Interior blank lines belong to a
/// block only when more indented content follows; trailing blanks do not.
fn items(lines: &[String]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((done, _)) = task_info(&lines[i]) {
            if leading_ws(&lines[i]) == 0 {
                let mut j = i + 1;
                loop {
                    let mut k = j;
                    while k < lines.len() && lines[k].trim().is_empty() {
                        k += 1;
                    }
                    if k < lines.len() && !is_heading(&lines[k]) && leading_ws(&lines[k]) > 0 {
                        j = k + 1;
                    } else {
                        break;
                    }
                }
                out.push(Item {
                    start: i,
                    len: j - i,
                    done,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

impl App {
    fn load(path: &Path) -> io::Result<Self> {
        let mut app = App {
            path: path.to_path_buf(),
            lines: Vec::new(),
            crlf: false,
            mtime: None,
            cursor: 0,
            hide_done: false,
            sub: None,
            expand_all: false,
            show_help: false,
            status: String::new(),
            notes: None,
            input: None,
            move_menu: None,
            history: None,
            quit_confirm: None,
            collapsed: HashSet::new(),
            undo: Vec::new(),
        };
        app.reload()?;
        app.rotate_done()?;
        Ok(app)
    }

    fn reload(&mut self) -> io::Result<()> {
        let raw = fs::read_to_string(&self.path)?;
        self.crlf = raw.contains("\r\n");
        self.lines = raw
            .replace("\r\n", "\n")
            .lines()
            .map(String::from)
            .collect();
        self.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        // Undo snapshots predate what's now on disk; undoing across a reload
        // would silently clobber external edits.
        self.undo.clear();
        self.clamp_cursor();
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(eol);
        out.push_str(eol);
        fs::write(&self.path, out)?;
        self.mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    fn externally_modified(&self) -> bool {
        let current = fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        current.is_some() && current != self.mtime
    }

    /// Cursor-navigable entries: visible tasks, plus one stop per collapsed
    /// section (whose tasks are skipped).
    fn entries(&self) -> Vec<Entry> {
        let its = items(&self.lines);
        let mut out = Vec::new();
        let mut it_idx = 0;
        let mut in_collapsed = false;
        let mut i = 0;
        while i < self.lines.len() {
            let line = &self.lines[i];
            if is_heading(line) {
                let name = line.trim_start_matches('#').trim().to_string();
                in_collapsed = line.starts_with("##") && self.collapsed.contains(&name);
                if in_collapsed && !(self.hide_done && section_kind(&name) == SectionKind::Done) {
                    out.push(Entry::Section(i));
                }
                i += 1;
                continue;
            }
            if it_idx < its.len() && its[it_idx].start == i {
                let it = its[it_idx];
                it_idx += 1;
                i = it.start + it.len;
                if !in_collapsed && !(self.hide_done && it.done) {
                    out.push(Entry::Task(it));
                }
                continue;
            }
            i += 1;
        }
        out
    }

    /// Test helper: visible tasks ignoring collapse state.
    #[cfg(test)]
    fn visible_items(&self) -> Vec<Item> {
        items(&self.lines)
            .into_iter()
            .filter(|it| !(self.hide_done && it.done))
            .collect()
    }

    /// The item under the cursor, unless a collapsed section is selected.
    fn current_item(&self) -> Option<Item> {
        match self.entries().get(self.cursor)? {
            Entry::Task(it) => Some(*it),
            Entry::Section(_) => None,
        }
    }

    fn clamp_cursor(&mut self) {
        let n = self.entries().len();
        if n == 0 {
            self.cursor = 0;
        } else if self.cursor >= n {
            self.cursor = n - 1;
        }
        if let Some(k) = self.sub {
            let nsubs = self
                .current_item()
                .map(|it| self.sub_line_indices(it).len())
                .unwrap_or(0);
            self.sub = if nsubs == 0 { None } else { Some(k.min(nsubs - 1)) };
        }
    }

    /// Line indices of the subtask lines inside an item's child block.
    fn sub_line_indices(&self, it: Item) -> Vec<usize> {
        (it.start + 1..it.start + it.len)
            .filter(|&i| task_info(&self.lines[i]).is_some())
            .collect()
    }

    /// Line index of the selected task or subtask, if any.
    fn current_task_line(&self) -> Option<usize> {
        let it = self.current_item()?;
        match self.sub {
            Some(k) => self.sub_line_indices(it).get(k).copied(),
            None => Some(it.start),
        }
    }

    fn nav_down(&mut self) {
        let entries = self.entries();
        let nsubs = self
            .current_item()
            .map(|it| self.sub_line_indices(it).len())
            .unwrap_or(0);
        match self.sub {
            None if nsubs > 0 => self.sub = Some(0),
            Some(k) if k + 1 < nsubs => self.sub = Some(k + 1),
            _ => {
                if self.cursor + 1 < entries.len() {
                    self.cursor += 1;
                    self.sub = None;
                }
            }
        }
    }

    fn nav_up(&mut self) {
        match self.sub {
            Some(0) => self.sub = None,
            Some(k) => self.sub = Some(k - 1),
            None => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.sub = None;
                    if self.expand_all {
                        if let Some(it) = self.current_item() {
                            let n = self.sub_line_indices(it).len();
                            if n > 0 {
                                self.sub = Some(n - 1);
                            }
                        }
                    }
                }
            }
        }
    }

    fn checkpoint(&mut self) {
        if self.undo.last() == Some(&self.lines) {
            return;
        }
        self.undo.push(self.lines.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    fn undo_last(&mut self) -> io::Result<()> {
        let Some(prev) = self.undo.pop() else {
            self.status = "nothing to undo".to_string();
            return Ok(());
        };
        self.lines = prev;
        self.sub = None;
        self.clamp_cursor();
        self.status = "undo".to_string();
        self.save()
    }

    /// Rolls Done tasks stamped more than 7 days ago into the todo.done.md
    /// sidecar, grouped under `## YYYY-MM` headings. Runs at startup; not
    /// undoable (the sidecar would keep its copy anyway).
    fn rotate_done(&mut self) -> io::Result<()> {
        let Some(h) = self.heading_line("done") else {
            return Ok(());
        };
        let end = self.lines[h + 1..]
            .iter()
            .position(|l| l.starts_with("##"))
            .map(|p| h + 1 + p)
            .unwrap_or(self.lines.len());
        let cutoff = Local::now().naive_local() - chrono::Duration::days(7);
        // read the sidecar before touching self.lines: a read failure here
        // must not lose the drained blocks or clobber existing history
        let sidecar = self.path.with_extension("done.md");
        let mut hist: Vec<String> = match fs::read_to_string(&sidecar) {
            Ok(raw) => raw.lines().map(String::from).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut rolled: Vec<(NaiveDateTime, Vec<String>)> = Vec::new();
        let old: Vec<Item> = items(&self.lines)
            .into_iter()
            .filter(|it| it.start > h && it.start < end)
            .collect();
        for it in old.iter().rev() {
            let (_, idx) = task_info(&self.lines[it.start]).unwrap();
            let (_, stamp) = split_stamp(&self.lines[it.start][idx + 2..]);
            let Some(dt) = stamp
                .and_then(|s| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M").ok())
            else {
                continue;
            };
            if dt < cutoff {
                let block = self.lines.drain(it.start..it.start + it.len).collect();
                rolled.push((dt, block));
            }
        }
        if rolled.is_empty() {
            return Ok(());
        }
        rolled.reverse();
        for (dt, block) in rolled {
            let month = dt.format("## %Y-%m").to_string();
            let insert_at = match hist.iter().position(|l| *l == month) {
                Some(p) => hist[p + 1..]
                    .iter()
                    .position(|l| l.starts_with("##"))
                    .map(|q| p + 1 + q)
                    .unwrap_or(hist.len()),
                None => {
                    // keep month headings in ascending (chronological) order
                    let pos = hist
                        .iter()
                        .position(|l| l.starts_with("## ") && l.as_str() > month.as_str())
                        .unwrap_or(hist.len());
                    hist.insert(pos, month);
                    hist.insert(pos + 1, String::new());
                    pos + 2
                }
            };
            for (k, line) in block.into_iter().enumerate() {
                hist.insert(insert_at + k, line);
            }
        }
        let hist = tidy_lines(&hist);
        fs::write(&sidecar, hist.join("\n") + "\n")?;
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        Ok(())
    }

    /// On a task: collapses its containing `##` section (cursor moves to the
    /// header stop). On a collapsed header: expands it.
    fn toggle_collapse(&mut self) {
        match self.entries().get(self.cursor).copied() {
            Some(Entry::Section(h)) => {
                let name = self.lines[h].trim_start_matches('#').trim().to_string();
                self.collapsed.remove(&name);
                self.clamp_cursor();
            }
            Some(Entry::Task(it)) => {
                let Some(h) = self.lines[..it.start]
                    .iter()
                    .rposition(|l| l.starts_with("##"))
                else {
                    self.status = "top area has no section to collapse".to_string();
                    return;
                };
                let name = self.lines[h].trim_start_matches('#').trim().to_string();
                self.collapsed.insert(name);
                self.sub = None;
                self.cursor = self
                    .entries()
                    .iter()
                    .position(|e| matches!(e, Entry::Section(x) if *x == h))
                    .unwrap_or(0);
            }
            None => {}
        }
    }

    /// Appends the selected collapsed section to the sidecar archive file
    /// and removes it from the todo file.
    fn archive_section(&mut self) -> io::Result<()> {
        let Some(Entry::Section(h)) = self.entries().get(self.cursor).copied() else {
            self.status = "collapse a section first (c), then A archives it".to_string();
            return Ok(());
        };
        self.checkpoint();
        let name = self.lines[h].trim_start_matches('#').trim().to_string();
        let end = self.lines[h + 1..]
            .iter()
            .position(|l| l.starts_with("##"))
            .map(|p| h + 1 + p)
            .unwrap_or(self.lines.len());
        let mut block: Vec<String> = self.lines.drain(h..end).collect();
        while block.last().is_some_and(|l| l.trim().is_empty()) {
            block.pop();
        }
        let sidecar = self.path.with_extension("archive.md");
        let mut out = fs::read_to_string(&sidecar).unwrap_or_default();
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block.join("\n"));
        out.push('\n');
        fs::write(&sidecar, out)?;
        self.collapsed.remove(&name);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        self.status = format!("archived: {name}");
        Ok(())
    }

    /// Returns the heading line of a custom section, creating it before the
    /// Later/Done headings if missing.
    fn create_section(&mut self, name: &str) -> usize {
        if let Some(h) = self.heading_line(name) {
            return h;
        }
        let pos = self
            .heading_line("later")
            .or_else(|| self.heading_line("done"))
            .unwrap_or(self.lines.len());
        self.lines.insert(pos, format!("## {name}"));
        self.lines.insert(pos + 1, String::new());
        pos
    }

    /// Moves the current main task's block to the end of a destination
    /// section (creating a custom section if needed).
    fn move_current_to(&mut self, dest: &MoveDest) -> io::Result<()> {
        if self.sub.is_some() {
            return Ok(());
        }
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        if it.done {
            self.status = "item is done — untick it first".to_string();
            return Ok(());
        }
        self.checkpoint();
        let (dest_idx, label) = match dest {
            MoveDest::Todo => (self.todo_end(), "todo".to_string()),
            MoveDest::Later => {
                self.ensure_section("Later");
                (self.later_end(), "later".to_string())
            }
            MoveDest::Section(name) => {
                let h = self.create_section(name);
                (self.section_end(h), name.clone())
            }
            MoveDest::NewSection => return Ok(()),
        };
        // creating a section may have shifted lines; re-locate the task
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        self.move_block(it.start, it.len, dest_idx);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        self.status = format!("moved to {label}");
        Ok(())
    }

    fn open_move_menu(&mut self) {
        if self.sub.is_some() {
            return;
        }
        let Some(it) = self.current_item() else {
            return;
        };
        if it.done {
            self.status = "item is done — untick it first".to_string();
            return;
        }
        let mut options = vec![MoveDest::Todo];
        for (_, name) in self.sections() {
            if section_kind(&name) == SectionKind::Todo {
                options.push(MoveDest::Section(name));
            }
        }
        options.push(MoveDest::Later);
        options.push(MoveDest::NewSection);
        self.move_menu = Some(MoveMenu { options, sel: 0 });
    }

    fn commit_move(&mut self) -> io::Result<()> {
        let Some(menu) = self.move_menu.take() else {
            return Ok(());
        };
        let dest = menu.options[menu.sel].clone();
        if dest == MoveDest::NewSection {
            self.open_input(InputKind::NewSection);
            return Ok(());
        }
        self.move_current_to(&dest)
    }

    /// Toggles the selected subtask; done subs sink below the parent's open
    /// subs but never leave the parent's block.
    fn toggle_sub(&mut self) -> io::Result<()> {
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        let subs = self.sub_line_indices(it);
        let Some(&line_idx) = self.sub.and_then(|k| subs.get(k)) else {
            return Ok(());
        };
        self.checkpoint();
        let (done, sidx) = task_info(&self.lines[line_idx]).unwrap();
        let mark = if done { " " } else { "x" };
        self.lines[line_idx].replace_range(sidx..sidx + 1, mark);
        // Stable-partition sub contents across the same line slots: open
        // first, done last. Note lines between subs keep their positions.
        let mut entries: Vec<(String, bool)> = subs
            .iter()
            .map(|&i| (self.lines[i].clone(), i == line_idx))
            .collect();
        entries.sort_by_key(|(l, _)| task_info(l).unwrap().0);
        for (pos, &i) in subs.iter().enumerate() {
            self.lines[i] = entries[pos].0.clone();
        }
        self.sub = entries.iter().position(|(_, toggled)| *toggled);
        self.status = if done { "sub back open" } else { "sub done" }.to_string();
        self.save()
    }

    /// Inserts a one-level subtask under the cursor's item, after its last
    /// open sub (above done ones), or right beneath the parent line.
    fn add_subtask(&mut self, text: &str) -> io::Result<()> {
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        self.checkpoint();
        let subs = self.sub_line_indices(it);
        let insert_at = subs
            .iter()
            .rev()
            .find(|&&i| !task_info(&self.lines[i]).unwrap().0)
            .map(|&i| i + 1)
            .or(subs.first().copied())
            .unwrap_or(it.start + 1);
        self.lines.insert(insert_at, format!("  - [ ] {text}"));
        self.save()?;
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        self.sub = self
            .sub_line_indices(it)
            .iter()
            .position(|&i| i == insert_at);
        self.status = "sub added".to_string();
        Ok(())
    }

    fn section_at(&self, line_idx: usize) -> SectionKind {
        let mut kind = SectionKind::Todo;
        for line in self.lines.iter().take(line_idx + 1) {
            if is_heading(line) {
                kind = section_kind(line.trim_start_matches('#').trim());
            }
        }
        kind
    }

    fn heading_line(&self, name: &str) -> Option<usize> {
        self.lines.iter().position(|l| {
            is_heading(l) && l.trim_start_matches('#').trim().eq_ignore_ascii_case(name)
        })
    }

    /// First line index past a heading's blank-line padding.
    fn after_heading(&self, h: usize) -> usize {
        let mut i = h + 1;
        while i < self.lines.len() && self.lines[i].trim().is_empty() {
            i += 1;
        }
        i
    }

    /// End of the top "inbox" area: just before the first `##` section
    /// heading of any kind, backed up over blank separator lines.
    fn todo_end(&self) -> usize {
        let end = self
            .lines
            .iter()
            .position(|l| l.starts_with("##"))
            .unwrap_or(self.lines.len());
        self.back_over_blanks(end)
    }

    /// Level-2 section headings as (line index, name), in file order.
    fn sections(&self) -> Vec<(usize, String)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("##"))
            .map(|(i, l)| (i, l.trim_start_matches('#').trim().to_string()))
            .collect()
    }

    /// End of the section headed at `h`: just before the next `##` heading
    /// or EOF, backed up over blank lines.
    fn section_end(&self, h: usize) -> usize {
        let end = self.lines[h + 1..]
            .iter()
            .position(|l| l.starts_with("##"))
            .map(|p| h + 1 + p)
            .unwrap_or(self.lines.len());
        self.back_over_blanks(end)
    }

    fn later_end(&self) -> usize {
        let h = self.heading_line("later").expect("later section exists");
        self.section_end(h)
    }

    fn back_over_blanks(&self, mut i: usize) -> usize {
        while i > 0 && self.lines[i - 1].trim().is_empty() {
            i -= 1;
        }
        i
    }

    /// Returns the heading line index, creating the section if missing.
    /// Later is created before Done; Done goes at the end of the file.
    fn ensure_section(&mut self, name: &str) -> usize {
        if let Some(h) = self.heading_line(name) {
            return h;
        }
        let heading = format!("## {name}");
        if name.eq_ignore_ascii_case("later") {
            if let Some(d) = self.heading_line("done") {
                self.lines.insert(d, heading);
                self.lines.insert(d + 1, String::new());
                return d;
            }
        }
        self.lines.push(String::new());
        self.lines.push(heading);
        self.lines.push(String::new());
        self.lines.len() - 2
    }

    fn move_block(&mut self, start: usize, len: usize, mut dest: usize) {
        let block: Vec<String> = self.lines.drain(start..start + len).collect();
        if dest > start {
            dest -= len;
        }
        for (k, line) in block.into_iter().enumerate() {
            self.lines.insert(dest + k, line);
        }
    }

    /// Normalizes blank lines: no runs of blanks, one blank around headings,
    /// no leading/trailing blanks.
    fn tidy(&mut self) {
        self.lines = tidy_lines(&self.lines);
    }

    fn toggle(&mut self) -> io::Result<()> {
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        self.checkpoint();
        let (done, idx) = task_info(&self.lines[it.start]).unwrap();
        if !done {
            let stamp = Local::now().format("%Y-%m-%d %H:%M").to_string();
            {
                let line = &mut self.lines[it.start];
                line.replace_range(idx..idx + 1, "x");
                line.push_str(&format!(" @done({stamp})"));
            }
            let h = self.ensure_section("Done");
            let dest = self.after_heading(h);
            self.move_block(it.start, it.len, dest);
            self.status = "done".to_string();
        } else {
            {
                let line = &mut self.lines[it.start];
                line.replace_range(idx..idx + 1, " ");
                if let Some(pos) = line.find("@done(") {
                    if let Some(end) = line[pos..].find(')') {
                        line.replace_range(pos..pos + end + 1, "");
                    }
                }
                *line = line.trim_end().to_string();
            }
            let dest = self.todo_end();
            self.move_block(it.start, it.len, dest);
            self.status = "back to todo".to_string();
        }
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        Ok(())
    }

    /// Moves the current item Todo -> Later, or Later -> Todo.
    /// Sections are for main tasks only; does nothing on a subtask.
    fn cycle_later(&mut self) -> io::Result<()> {
        if self.sub.is_some() {
            return Ok(());
        }
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        if it.done {
            self.status = "item is done — untick it first".to_string();
            return Ok(());
        }
        self.checkpoint();
        let dest = if self.section_at(it.start) == SectionKind::Later {
            self.status = "moved to todo".to_string();
            self.todo_end()
        } else {
            self.ensure_section("Later");
            self.status = "moved to later".to_string();
            self.later_end()
        };
        self.move_block(it.start, it.len, dest);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        Ok(())
    }

    fn move_item(&mut self, delta: isize) -> io::Result<()> {
        if self.sub.is_some() {
            return Ok(());
        }
        let entries = self.entries();
        let target = self.cursor as isize + delta;
        if target < 0 || target as usize >= entries.len() {
            return Ok(());
        }
        let (Some(Entry::Task(cur)), Some(Entry::Task(tgt))) =
            (entries.get(self.cursor).copied(), entries.get(target as usize).copied())
        else {
            return Ok(());
        };
        self.checkpoint();
        let dest = if delta < 0 {
            tgt.start
        } else {
            tgt.start + tgt.len
        };
        self.move_block(cur.start, cur.len, dest);
        self.cursor = target as usize;
        self.tidy();
        self.save()
    }

    fn add_todo(&mut self, text: &str) -> io::Result<()> {
        self.checkpoint();
        let dest = self.todo_end();
        self.lines.insert(dest, format!("- [ ] {text}"));
        self.tidy();
        self.save()?;
        self.sub = None;
        let top_end = self.todo_end();
        self.cursor = self
            .entries()
            .iter()
            .rposition(|e| matches!(e, Entry::Task(it) if it.start < top_end))
            .unwrap_or(0);
        self.status = "added".to_string();
        Ok(())
    }

    fn delete_current(&mut self) -> io::Result<()> {
        let Some(it) = self.current_item() else {
            return Ok(());
        };
        self.checkpoint();
        if self.sub.is_some() {
            let Some(line_idx) = self.current_task_line() else {
                return Ok(());
            };
            let (_, idx) = task_info(&self.lines[line_idx]).unwrap();
            let title = self.lines[line_idx][idx + 2..].trim().to_string();
            self.lines.remove(line_idx);
            self.save()?;
            self.clamp_cursor();
            self.status = format!("deleted sub: {title}");
            return Ok(());
        }
        let (_, idx) = task_info(&self.lines[it.start]).unwrap();
        let (title, _) = split_stamp(&self.lines[it.start][idx + 2..]);
        self.lines.drain(it.start..it.start + it.len);
        self.tidy();
        self.save()?;
        self.clamp_cursor();
        self.status = format!("deleted: {title}");
        Ok(())
    }

    /// Rewrites the selected task or subtask title, preserving the checkbox
    /// prefix and any @done stamp.
    fn edit_title(&mut self, text: &str) -> io::Result<()> {
        let Some(li) = self.current_task_line() else {
            return Ok(());
        };
        self.checkpoint();
        let (_, idx) = task_info(&self.lines[li]).unwrap();
        let (_, stamp) = split_stamp(&self.lines[li][idx + 2..]);
        let mut line = format!("{} {}", &self.lines[li][..idx + 2], text);
        if let Some(st) = stamp {
            line.push_str(&format!(" @done({st})"));
        }
        self.lines[li] = line;
        self.save()
    }

    fn open_input(&mut self, kind: InputKind) {
        let mut textarea = if kind == InputKind::Edit {
            let Some(li) = self.current_task_line() else {
                return;
            };
            let (_, idx) = task_info(&self.lines[li]).unwrap();
            let (title, _) = split_stamp(&self.lines[li][idx + 2..]);
            TextArea::from(vec![title])
        } else {
            TextArea::default()
        };
        if kind == InputKind::NewSub && self.current_item().is_none() {
            return;
        }
        textarea.set_cursor_line_style(Style::default());
        textarea.move_cursor(ratatui_textarea::CursorMove::End);
        self.input = Some(InputState { kind, textarea });
    }

    fn commit_input(&mut self) -> io::Result<()> {
        let Some(is) = self.input.take() else {
            return Ok(());
        };
        let text = is.textarea.lines().join(" ").trim().to_string();
        if text.is_empty() {
            self.status = "cancelled".to_string();
            return Ok(());
        }
        match is.kind {
            InputKind::Edit => {
                self.edit_title(&text)?;
                self.status = "edited".to_string();
            }
            InputKind::NewTask => self.add_todo(&text)?,
            InputKind::NewSub => self.add_subtask(&text)?,
            InputKind::NewSection => self.move_current_to(&MoveDest::Section(text))?,
        }
        Ok(())
    }

    fn open_notes(&mut self) {
        let Some(it) = self.current_item() else {
            return;
        };
        let children: Vec<String> = self.lines[it.start + 1..it.start + it.len].to_vec();
        let indent = children
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| leading_ws(l))
            .min()
            .unwrap_or(2);
        let content: Vec<String> = children
            .iter()
            .map(|l| {
                let cut = indent.min(leading_ws(l));
                l[cut..].to_string()
            })
            .collect();
        let raw = &self.lines[it.start];
        let (_, idx) = task_info(raw).unwrap();
        let (title, _) = split_stamp(&raw[idx + 2..]);
        let mut textarea = if content.is_empty() {
            TextArea::default()
        } else {
            TextArea::from(content)
        };
        textarea.set_cursor_line_style(Style::default());
        textarea.set_wrap_mode(ratatui_textarea::WrapMode::Word);
        textarea.move_cursor(ratatui_textarea::CursorMove::Bottom);
        textarea.move_cursor(ratatui_textarea::CursorMove::End);
        self.notes = Some(NotesState {
            start: it.start,
            child_len: it.len - 1,
            indent,
            title,
            textarea,
        });
    }

    fn close_notes(&mut self) -> io::Result<()> {
        let Some(ns) = self.notes.take() else {
            return Ok(());
        };
        self.checkpoint();
        let mut children: Vec<String> = ns
            .textarea
            .lines()
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}{}", " ".repeat(ns.indent), l)
                }
            })
            .collect();
        while children.last().is_some_and(|l| l.trim().is_empty()) {
            children.pop();
        }
        self.lines
            .splice(ns.start + 1..ns.start + 1 + ns.child_len, children);
        self.tidy();
        self.save()
    }

    fn rows(&self) -> Vec<Row> {
        let its = items(&self.lines);
        let mut rows = Vec::new();
        let mut it_idx = 0;
        let mut vis_seen = 0;
        let mut in_collapsed = false;
        let mut i = 0;
        while i < self.lines.len() {
            let line = &self.lines[i];
            if is_heading(line) {
                let name = line.trim_start_matches('#').trim().to_string();
                let hidden = self.hide_done && section_kind(&name) == SectionKind::Done;
                in_collapsed = line.starts_with("##") && self.collapsed.contains(&name);
                if in_collapsed {
                    if !hidden {
                        let end = self.lines[i + 1..]
                            .iter()
                            .position(|l| l.starts_with("##"))
                            .map(|p| i + 1 + p)
                            .unwrap_or(self.lines.len());
                        let inside = its.iter().filter(|t| t.start > i && t.start < end);
                        let total = inside.clone().count();
                        let done = inside.filter(|t| t.done).count();
                        rows.push(Row::CollapsedSection { name, done, total });
                        vis_seen += 1;
                    }
                } else if !hidden {
                    rows.push(Row::Header(name));
                }
                i += 1;
                continue;
            }
            if it_idx < its.len() && its[it_idx].start == i {
                let it = its[it_idx];
                it_idx += 1;
                i = it.start + it.len;
                if in_collapsed || (self.hide_done && it.done) {
                    continue;
                }
                let src = &self.lines[it.start];
                let (_, idx) = task_info(src).unwrap();
                let (text, stamp) = split_stamp(&src[idx + 2..]);
                let child = &self.lines[it.start + 1..it.start + it.len];
                let mut sub_total = 0;
                let mut sub_done = 0;
                let mut has_notes = false;
                for l in child {
                    if let Some((d, _)) = task_info(l) {
                        sub_total += 1;
                        if d {
                            sub_done += 1;
                        }
                    } else if !l.trim().is_empty() {
                        has_notes = true;
                    }
                }
                rows.push(Row::Task {
                    text,
                    done: it.done,
                    stamp,
                    subs: (sub_total > 0).then_some((sub_done, sub_total)),
                    has_notes,
                });
                let expanded = self.expand_all || vis_seen == self.cursor;
                vis_seen += 1;
                if expanded {
                    for &si in &self.sub_line_indices(it) {
                        let (d, sidx) = task_info(&self.lines[si]).unwrap();
                        rows.push(Row::Sub {
                            text: self.lines[si][sidx + 2..].trim().to_string(),
                            done: d,
                        });
                    }
                }
                continue;
            }
            i += 1;
        }
        rows
    }
}

fn run(path: &Path, startup_status: Option<String>) -> io::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::load(path)?;
    if let Some(s) = startup_status {
        app.status = s;
    }

    loop {
        if app.notes.is_none()
            && app.input.is_none()
            && app.move_menu.is_none()
            && app.history.is_none()
            && app.externally_modified()
        {
            app.reload()?;
            app.status = "reloaded (changed on disk)".to_string();
        }

        terminal.draw(|f| draw(f, &app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.notes.is_some() {
            if key.code == KeyCode::BackTab || key.code == KeyCode::Esc {
                app.close_notes()?;
                app.status = "notes saved".to_string();
            } else if let Some(ns) = app.notes.as_mut() {
                ns.textarea.input(key);
            }
            continue;
        }

        if app.input.is_some() {
            match key.code {
                KeyCode::Enter => app.commit_input()?,
                KeyCode::Esc => {
                    app.input = None;
                    app.status = "cancelled".to_string();
                }
                _ => {
                    if let Some(is) = app.input.as_mut() {
                        is.textarea.input(key);
                    }
                }
            }
            continue;
        }

        if app.history.is_some() {
            if matches!(
                key.code,
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('D')
            ) {
                app.history = None;
            } else if let Some(hv) = app.history.as_mut() {
                // Paragraph scrolls by rendered rows, and wrap makes those
                // outnumber logical lines; estimate with a row of slack each
                let width = crossterm::terminal::size()
                    .map(|(w, _)| (w as usize).saturating_sub(4).max(1))
                    .unwrap_or(76);
                let rows: usize = hv.lines.iter().map(|l| l.chars().count() / width + 1).sum();
                let max = rows.saturating_sub(1) as u16;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => hv.scroll = (hv.scroll + 1).min(max),
                    KeyCode::Char('k') | KeyCode::Up => hv.scroll = hv.scroll.saturating_sub(1),
                    KeyCode::Char('g') => hv.scroll = 0,
                    KeyCode::Char('G') => hv.scroll = max,
                    _ => {}
                }
            }
            continue;
        }

        if let Some(menu) = app.move_menu.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    app.move_menu = None;
                    app.status = "cancelled".to_string();
                }
                KeyCode::Enter => app.commit_move()?,
                KeyCode::Char('j') | KeyCode::Down => {
                    menu.sel = (menu.sel + 1).min(menu.options.len() - 1);
                }
                KeyCode::Char('k') | KeyCode::Up => menu.sel = menu.sel.saturating_sub(1),
                _ => {}
            }
            continue;
        }

        if app.show_help {
            app.show_help = false;
            continue;
        }

        if let Some(yes) = app.quit_confirm {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('y') => break,
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if yes {
                        break;
                    }
                    app.quit_confirm = None;
                }
                KeyCode::Char('n') => app.quit_confirm = None,
                KeyCode::Char('h')
                | KeyCode::Char('l')
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab => app.quit_confirm = Some(!yes),
                _ => {}
            }
            continue;
        }

        app.status.clear();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.undo_last()?;
            }
            KeyCode::Char('q') | KeyCode::Esc => app.quit_confirm = Some(true),
            KeyCode::Char('j') | KeyCode::Down if !shift => app.nav_down(),
            KeyCode::Char('k') | KeyCode::Up if !shift => app.nav_up(),
            KeyCode::Char('J') | KeyCode::Down => app.move_item(1)?,
            KeyCode::Char('K') | KeyCode::Up => app.move_item(-1)?,
            KeyCode::Char('n') | KeyCode::Char('N') => app.open_input(InputKind::NewTask),
            KeyCode::Char('s') => app.open_input(InputKind::NewSub),
            KeyCode::Char('e') => app.open_input(InputKind::Edit),
            KeyCode::Char('X') => app.delete_current()?,
            KeyCode::Char('c') => app.toggle_collapse(),
            KeyCode::Char('A') => app.archive_section()?,
            KeyCode::Char('m') | KeyCode::Char('M') => app.open_move_menu(),
            KeyCode::Char('O') => {
                app.expand_all = !app.expand_all;
                if !app.expand_all {
                    app.sub = None;
                }
                app.status = if app.expand_all {
                    "expanded all"
                } else {
                    "collapsed"
                }
                .to_string();
            }
            KeyCode::Char('~') => app.show_help = true,
            KeyCode::Char('S') => {
                app.status = match sync_commit_push(&app.path) {
                    Ok(m) => {
                        app.reload()?;
                        format!("sync: {m}")
                    }
                    Err(e) => format!("sync: {e}"),
                };
            }
            KeyCode::Char('D') => {
                let sidecar = app.path.with_extension("done.md");
                match fs::read_to_string(&sidecar) {
                    Ok(raw) => {
                        app.history = Some(HistoryView {
                            lines: raw.lines().map(String::from).collect(),
                            scroll: 0,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        app.status = "no done history yet".to_string();
                    }
                    Err(e) => app.status = format!("done history: {e}"),
                }
            }
            KeyCode::Tab => app.open_notes(),
            KeyCode::Char(' ') | KeyCode::Enter => {
                if matches!(app.entries().get(app.cursor), Some(Entry::Section(_))) {
                    app.toggle_collapse();
                } else if app.sub.is_some() {
                    app.toggle_sub()?;
                } else {
                    app.toggle()?;
                }
            }
            KeyCode::Char('l') => app.cycle_later()?,
            KeyCode::Char('h') => {
                app.hide_done = !app.hide_done;
                app.sub = None;
                app.clamp_cursor();
            }
            KeyCode::Char('g') => {
                app.cursor = 0;
                app.sub = None;
            }
            KeyCode::Char('G') => {
                app.cursor = app.entries().len().saturating_sub(1);
                app.sub = None;
            }
            KeyCode::Char('r') => {
                app.reload()?;
                app.sub = None;
                app.status = "reloaded".to_string();
            }
            _ => {}
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());

    if let Some(hv) = &app.history {
        let styled: Vec<Line> = hv
            .lines
            .iter()
            .map(|l| {
                if is_heading(l) {
                    Line::from(Span::styled(
                        l.clone(),
                        Style::new().fg(Color::Cyan).bold(),
                    ))
                } else if task_info(l).is_some() {
                    Line::from(Span::raw(l.clone()))
                } else {
                    Line::from(Span::styled(l.clone(), Style::new().fg(Color::DarkGray)))
                }
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(" done history (read-only) ");
        f.render_widget(
            Paragraph::new(styled)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((hv.scroll, 0)),
            main,
        );
        f.render_widget(
            Line::from(Span::styled(
                " j/k scroll · g/G top/bottom · D or q close",
                Style::new().fg(Color::DarkGray),
            )),
            footer,
        );
        return;
    }

    if let Some(ns) = &app.notes {
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(format!(" notes: {} ", ns.title));
        let inner = block.inner(main);
        f.render_widget(block, main);
        f.render_widget(&ns.textarea, inner);
        f.render_widget(
            Line::from(Span::styled(
                " shift+tab save & close · plain lines = notes · \"- [ ] …\" lines = sub-todos",
                Style::new().fg(Color::DarkGray),
            )),
            footer,
        );
        return;
    }

    let inner_width = (main.width as usize).saturating_sub(4);
    let rows = app.rows();
    let mut task_seen = 0usize;
    let mut sub_seen = 0usize;
    let mut selected_row = None;
    let items_ui: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(ri, row)| match row {
            Row::Header(text) => ListItem::new(Line::from(Span::styled(
                text.clone(),
                Style::new().fg(Color::Cyan).bold(),
            ))),
            Row::Task {
                text,
                done,
                stamp,
                subs,
                has_notes,
            } => {
                if task_seen == app.cursor && app.sub.is_none() {
                    selected_row = Some(ri);
                }
                task_seen += 1;
                sub_seen = 0;
                let (mark, mark_style, text_style) = if *done {
                    (
                        "[x] ",
                        Style::new().fg(Color::DarkGray),
                        Style::new().fg(Color::DarkGray).crossed_out(),
                    )
                } else {
                    ("[ ] ", Style::new().fg(Color::Green), Style::new())
                };
                let mut spans = vec![
                    Span::styled(mark, mark_style),
                    Span::styled(text.clone(), text_style),
                ];
                let mut left = 4 + text.chars().count();
                if let Some((d, t)) = subs {
                    let s = format!("  {d}/{t}");
                    left += s.chars().count();
                    spans.push(Span::styled(s, Style::new().fg(Color::DarkGray)));
                }
                if *has_notes {
                    spans.push(Span::styled("  ≡", Style::new().fg(Color::DarkGray)));
                    left += 3;
                }
                if let Some(st) = stamp {
                    let disp = fmt_stamp(st);
                    let pad = inner_width
                        .saturating_sub(left + disp.chars().count())
                        .max(1);
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(disp, Style::new().fg(Color::DarkGray).dim()));
                }
                ListItem::new(Line::from(spans))
            }
            Row::Sub { text, done } => {
                if task_seen == app.cursor + 1 && app.sub == Some(sub_seen) {
                    selected_row = Some(ri);
                }
                sub_seen += 1;
                let (mark, mark_style, text_style) = if *done {
                    (
                        "[x] ",
                        Style::new().fg(Color::DarkGray),
                        Style::new().fg(Color::DarkGray).crossed_out(),
                    )
                } else {
                    ("[ ] ", Style::new().fg(Color::Green), Style::new())
                };
                ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(mark, mark_style),
                    Span::styled(text.clone(), text_style),
                ]))
            }
            Row::CollapsedSection { name, done, total } => {
                if task_seen == app.cursor {
                    selected_row = Some(ri);
                }
                task_seen += 1;
                sub_seen = 0;
                ListItem::new(Line::from(vec![
                    Span::styled("▸ ", Style::new().fg(Color::Cyan)),
                    Span::styled(name.clone(), Style::new().fg(Color::Cyan).bold()),
                    Span::styled(
                        format!("  {done}/{total}"),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]))
            }
        })
        .collect();

    let all = items(&app.lines);
    let done = all.iter().filter(|it| it.done).count();
    let title = format!(
        " {} — {done}/{} done{} ",
        app.path.display(),
        all.len(),
        if app.hide_done { " (hiding done)" } else { "" }
    );

    let list = List::new(items_ui)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .padding(Padding::horizontal(1))
                .title(title),
        )
        .highlight_style(Style::new().bg(Color::Rgb(50, 55, 70)));

    let mut state = ListState::default();
    state.select(selected_row);
    f.render_stateful_widget(list, main, &mut state);

    if let Some(is) = &app.input {
        let h = 3.min(main.height);
        let area = Rect::new(main.x, main.y + main.height - h, main.width, h);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(match is.kind {
                InputKind::NewTask => " new todo ",
                InputKind::NewSub => " new subtask ",
                InputKind::NewSection => " new section ",
                InputKind::Edit => " edit ",
            });
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        f.render_widget(&is.textarea, inner);
    }

    if let Some(menu) = &app.move_menu {
        let w = 36.min(main.width);
        let h = (menu.options.len() as u16 + 2).min(main.height);
        let area = Rect::new(
            main.x + (main.width - w) / 2,
            main.y + (main.height - h) / 2,
            w,
            h,
        );
        let opts: Vec<ListItem> = menu
            .options
            .iter()
            .map(|d| ListItem::new(d.label().to_string()))
            .collect();
        let list = List::new(opts)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .padding(Padding::horizontal(1))
                    .title(" move to "),
            )
            .highlight_style(Style::new().bg(Color::Rgb(50, 55, 70)));
        let mut st = ListState::default();
        st.select(Some(menu.sel));
        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut st);
    }

    if let Some(yes) = app.quit_confirm {
        let w = 26.min(main.width);
        let h = 3.min(main.height);
        let area = Rect::new(
            main.x + (main.width - w) / 2,
            main.y + (main.height - h) / 2,
            w,
            h,
        );
        let sel = Style::new().bg(Color::Rgb(50, 55, 70)).bold();
        let buttons = Line::from(vec![
            Span::styled("[ yes ]", if yes { sel } else { Style::new() }),
            Span::raw("    "),
            Span::styled("[ no ]", if !yes { sel } else { Style::new() }),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" quit? ");
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(buttons)
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
    }

    if app.show_help {
        let binds = [
            ("j/k", "move (walks into subtasks)"),
            ("space/enter", "toggle done"),
            ("n", "new todo"),
            ("s", "new subtask"),
            ("e", "edit title"),
            ("X", "delete"),
            ("m", "move to section"),
            ("c", "collapse/expand section"),
            ("A", "archive collapsed section"),
            ("J/K", "reorder"),
            ("l", "todo <-> later"),
            ("O", "expand/collapse all subtasks"),
            ("tab", "notes editor"),
            ("h", "hide done"),
            ("D", "done history (older than 7 days)"),
            ("S", "git sync (pull + push)"),
            ("g/G", "top / bottom"),
            ("r", "reload from disk"),
            ("ctrl+z", "undo"),
            ("~", "this help"),
            ("q/esc", "quit (press twice to confirm)"),
        ];
        let lines: Vec<Line> = binds
            .iter()
            .map(|(k, desc)| {
                Line::from(vec![
                    Span::styled(format!("{k:>12}  "), Style::new().fg(Color::Cyan)),
                    Span::raw(*desc),
                ])
            })
            .collect();
        let w = 48.min(main.width);
        let h = (binds.len() as u16 + 2).min(main.height);
        let area = Rect::new(
            main.x + (main.width - w) / 2,
            main.y + (main.height - h) / 2,
            w,
            h,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(" keybinds ");
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    let help = if app.show_help {
        " any key to close".to_string()
    } else if app.quit_confirm.is_some() {
        " q/esc/y quit · n stay · h/l choose · enter select".to_string()
    } else if app.move_menu.is_some() {
        " j/k choose · enter move · esc cancel".to_string()
    } else if app.input.is_some() {
        " enter save · esc cancel".to_string()
    } else if app.status.is_empty() {
        " j/k move · space done · n new · s sub · e edit · m move · c collapse · X del · ~ help · q quit"
            .to_string()
    } else {
        format!(" {}", app.status)
    };
    f.render_widget(
        Line::from(Span::styled(help, Style::new().fg(Color::DarkGray))),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_from(lines: &[&str]) -> App {
        App {
            path: std::env::temp_dir().join(format!("locdo-test-{}.md", std::process::id())),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            crlf: false,
            mtime: None,
            cursor: 0,
            hide_done: false,
            sub: None,
            expand_all: false,
            show_help: false,
            status: String::new(),
            notes: None,
            input: None,
            move_menu: None,
            history: None,
            quit_confirm: None,
            collapsed: HashSet::new(),
            undo: Vec::new(),
        }
    }

    #[test]
    fn parses_task_lines() {
        assert_eq!(task_info("- [ ] open"), Some((false, 3)));
        assert_eq!(task_info("- [x] done"), Some((true, 3)));
        assert_eq!(task_info("- [X] done caps"), Some((true, 3)));
        assert_eq!(task_info("  - [ ] nested"), Some((false, 5)));
        assert_eq!(task_info("* [ ] star bullet"), Some((false, 3)));
        assert_eq!(task_info("# heading"), None);
        assert_eq!(task_info("- plain bullet"), None);
        assert_eq!(task_info("- [?] weird status"), None);
        assert_eq!(task_info(""), None);
    }

    #[test]
    fn splits_stamps() {
        assert_eq!(split_stamp("plain task"), ("plain task".into(), None));
        assert_eq!(
            split_stamp("task @done(2026-08-30 14:32)"),
            ("task".into(), Some("2026-08-30 14:32".into()))
        );
    }

    #[test]
    fn items_capture_child_blocks() {
        let lines: Vec<String> = [
            "# Todo",
            "",
            "- [ ] parent",
            "  note line",
            "  - [ ] sub",
            "",
            "- [ ] second",
            "",
            "## Later",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let its = items(&lines);
        assert_eq!(its.len(), 2);
        assert_eq!((its[0].start, its[0].len), (2, 3));
        assert_eq!((its[1].start, its[1].len), (6, 1));
    }

    #[test]
    fn toggle_moves_to_done_top_and_back() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] first",
            "- [ ] second",
            "",
            "## Done",
            "",
            "- [x] older @done(2026-08-29 09:00)",
        ]);
        app.toggle().unwrap();
        let done_h = app.heading_line("done").unwrap();
        let top = &app.lines[app.after_heading(done_h)];
        assert!(top.starts_with("- [x] first @done("), "got: {top}");
        // untoggle it: it is now the second visible item (second, then first)
        app.cursor = 1;
        app.toggle().unwrap();
        let todo_tasks: Vec<_> = app
            .visible_items()
            .iter()
            .filter(|it| !it.done)
            .map(|it| app.lines[it.start].clone())
            .collect();
        assert_eq!(todo_tasks, vec!["- [ ] second", "- [ ] first"]);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn cycle_later_round_trips() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "- [ ] b", "", "## Later"]);
        app.cycle_later().unwrap();
        assert_eq!(
            app.section_at(app.heading_line("later").unwrap() + 2),
            SectionKind::Later
        );
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] a");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Later);
        // move it back
        app.cursor = 1;
        app.cycle_later().unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] a");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Todo);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn add_todo_appends_to_todo_section() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "", "## Later", "", "- [ ] l1"]);
        app.add_todo("new task").unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[1].start], "- [ ] new task");
        assert_eq!(app.section_at(vis[1].start), SectionKind::Todo);
        assert_eq!(app.cursor, 1);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn delete_removes_item_with_children() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] parent",
            "  note line",
            "  - [ ] sub",
            "",
            "- [ ] second",
        ]);
        app.delete_current().unwrap();
        assert!(!app.lines.iter().any(|l| l.contains("parent")));
        assert!(!app.lines.iter().any(|l| l.contains("note line")));
        assert!(!app.lines.iter().any(|l| l.contains("sub")));
        let vis = app.visible_items();
        assert_eq!(vis.len(), 1);
        assert_eq!(app.lines[vis[0].start], "- [ ] second");
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn edit_title_preserves_children_and_stamp() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] old title",
            "  note line",
            "",
            "## Done",
            "",
            "- [x] finished @done(2026-08-29 09:00)",
        ]);
        app.edit_title("new title").unwrap();
        let vis = app.visible_items();
        assert_eq!(app.lines[vis[0].start], "- [ ] new title");
        assert_eq!(app.lines[vis[0].start + 1], "  note line");

        app.cursor = 1;
        app.edit_title("renamed").unwrap();
        let vis = app.visible_items();
        assert_eq!(
            app.lines[vis[1].start],
            "- [x] renamed @done(2026-08-29 09:00)"
        );
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn nav_walks_into_subs_of_current_item_only() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] a",
            "  - [ ] a1",
            "  - [ ] a2",
            "",
            "- [ ] b",
        ]);
        app.nav_down();
        assert_eq!((app.cursor, app.sub), (0, Some(0)));
        app.nav_down();
        assert_eq!((app.cursor, app.sub), (0, Some(1)));
        app.nav_down();
        assert_eq!((app.cursor, app.sub), (1, None));
        // collapsed: moving up lands on a's main line, not its subs
        app.nav_up();
        assert_eq!((app.cursor, app.sub), (0, None));
        // expand_all: moving up from b lands on a's last sub
        app.expand_all = true;
        app.cursor = 1;
        app.sub = None;
        app.nav_up();
        assert_eq!((app.cursor, app.sub), (0, Some(1)));
    }

    #[test]
    fn sub_toggle_sinks_below_open_subs_and_stays_in_block() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] p",
            "  - [ ] s1",
            "  - [ ] s2",
            "  - [ ] s3",
            "",
            "## Done",
        ]);
        app.sub = Some(0);
        app.toggle_sub().unwrap();
        assert_eq!(app.lines[3], "  - [ ] s2");
        assert_eq!(app.lines[4], "  - [ ] s3");
        assert_eq!(app.lines[5], "  - [x] s1");
        assert_eq!(app.sub, Some(2));
        // parent block untouched: still one top-level item, nothing in Done
        assert_eq!(app.visible_items().len(), 1);
        assert_eq!(app.section_at(5), SectionKind::Todo);
        // untoggle: floats back to end of the open group
        app.toggle_sub().unwrap();
        assert_eq!(app.lines[3], "  - [ ] s2");
        assert_eq!(app.lines[4], "  - [ ] s3");
        assert_eq!(app.lines[5], "  - [ ] s1");
        assert_eq!(app.sub, Some(2));
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn add_subtask_goes_after_open_subs_before_done() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] p",
            "  - [ ] open1",
            "  - [x] done1",
            "",
            "- [ ] q",
        ]);
        app.add_subtask("newsub").unwrap();
        assert_eq!(app.lines[3], "  - [ ] open1");
        assert_eq!(app.lines[4], "  - [ ] newsub");
        assert_eq!(app.lines[5], "  - [x] done1");
        assert_eq!(app.sub, Some(1));
        let _ = fs::remove_file(&app.path);

        // no subtasks yet: goes directly beneath the parent line, above notes
        let mut app2 = app_from(&["# Todo", "", "- [ ] p", "  note line"]);
        app2.add_subtask("first").unwrap();
        assert_eq!(app2.lines[3], "  - [ ] first");
        assert_eq!(app2.lines[4], "  note line");
        let _ = fs::remove_file(&app2.path);
    }

    #[test]
    fn todo_end_stops_before_custom_sections() {
        let app = app_from(&[
            "# Todo",
            "",
            "- [ ] inbox1",
            "",
            "## Groceries",
            "",
            "- [ ] milk",
            "",
            "## Done",
        ]);
        assert_eq!(app.todo_end(), 3);
    }

    #[test]
    fn move_to_section_appends_and_creates() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] task a",
            "",
            "## Groceries",
            "",
            "- [ ] milk",
        ]);
        app.move_current_to(&MoveDest::Section("Groceries".into()))
            .unwrap();
        let g = app.heading_line("groceries").unwrap();
        assert_eq!(app.lines[g + 2], "- [ ] milk");
        assert_eq!(app.lines[g + 3], "- [ ] task a");

        // moving to a section that doesn't exist creates it
        app.cursor = 0; // milk
        app.move_current_to(&MoveDest::Section("Work".into())).unwrap();
        let w = app.heading_line("work").unwrap();
        assert_eq!(app.lines[w + 2], "- [ ] milk");
        assert_eq!(app.section_at(app.heading_line("groceries").unwrap() + 2), SectionKind::Todo);
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn entries_include_collapsed_sections_as_single_stop() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] a",
            "",
            "## Later",
            "",
            "- [ ] l1",
            "",
            "## Done",
            "",
            "- [x] d1",
        ]);
        let e = app.entries();
        assert_eq!(e.len(), 3);
        assert!(e.iter().all(|x| matches!(x, Entry::Task(_))));
        app.collapsed.insert("Later".to_string());
        let e = app.entries();
        assert_eq!(e.len(), 3);
        assert!(matches!(e[0], Entry::Task(_)));
        assert!(matches!(e[1], Entry::Section(4)));
        assert!(matches!(e[2], Entry::Task(_)));
    }

    #[test]
    fn collapse_key_targets_containing_section() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] inbox",
            "",
            "## Groceries",
            "",
            "- [ ] milk",
        ]);
        app.cursor = 1; // milk
        app.toggle_collapse();
        assert!(app.collapsed.contains("Groceries"));
        assert!(matches!(app.entries()[app.cursor], Entry::Section(4)));
        app.toggle_collapse();
        assert!(app.collapsed.is_empty());
        // top inbox area has no section to collapse
        app.cursor = 0;
        app.toggle_collapse();
        assert!(app.collapsed.is_empty());
    }

    #[test]
    fn archive_section_moves_to_sidecar() {
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] keep",
            "",
            "## Old",
            "",
            "- [ ] gone",
            "  - [x] sub",
        ]);
        app.collapsed.insert("Old".to_string());
        app.cursor = 1; // the collapsed section entry
        app.archive_section().unwrap();
        assert!(app.heading_line("old").is_none());
        assert!(!app.lines.iter().any(|l| l.contains("gone")));
        assert_eq!(app.lines[2], "- [ ] keep");
        assert!(app.collapsed.is_empty());
        let sidecar = app.path.with_extension("archive.md");
        let archived = fs::read_to_string(&sidecar).unwrap();
        assert!(archived.contains("## Old"));
        assert!(archived.contains("- [ ] gone"));
        assert!(archived.contains("  - [x] sub"));
        let _ = fs::remove_file(&app.path);
        let _ = fs::remove_file(&sidecar);
    }

    #[test]
    fn reload_discards_stale_undo_snapshots() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "- [ ] b"]);
        app.path = std::env::temp_dir().join(format!("locdo-test-reload-{}.md", std::process::id()));
        app.delete_current().unwrap(); // builds an undo snapshot
        assert!(!app.undo.is_empty());
        fs::write(&app.path, "# Todo\n\n- [ ] external edit\n").unwrap();
        app.reload().unwrap();
        app.undo_last().unwrap();
        assert_eq!(app.status, "nothing to undo");
        assert_eq!(app.lines[2], "- [ ] external edit");
        let _ = fs::remove_file(&app.path);
    }

    #[test]
    fn rotate_done_moves_week_old_tasks_to_sidecar() {
        let old = (Local::now() - chrono::Duration::days(10))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let recent = (Local::now() - chrono::Duration::days(2))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let mut app = app_from(&[
            "# Todo",
            "",
            "- [ ] open task",
            "",
            "## Done",
            "",
            &format!("- [x] recent one @done({recent})"),
            &format!("- [x] old one @done({old})"),
            "  a note on the old one",
            "- [x] unstamped",
        ]);
        app.path = std::env::temp_dir().join(format!("locdo-test-rotate-{}.md", std::process::id()));
        app.rotate_done().unwrap();
        assert!(!app.lines.iter().any(|l| l.contains("old one")));
        assert!(!app.lines.iter().any(|l| l.contains("a note on the old one")));
        assert!(app.lines.iter().any(|l| l.contains("recent one")));
        assert!(app.lines.iter().any(|l| l.contains("unstamped")));
        let sidecar = app.path.with_extension("done.md");
        let hist = fs::read_to_string(&sidecar).unwrap();
        let month = (Local::now() - chrono::Duration::days(10))
            .format("## %Y-%m")
            .to_string();
        assert!(hist.contains(&month), "got: {hist}");
        assert!(hist.contains("- [x] old one @done("));
        assert!(hist.contains("  a note on the old one"));
        // rotating again is a no-op and doesn't duplicate
        app.rotate_done().unwrap();
        let hist2 = fs::read_to_string(&sidecar).unwrap();
        assert_eq!(hist, hist2);
        let _ = fs::remove_file(&app.path);
        let _ = fs::remove_file(&sidecar);
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    }

    /// Bare "remote" plus a configured clone, under a unique temp dir.
    fn git_sandbox(tag: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("locdo-{tag}-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let remote = base.join("remote.git");
        fs::create_dir_all(&remote).unwrap();
        sh(&remote, &["init", "--bare", "--quiet"]);
        sh(&base, &["clone", "--quiet", remote.to_str().unwrap(), "work"]);
        let work = base.join("work");
        sh(&work, &["config", "user.email", "test@example.com"]);
        sh(&work, &["config", "user.name", "Test"]);
        (base, work)
    }

    #[test]
    fn sync_detects_repo_and_pushes_todo_files() {
        let (base, work) = git_sandbox("sync");
        let todo = work.join("todo.md");
        fs::write(&todo, "# Todo\n\n- [ ] synced task\n").unwrap();

        // a file outside any repo is not synced (guarded: skip if the temp
        // dir itself happens to sit inside some work tree)
        if run_git(&base, &["rev-parse", "--is-inside-work-tree"]).is_err() {
            assert!(sync_repo(&base.join("nowhere.md")).is_none());
        }
        assert!(sync_repo(&todo).is_some());

        let msg = sync_commit_push(&todo).unwrap();
        assert!(msg.contains("pushed"), "got: {msg}");
        let remote = base.join("remote.git");
        let log = std::process::Command::new("git")
            .args(["-C", remote.to_str().unwrap(), "log", "--oneline"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout).to_string();
        assert!(log.contains("locdo sync"), "remote log: {log}");

        // nothing new: no duplicate commit, still succeeds
        let msg = sync_commit_push(&todo).unwrap();
        assert!(msg.contains("nothing"), "got: {msg}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn sync_pulls_before_push_when_remote_is_ahead() {
        let (base, work) = git_sandbox("diverge");
        let todo = work.join("todo.md");
        fs::write(&todo, "# Todo\n\n- [ ] first\n").unwrap();
        sync_commit_push(&todo).unwrap();

        // another machine pushes something else to the remote
        let remote = base.join("remote.git");
        sh(&base, &["clone", "--quiet", remote.to_str().unwrap(), "work2"]);
        let work2 = base.join("work2");
        sh(&work2, &["config", "user.email", "test@example.com"]);
        sh(&work2, &["config", "user.name", "Test"]);
        fs::write(work2.join("other.md"), "elsewhere\n").unwrap();
        sh(&work2, &["add", "other.md"]);
        sh(&work2, &["commit", "--quiet", "-m", "from machine two"]);
        sh(&work2, &["push", "--quiet"]);

        // local change on machine one must rebase onto that and push
        fs::write(&todo, "# Todo\n\n- [ ] first\n- [ ] second\n").unwrap();
        let msg = sync_commit_push(&todo).unwrap();
        assert!(msg.contains("pushed"), "got: {msg}");
        let log = std::process::Command::new("git")
            .args(["-C", remote.to_str().unwrap(), "log", "--oneline"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout).to_string();
        assert!(log.contains("from machine two"), "remote log: {log}");
        assert_eq!(log.matches("locdo sync").count(), 2, "remote log: {log}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rotate_done_orders_months_ascending_with_separation() {
        let old_aug = (Local::now() - chrono::Duration::days(10))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let old_far = (Local::now() - chrono::Duration::days(400))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let mut app = app_from(&[
            "# Todo",
            "",
            "## Done",
            "",
            &format!("- [x] newer old @done({old_aug})"),
            &format!("- [x] ancient @done({old_far})"),
        ]);
        app.path =
            std::env::temp_dir().join(format!("locdo-test-months-{}.md", std::process::id()));
        app.rotate_done().unwrap();
        let sidecar = app.path.with_extension("done.md");
        let hist = fs::read_to_string(&sidecar).unwrap();
        let m_far = (Local::now() - chrono::Duration::days(400))
            .format("## %Y-%m")
            .to_string();
        let m_aug = (Local::now() - chrono::Duration::days(10))
            .format("## %Y-%m")
            .to_string();
        let (pf, pa) = (hist.find(&m_far).unwrap(), hist.find(&m_aug).unwrap());
        assert!(pf < pa, "months not ascending:\n{hist}");
        assert!(
            hist.contains(&format!("\n\n{m_aug}")),
            "no blank line before later month heading:\n{hist}"
        );
        let _ = fs::remove_file(&app.path);
        let _ = fs::remove_file(&sidecar);
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut app = app_from(&["# Todo", "", "- [ ] a", "- [ ] b"]);
        app.delete_current().unwrap();
        assert_eq!(app.visible_items().len(), 1);
        app.undo_last().unwrap();
        assert_eq!(app.visible_items().len(), 2);
        assert_eq!(app.lines[2], "- [ ] a");
        // empty stack: no panic, just a status
        app.undo_last().unwrap();
        assert_eq!(app.status, "nothing to undo");
        let _ = fs::remove_file(&app.path);
    }
}
