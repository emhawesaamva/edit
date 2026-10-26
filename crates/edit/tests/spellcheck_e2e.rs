// Licensed under the MIT License.
// Fork-only: spellcheck (not part of upstream microsoft/edit).

//! End-to-end tests for spell checking: the real `edit` binary, driven through a pseudo-terminal.
//!
//! * Every test runs the editor with `HOME` and `XDG_CONFIG_HOME` pointing at a fresh directory
//!   under `CARGO_TARGET_TMPDIR`, so the user's own settings are never read or written.
//! * The harness answers the queries that `setup_terminal()` in main.rs sends at startup
//!   (CPR for the width of "…", and DA1, which it waits for). The OSC color queries stay
//!   unanswered, so the editor uses its default palette.
//! * Every test has a hard deadline ([`TEST_TIMEOUT`]). The editor is always killed when a test
//!   ends, also when it fails, so a hang can't block CI.
//! * Assertions look at the SGR sequences right before and after a word, and at the underline
//!   of the word's cells on a minimal emulated screen. Nothing compares whole screens.

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use std::{ptr, thread};

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// The longest a single test may take, including starting and stopping the editor.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

// Keys, as a terminal sends them.
const F7: &str = "\x1b[18~";
const CTRL_T: &str = "\x14";
const CTRL_S: &str = "\x13";
const CTRL_Q: &str = "\x11";
const CTRL_F: &str = "\x06";
const ENTER: &str = "\r";
const DOWN: &str = "\x1b[B";
const RIGHT: &str = "\x1b[C";

/// The curly underline as `Framebuffer::format_underline()` emits it, without the color values.
const CURLY_ON: &str = "\x1b[4:3m";
const CURLY_COLOR: &str = "\x1b[58:2::";
const CURLY_OFF: &str = "\x1b[59m\x1b[24m";

/// A temporary directory for one test: the files it opens, and the editor's `HOME`.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("spellcheck-e2e-{}-{n}", std::process::id()));
        _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("home")).unwrap();
        fs::create_dir_all(path.join("xdg")).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// Writes the editor's settings.json.
    fn write_settings(&self, json: &str) -> PathBuf {
        self.write(&format!("{}/settings.json", Self::CONFIG_DIR), json)
    }

    /// Reads a file in the editor's settings directory.
    fn read_config(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(Self::CONFIG_DIR).join(name)).unwrap_or_default()
    }

    /// See `config_dir()` in settings.rs.
    const CONFIG_DIR: &str = if cfg!(any(target_os = "macos", target_os = "ios")) {
        "home/Library/Application Support/com.microsoft.edit"
    } else {
        "xdg/msedit"
    };
}

impl Drop for TempDir {
    fn drop(&mut self) {
        _ = fs::remove_dir_all(&self.0);
    }
}

/// Serializes creating ptys and spawning children, so that no child inherits
/// another test's pty before it's marked close-on-exec.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

fn set_cloexec(fd: &OwnedFd) {
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_ne!(ret, -1, "F_SETFD: {}", io::Error::last_os_error());
}

/// Returns the master and slave side of a new pseudo-terminal of `COLS` x `ROWS`.
fn openpty() -> (OwnedFd, OwnedFd) {
    let size = libc::winsize { ws_row: ROWS, ws_col: COLS, ws_xpixel: 0, ws_ypixel: 0 };
    let mut master = -1;
    let mut slave = -1;
    let ret = unsafe {
        libc::openpty(&mut master, &mut slave, ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
    };
    assert_eq!(ret, 0, "openpty: {}", io::Error::last_os_error());
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(&master);
    set_cloexec(&slave);

    // The same ioctl a terminal emulator uses when its window is resized.
    let ret = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &raw const size) };
    assert_eq!(ret, 0, "TIOCSWINSZ: {}", io::Error::last_os_error());

    (master, slave)
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    /// The pty reported EOF or EIO, i.e. the editor is gone.
    closed: bool,
}

/// A running editor.
struct Editor {
    child: Child,
    /// Whether the child was reaped with `wait4()` behind `child`'s back.
    reaped: bool,
    pty: File,
    output: Arc<(Mutex<Output>, Condvar)>,
    deadline: Instant,
}

impl Editor {
    /// Starts `edit` with the given arguments in `dir`, and waits until it's past its startup queries.
    fn spawn(dir: &TempDir, args: &[&Path]) -> Self {
        Self::spawn_with(dir, args, &[], TEST_TIMEOUT)
    }

    /// [`Editor::spawn`] with extra environment variables (and a different timeout).
    fn spawn_with(dir: &TempDir, args: &[&Path], env: &[(&str, &str)], timeout: Duration) -> Self {
        let deadline = Instant::now() + timeout;

        let (child, pty) = {
            let _lock = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let (master, slave) = openpty();

            let mut cmd = Command::new(env!("CARGO_BIN_EXE_edit"));
            cmd.args(args)
                .current_dir(dir.path())
                .env_clear()
                .env("HOME", dir.path().join("home"))
                .env("XDG_CONFIG_HOME", dir.path().join("xdg"))
                .env("PATH", "/usr/bin:/bin")
                .env("TERM", "xterm-256color")
                .env("LANG", "en_US.UTF-8")
                .env("LANGUAGE", "en")
                .envs(env.iter().copied())
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave));

