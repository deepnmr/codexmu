use crate::{
    Result,
    accounts::{Update, Usage, now},
};
use crossterm::{cursor, execute, terminal};
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::mpsc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
pub struct Status {
    cwd: String,
    git: String,
    name: String,
    email: String,
    plan: String,
    usage: BTreeMap<String, Usage>,
    notice: Option<(String, Instant)>,
}

impl Status {
    fn update(&mut self, update: Update) {
        match update {
            Update::Active { name, email, plan } => {
                self.name = name;
                self.email = email;
                self.plan = plan;
            }
            Update::Usage { name, usage } => {
                self.usage.insert(name, usage);
            }
            Update::RateLimits { name, limits } => {
                let usage = self.usage.entry(name).or_default();
                for (current, update) in [
                    (&mut usage.rate_limit.primary_window, limits.primary_window),
                    (
                        &mut usage.rate_limit.secondary_window,
                        limits.secondary_window,
                    ),
                ] {
                    if let Some(mut window) = update {
                        // Native notifications are sparse; absent metadata does not clear it.
                        window.reset_at = window
                            .reset_at
                            .or_else(|| current.as_ref().and_then(|w| w.reset_at));
                        *current = Some(window);
                    }
                }
            }
            Update::Notice(message) => self.notice = Some((message, Instant::now())),
            Update::Session { cwd } => self.cwd = cwd,
        }
    }

    fn header(&self, model: &str, width: usize) -> String {
        let quota = self
            .usage
            .get(&self.name)
            .and_then(|u| u.rate_limit.primary_window.as_ref())
            .filter(|w| {
                w.used_percent.is_finite()
                    && (0.0..=100.0).contains(&w.used_percent)
                    && w.reset_at.is_none_or(|t| t > now())
            })
            .map(|w| {
                let reset = w
                    .reset_at
                    .map(|t| {
                        let minutes = (t - now()).max(0) / 60;
                        format!(" · {}h{:02}m", minutes / 60, minutes % 60)
                    })
                    .unwrap_or_default();
                format!("5h {:.0}%{reset}", 100.0 - w.used_percent)
            })
            .unwrap_or_else(|| "5h —".to_owned());
        let account = if self.email.is_empty() {
            "connecting…".to_owned()
        } else {
            format!("{} ({})", self.email, self.plan)
        };
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(20))
            .map(|(message, _)| clip(message, 40))
            .unwrap_or_default();
        let mut segments = vec![
            ("codexmu".to_owned(), 111),
            (notice, 222),
            (clip(model, 27), 116),
            (clip_path(&self.cwd, (width / 5).clamp(10, 36)), 222),
            (clip(&self.git, 20), 111),
            (quota, 116),
            (account, 116),
        ];
        if width < 100 {
            segments.remove(3);
        }
        if width < 75 {
            segments.retain(|(s, _)| s != &self.git);
        }
        let mut result = String::new();
        let mut left = width.saturating_sub(2);
        for (segment, color) in segments.into_iter().filter(|(s, _)| !s.is_empty()) {
            if left < 4 {
                break;
            }
            if !result.is_empty() {
                result.push_str("\x1b[38;5;240m │ ");
                left = left.saturating_sub(3);
            }
            let segment = clip(&segment, left);
            left = left.saturating_sub(segment.width());
            result.push_str(&format!("\x1b[38;5;{color}m{segment}"));
        }
        format!(" {result}\x1b[0m")
    }

    fn footer(&self, native: &str, width: usize) -> String {
        let mut parts: Vec<String> = native
            .trim()
            .split('·')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        // The native status line puts model-with-reasoning immediately after context.
        // It follows local /model changes and the selected thread before a turn starts.
        let model = if parts.len() > 1 {
            parts.remove(1)
        } else {
            String::new()
        };
        // Both quota displays use the current account's latest query or live update.
        parts.retain(|s| !s.starts_with("5h") && !s.starts_with("weekly"));
        if let Some(usage) = self.usage.get(&self.name) {
            for (label, window) in [
                ("5h", &usage.rate_limit.primary_window),
                ("weekly", &usage.rate_limit.secondary_window),
            ] {
                if let Some(window) = window
                    && window.used_percent.is_finite()
                    && (0.0..=100.0).contains(&window.used_percent)
                    && window.reset_at.is_none_or(|t| t > now())
                {
                    parts.insert(
                        parts.len().saturating_sub(1),
                        format!("{label} {:.0}%", 100.0 - window.used_percent),
                    );
                }
            }
        }
        let native_width: usize = parts
            .iter()
            .map(|p| p.width() + 3)
            .sum::<usize>()
            .saturating_sub(3);
        let header = self.header(&model, width.saturating_sub(native_width + 3).max(40));
        let mut remaining = width.saturating_sub(visible_width(&header) + 3);
        let mut result = format!("{header}   ");
        for (i, part) in parts.iter().enumerate() {
            if remaining < 4 {
                break;
            }
            if i > 0 {
                result.push_str("\x1b[38;5;240m · ");
                remaining = remaining.saturating_sub(3);
            }
            let color = if part.starts_with("Context") {
                216
            } else if part.starts_with("Fast") {
                183
            } else if part.starts_with("5h") || part.starts_with("weekly") {
                211
            } else {
                246
            };
            let part = clip(part, remaining);
            remaining = remaining.saturating_sub(part.width());
            result.push_str(&format!("\x1b[38;5;{color}m{part}"));
        }
        result.push_str("\x1b[0m");
        result
    }
}

