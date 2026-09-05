use crate::{
    Result,
    accounts::{Update, Usage, now},
};
use crossterm::{cursor, execute, terminal};
use portable_pty::{CommandBuilder, PtySize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::mpsc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
pub struct Status {
    model: String,
    effort: String,
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
            Update::Notice(message) => self.notice = Some((message, Instant::now())),
            Update::Session { model, effort, cwd } => {
                if let Some(model) = model {
                    self.model = model;
                }
                if let Some(effort) = effort {
                    self.effort = effort;
                }
                if let Some(cwd) = cwd {
                    self.cwd = cwd;
                }
            }
        }
    }

    fn header(&self, width: usize) -> String {
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
        let effort = if self.effort.is_empty() && !self.model.is_empty() {
            "default"
        } else {
            &self.effort
        };
        let model = format!("{} {effort}", self.model).trim().to_owned();
        let mut segments = vec![
            ("codexmu".to_owned(), 111),
            (clip(&model, 27), 116),
            (clip_path(&self.cwd, (width / 5).clamp(10, 36)), 222),
            (clip(&self.git, 20), 111),
            (quota, 116),
            (account, 116),
        ];
        if width < 100 {
            segments.remove(2);
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
            .collect();
        if let Some(usage) = self.usage.get(&self.name) {
            for (label, window) in [
                ("5h", &usage.rate_limit.primary_window),
                ("weekly", &usage.rate_limit.secondary_window),
            ] {
                if !parts.iter().any(|s| s.starts_with(label))
                    && let Some(window) = window
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
        let mut remaining = width.saturating_sub(2);
        let mut result = "  ".to_owned();
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

pub struct View {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    parser: vt100::Parser<Callbacks>,
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
            rows: rows.saturating_sub(2).max(1),
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
            parser: vt100::Parser::new_with_callbacks(
                size.rows,
                size.cols,
                0,
                Callbacks::default(),
            ),
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
                    self.writer.write_all(&bytes[..n])?;
                    self.writer.flush()?;
                }
                output = self.output.recv() => match output {
                    Some(bytes) => {
                        dirty = true;
                        self.parser.process(&bytes);
                        let callback = self.parser.callbacks_mut();
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
                        self.master.resize(PtySize { rows: rows.saturating_sub(2).max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 }).map_err(|e| e.to_string())?;
                        self.parser.screen_mut().set_size(rows.saturating_sub(2).max(1), cols.max(1));
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

    fn draw(&mut self) -> Result<()> {
        let screen = self.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let (rows, cols) = screen.size();
        let text: Vec<String> = screen.rows(0, cols).collect();
        // Locate the live composer's prompt in the parsed screen, not in raw output chunks.
        let header_row = (0..=cursor_row.min(rows - 1))
            .rev()
            .find(|row| text[*row as usize].trim_start().starts_with('›'))
            .unwrap_or(0) as usize;
        let mut lines: Vec<String> = (0..rows)
            .map(|row| {
                let content = &text[row as usize];
                if row as usize > header_row && content.trim_start().starts_with("Context ") {
                    self.status.footer(content, cols as usize)
                } else {
                    render_row(
                        screen,
                        row,
                        cols,
                        row as usize == header_row && content.trim_start().starts_with('›'),
                    )
                }
            })
            .collect();
        lines.insert(header_row, self.status.header(cols as usize));
        let notice = self
            .status
            .notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(20))
            .map(|(message, _)| {
                format!(
                    " \x1b[38;5;222m{}\x1b[0m",
                    clip(message, cols.saturating_sub(2) as usize)
                )
            })
            .unwrap_or_default();
        lines.insert(header_row + 1, notice);
        let mut frame = Vec::new();
        frame.extend(b"\x1b[?2026h\x1b[?25l\x1b[?7l");
        frame.extend(std::mem::take(&mut self.parser.callbacks_mut().effects));
        for (row, line) in lines.iter().enumerate().take(self.size.1 as usize) {
            if self.previous.get(row) != Some(line) {
                write!(frame, "\x1b[{};1H\x1b[0m\x1b[2K{line}", row + 1)?;
            }
        }
        let screen = self.parser.screen();
        let row = cursor_row as usize
            + if cursor_row as usize >= header_row {
                2
            } else {
                0
            };
        write!(
            frame,
            "\x1b[0m\x1b[{};{}H\x1b[?7h\x1b[?2004{}\x1b[?25{}\x1b[?2026l",
            (row + 1).min(self.size.1 as usize),
            cursor_col + 1,
            if screen.bracketed_paste() { 'h' } else { 'l' },
            if screen.hide_cursor() { 'l' } else { 'h' }
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
        let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?2004l\x1b[?2026l\x1b[?7h");
        let _ = execute!(
            std::io::stdout(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn render_row(screen: &vt100::Screen, row: u16, cols: u16, composer: bool) -> String {
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
        let background = if composer && cell.bgcolor() == vt100::Color::Default {
            vt100::Color::Idx(236)
        } else {
            cell.bgcolor()
        };
        for (color, code) in [(cell.fgcolor(), 38), (background, 48)] {
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
    fn header_tracks_only_active_quota_and_clips_terminal_controls() {
        let mut state = Status::default();
        state.update(Update::Usage { name: "a".into(), usage: serde_json::from_value(serde_json::json!({"rate_limit":{"primary_window":{"used_percent":100,"reset_at":now()+600}}})).unwrap() });
        state.update(Update::Active {
            name: "b".into(),
            email: "b@example.test".into(),
            plan: "plus".into(),
        });
        assert!(state.header(120).contains("5h —"));
        assert!(state.header(120).contains("b@example.test"));
        assert_eq!(clip("ab\x1b\ncd", 3), "ab…");
        assert!(clip("한국어", 4).width() <= 4);
        let mut parser = vt100::Parser::new_with_callbacks(20, 80, 0, Callbacks::default());
        parser.process(b"\x1b[3;4H\x1b[");
        parser.process(b"6n");
        assert_eq!(parser.callbacks().replies, b"\x1b[3;4R");
    }
}