            // SAFETY: Only async-signal-safe functions are called between fork and exec.
            unsafe {
                cmd.pre_exec(|| {
                    // A new session with the pty as its controlling terminal, like a terminal emulator does it.
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            let child = cmd.spawn().expect("failed to spawn edit");
            // Dropping `cmd` closes our copies of the slave side, so that reading
            // the master side ends once the editor exits.
            drop(cmd);
            (child, File::from(master))
        };

        let output = Arc::new((Mutex::new(Output::default()), Condvar::new()));
        {
            let output = output.clone();
            let mut reader = pty.try_clone().unwrap();
            thread::spawn(move || {
                let mut buf = [0; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let (lock, cvar) = &*output;
                            let mut out = lock.lock().unwrap_or_else(PoisonError::into_inner);
                            out.bytes.extend_from_slice(&buf[..n]);
                            cvar.notify_all();
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        // EIO, once the editor (the last holder of the slave side) is gone.
                        Err(_) => break,
                    }
                }
                let (lock, cvar) = &*output;
                lock.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
                cvar.notify_all();
            });
        }

        let mut editor = Self { child, reaped: false, pty, output, deadline };

        // `setup_terminal()` prints "…" and asks for the cursor position (CPR) to learn its width,
        // then waits for the DA1 response. Column 2 means that "…" is 1 column wide.
        editor.wait_raw("the startup queries", 0, |out| out.contains("\x1b[6n\x1b[c"));
        editor.send("\x1b[1;2R\x1b[?1;2c");
        editor
    }

    fn send(&mut self, input: &str) {
        self.pty.write_all(input.as_bytes()).expect("failed to write to the pty");
    }

    /// Right-clicks the given cell, as an SGR mouse report (1006, 1-based coordinates).
    fn right_click(&mut self, (x, y): (usize, usize)) {
        self.send(&format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", x + 1, y + 1, x + 1, y + 1));
    }

    fn left_click(&mut self, (x, y): (usize, usize)) {
        self.send(&format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1));
    }