fn visible_width(value: &str) -> usize {
    let mut escape = false;
    value
        .chars()
        .filter(|&c| {
            if c == '\x1b' {
                escape = true;
            }
            let keep = !escape;
            if escape && c == 'm' {
                escape = false;
            }
            keep
        })
        .collect::<String>()
        .width()
}

fn clip(value: &str, width: usize) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    if clean.width() <= width {
        return clean;
    }
    let mut used = 0;
    let mut result = String::new();
    for c in clean.chars() {
        used += c.width().unwrap_or(0);
        if used >= width {
            break;
        }
        result.push(c);
    }
    if width > 0 {
        result.push('…');
    }
    result
}

fn clip_path(value: &str, width: usize) -> String {
    if value.width() <= width {
        return clip(value, width);
    }
    let name = std::path::Path::new(value)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    clip(&format!("…/{name}"), width)
}

// The emulator answers terminal probes in its own coordinates, including split escape sequences.
#[derive(Default)]
struct Callbacks {
    replies: Vec<u8>,
    effects: Vec<u8>,
}
impl vt100::Callbacks for Callbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        let first_param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (first, command, first_param) {
            (None, 'n', 6) => {
                let (r, c) = screen.cursor_position();
                self.replies
                    .extend(format!("\x1b[{};{}R", r + 1, c + 1).bytes());
            }
            (None, 'n', 5) => self.replies.extend(b"\x1b[0n"),
            (Some(b'?'), 'u', _) => self.replies.extend(b"\x1b[?0u"),
            (None, 'c', _) => self.replies.extend(b"\x1b[?1;2c"),
            _ => {}
        }
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"10", b"?"] => self.replies.extend(b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
            [b"11", b"?"] => self.replies.extend(b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            _ => {}
        }
    }
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        if ty.iter().all(u8::is_ascii_alphanumeric)
            && data
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(b))
        {
            self.effects.extend(b"\x1b]52;");
            self.effects.extend(ty);
            self.effects.push(b';');
            self.effects.extend(data);
            self.effects.extend(b"\x1b\\");
        }
    }
}

// Screen mirror with its own scrollback. Codex inserts history through a scroll region pinned to
// the top row, which vt100 discards, so rows are captured just before each scroll drops them.
struct Mirror {
    parser: vt100::Parser<Callbacks>,
    region: (u16, u16),
    pending: Vec<u8>,
    history: VecDeque<String>,
    offset: usize,
}

