use crate::{
    Result,
    accounts::{Manager, Update},
    bridge, dashboard, native_command,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    ffi::OsString,
    io::IsTerminal,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tokio_tungstenite::{
    accept_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

// Restore the user's terminal even if the server fails while Codex is in raw mode.
struct TerminalMode(String);
impl TerminalMode {
    fn save() -> Result<Self> {
        let output = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err("cannot read terminal settings with stty".into());
        }
        Ok(Self(String::from_utf8(output.stdout)?.trim().to_owned()))
    }
}
impl Drop for TerminalMode {
    fn drop(&mut self) {
        let _ = Command::new("stty")
            .arg(&self.0)
            .stdin(Stdio::inherit())
            .status();
    }
}

async fn transport(
    stream: UnixStream,
    pipe: tokio::io::DuplexStream,
    manager: &Manager,
) -> Result<()> {
    let config = WebSocketConfig::default().max_message_size(Some(16 * 1024 * 1024));
    let socket = tokio::time::timeout(
        Duration::from_secs(10),
        accept_async_with_config(stream, Some(config)),
    )
    .await??;
    let (mut sink, mut messages) = socket.split();
    let (read, mut write) = tokio::io::split(pipe);
    let mut lines = BufReader::new(read).lines();
    loop {
        tokio::select! {
            message = messages.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    // Re-encode each JSON frame so embedded whitespace cannot create extra RPC lines.
                    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| "invalid terminal JSON frame")?;
                    if value["method"] == "turn/start" {
                        session_update(manager, &value["params"]);
                    }
                    let mut bytes = serde_json::to_vec(&value)?;
                    bytes.push(b'\n');
                    write.write_all(&bytes).await?;
                }
                Some(Ok(Message::Ping(_))) => sink.flush().await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(Message::Pong(_))) => {},
                Some(Ok(_)) => return Err("terminal transport requires JSON text frames".into()),
                Some(Err(error)) => return Err(error.into()),
            },
            line = lines.next_line() => match line? {
                Some(line) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                        && value["result"].is_object() { session_update(manager, &value["result"]); }
                    sink.send(Message::Text(line.into())).await?;
                },
                None => { sink.close().await?; return Ok(()); },
            }
        }
    }
}

fn session_update(manager: &Manager, value: &serde_json::Value) {
    let string = |v: &serde_json::Value| v.as_str().map(str::to_owned);
    let model = string(&value["model"]);
    let effort = string(&value["reasoningEffort"]).or_else(|| string(&value["effort"]));
    let cwd = string(&value["cwd"]).or_else(|| string(&value["thread"]["cwd"]));
    if model.is_some() || effort.is_some() || cwd.is_some() {
        manager.update(Update::Session { model, effort, cwd });
    }
}

pub async fn run(
    manager: Manager,
    binary: PathBuf,
    args: Vec<OsString>,
    interval: u64,
    resume: bool,
    plain: bool,
) -> Result<u8> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(
            "codexmu needs an interactive terminal; use app-server for a JSON-RPC client".into(),
        );
    }
    if args
        .iter()
        .any(|a| a.to_string_lossy().starts_with("--remote"))
    {
        return Err("codexmu manages --remote automatically; remove that option".into());
    }
    manager.credentials().await?;
    let _terminal = TerminalMode::save()?;
    // A short, private path fits macOS sockaddr_un and exposes no TCP listener.
    let directory = tempfile::Builder::new()
        .prefix("codexmu-")
        .tempdir_in("/tmp")?;
    let path = directory.path().join("rpc.sock");
    let listener = UnixListener::bind(&path)?;
    let endpoint = format!("unix://{}", path.display());
    if !plain {
        return run_dashboard(
            manager,
            binary,
            args,
            interval,
            resume,
            (listener, endpoint),
        )
        .await;
    }
    let mut ui = native_command(&binary, &manager.store)?
        .args(["--remote", &endpoint])
        .args(args)
        .spawn()?;
    let connection = tokio::select! {
        status = ui.wait() => return Ok(status?.code().unwrap_or(1) as u8),
        connection = tokio::time::timeout(Duration::from_secs(30), listener.accept()) => connection??.0,
    };
    drop(listener);
    let (pipe, server_pipe) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server_pipe);
    let outcome = {
        let relay = transport(connection, pipe, &manager);
        let server = bridge::run_with_io(
            manager.clone(),
            binary,
            vec!["app-server".into()],
            interval,
            resume,
            input,
            output,
        );
        tokio::pin!(relay, server);
        tokio::select! {
            result = &mut server => result,
            result = &mut relay => result.map(|()| 0),
            status = ui.wait() => status.map(|s| s.code().unwrap_or(1) as u8).map_err(Into::into),
        }
    }; // Closing the transport lets the native TUI exit and restore its own screen.
    match tokio::time::timeout(Duration::from_secs(3), ui.wait()).await {
        Ok(status) => {
            let status = status?;
            if outcome.as_ref().is_ok_and(|code| *code == 0) {
                return Ok(status.code().unwrap_or(1) as u8);
            }
        }
        Err(_) => {
            ui.kill().await?;
            let _ = ui.wait().await;
        }
    }
    outcome
}

async fn run_dashboard(
    mut manager: Manager,
    binary: PathBuf,
    args: Vec<OsString>,
    interval: u64,
    resume: bool,
    (listener, endpoint): (UnixListener, String),
) -> Result<u8> {
    let (updates, receiver) = tokio::sync::mpsc::unbounded_channel();
    manager.updates = Some(updates);
    manager.activated(&manager.credentials().await?);
    let mut command = native_command(&binary, &manager.store)?;
    command.args(["--remote", &endpoint, "-c", "tui.status_line=[\"context-remaining\",\"fast-mode\",\"five-hour-limit\",\"weekly-limit\",\"codex-version\"]", "-c", "tui.status_line_use_colors=true"])
        .args(args);
    let mut view = dashboard::View::start(command.as_std(), receiver)?;
    let connection = tokio::select! {
        result = view.run() => return result,
        connection = tokio::time::timeout(Duration::from_secs(30), listener.accept()) => connection??.0,
    };
    drop(listener);
    let (pipe, server_pipe) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server_pipe);
    let outcome = {
        let relay = transport(connection, pipe, &manager);
        let server = bridge::run_with_io(
            manager.clone(),
            binary,
            vec!["app-server".into()],
            interval,
            resume,
            input,
            output,
        );
        tokio::pin!(relay, server);
        tokio::select! {
            result = &mut server => result,
            result = &mut relay => result.map(|()| 0),
            result = view.run() => return result,
        }
    };
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(3), view.run()).await
        && outcome.as_ref().is_ok_and(|code| *code == 0)
    {
        return result;
    }
    outcome
}