    /// Resizes the terminal like a terminal emulator does: TIOCSWINSZ on the master side, which
    /// signals the foreground process group. The explicit SIGWINCH covers kernels that don't.
    /// [`Screen`] stays at `COLS` x `ROWS`, so only shrink it.
    fn resize(&mut self, cols: u16, rows: u16) {
        assert!(cols <= COLS && rows <= ROWS);
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let ret =
            unsafe { libc::ioctl(self.pty.as_raw_fd(), libc::TIOCSWINSZ as _, &raw const size) };
        assert_eq!(ret, 0, "TIOCSWINSZ: {}", io::Error::last_os_error());
        let ret = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGWINCH) };
        assert_eq!(ret, 0, "kill: {}", io::Error::last_os_error());
    }

    fn lock(&self) -> MutexGuard<'_, Output> {
        self.output.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The amount of output so far. Pass it to [`Editor::wait_raw`] to wait for newer output.
    fn mark(&self) -> usize {
        self.lock().bytes.len()
    }

    /// All output so far, as (lossy) UTF-8.
    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.lock().bytes).into_owned()
    }

    /// Waits until `pred` is true for the output since `from`. Panics at the deadline.
    fn wait_raw(&self, what: &str, from: usize, mut pred: impl FnMut(&str) -> bool) -> String {
        let cvar = &self.output.1;
        let mut out = self.lock();
        loop {
            let text =
                String::from_utf8_lossy(&out.bytes[from.min(out.bytes.len())..]).into_owned();
            if pred(&text) {
                return text;
            }

            let now = Instant::now();
            if out.closed || now >= self.deadline {
                let state = if out.closed { "the editor exited" } else { "timed out" };
                let all = String::from_utf8_lossy(&out.bytes).into_owned();
                let screen = Screen::parse(&all).dump();
                let tail: String = {
                    let chars: Vec<char> = all.chars().collect();
                    chars[chars.len().saturating_sub(1500)..].iter().collect()
                };
                drop(out);
                panic!(
                    "{state} while waiting for {what}\n--- screen ---\n{screen}--- output tail ---\n{tail:?}"
                );
            }

            out = cvar
                .wait_timeout(out, self.deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Waits until the screen, as rendered by all output so far, satisfies `pred`, and returns it.
    fn wait_screen(&self, what: &str, mut pred: impl FnMut(&Screen) -> bool) -> Screen {
        let mut last = None;
        self.wait_raw(what, 0, |out| {
            let screen = Screen::parse(complete_frames(out));
            let done = pred(&screen);
            last = Some(screen);
            done
        });
        last.unwrap()
    }

    /// Waits for the editor to exit. Panics at the deadline.
    fn wait_exit(&mut self) -> ExitStatus {
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < self.deadline,
                "the editor didn't exit\n{}",
                self.screen().dump()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Like [`Editor::wait_exit`], but also returns the editor's resource usage.
    fn wait_exit_rusage(&mut self) -> (libc::c_int, libc::rusage) {
        let pid = self.child.id() as libc::pid_t;
        loop {
            let mut status = 0;
            let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
            let ret = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
            if ret == pid {
                self.reaped = true;
                return (status, usage);
            }
            assert_ne!(ret, -1, "wait4: {}", io::Error::last_os_error());
            assert!(Instant::now() < self.deadline, "the editor didn't exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn screen(&self) -> Screen {
        Screen::parse(complete_frames(&self.raw()))
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Always stop the editor, also if the test failed. After `try_wait()` returned its status,
        // this does nothing; after `wait4()`, the pid may already belong to another process.
        if !self.reaped {
            _ = self.child.kill();
            _ = self.child.wait();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Underline {
    None,
    Single,
    Curly,
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    underline: Underline,
    /// Whether an underline color (SGR 58) was set.
    colored: bool,
}

/// Just enough of a terminal to tell which text is where, and how it's underlined.
/// Handles CUP, SGR underline state and printable characters (all assumed to be 1 column wide).
struct Screen {
    rows: Vec<Vec<Cell>>,
}

impl Screen {
    fn parse(vt: &str) -> Self {
        let blank = Cell { ch: ' ', underline: Underline::None, colored: false };
        let mut rows = vec![vec![blank; COLS as usize]; ROWS as usize];
        let (mut x, mut y) = (0, 0);
        let mut saved = (0, 0);
        let mut underline = Underline::None;
        let mut colored = false;
        let mut chars = vt.chars();

        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut final_byte = '\0';
                        for c in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c) {
                                final_byte = c;
                                break;
                            }
                            params.push(c);
                        }
                        match final_byte {
                            'H' | 'f' => {
                                let mut it = params
                                    .split(';')
                                    .map(|p| p.parse::<usize>().unwrap_or(1).max(1));
                                y = it.next().unwrap_or(1) - 1;
                                x = it.next().unwrap_or(1) - 1;
                            }
                            'm' => sgr(&params, &mut underline, &mut colored),
                            'J' if params == "2" => rows.iter_mut().for_each(|r| r.fill(blank)),
                            _ => {}
                        }
                    }
                    // OSC, terminated by BEL or ST.
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' {
                                break;
                            }
                            if c == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some('7') => saved = (x, y),
                    Some('8') => (x, y) = saved,
                    _ => {}
                },
                '\r' => x = 0,
                '\n' => y += 1,
                c if c < ' ' => {}
                c => {
                    if let Some(cell) = rows.get_mut(y).and_then(|r| r.get_mut(x)) {
                        *cell = Cell { ch: c, underline, colored };
                    }
                    x += 1;
                }
            }
        }

        Self { rows }
    }

    fn row_text(&self, y: usize) -> String {
        self.rows[y].iter().map(|c| c.ch).collect()
    }

    fn dump(&self) -> String {
        (0..self.rows.len()).map(|y| format!("{:2}|{}|\n", y, self.row_text(y))).collect()
    }

    fn contains(&self, text: &str) -> bool {
        (0..self.rows.len()).any(|y| self.row_text(y).contains(text))
    }

    /// Positions (column, row) where `word` occurs as a whole word.
    fn find_all(&self, word: &str) -> Vec<(usize, usize)> {
        let word: Vec<char> = word.chars().collect();
        let mut res = Vec::new();
        for (y, row) in self.rows.iter().enumerate() {
            let row: Vec<char> = row.iter().map(|c| c.ch).collect();
            for x in 0..row.len().saturating_sub(word.len() - 1) {
                let boundary = |i: Option<&char>| !i.is_some_and(|c| c.is_alphanumeric());
                if row[x..x + word.len()] == word[..]
                    && boundary(x.checked_sub(1).and_then(|i| row.get(i)))
                    && boundary(row.get(x + word.len()))
                {
                    res.push((x, y));
                }
            }
        }
        res
    }

    fn find(&self, word: &str) -> Option<(usize, usize)> {
        self.find_all(word).into_iter().next()
    }

    /// The underline of the first occurrence of `word`, if all of its cells share it.
    fn underline_of(&self, word: &str) -> Option<Underline> {
        let (x, y) = self.find(word)?;
        let cells = &self.rows[y][x..x + word.chars().count()];
        let first = cells[0].underline;
        cells.iter().all(|c| c.underline == first).then_some(first)
    }

    fn is_curly(&self, word: &str) -> bool {
        self.underline_of(word) == Some(Underline::Curly) && {
            let (x, y) = self.find(word).unwrap();
            self.rows[y][x..x + word.chars().count()].iter().all(|c| c.colored)
        }
    }

    /// The cursor position that the status bar shows ("line:column"), searched from the bottom.
    fn location(&self) -> Option<(usize, usize)> {
        (0..self.rows.len()).rev().find_map(|y| {
            self.row_text(y).split_whitespace().find_map(|token| {
                let (line, column) = token.split_once(':')?;
                Some((line.parse().ok()?, column.parse().ok()?))
            })
        })
    }

    fn any_underline(&self, y: usize) -> bool {
        self.rows[y].iter().any(|c| c.underline != Underline::None)
    }

    /// The suggestions popup: the only bordered box, as long as no menu or dialog is open.
    fn popup(&self) -> Option<Popup> {
        for (top, row) in self.rows.iter().enumerate() {
            let Some(left) = row.iter().position(|c| c.ch == '┌') else {
                continue;
            };
            let width = row[left..].iter().position(|c| c.ch == '┐')? + 1;
            let mut items = Vec::new();
            let mut selected = None;
            for row in &self.rows[top + 1..] {
                let cells = &row[left..left + width];
                if cells[0].ch != '│' {
                    break;
                }
                let inner: String = cells[1..width - 1].iter().map(|c| c.ch).collect();
                if inner.starts_with("> ") {
                    selected = Some(items.len());
                }
                items.push(inner.get(2..).unwrap_or("").trim_end().to_string());
            }
            return Some(Popup { left, top, items, selected: selected? });
        }
        None
    }
}

struct Popup {
    left: usize,
    top: usize,
    items: Vec<String>,
    selected: usize,
}

/// Cuts off a frame that's still being received. `Framebuffer::render()` ends every
/// frame that changed anything with DECTCEM, which shows or hides the cursor.
fn complete_frames(out: &str) -> &str {
    match [out.rfind("\x1b[?25h"), out.rfind("\x1b[?25l")].into_iter().flatten().max() {
        Some(i) => &out[..i + "\x1b[?25h".len()],
        None => "",
    }
}

fn sgr(params: &str, underline: &mut Underline, colored: &mut bool) {
    let mut params = params.split(';');
    while let Some(param) = params.next() {
        let mut sub = param.split(':');
        match sub.next().unwrap_or("") {
            "" | "0" => {
                *underline = Underline::None;
                *colored = false;
            }
            "4" => {
                *underline = match sub.next() {
                    Some("0") => Underline::None,
                    Some("3") => Underline::Curly,
                    _ => Underline::Single,
                }
            }
            "24" => *underline = Underline::None,
            "58" => *colored = true,
            "59" => *colored = false,
            _ => {}
        }
        // `38;2;r;g;b` and `38;5;n` (also 48 and 58): skip the color values.
        if matches!(param, "38" | "48" | "58") {
            let n = if params.next() == Some("2") { 3 } else { 1 };
            params.nth(n - 1);
        }
    }
}

fn is_csi_final(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '~'
}

/// For each whole-word occurrence of `word` in the raw output, the escape sequences right in front of it.
fn escapes_before<'a>(out: &'a str, word: &str) -> Vec<&'a str> {
    out.match_indices(word)
        .filter(|&(i, _)| !out[i + word.len()..].starts_with(char::is_alphanumeric))
        .filter_map(|(i, _)| {
            let mut beg = i;
            // Walk back over complete CSI sequences.
            while out[..beg].ends_with(is_csi_final) {
                let esc = out[..beg].rfind('\x1b')?;
                let seq = &out[esc..beg];
                let ok = seq.starts_with("\x1b[")
                    && seq[2..seq.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_digit() || ";:? ".contains(c));
                if !ok {
                    break;
                }
                beg = esc;
            }
            // Whole word: no letter right in front of it.
            if beg == i && out[..i].ends_with(char::is_alphanumeric) {
                return None;
            }
            Some(&out[beg..i])
        })
        .collect()
}