impl Mirror {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, Callbacks::default()),
            region: (0, rows.saturating_sub(1)),
            pending: Vec::new(),
            history: VecDeque::new(),
            offset: 0,
        }
    }

    fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.region = (0, rows.saturating_sub(1));
    }

    fn feed(&mut self, chunk: &[u8]) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let rows = self.screen().size().0;
        let (mut start, mut i) = (0, 0);
        while i < bytes.len() {
            match bytes[i] {
                b'\n' | 0x0b | 0x0c => {
                    self.parser.process(&bytes[start..i]);
                    self.line_feed();
                    start = i;
                    i += 1;
                }
                0x1b => {
                    let Some(&next) = bytes.get(i + 1) else { break };
                    match next {
                        b'[' => {
                            let Some(end) = bytes[i + 2..]
                                .iter()
                                .position(|b| (0x40..=0x7e).contains(b))
                            else {
                                break;
                            };
                            let end = i + 2 + end;
                            let params = csi_params(&bytes[i + 2..end]);
                            match bytes[end] {
                                b'S' => {
                                    self.parser.process(&bytes[start..i]);
                                    self.scroll_up(params.first().copied().unwrap_or(1).max(1));
                                    start = i;
                                }
                                b'r' => {
                                    self.parser.process(&bytes[start..=end]);
                                    start = end + 1;
                                    let top = params.first().copied().unwrap_or(1).max(1);
                                    let bottom = params
                                        .get(1)
                                        .copied()
                                        .unwrap_or(rows as usize)
                                        .clamp(1, rows.max(1) as usize);
                                    self.region = ((top - 1) as u16, (bottom - 1) as u16);
                                }
                                _ => {}
                            }
                            i = end + 1;
                        }
                        b'D' | b'E' => {
                            self.parser.process(&bytes[start..i]);
                            self.line_feed();
                            start = i;
                            i += 2;
                        }
                        _ => i += 2,
                    }
                }
                _ => i += 1,
            }
        }
        self.parser.process(&bytes[start..i]);
        self.pending = bytes[i..].to_vec();
    }

    fn line_feed(&mut self) {
        if self.screen().cursor_position().0 == self.region.1 {
            self.capture(1);
        }
    }

    fn scroll_up(&mut self, count: usize) {
        self.capture(count.min(self.region.1 as usize + 1));
    }

    fn capture(&mut self, count: usize) {
        let screen = self.screen();
        if screen.alternate_screen() || self.region.0 != 0 {
            return;
        }
        let cols = screen.size().1;
        let lines: Vec<String> = (0..count)
            .map(|row| render_row(screen, row as u16, cols))
            .collect();
        for line in lines {
            if self.history.is_empty() && visible_width(&line) == 0 {
                continue;
            }
            self.history.push_back(line);
        }
        while self.history.len() > 10_000 {
            self.history.pop_front();
        }
        if self.offset > 0 {
            self.offset = (self.offset + count).min(self.history.len());
        }
    }

    fn scroll(&mut self, delta: isize) {
        self.offset = (self.offset as isize + delta).clamp(0, self.history.len() as isize) as usize;
    }
}

fn csi_params(bytes: &[u8]) -> Vec<usize> {
    bytes
        .split(|b| *b == b';')
        .map(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse().ok()))
        .map(|p| p.unwrap_or(0))
        .collect()
}

// Terminals send wheel motion on the alternate screen as bursts of arrow keys. A burst scrolls the
// mirror, a lone arrow reaches Codex after a short hold, and any other key jumps back to the live view.
const HOLD: Duration = Duration::from_millis(20);
const SEQUENCES: [&[u8]; 8] = [
    b"\x1b[A",
    b"\x1b[B",
    b"\x1bOA",
    b"\x1bOB",
    b"\x1b[5~",
    b"\x1b[6~",
    b"\x1b[200~",
    b"\x1b[201~",
];

#[derive(Default)]
struct Input {
    pending: Vec<u8>,
    pending_at: Option<Instant>,
    held: Vec<u8>,
    held_up: bool,
    held_at: Option<Instant>,
    pasting: bool,
}

