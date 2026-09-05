use crate::{
    Result,
    accounts::{Account, Manager},
    native_command,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    task::JoinHandle,
};

async fn send(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn limit_error(value: &Value) -> bool {
    value["params"]["turn"]["error"]["codexErrorInfo"] == "usageLimitExceeded"
}
fn gated(method: &str) -> bool {
    matches!(
        method,
        "turn/start" | "thread/start" | "thread/resume" | "thread/fork"
    )
}

struct Request {
    id: Value,
    method: String,
    thread: Option<String>,
}

struct Bridge {
    sequence: u64,
    requests: BTreeMap<String, Request>,
    busy: BTreeSet<String>,
    queued: VecDeque<Value>,
    recovery: BTreeMap<String, BTreeSet<String>>,
    active: Option<Account>,
    applying: Option<Account>,
    initialized: bool,
    login_pending: bool,
    limited: Option<String>,
}

impl Bridge {
    async fn forward(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        mut value: Value,
    ) -> Result<()> {
        let method = value["method"].as_str().unwrap_or("").to_owned();
        if value.get("id").is_some() && value.get("method").is_some() {
            self.sequence += 1;
            let key = format!("codexmu-client-{}", self.sequence);
            let thread = value["params"]["threadId"].as_str().map(str::to_owned);
            if method == "turn/start"
                && let Some(thread) = &thread
            {
                self.busy.insert(thread.clone());
            }
            self.requests.insert(
                key.clone(),
                Request {
                    id: value["id"].clone(),
                    method,
                    thread,
                },
            );
            value["id"] = json!(key);
        }
        send(writer, &value).await
    }
    async fn release(
        &mut self,
        writer: &mut (impl AsyncWrite + Unpin),
        resume: bool,
    ) -> Result<()> {
        while let Some(value) = self.queued.pop_front() {
            if value["method"] == "turn/start"
                && let Some(thread) = value["params"]["threadId"].as_str()
            {
                self.recovery.remove(thread);
            }
            self.forward(writer, value).await?;
        }
        if !resume {
            return Ok(());
        }
        if let Some(active) = &self.active {
            // ponytail: one recovery turn per account per failure chain; add reset-aware retry budgets if needed.
            for (thread, tried) in &mut self.recovery {
                if self.busy.contains(thread) || !tried.insert(active.name.clone()) {
                    continue;
                }
                self.sequence += 1;
                let id = format!("codexmu-resume-{}", self.sequence);
                self.busy.insert(thread.clone());
                self.requests.insert(
                    id.clone(),
                    Request {
                        id: Value::Null,
                        method: "turn/start".to_owned(),
                        thread: Some(thread.clone()),
                    },
                );
                send(writer, &json!({"id":id, "method":"turn/start", "params":{"threadId":thread,
                    "input":[{"type":"text", "text":"The account has been switched because the previous account reached its usage limit. Continue from the existing conversation state; check completed work before repeating any actions."}]}})).await?;
            }
        }
        Ok(())
    }
}

pub async fn run(
    manager: Manager,
    binary: PathBuf,
    args: Vec<OsString>,
    interval: u64,
    resume: bool,
) -> Result<u8> {
    run_with_io(
        manager,
        binary,
        args,
        interval,
        resume,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

/// Each connection owns its native server; account storage is locked only during updates.
pub async fn run_with_io(
    manager: Manager,
    binary: PathBuf,
    args: Vec<OsString>,
    interval: u64,
    resume: bool,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> Result<u8> {
    // Desktop stdio is the supported transport; never start an unmonitored network listener.
    if args.iter().any(|arg| {
        arg.to_string_lossy().starts_with("--listen") || arg == "daemon" || arg == "proxy"
    }) {
        return Err("codexmu app-server supports stdio only; omit --listen/daemon/proxy".into());
    }
    manager.credentials().await?;
    let diagnostics = if manager.updates.is_some() {
        // Keep native diagnostics private and available without writing over the composer.
        let path = manager
            .store
            .home
            .join(format!("codexmu/terminal-{}.log", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Stdio::from(options.open(path)?)
    } else {
        Stdio::inherit()
    };
    // Native SQLite migrations race in a fresh home. Serialize startup, not running sessions.
    let mut startup_lock = Some(manager.store.server_start_lock().await?);
    let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut child = native_command(&binary, &manager.store)?
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(diagnostics)
        .spawn()?;
    let mut to_server = child.stdin.take().ok_or("missing child stdin")?;
    let mut from_server =
        BufReader::new(child.stdout.take().ok_or("missing child stdout")?).lines();
    let mut from_client = BufReader::new(input).lines();
    let mut to_client = output;
    let mut state = Bridge {
        sequence: 0,
        requests: BTreeMap::new(),
        busy: BTreeSet::new(),
        queued: VecDeque::new(),
        recovery: BTreeMap::new(),
        active: None,
        applying: None,
        initialized: false,
        login_pending: false,
        limited: None,
    };
    let mut job: Option<JoinHandle<Result<Account>>> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut check_due = false;
    let mut login_deadline = tokio::time::Instant::now();
    loop {
        if state.initialized
            && state.busy.is_empty()
            && job.is_none()
            && !state.login_pending
            && (state.active.is_none() || check_due || state.limited.is_some())
        {
            let manager = manager.clone();
            let limited = state.limited.take();
            let initial = state.active.is_none();
            job = Some(tokio::spawn(async move {
                if initial {
                    manager.credentials().await
                } else {
                    manager.prepare(limited.as_deref(), false).await
                }
            }));
            check_due = false;
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { child.kill().await?; let _ = child.wait().await; return Ok(130); }
            _ = tokio::time::sleep_until(startup_deadline), if startup_lock.is_some() => {
                return Err("Codex initialization did not complete within 30 seconds".into());
            }
            _ = tick.tick() => check_due = true,
            _ = tokio::time::sleep_until(login_deadline), if state.login_pending => {
                return Err("Codex did not acknowledge account login within 30 seconds".into());
            }
            result = async { job.as_mut().expect("guarded job").await }, if job.is_some() => {
                job = None;
                match result? {
                    Ok(account) => {
                        let changed = state.active.as_ref().is_none_or(|a| a.auth.0 != account.auth.0);
                        if changed {
                            send(&mut to_server, &json!({"id":"codexmu-login", "method":"account/login/start", "params":account.auth.login_params()})).await?;
                            state.applying = Some(account);
                            state.login_pending = true;
                            login_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                        } else { state.release(&mut to_server, resume).await?; }
                    }
                    Err(error) => {
                        if state.active.is_none() { return Err(error); }
                        manager.notice(format!("account check failed: {error}"));
                        state.release(&mut to_server, false).await?;
                    }
                }
            }
            line = from_client.next_line() => {
                let Some(line) = line? else {
                    drop(to_server);
                    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                        Ok(status) => return Ok(status?.code().unwrap_or(1) as u8),
                        Err(_) => { child.kill().await?; let _ = child.wait().await; return Ok(0); }
                    }
                };
                if line.len() > 16 * 1024 * 1024 { return Err("client JSON frame exceeds 16 MiB".into()); }
                let mut value: Value = serde_json::from_str(&line).map_err(|_| "invalid client JSON frame")?;
                let method = value["method"].as_str().unwrap_or("").to_owned();
                if method == "initialize" {
                    if !value["params"].is_object() { return Err("initialize params must be an object".into()); }
                    if !value["params"]["capabilities"].is_object() { value["params"]["capabilities"] = json!({}); }
                    value["params"]["capabilities"]["experimentalApi"] = json!(true);
                }
                if method == "initialized" { state.initialized = true; }
                if matches!(method.as_str(), "account/login/start" | "account/logout") {
                    send(&mut to_client, &json!({"id":value["id"], "error":{"code":-32602,"message":"Accounts are managed by codexmu. Use codexmu login/add/switch in a terminal."}})).await?;
                    continue;
                }
                if method == "turn/interrupt" && let Some(thread) = value["params"]["threadId"].as_str() {
                    state.recovery.remove(thread);
                    let mut keep = VecDeque::new();
                    while let Some(queued) = state.queued.pop_front() {
                        if queued["method"] == "turn/start" && queued["params"]["threadId"] == thread {
                            send(&mut to_client, &json!({"id":queued["id"],"error":{"code":-32000,"message":"Queued turn canceled"}})).await?;
                        } else { keep.push_back(queued); }
                    }
                    state.queued = keep;
                }
                if gated(&method) && (job.is_some() || state.login_pending || state.active.is_none() || state.limited.is_some()) {
                    if state.queued.len() >= 128 {
                        send(&mut to_client, &json!({"id":value["id"],"error":{"code":-32000,"message":"Account switching queue is full; retry later"}})).await?;
                    } else { state.queued.push_back(value); }
                } else {
                    if method == "turn/start" && let Some(thread) = value["params"]["threadId"].as_str() { state.recovery.remove(thread); }
                    state.forward(&mut to_server, value).await?;
                }
            }
            line = from_server.next_line() => {
                let Some(line) = line? else { return Ok(child.wait().await?.code().unwrap_or(1) as u8); };
                if line.len() > 16 * 1024 * 1024 { return Err("server JSON frame exceeds 16 MiB".into()); }
                let mut value: Value = serde_json::from_str(&line).map_err(|_| "invalid Codex JSON frame")?;
                if value["method"] == "account/chatgptAuthTokens/refresh" {
                    let refreshed = match &state.active {
                        Some(active) if value["params"]["previousAccountId"].as_str().is_none_or(|id| id == active.auth.account_id()) => manager.refresh_session(active).await,
                        _ => Err("no matching authenticated session".into()),
                    };
                    match refreshed {
                        Ok(account) => {
                            send(&mut to_server, &json!({"id":value["id"], "result":account.auth.refresh_result()})).await?;
                            state.active = Some(account);
                        }
                        _ => send(&mut to_server, &json!({"id":value["id"],"error":{"code":-32000,"message":"Unable to refresh this account; use codexmu login"}})).await?,
                    }
                    continue;
                }
                if value.get("method").is_none() {
                    if value["id"] == "codexmu-login" {
                        if value.get("error").is_some() { return Err("official Codex rejected external account login; update Codex or check workspace restrictions".into()); }
                        if state.active.is_none() { check_due = true; }
                        state.active = state.applying.take();
                        if let Some(active) = &state.active { manager.activated(active); }
                        state.login_pending = false;
                        state.release(&mut to_server, resume).await?;
                        continue;
                    }
                    if let Some(id) = value["id"].as_str() && let Some(request) = state.requests.remove(id) {
                        if request.method == "initialize" { startup_lock.take(); }
                        if request.method == "turn/start" && value.get("error").is_some() && let Some(thread) = &request.thread { state.busy.remove(thread); }
                        if request.id.is_null() {
                            if value.get("error").is_some() { manager.notice("automatic continuation was rejected; continue manually".to_owned()); }
                            continue;
                        }
                        value["id"] = request.id;
                    }
                }
                let thread = value["params"]["threadId"].as_str().map(str::to_owned);
                if value["method"] == "turn/started" && let Some(thread) = &thread { state.busy.insert(thread.clone()); }
                if value["method"] == "turn/completed" && let Some(thread) = thread {
                    state.busy.remove(&thread);
                    if limit_error(&value) {
                        if let Some(active) = &state.active {
                            state.limited = Some(active.name.clone());
                            if resume { state.recovery.entry(thread).or_default().insert(active.name.clone()); }
                        }
                    } else { state.recovery.remove(&thread); }
                }
                send(&mut to_client, &value).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_structured_quota_errors_trigger_switching() {
        assert!(limit_error(
            &json!({"params":{"turn":{"error":{"codexErrorInfo":"usageLimitExceeded"}}}})
        ));
        assert!(!limit_error(
            &json!({"params":{"turn":{"error":{"message":"usage limit exceeded","codexErrorInfo":"serverOverloaded"}}}})
        ));
        assert!(!limit_error(
            &json!({"params":{"delta":"usageLimitExceeded"}})
        ));
    }
}