/// For each whole-word occurrence of `word` in the raw output, the escape sequences right after it.
fn escapes_after<'a>(out: &'a str, word: &str) -> Vec<&'a str> {
    out.match_indices(word)
        .map(|(i, _)| {
            let rest = &out[i + word.len()..];
            let mut end = 0;
            while rest[end..].starts_with("\x1b[") {
                match rest[end + 2..].find(is_csi_final) {
                    Some(f) => end += 2 + f + 1,
                    None => break,
                }
            }
            &rest[..end]
        })
        .collect()
}

/// Whether the escapes end with the curly underline, i.e. `ESC[4:3m ESC[58:2::r:g:bm`.
fn ends_with_curly(escapes: &str) -> bool {
    let Some(color) = escapes.rfind(CURLY_COLOR) else {
        return false;
    };
    let rgb = &escapes[color + CURLY_COLOR.len()..];
    escapes[..color].ends_with(CURLY_ON)
        && rgb.strip_suffix('m').is_some_and(|rgb| {
            let parts: Vec<_> = rgb.split(':').collect();
            parts.len() == 3 && parts.iter().all(|p| p.parse::<u8>().is_ok())
        })
}

fn assert_curly(editor: &Editor, word: &str) {
    let out = editor.raw();
    let before = escapes_before(&out, word);
    assert!(before.iter().any(|e| ends_with_curly(e)), "{word}: {before:?}");
    let after = escapes_after(&out, word);
    assert!(after.iter().any(|e| e.contains(CURLY_OFF)), "{word}: {after:?}");
}

const TYPO_TXT: &str = "Teh quick brown fox\n";

/// Opens `TYPO_TXT` and waits until "Teh" is curly underlined.
fn open_typo(dir: &TempDir) -> (Editor, PathBuf, Screen) {
    let file = dir.write("typo.txt", TYPO_TXT);
    let editor = Editor::spawn(dir, &[&file]);
    let screen = editor.wait_screen("a curly underline on \"Teh\"", |s| s.is_curly("Teh"));
    (editor, file, screen)
}

#[test]
fn underline_curly() {
    let dir = TempDir::new();
    // No input is sent after startup: the underline appears once the dictionary has loaded,
    // because `poll_timeout()` wakes up the main loop.
    let (editor, _, screen) = open_typo(&dir);

    assert_curly(&editor, "Teh");
    for word in ["quick", "brown", "fox"] {
        assert_eq!(screen.underline_of(word), Some(Underline::None), "{word}\n{}", screen.dump());
    }
    // The text is rendered in the first textarea row, right after the line number margin.
    let (x, y) = screen.find("Teh").unwrap();
    assert_eq!(y, 1, "{}", screen.dump());
    assert!(x > 0, "{}", screen.dump());
}