impl Input {
    // Returns bytes for Codex and lines to scroll (positive scrolls into history).
    fn feed(
        &mut self,
        chunk: &[u8],
        now: Instant,
        scrolled: bool,
        native: bool,
        page: isize,
    ) -> (Vec<u8>, isize) {
        let mut bytes = std::mem::take(&mut self.pending);
        self.pending_at = None;
        bytes.extend_from_slice(chunk);
        let mut forward = Vec::new();
        let mut scroll = 0;
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if self.pasting {
                let found = rest.windows(6).position(|w| w == b"\x1b[201~");
                let end = found.map_or(rest.len(), |p| p + 6);
                self.pasting = found.is_none();
                forward.extend_from_slice(&rest[..end]);
                i += end;
                continue;
            }
            if rest[0] != 0x1b {
                forward.extend(self.release());
                forward.push(rest[0]);
                i += 1;
                continue;
            }
            let Some(sequence) = SEQUENCES.iter().find(|s| rest.starts_with(s)) else {
                if rest.len() < 6 && SEQUENCES.iter().any(|s| s.starts_with(rest)) {
                    self.pending = rest.to_vec();
                    self.pending_at = Some(now);
                    break;
                }
                forward.extend(self.release());
                forward.push(0x1b);
                i += 1;
                continue;
            };
            i += sequence.len();
            let arrow = match *sequence {
                b"\x1b[200~" => {
                    self.pasting = true;
                    forward.extend(self.release());
                    forward.extend_from_slice(sequence);
                    continue;
                }
                b"\x1b[5~" if !native => {
                    scroll += page;
                    continue;
                }
                b"\x1b[6~" if !native && scrolled => {
                    scroll -= page;
                    continue;
                }
                b"\x1b[A" | b"\x1bOA" => Some(true),
                b"\x1b[B" | b"\x1bOB" => Some(false),
                _ => None,
            };
            let Some(up) = arrow.filter(|_| !native) else {
                forward.extend(self.release());
                forward.extend_from_slice(sequence);
                continue;
            };
            let step = if up { 1 } else { -1 };
            if scrolled || scroll != 0 {
                scroll += step;
                continue;
            }
            match self.held_at {
                Some(at) if self.held_up == up && now.duration_since(at) <= HOLD => {
                    scroll += step * (self.held.len() / 3 + 1) as isize;
                    self.held.clear();
                    self.held_at = None;
                }
                _ => {
                    forward.extend(self.release());
                    self.held = sequence.to_vec();
                    self.held_up = up;
                    self.held_at = Some(now);
                }
            }
        }
        (forward, scroll)
    }

    fn release(&mut self) -> Vec<u8> {
        self.held_at = None;
        std::mem::take(&mut self.held)
    }

    fn deadline(&self) -> Option<Instant> {
        self.held_at
            .into_iter()
            .chain(self.pending_at)
            .min()
            .map(|at| at + HOLD)
    }

    fn expire(&mut self, now: Instant) -> Vec<u8> {
        let mut forward = Vec::new();
        if self
            .held_at
            .is_some_and(|at| now.duration_since(at) >= HOLD)
        {
            forward.extend(self.release());
        }
        if self
            .pending_at
            .is_some_and(|at| now.duration_since(at) >= HOLD)
        {
            self.pending_at = None;
            forward.extend(std::mem::take(&mut self.pending));
        }
        forward
    }
}

pub struct View {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    mirror: Mirror,
    keys: Input,
    native: String,
    previous: Vec<String>,
    status: Status,
    updates: mpsc::UnboundedReceiver<Update>,
    input: tokio::io::Stdin,
    size: (u16, u16),
}