/// Opens the popup with `key`, picks "The", saves and quits. The file must then be fixed.
fn replace_with_key(key: &str) {
    let dir = TempDir::new();
    let (mut editor, file, screen) = open_typo(&dir);
    let (x, y) = screen.find("Teh").unwrap();

    editor.send(key);
    let screen = editor.wait_screen("the popup with \"The\"", |s| {
        s.popup().is_some_and(|p| p.items.iter().any(|i| i == "The"))
    });
    let popup = screen.popup().unwrap();
    // Right below the word. The border is at the word's column.
    assert_eq!((popup.left, popup.top), (x, y + 1), "{}", screen.dump());
    assert!(popup.items.len() <= 8, "{:?}", popup.items);

    let target = popup.items.iter().position(|i| i == "The").unwrap();
    assert!(target >= popup.selected, "{:?}", popup.items);
    for _ in popup.selected..target {
        editor.send(DOWN);
    }
    editor
        .wait_screen("\"The\" to be selected", |s| s.popup().is_some_and(|p| p.selected == target));

    editor.send(ENTER);
    let screen = editor.wait_screen("the replacement", |s| {
        s.popup().is_none() && s.contains("The quick brown fox")
    });
    assert_eq!(screen.underline_of("The"), Some(Underline::None), "{}", screen.dump());

    let mark = editor.mark();
    editor.send(CTRL_S);
    editor.wait_raw("the title without the unsaved marker", mark, |out| {
        out.contains("\x1b]0;typo.txt - edit")
    });
    editor.send(CTRL_Q);
    assert!(editor.wait_exit().success());
    assert_eq!(fs::read_to_string(&file).unwrap(), "The quick brown fox\n");
}

#[test]
fn replace_with_f7() {
    replace_with_key(F7);
}

#[test]
fn replace_with_ctrl_t() {
    replace_with_key(CTRL_T);
}

#[test]
fn right_click() {
    let dir = TempDir::new();
    let (mut editor, _, screen) = open_typo(&dir);
    let (x, y) = screen.find("Teh").unwrap();
    let (qx, qy) = screen.find("quick").unwrap();

    // On a correct word: Only moves the cursor. The status bar shows the new position
    // in the same frame in which the popup would have opened.
    editor.right_click((qx + 2, qy));
    let column = qx + 2 - x + 1;
    let screen = editor.wait_screen("the cursor to move", |s| s.location() == Some((1, column)));
    assert!(screen.popup().is_none(), "{}", screen.dump());

    // On the misspelled word: Moves the cursor and opens the popup below the word.
    editor.right_click((x + 1, y));
    let screen = editor.wait_screen("the popup with \"The\"", |s| {
        s.popup().is_some_and(|p| p.items.iter().any(|i| i == "The"))
    });
    assert_eq!(screen.location(), Some((1, 2)), "{}", screen.dump());
    let popup = screen.popup().unwrap();
    assert_eq!((popup.left, popup.top), (x, y + 1), "{}", screen.dump());
}