impl View {
    pub fn start(
        command: &std::process::Command,
        updates: mpsc::UnboundedReceiver<Update>,
    ) -> Result<Self> {
        let (cols, rows) = terminal::size()?;
        let size = PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(|e| e.to_string())?;
        let mut native = CommandBuilder::new(command.get_program());
        native.args(command.get_args());
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => native.env(key, value),
                None => native.env_remove(key),
            }
        }
        native.cwd(
            command
                .get_current_dir()
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?),
        );
        native.env("TERM", "xterm-256color");
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let child = pair
            .slave
            .spawn_command(native)
            .map_err(|e| e.to_string())?;
        drop(pair.slave);
        let (tx, output) = mpsc::channel(16);
        std::thread::spawn(move || {
            let mut bytes = [0; 16384];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 || tx.blocking_send(bytes[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            master: pair.master,
            child,
            writer,
            output,
            mirror: Mirror::new(size.rows, size.cols),
            keys: Input::default(),
            native: String::new(),
            previous: Vec::new(),
            status: Status {
                cwd,
                ..Status::default()
            },
            updates,
            input: tokio::io::stdin(),
            size: (cols, rows),
        };
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide
        )?;
        // Application cursor keys: Terminal.app turns wheel motion into arrow keys only in this mode.
        std::io::stdout().write_all(b"\x1b[?1h\x1b[?1007h")?;
        view.draw()?;
        Ok(view)
    }

    pub async fn run(&mut self) -> Result<u8> {
        let mut bytes = [0; 4096];
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut git_tick = tokio::time::interval(Duration::from_secs(15));
        let mut git_job = None;
        let mut dirty = true;
        let mut last_draw = Instant::now();
        loop {
            tokio::select! {
                n = self.input.read(&mut bytes) => {
                    let n = n?;
                    if n == 0 { return Ok(0); }
                    let screen = self.mirror.screen();
                    let (native, page) = (screen.alternate_screen(), screen.size().0 as isize - 1);
                    let (forward, scroll) = self.keys.feed(&bytes[..n], Instant::now(), self.mirror.offset > 0, native, page);
                    self.mirror.scroll(scroll);
                    self.forward(&forward)?;
                    dirty = true;
                }
                _ = tokio::time::sleep_until(self.keys.deadline().unwrap_or(Instant::now()).into()), if self.keys.deadline().is_some() => {
                    let forward = self.keys.expire(Instant::now());
                    self.forward(&forward)?;
                }
                output = self.output.recv() => match output {
                    Some(bytes) => {
                        dirty = true;
                        self.mirror.feed(&bytes);
                        let callback = self.mirror.parser.callbacks_mut();
                        self.writer.write_all(&std::mem::take(&mut callback.replies))?;
                        self.writer.flush()?;
                    }
                    None => return Ok(self.child.wait()?.exit_code() as u8),
                },
                Some(update) = self.updates.recv() => { self.status.update(update); dirty = true; },
                _ = tick.tick() => {
                    if let Some(status) = self.child.try_wait()? { return Ok(status.exit_code() as u8); }
                    let size = terminal::size()?;
                    if size != self.size {
                        self.size = size;
                        let (cols, rows) = size;
                        self.master.resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }).map_err(|e| e.to_string())?;
                        self.mirror.resize(rows.max(1), cols.max(1));
                        self.previous.clear();
                        dirty = true;
                    }
                    if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
                        self.draw()?;
                        dirty = false;
                        last_draw = Instant::now();
                    }
                }
                _ = git_tick.tick(), if git_job.is_none() => {
                    let cwd = self.status.cwd.clone();
                    git_job = Some(tokio::spawn(async move { let git = git_status(&cwd).await; (cwd, git) }));
                }
                result = async { git_job.as_mut().expect("guarded git job").await }, if git_job.is_some() => {
                    git_job = None;
                    if let Ok((cwd, git)) = result && cwd == self.status.cwd { self.status.git = git; dirty = true; }
                }
            }
        }
    }

    fn forward(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.mirror.offset = 0;
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let screen = self.mirror.screen();
        let (rows, cols) = screen.size();
        let (rows_u, cols_u) = (rows as usize, cols as usize);
        let alt = screen.alternate_screen();
        let text: Vec<String> = screen.rows(0, cols).collect();
        let context_row = (!alt)
            .then(|| {
                (0..rows_u)
                    .rev()
                    .find(|row| text[*row].trim_start().starts_with("Context "))
            })
            .flatten();
        if let Some(row) = context_row {
            self.native = text[row].clone();
        }
        let offset = if alt {
            0
        } else {
            self.mirror.offset.min(self.mirror.history.len())
        };
        let mut lines: Vec<String> = self
            .mirror
            .history
            .iter()
            .skip(self.mirror.history.len() - offset)
            .take(rows_u)
            .cloned()
            .collect();
        let live = rows_u - lines.len();
        lines.extend((0..live).map(|row| render_row(screen, row as u16, cols)));
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut cursor_row = cursor_row as usize;
        if !alt {
            match context_row.filter(|_| offset == 0) {
                Some(row) => {
                    lines.remove(row);
                    if cursor_row > row {
                        cursor_row -= 1;
                    }
                }
                None => {
                    lines.pop();
                }
            }
            lines.push(self.status.footer(&self.native, cols_u));
        }
        let mut frame = Vec::new();
        frame.extend(b"\x1b[?2026h\x1b[?25l\x1b[?7l");
        frame.extend(std::mem::take(
            &mut self.mirror.parser.callbacks_mut().effects,
        ));
        for (row, line) in lines.iter().enumerate().take(self.size.1 as usize) {
            if self.previous.get(row) != Some(line) {
                write!(frame, "\x1b[{};1H\x1b[0m\x1b[2K{line}", row + 1)?;
            }
        }
        let screen = self.mirror.screen();
        write!(
            frame,
            "\x1b[0m\x1b[{};{}H\x1b[?7h\x1b[?2004{}\x1b[?25{}\x1b[?2026l",
            (cursor_row + 1).min(self.size.1 as usize),
            cursor_col + 1,
            if screen.bracketed_paste() { 'h' } else { 'l' },
            if screen.hide_cursor() || offset > 0 {
                'l'
            } else {
                'h'
            }
        )?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&frame)?;
        stdout.flush()?;
        self.previous = lines;
        Ok(())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?1l\x1b[?2004l\x1b[?2026l\x1b[?7h");
        let _ = execute!(
            std::io::stdout(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn render_row(screen: &vt100::Screen, row: u16, cols: u16) -> String {
    let mut result = String::new();
    let mut previous = String::new();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let mut style = "\x1b[0".to_owned();
        for (enabled, code) in [
            (cell.bold(), 1),
            (cell.dim(), 2),
            (cell.italic(), 3),
            (cell.underline(), 4),
            (cell.inverse(), 7),
        ] {
            if enabled {
                style.push_str(&format!(";{code}"));
            }
        }
        for (color, code) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
            match color {
                vt100::Color::Default => {}
                vt100::Color::Idx(n) => style.push_str(&format!(";{code};5;{n}")),
                vt100::Color::Rgb(r, g, b) => style.push_str(&format!(";{code};2;{r};{g};{b}")),
            }
        }
        style.push('m');
        if style != previous {
            result.push_str(&style);
            previous = style;
        }
        result.push_str(if cell.has_contents() {
            cell.contents()
        } else {
            " "
        });
    }
    result
}

async fn git_status(cwd: &str) -> String {
    let command = tokio::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "-C",
            cwd,
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(3), command).await else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = String::new();
    let mut changed = 0;
    let mut untracked = 0;
    let mut tracking = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("# branch.head ") {
            branch = name.to_owned();
        }
        if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split_whitespace() {
                if let Ok(n) = count.parse::<i64>()
                    && n != 0
                {
                    tracking.push_str(&format!(
                        " {}{}",
                        if n > 0 { '↑' } else { '↓' },
                        n.unsigned_abs()
                    ));
                }
            }
        }
        if line.starts_with(['1', '2', 'u']) {
            changed += 1;
        }
        if line.starts_with("? ") {
            untracked += 1;
        }
    }
    branch.push_str(&tracking);
    if changed > 0 {
        branch.push_str(&format!(" +{changed}"));
    }
    if untracked > 0 {
        branch.push_str(&format!(" ?{untracked}"));
    }
    branch
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn footer_tracks_native_model_and_effort_before_the_next_turn() {
        let state = Status::default();
        for model in ["gpt-6-astra high", "gpt-5.1 low", "gpt-5.1 medium"] {
            let native = format!("Context 87% left · {model} · Fast off · 0.153.4");
            let line = state.footer(&native, 200);
            assert!(
                line.find(model).unwrap() < line.find("Context 87% left").unwrap(),
                "the model and effort must appear in the codexmu header: {line}"
            );
            assert_eq!(line.matches(model).count(), 1);
            assert!(visible_width(&line) <= 200);
            assert!(state.footer(&native, 72).contains(model));
        }
    }

    #[test]
    fn footer_keeps_quota_bound_to_the_active_account() {
        let mut state = Status::default();
        state.update(Update::Active {
            name: "b".into(),
            email: "b@example.test".into(),
            plan: "plus".into(),
        });
        state.update(Update::Usage {
            name: "b".into(),
            usage: serde_json::from_value(serde_json::json!({
                "rate_limit": {"primary_window": {"used_percent": 15, "reset_at": now() + 600}}
            }))
            .unwrap(),
        });
        let line = state.footer(
            "Context 87% left · gpt-5.1 medium · 5h 90% · weekly 90% · 0.153.4",
            240,
        );
        assert!(line.contains("5h 85%"));
        assert!(
            !line.contains("90%"),
            "stale native quota leaked into the footer: {line}"
        );
        state.update(Update::RateLimits {
            name: "b".into(),
            limits: serde_json::from_value(serde_json::json!({
                "primary": {"usedPercent": 27, "resetsAt": null}
            }))
            .unwrap(),
        });
        let line = state.footer("Context 87% left · gpt-5.1 medium · 5h 85% · 0.153.4", 240);
        assert_eq!(line.matches("5h 73%").count(), 2);
        assert!(!line.contains("85%"));
        assert!(
            line.contains(" · 0h"),
            "a sparse update must retain the known reset"
        );
    }

    #[test]
    fn header_tracks_only_active_quota_and_clips_terminal_controls() {
        let mut state = Status::default();
        state.update(Update::Usage { name: "a".into(), usage: serde_json::from_value(serde_json::json!({"rate_limit":{"primary_window":{"used_percent":100,"reset_at":now()+600}}})).unwrap() });
        state.update(Update::Active {
            name: "b".into(),
            email: "b@example.test".into(),
            plan: "plus".into(),
        });
        assert!(state.header("", 120).contains("5h —"));
        assert!(state.header("", 120).contains("b@example.test"));
        let line = state.footer(
            "  Context 87% left · gpt-5.1 medium · Fast off · 0.153.4",
            200,
        );
        assert!(line.contains("codexmu") && line.contains("b@example.test"));
        assert!(line.contains("Context 87% left") && line.contains("0.153.4"));
        assert!(visible_width(&line) <= 200);
        assert_eq!(visible_width("\x1b[38;5;111mab\x1b[0m"), 2);
        assert_eq!(clip("ab\x1b\ncd", 3), "ab…");
        assert!(clip("한국어", 4).width() <= 4);
        state.update(Update::Notice("switched a -> b".into()));
        assert!(
            state
                .footer("Context 50% left", 200)
                .contains("switched a -> b")
        );
    }

    #[test]
    fn wheel_bursts_scroll_while_lone_arrows_reach_codex() {
        let mut keys = Input::default();
        let t0 = Instant::now();
        // A wheel notch arrives as three arrows within a millisecond: scroll, forward nothing.
        assert_eq!(
            keys.feed(b"\x1bOA\x1bOA\x1bOA", t0, false, false, 10),
            (vec![], 3)
        );
        // A lone arrow is held, then released to Codex once the hold window passes.
        assert_eq!(keys.feed(b"\x1bOA", t0, false, false, 10), (vec![], 0));
        assert!(keys.expire(t0 + Duration::from_millis(5)).is_empty());
        assert_eq!(keys.expire(t0 + HOLD), b"\x1bOA");
        // A second same-direction arrow inside the window turns the held one into a scroll.
        assert_eq!(keys.feed(b"\x1b[B", t0, false, false, 10), (vec![], 0));
        assert_eq!(
            keys.feed(b"\x1b[B", t0 + Duration::from_millis(10), false, false, 10),
            (vec![], -2)
        );
        // Other keys release the held arrow first and are forwarded in order.
        assert_eq!(keys.feed(b"\x1bOA", t0, false, false, 10), (vec![], 0));
        assert_eq!(
            keys.feed(b"x", t0, false, false, 10),
            (b"\x1bOAx".to_vec(), 0)
        );
        // While scrolled, arrows and paging scroll immediately; pasted arrows pass through.
        assert_eq!(
            keys.feed(b"\x1bOB\x1b[6~", t0, true, false, 10),
            (vec![], -11)
        );
        assert_eq!(
            keys.feed(b"\x1b[200~\x1bOA\x1bOA\x1b[201~", t0, false, false, 10)
                .0,
            b"\x1b[200~\x1bOA\x1bOA\x1b[201~"
        );
        // Codex's own alternate screen (transcript view) gets every key untouched.
        assert_eq!(
            keys.feed(b"\x1bOA\x1bOA", t0, false, true, 10),
            (b"\x1bOA\x1bOA".to_vec(), 0)
        );
        // A split sequence waits for its tail; a lone Escape is released after the window.
        assert_eq!(keys.feed(b"\x1bO", t0, false, false, 10), (vec![], 0));
        assert_eq!(keys.feed(b"A\x1bOA", t0, false, false, 10), (vec![], 2));
        assert_eq!(keys.feed(b"\x1b", t0, false, false, 10), (vec![], 0));
        assert_eq!(keys.expire(t0 + HOLD), b"\x1b");
    }

    #[test]
    fn mirror_keeps_rows_dropped_by_top_pinned_scroll_regions() {
        fn plain(line: &str) -> String {
            let mut out = String::new();
            let mut escape = false;
            for c in line.chars() {
                if c == '\x1b' {
                    escape = true;
                }
                if !escape {
                    out.push(c);
                }
                if escape && c == 'm' {
                    escape = false;
                }
            }
            out.trim_end().to_owned()
        }
        let mut mirror = Mirror::new(6, 20);
        mirror.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
        // Codex inserts history: pin rows 1-4, newline at the region bottom, then scroll up twice.
        mirror.feed(b"\x1b[1;4r\x1b[4;1H\r\nseven\x1b[2S\x1b[r");
        let history: Vec<String> = mirror.history.iter().map(|l| plain(l)).collect();
        assert_eq!(history, ["one", "two", "three"]);
        assert_eq!(
            mirror.screen().rows(0, 20).nth(5).unwrap().trim_end(),
            "six"
        );
        // A split escape sequence and a scroll inside a region that is not pinned to the top are ignored.
        mirror.feed(b"\x1b[3;6r\x1b[6;1H\x1b");
        mirror.feed(b"D");
        assert_eq!(mirror.history.len(), 3);
        // The scrolled view stays anchored while new rows keep arriving.
        mirror.scroll(3);
        assert_eq!(mirror.offset, 3);
        mirror.feed(b"\x1b[r\x1b[6;1H\n");
        assert_eq!((mirror.history.len(), mirror.offset), (4, 4));
        // Terminal probes are answered in the mirror's coordinates, even when split across chunks.
        let mut parser = vt100::Parser::new_with_callbacks(20, 80, 0, Callbacks::default());
        parser.process(b"\x1b[3;4H\x1b[");
        parser.process(b"6n");
        assert_eq!(parser.callbacks().replies, b"\x1b[3;4R");
        // A switch notice becomes a status line segment.
        let mut state = Status::default();
        state.update(Update::Notice("switched a -> b".into()));
        assert!(
            state
                .footer("Context 50% left", 200)
                .contains("switched a -> b")
        );
    }
}