#[test]
fn underline_plain() {
    let dir = TempDir::new();
    dir.write_settings(r#"{ "spellcheck.underline": "plain" }"#);
    let file = dir.write("typo.txt", TYPO_TXT);
    let editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("a plain underline on \"Teh\"", |s| {
        s.underline_of("Teh") == Some(Underline::Single)
    });
    assert_eq!(screen.underline_of("quick"), Some(Underline::None), "{}", screen.dump());

    let out = editor.raw();
    assert!(!out.contains("4:3m") && !out.contains("58:2"), "{out:?}");
    let before = escapes_before(&out, "Teh");
    assert!(before.iter().any(|e| e.ends_with("\x1b[4m")), "{before:?}");
}

// macOS Terminal can't draw the curly underline, so "auto" (the default) uses a plain one there.
#[test]
fn underline_auto_in_macos_terminal() {
    let dir = TempDir::new();
    let file = dir.write("typo.txt", TYPO_TXT);
    let env = [("TERM_PROGRAM", "Apple_Terminal")];
    let editor = Editor::spawn_with(&dir, &[&file], &env, TEST_TIMEOUT);

    editor.wait_screen("a plain underline on \"Teh\"", |s| {
        s.underline_of("Teh") == Some(Underline::Single)
    });
    let out = editor.raw();
    assert!(!out.contains("4:3m") && !out.contains("58:2"), "{out:?}");
    drop(editor);

    // An explicit "curly" wins.
    dir.write_settings(r#"{ "spellcheck.underline": "curly" }"#);
    let editor = Editor::spawn_with(&dir, &[&file], &env, TEST_TIMEOUT);
    editor.wait_screen("a curly underline on \"Teh\"", |s| s.is_curly("Teh"));
}

#[test]
fn disabled_in_settings() {
    let dir = TempDir::new();
    dir.write_settings(r#"{ "spellcheck.enabled": false }"#);
    let file = dir.write("typo.txt", TYPO_TXT);
    let mut editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("the text", |s| s.contains("Teh quick brown fox"));
    let (x, y) = screen.find("Teh").unwrap();

    // Give a (wrongly) loading dictionary plenty of time, then force a redraw of the row.
    thread::sleep(Duration::from_millis(1500));
    editor.send(RIGHT);
    let screen = editor.wait_screen("the cursor to move", |s| s.location() == Some((1, 2)));
    assert!(!screen.any_underline(y), "{}", screen.dump());
    assert!(!editor.raw().contains("4:3m"));

    // View > Spell Check is unchecked. Checking it starts loading the dictionary,
    // and the underline appears without further input.
    let view = screen.find("View").unwrap();
    editor.left_click(view);
    let screen = editor.wait_screen("the View menu", |s| s.contains("Spell Check"));
    let item = screen.find("Spell").unwrap();
    assert!(!screen.row_text(item.1).contains('◼'), "{}", screen.dump());
    editor.left_click(item);
    let screen = editor.wait_screen("a curly underline on \"Teh\"", |s| s.is_curly("Teh"));
    assert_eq!(screen.find("Teh"), Some((x, y)));

    // Now it's checked.
    editor.left_click(view);
    editor.wait_screen("a checked Spell Check item", |s| {
        s.find("Spell").is_some_and(|(_, y)| s.row_text(y).contains('◼'))
    });
}

// View > Spell Check is remembered, but an explicit "spellcheck.enabled" in settings.json wins.
#[test]
fn spell_check_is_remembered() {
    let dir = TempDir::new();
    let (mut editor, file, screen) = open_typo(&dir);
    editor.left_click(screen.find("View").unwrap());
    let screen = editor.wait_screen("the View menu", |s| s.contains("Spell Check"));
    editor.left_click(screen.find("Spell").unwrap());
    editor.wait_screen("no underline", |s| s.underline_of("Teh") == Some(Underline::None));
    assert_eq!(dir.read_config("view-state.json"), "{\n    \"spellCheck\": false\n}\n");
    drop(editor);

    // The next session starts with it off: the menu item is unchecked.
    let mut editor = Editor::spawn(&dir, &[&file]);
    let screen = editor.wait_screen("the text", |s| s.contains("Teh quick brown fox"));
    editor.left_click(screen.find("View").unwrap());
    let screen = editor.wait_screen("the View menu", |s| s.contains("Spell Check"));
    let item = screen.find("Spell").unwrap();
    assert!(!screen.row_text(item.1).contains('◼'), "{}", screen.dump());
    drop(editor);

    dir.write_settings(r#"{ "spellcheck.enabled": true }"#);
    let editor = Editor::spawn(&dir, &[&file]);
    editor.wait_screen("a curly underline on \"Teh\"", |s| s.is_curly("Teh"));
}

// Word wrap (Alt+Z) is remembered, and documents open with the last choice.
#[test]
fn word_wrap_is_remembered() {
    let dir = TempDir::new();
    let file = dir.write("long.txt", &format!("{}\nSecond line\n", "lorem ipsum ".repeat(12)));
    // Whether the long first line takes more than one row.
    let wrapped = |s: &Screen| Some(s.find("Second")?.1 > s.find("lorem")?.1 + 1);

    let mut editor = Editor::spawn(&dir, &[&file]);
    let screen = editor.wait_screen("the text", |s| s.contains("Second line"));
    assert_eq!(wrapped(&screen), Some(false), "{}", screen.dump());
    editor.send("\x1bz");
    editor.wait_screen("wrapped text", |s| wrapped(s) == Some(true));
    assert_eq!(dir.read_config("view-state.json"), "{\n    \"wordWrap\": true\n}\n");
    drop(editor);

    let mut editor = Editor::spawn(&dir, &[&file]);
    editor.wait_screen("wrapped text", |s| wrapped(s) == Some(true));
    editor.send("\x1bz");
    editor.wait_screen("unwrapped text", |s| wrapped(s) == Some(false));
    drop(editor);

    // View > Word Wrap is remembered, too.
    let mut editor = Editor::spawn(&dir, &[&file]);
    let screen = editor.wait_screen("unwrapped text", |s| wrapped(s) == Some(false));
    editor.left_click(screen.find("View").unwrap());
    let screen = editor.wait_screen("the View menu", |s| s.contains("Word Wrap"));
    editor.left_click(screen.find("Wrap").unwrap());
    editor.wait_screen("wrapped text", |s| wrapped(s) == Some(true));
    assert_eq!(dir.read_config("view-state.json"), "{\n    \"wordWrap\": true\n}\n");
}

// File > Reopen Files at Startup: starting without files reopens the last ones, with their cursors.
#[test]
fn reopen_files_at_startup() {
    let dir = TempDir::new();
    let alpha = dir.write("alpha.txt", "Alpha file\n");
    let bravo = dir.write("bravo.txt", "Bravo file\nsecond line here\n");
    let charlie = dir.write("charlie.txt", "Charlie file\n");
    let open_file_menu = |editor: &mut Editor| {
        let screen = editor.wait_screen("the menu bar", |s| s.contains("File"));
        editor.left_click(screen.find("File").unwrap());
        let screen = editor.wait_screen("the File menu", |s| s.contains("Reopen Files"));
        screen.find("Reopen").unwrap()
    };

    // Turn the option on, move the cursor, and quit.
    let mut editor = Editor::spawn(&dir, &[&alpha, &bravo]);
    editor.wait_screen("bravo.txt", |s| s.contains("Bravo file"));
    let item = open_file_menu(&mut editor);
    editor.left_click(item);
    editor.wait_screen("the menu to close", |s| !s.contains("Reopen Files"));
    assert!(dir.read_config("view-state.json").contains("\"reopenFiles\": true"));
    let session = dir.read_config("session.json");
    assert!(session.contains("alpha.txt") && session.contains("bravo.txt"), "{session}");
    editor.send(&format!("{DOWN}{RIGHT}{RIGHT}{RIGHT}"));
    editor.wait_screen("the cursor at 2:4", |s| s.location() == Some((2, 4)));
    editor.send("\x11"); // Ctrl+Q
    assert!(editor.wait_exit().success());

    // Both files come back, bravo.txt in front, with its cursor.
    let mut editor = Editor::spawn(&dir, &[]);
    editor.wait_screen("bravo.txt at 2:4", |s| {
        s.contains("Bravo file") && s.location() == Some((2, 4))
    });
    // Closing it is saved right away, even if the editor is killed (like closing the terminal).
    editor.send("\x17"); // Ctrl+W
    editor.wait_screen("alpha.txt", |s| s.contains("Alpha file"));
    drop(editor);
    let editor = Editor::spawn(&dir, &[]);
    editor.wait_screen("alpha.txt", |s| s.contains("Alpha file"));
    drop(editor);

    // Files on the command line win, and become the last session.
    let mut editor = Editor::spawn(&dir, &[&charlie]);
    let screen = editor.wait_screen("charlie.txt", |s| s.contains("Charlie file"));
    assert!(!screen.contains("Alpha file"), "{}", screen.dump());

    // Turning the option off forgets the files.
    let item = open_file_menu(&mut editor);
    editor.left_click(item);
    editor.wait_screen("the menu to close", |s| !s.contains("Reopen Files"));
    assert_eq!(dir.read_config("session.json"), "");
    drop(editor);
    let editor = Editor::spawn(&dir, &[]);
    let screen = editor.wait_screen("an untitled document", |s| s.contains("Untitled"));
    assert!(!screen.contains("Charlie file"), "{}", screen.dump());
}

#[test]
fn markdown_code_block() {
    let dir = TempDir::new();
    let file = dir.write(
        "notes.md",
        "# Notes\n\
         \n\
         Some prose with a speling mistake.\n\
         \n\
         ```rust\n\
         let recieve = wierd(1);\n\
         ```\n\
         \n\
         More prose, untill the end.\n",
    );
    let editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("curly underlines on the prose typos", |s| {
        s.is_curly("speling") && s.is_curly("untill")
    });
    assert_curly(&editor, "speling");
    // The code block isn't checked, even though its identifiers aren't words.
    let (_, code_row) = screen.find("recieve").unwrap();
    assert!(!screen.any_underline(code_row), "{}", screen.dump());
    for word in ["Notes", "prose", "mistake", "rust"] {
        assert_eq!(screen.underline_of(word), Some(Underline::None), "{word}\n{}", screen.dump());
    }
}

#[test]
fn markdown_yaml_block_scalar_before_prose() {
    let dir = TempDir::new();
    // The highlighter loses track of the fences after the YAML block scalar (it runs into the
    // closing fence). The checked lines must not depend on it.
    let file = dir.write(
        "notes.md",
        "# Notes\n\
         \n\
         ```yaml\n\
         jobs:\n\
         \x20 build:\n\
         \x20   steps:\n\
         \x20     - run: |\n\
         ```\n\
         \n\
         Prose after the fence with a recieve typo.\n\
         \n\
         ```rust\n\
         let x = wierd(1);\n\
         ```\n",
    );
    let editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("a curly underline on \"recieve\"", |s| s.is_curly("recieve"));
    assert_curly(&editor, "recieve");
    let (_, code_row) = screen.find("wierd").unwrap();
    assert!(!screen.any_underline(code_row), "{}", screen.dump());
    for word in ["jobs", "build", "steps", "run", "Prose", "typo"] {
        assert_eq!(screen.underline_of(word), Some(Underline::None), "{word}\n{}", screen.dump());
    }
}

#[test]
fn windows_paths_and_accents() {
    let dir = TempDir::new();
    let file = dir.write(
        "paths.txt",
        "Path: C:\\Users\\Admin\\Documnets\\Reprots\n\
         Also tools\\bild\\relese here.\n\
         A na\u{00EF}ve caf\u{00E9} owner wrote teh menu.\n",
    );
    let editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("a curly underline on \"teh\"", |s| s.is_curly("teh"));
    for word in ["Documnets", "Reprots", "bild", "relese", "na\u{00EF}ve", "caf\u{00E9}"] {
        assert_eq!(screen.underline_of(word), Some(Underline::None), "{word}\n{}", screen.dump());
    }
}

#[test]
fn right_click_while_find_bar_open() {
    let dir = TempDir::new();
    let (mut editor, _, _) = open_typo(&dir);

    // The Find bar opens above the text and moves it down.
    editor.send(CTRL_F);
    let screen = editor.wait_screen("the Find bar above the text", |s| {
        s.find("Find").zip(s.find("Teh")).is_some_and(|(find, teh)| find.1 < teh.1)
    });
    let (x, y) = screen.find("Teh").unwrap();

    // The right-click reaches the document, although the Find bar has the focus.
    editor.right_click((x + 1, y));
    let screen = editor.wait_screen("the popup with \"The\"", |s| {
        s.popup().is_some_and(|p| p.items.iter().any(|i| i == "The"))
    });
    assert_eq!(screen.location(), Some((1, 2)), "{}", screen.dump());
    let popup = screen.popup().unwrap();
    assert_eq!((popup.left, popup.top), (x, y + 1), "{}", screen.dump());
    assert!(screen.contains("Find:"), "{}", screen.dump());
}

#[test]
fn popup_closes_on_resize() {
    let dir = TempDir::new();
    let (mut editor, _, _) = open_typo(&dir);

    editor.send(F7);
    editor.wait_screen("the popup with \"The\"", |s| {
        s.popup().is_some_and(|p| p.items.iter().any(|i| i == "The"))
    });

    // Narrower, so that the full redraw at the new size covers every cell the popup was in.
    editor.resize(COLS - 10, ROWS);
    let screen = editor.wait_screen("the popup to close", |s| s.popup().is_none());
    assert!(screen.is_curly("Teh"), "{}", screen.dump());
}

#[test]
fn git_commit_message() {
    let dir = TempDir::new();
    let file = dir.write(
        "COMMIT_EDITMSG",
        "Fix teh widget\n\
         \n\
         The body has a mispeled word.\n\
         # Please enter the commit mesage for your changes.\n",
    );
    let editor = Editor::spawn(&dir, &[&file]);

    let screen = editor.wait_screen("curly underlines in the message", |s| {
        s.is_curly("teh") && s.is_curly("mispeled")
    });
    assert_eq!(screen.underline_of("mesage"), Some(Underline::None), "{}", screen.dump());
}

#[test]
fn invalid_setting_is_reported() {
    let dir = TempDir::new();
    dir.write_settings(r#"{ "spellcheck.underline": "wavy" }"#);
    let file = dir.write("typo.txt", TYPO_TXT);
    let editor = Editor::spawn(&dir, &[&file]);

    editor.wait_screen("the settings error", |s| s.contains("spellcheck.underline"));
}

// Measurement, not a test. Run it with the binary to measure, e.g.:
//   cargo test --release -p edit --test spellcheck_e2e -- --ignored --nocapture
//   cargo test --release -p edit --features debug-latency --test spellcheck_e2e -- --ignored --nocapture
// Opens a ~5 MB synthetic text file with spell checking on and off, pages through it,
// and reports the maximum RSS of the editor and, with `debug-latency`, its frame times.
#[test]
#[ignore]
fn measure_large_file() {
    let dir = TempDir::new();
    let file = dir.path().join("large.txt");
    fs::write(&file, synthetic_text(5 * 1024 * 1024)).unwrap();
    let size = fs::metadata(&file).unwrap().len();
    println!("file: {size} bytes, binary: {}", env!("CARGO_BIN_EXE_edit"));

    // Alternating, so that neither setting benefits from running later (e.g. the first launch
    // of a freshly built binary can be slow on macOS).
    for (round, enabled) in [false, true, false, true, false, true].into_iter().enumerate() {
        dir.write_settings(&format!(r#"{{ "spellcheck.enabled": {enabled} }}"#));
        let start = Instant::now();
        let mut editor = Editor::spawn_with(&dir, &[&file], &[], Duration::from_secs(120));
        editor.wait_screen("the text", |s| s.contains("lorem"));
        let first_frame = start.elapsed();
        if enabled {
            editor.wait_screen("an underline", |s| {
                (1..ROWS as usize - 2).any(|y| s.any_underline(y))
            });
        }
        let underlined = start.elapsed();

        // 100 pages down, one batch each, then to the end of the file.
        let mark = editor.mark();
        for _ in 0..100 {
            let mark = editor.mark();
            editor.send("\x1b[6~");
            editor.wait_raw("a page down", mark, |out| out.contains('\x1b'));
        }
        editor.send("\x1b[1;5F");
        let lines = fs::read_to_string(&file).unwrap().lines().count();
        editor.wait_screen("the end of the file", |s| s.location().is_some_and(|(l, _)| l > lines));
        let raw = editor.raw();
        let latencies = frame_latencies(raw.get(mark..).unwrap_or(&raw));

        editor.send(CTRL_Q);
        let (status, usage) = editor.wait_exit_rusage();
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "status {status}");
        // Bytes on macOS, KiB elsewhere.
        let max_rss = if cfg!(any(target_os = "macos", target_os = "ios")) {
            usage.ru_maxrss as f64
        } else {
            usage.ru_maxrss as f64 * 1024.0
        };

        print!(
            "round {}, spellcheck {}: max RSS {:.1} MiB, first frame {:?}, first underline {:?}",
            round / 2 + 1,
            if enabled { "on " } else { "off" },
            max_rss / 1024.0 / 1024.0,
            first_frame,
            underlined,
        );
        if latencies.is_empty() {
            println!(" (build with --features debug-latency for frame times)");
        } else {
            let mut sorted = latencies.clone();
            sorted.sort_by(f64::total_cmp);
            println!(
                ", {} frames: median {:.0} us, p90 {:.0} us, max {:.0} us",
                sorted.len(),
                sorted[sorted.len() / 2],
                sorted[sorted.len() * 9 / 10],
                sorted[sorted.len() - 1]
            );
        }
    }
}

/// Lorem-ipsum-like English text with about 2% typos, in lines of up to 72 columns.
fn synthetic_text(len: usize) -> String {
    const WORDS: &[&str] = &[
        "lorem",
        "ipsum",
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "and",
        "then",
        "some",
        "more",
        "words",
        "follow",
        "while",
        "nothing",
        "really",
        "happens",
        "here",
        "because",
        "this",
        "text",
        "is",
        "only",
        "meant",
        "to",
        "fill",
        "space",
        "with",
        "plausible",
        "sentences",
        "about",
        "editors",
        "terminals",
        "spelling",
        "dictionaries",
        "underlines",
        "suggestions",
        "keyboard",
        "shortcuts",
        "menus",
        "settings",
        "files",
    ];
    const TYPOS: &[&str] = &["teh", "recieve", "speling", "wierd", "untill", "seperate"];

    let mut state = 1442695040888963407u64;
    let mut rng = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };

    let mut text = String::with_capacity(len + 128);
    let mut line_len = 0;
    while text.len() < len {
        let word =
            if rng() % 50 == 0 { TYPOS[rng() % TYPOS.len()] } else { WORDS[rng() % WORDS.len()] };
        if line_len + 1 + word.len() > 72 {
            text.push_str(".\n");
            line_len = 0;
        } else if line_len > 0 {
            text.push(' ');
            line_len += 1;
        }
        text.push_str(word);
        line_len += word.len();
    }
    text.push('\n');
    text
}

/// Parses the `debug-latency` status ("{passes}P {bytes}B {time}μs") out of the output.
fn frame_latencies(out: &str) -> Vec<f64> {
    out.match_indices("μs")
        .filter_map(|(i, _)| {
            let beg = out[..i].rfind(' ')? + 1;
            out[beg..i].parse::<f64>().ok()
        })
        .collect()
}
