use crate::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("account name must be 1–64 ASCII letters, digits, '-' or '_'".into());
    }
    Ok(())
}

fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Auth(pub Value);

impl Auth {
    pub fn read(path: &Path) -> Result<Self> {
        let auth = Self(read_json(path)?);
        auth.validate()?;
        Ok(auth)
    }
    pub fn validate(&self) -> Result<()> {
        if self
            .0
            .get("auth_mode")
            .and_then(Value::as_str)
            .is_some_and(|m| m != "chatgpt")
            || self.0["OPENAI_API_KEY"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        {
            return Err("only ChatGPT OAuth accounts are supported".into());
        }
        for key in ["access_token", "account_id"] {
            if self.0["tokens"][key]
                .as_str()
                .is_none_or(|s| s.is_empty() || s.contains(['\r', '\n']))
            {
                return Err(
                    "auth.json requires valid tokens.access_token and tokens.account_id".into(),
                );
            }
        }
        self.identity()?;
        Ok(())
    }
    pub fn access(&self) -> &str {
        self.0["tokens"]["access_token"].as_str().unwrap_or("")
    }
    pub fn account_id(&self) -> &str {
        self.0["tokens"]["account_id"].as_str().unwrap_or("")
    }
    fn metadata(&self) -> Value {
        self.0["tokens"]["id_token"]
            .as_str()
            .and_then(claims)
            .or_else(|| claims(self.access()))
            .unwrap_or(Value::Null)
    }
    pub fn identity(&self) -> Result<String> {
        let metadata = self.metadata();
        let user = metadata["https://api.openai.com/auth"]["chatgpt_user_id"]
            .as_str()
            .or_else(|| metadata["sub"].as_str())
            .or_else(|| metadata["email"].as_str())
            .filter(|s| !s.is_empty())
            .ok_or("auth token is missing user identity claims")?;
        Ok(format!("{user}::{}", self.account_id()))
    }
    pub fn email(&self) -> String {
        self.metadata()["email"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned()
    }
    pub fn plan(&self) -> Value {
        self.metadata()["https://api.openai.com/auth"]["chatgpt_plan_type"].clone()
    }
    fn expired(&self) -> bool {
        claims(self.access())
            .and_then(|c| c["exp"].as_i64())
            .is_some_and(|exp| exp <= now() + 60)
    }
    pub fn login_params(&self) -> Value {
        json!({"type":"chatgptAuthTokens", "accessToken":self.access(),
            "chatgptAccountId":self.account_id(), "chatgptPlanType":self.plan()})
    }
    pub fn refresh_result(&self) -> Value {
        json!({"accessToken":self.access(), "chatgptAccountId":self.account_id(), "chatgptPlanType":self.plan()})
    }
}

pub fn read_json(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("JSON file exceeds 4 MiB".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid JSON file (contents omitted)".into())
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().ok_or("file needs a parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub auth: Auth,
    #[serde(default)]
    pub blocked_until: i64,
    /// Higher tiers are chosen first; usage decides only within a tier.
    #[serde(default)]
    pub priority: i64,
}

#[derive(Serialize, Deserialize)]
struct PendingRefresh {
    before: Auth,
    updated: Account,
}

#[derive(Clone)]
pub struct Store {
    pub home: PathBuf,
    root: PathBuf,
}

impl Store {
    pub fn new(home: PathBuf) -> Result<Self> {
        let root = home.join("codexmu");
        private_dir(&root)?;
        private_dir(&root.join("accounts"))?;
        Ok(Self { home, root })
    }
    pub async fn lock(&self) -> Result<File> {
        let file = self.lock_file("store.lock").await?;
        self.recover_refresh()?;
        Ok(file)
    }
    pub async fn server_start_lock(&self) -> Result<File> {
        self.lock_file("server-start.lock").await
    }
    async fn lock_file(&self, name: &str) -> Result<File> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(name))?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock)
                    if started.elapsed() < Duration::from_secs(30) =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(_) => return Err("account store is busy or cannot be locked".into()),
            }
        }
    }
    pub fn watch_lock(&self) -> Result<File> {
        let path = self.root.join("bridge.lock");
        let mut file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let owner = read_json(&path).unwrap_or(Value::Null);
                let mode = owner["mode"]
                    .as_str()
                    .filter(|m| matches!(*m, "terminal" | "watch" | "app-server"))
                    .unwrap_or("session");
                let pid = owner["pid"]
                    .as_u64()
                    .map(|p| format!(" (PID {p})"))
                    .unwrap_or_default();
                return Err(format!("this CODEX_HOME is in use by codexmu {mode}{pid}; close that session first. Running codexmu already monitors usage; a separate watch is unnecessary").into());
            }
            Err(error) => return Err(format!("cannot lock {}: {error}", path.display()).into()),
        }
        // Write ownership only after acquiring the OS lock; never replace the locked inode.
        file.set_len(0)?;
        serde_json::to_writer(
            &mut file,
            &json!({"pid":std::process::id(), "mode":"watch"}),
        )?;
        file.flush()?;
        Ok(file)
    }
    fn path(&self, name: &str) -> Result<PathBuf> {
        valid_name(name)?;
        Ok(self.root.join("accounts").join(format!("{name}.json")))
    }
    pub fn all(&self) -> Result<Vec<Account>> {
        let mut accounts = Vec::new();
        for entry in fs::read_dir(self.root.join("accounts"))? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let account: Account =
                serde_json::from_value(read_json(&path)?).map_err(|_| "invalid account record")?;
            valid_name(&account.name)?;
            if self.path(&account.name)? != path {
                return Err("account filename does not match its name".into());
            }
            account.auth.validate()?;
            accounts.push(account);
        }
        accounts.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(accounts)
    }
    pub fn save(&self, account: &Account) -> Result<()> {
        atomic_json(&self.path(&account.name)?, account)
    }
    fn recover_refresh(&self) -> Result<()> {
        let path = self.root.join("pending-refresh.json");
        if !path.try_exists()? {
            return Ok(());
        }
        let pending: PendingRefresh = serde_json::from_value(read_json(&path)?)
            .map_err(|_| "invalid pending refresh; restore from a saved auth file")?;
        pending.updated.auth.validate()?;
        if pending.before.identity()? != pending.updated.auth.identity()? {
            return Err("pending refresh identity mismatch".into());
        }
        self.save(&pending.updated)?;
        if self
            .active_auth()?
            .is_some_and(|auth| auth.0 == pending.before.0)
        {
            atomic_json(&self.home.join("auth.json"), &pending.updated.auth.0)?;
        }
        fs::remove_file(path)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn active_auth(&self) -> Result<Option<Auth>> {
        let path = self.home.join("auth.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        Ok(Some(Auth::read(&path)?))
    }
    // The active auth file is authoritative, including refreshes made by official Codex.
    pub fn active(&self) -> Result<Account> {
        let auth = self
            .active_auth()?
            .ok_or("no active auth.json; run codexmu switch NAME")?;
        let id = auth.identity()?;
        let mut account = self
            .all()?
            .into_iter()
            .find(|a| a.auth.identity().ok().as_ref() == Some(&id))
            .ok_or("active account is not registered; run codexmu add NAME")?;
        if account.auth.0 != auth.0 {
            account.auth = auth;
            self.save(&account)?;
        }
        Ok(account)
    }
    pub fn add(&self, name: &str, auth: Auth) -> Result<()> {
        valid_name(name)?;
        auth.validate()?;
        if self.path(name)?.try_exists()? {
            return Err(
                "account name already exists; remove it explicitly before replacing".into(),
            );
        }
        let id = auth.identity()?;
        if self
            .all()?
            .iter()
            .any(|a| a.auth.identity().ok().as_ref() == Some(&id))
        {
            return Err("this account is already registered".into());
        }
        self.save(&Account {
            name: name.to_owned(),
            auth,
            blocked_until: 0,
            priority: 0,
        })
    }
    pub fn set_priority(&self, name: &str, priority: i64) -> Result<()> {
        let mut account = self
            .all()?
            .into_iter()
            .find(|a| a.name == name)
            .ok_or("unknown account")?;
        account.priority = priority;
        self.save(&account)
    }
    pub fn remove(&self, name: &str) -> Result<()> {
        let current = self.active_auth()?.map(|a| a.identity()).transpose()?;
        let account = self
            .all()?
            .into_iter()
            .find(|a| a.name == name)
            .ok_or("unknown account")?;
        if account.auth.identity().ok() == current {
            return Err("switch away before removing the active account".into());
        }
        fs::remove_file(self.path(name)?)?;
        Ok(())
    }
    pub fn activate(&self, account: &Account) -> Result<()> {
        if let Some(auth) = self.active_auth()? {
            // Keep one recoverable backup and save outgoing rotated tokens before replacing auth.json.
            atomic_json(&self.root.join("previous-auth.json"), &auth.0)?;
            let id = auth.identity()?;
            if let Some(mut old) = self
                .all()?
                .into_iter()
                .find(|a| a.auth.identity().ok().as_ref() == Some(&id))
            {
                old.auth = auth;
                self.save(&old)?;
            }
        }
        atomic_json(&self.home.join("auth.json"), &account.auth.0)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Window {
    pub used_percent: f64,
    pub reset_at: Option<i64>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Limits {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary_window: Option<Window>,
    pub secondary_window: Option<Window>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Usage {
    pub rate_limit: Limits,
}

// UI updates contain display metadata only, never authentication tokens.
pub enum Update {
    Active {
        name: String,
        email: String,
        plan: String,
    },
    Usage {
        name: String,
        usage: Usage,
    },
    Notice(String),
    Session {
        model: Option<String>,
        effort: Option<String>,
        cwd: Option<String>,
    },
}
impl Usage {
    pub fn used(&self) -> Option<f64> {
        let mut worst: Option<f64> = None;
        for w in [
            &self.rate_limit.primary_window,
            &self.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        {
            if !w.used_percent.is_finite()
                || !(0.0..=100.0).contains(&w.used_percent)
                || w.reset_at.is_some_and(|t| t <= now())
            {
                return None;
            }
            worst = Some(worst.unwrap_or(0.0).max(w.used_percent));
        }
        worst
    }
    pub fn exhausted(&self) -> bool {
        self.rate_limit.allowed == Some(false)
            || self.rate_limit.limit_reached == Some(true)
            || [
                &self.rate_limit.primary_window,
                &self.rate_limit.secondary_window,
            ]
            .into_iter()
            .flatten()
            .any(|w| w.used_percent == 100.0 && w.reset_at.is_none_or(|t| t > now()))
    }
    pub fn reset_at(&self) -> Option<i64> {
        [
            &self.rate_limit.primary_window,
            &self.rate_limit.secondary_window,
        ]
        .into_iter()
        .flatten()
        .filter_map(|w| w.reset_at)
        .min()
    }
    pub fn available(&self) -> Option<f64> {
        if self.exhausted() {
            None
        } else {
            self.used().filter(|p| *p < 100.0)
        }
    }
}

#[derive(Clone)]
pub struct Manager {
    pub store: Store,
    pub updates: Option<tokio::sync::mpsc::UnboundedSender<Update>>,
    client: Client,
    usage_url: Url,
    token_url: Url,
    switch_at: f64,
}

fn endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("endpoints require HTTPS (HTTP is allowed only on loopback for tests)".into());
    }
    Ok(url)
}

impl Manager {
    pub fn new(store: Store, usage: &str, token: &str, switch_at: u8) -> Result<Self> {
        Ok(Self {
            store,
            updates: None,
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .user_agent("codexmu/0.1.0")
                .build()?,
            usage_url: endpoint(usage)?,
            token_url: endpoint(token)?,
            switch_at: f64::from(switch_at),
        })
    }
    pub fn activated(&self, account: &Account) {
        self.update(Update::Active {
            name: account.name.clone(),
            email: account.auth.email(),
            plan: account.auth.plan().as_str().unwrap_or("unknown").to_owned(),
        });
    }
    pub fn update(&self, update: Update) {
        if let Some(sender) = &self.updates {
            let _ = sender.send(update);
        }
    }
    pub fn notice(&self, message: String) {
        if self.updates.is_some() {
            self.update(Update::Notice(message));
        } else {
            eprintln!("codexmu: {message}");
        }
    }
    async fn refresh(&self, account: &mut Account) -> Result<()> {
        let refresh = account.auth.0["tokens"]["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("missing refresh token; login again")?;
        let response = self.client.post(self.token_url.clone()).json(&json!({"client_id":"app_EMoamEEZ73f0CkXaXp7hrann", "grant_type":"refresh_token", "refresh_token":refresh})).send().await?;
        if !response.status().is_success() {
            return Err(format!(
                "OAuth refresh HTTP {}; login again if credentials expired",
                response.status().as_u16()
            )
            .into());
        }
        let value = bounded_json(response).await?;
        let mut updated = account.clone();
        if value["access_token"].as_str().is_none_or(|s| s.is_empty()) {
            return Err("OAuth refresh returned no access token".into());
        }
        for key in ["access_token", "refresh_token", "id_token"] {
            if let Some(token) = value[key].as_str().filter(|s| !s.is_empty()) {
                updated.auth.0["tokens"][key] = json!(token);
            }
        }
        updated.auth.0["last_refresh"] = json!(OffsetDateTime::now_utc().format(&Rfc3339)?);
        updated.auth.validate()?;
        if updated.auth.identity()? != account.auth.identity()? {
            return Err("OAuth refresh changed account identity".into());
        }
        // A journal prevents a crash between the two writes from restoring a spent refresh token.
        atomic_json(
            &self.store.root.join("pending-refresh.json"),
            &PendingRefresh {
                before: account.auth.clone(),
                updated: updated.clone(),
            },
        )?;
        self.store.recover_refresh()?;
        *account = updated;
        Ok(())
    }
    async fn usage(&self, account: &mut Account) -> Result<Usage> {
        if account.auth.expired() {
            self.refresh(account).await?;
        }
        for attempt in 0..2 {
            let response = self
                .client
                .get(self.usage_url.clone())
                .bearer_auth(account.auth.access())
                .header("ChatGPT-Account-Id", account.auth.account_id())
                .send()
                .await?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.refresh(account).await?;
                continue;
            }
            if !response.status().is_success() {
                return Err(format!("usage HTTP {}", response.status().as_u16()).into());
            }
            let usage: Usage = serde_json::from_value(bounded_json(response).await?)
                .map_err(|_| "invalid usage response")?;
            self.update(Update::Usage {
                name: account.name.clone(),
                usage: usage.clone(),
            });
            return Ok(usage);
        }
        Err("usage authentication failed".into())
    }
    pub async fn credentials(&self) -> Result<Account> {
        let _lock = self.store.lock().await?;
        let mut active = self.store.active()?;
        if active.auth.expired() {
            self.refresh(&mut active).await?;
        }
        Ok(active)
    }
    /// Refresh this server's account even if another session has changed the shared default.
    pub async fn refresh_session(&self, previous: &Account) -> Result<Account> {
        let _lock = self.store.lock().await?;
        if self
            .store
            .active_auth()?
            .is_some_and(|auth| auth.identity().ok() == previous.auth.identity().ok())
        {
            self.store.active()?; // Preserve tokens rotated by an external Codex process.
        }
        let mut account = self
            .store
            .all()?
            .into_iter()
            .find(|a| {
                a.name == previous.name && a.auth.identity().ok() == previous.auth.identity().ok()
            })
            .ok_or("this session's account is no longer registered; log in again")?;
        // A different session may have already rotated this refresh token while we waited.
        if account.auth.0 == previous.auth.0 || account.auth.expired() {
            self.refresh(&mut account).await?;
        }
        Ok(account)
    }
    pub async fn switch(&self, name: &str) -> Result<()> {
        let _lock = self.store.lock().await?;
        let mut account = self
            .store
            .all()?
            .into_iter()
            .find(|a| a.name == name)
            .ok_or("unknown account")?;
        if account.auth.expired() {
            self.refresh(&mut account).await?;
        }
        self.store.activate(&account)
    }
    /// A limit outranks the usage report: exclude the account until its next reported reset.
    async fn limit(&self, account: &mut Account) -> Result<()> {
        let reset = match self.usage(account).await {
            Ok(usage) => usage.reset_at(),
            Err(_) => None,
        };
        account.blocked_until = reset.unwrap_or(0).max(now() + 60);
        self.store.save(account)
    }
    /// Probe current quota, or rotate after a structured limit error from this account.
    /// Below a real limit, switch early once usage reaches `switch_at` and a cooler account exists.
    pub async fn prepare(&self, limited_account: Option<&str>, dry_run: bool) -> Result<Account> {
        let _lock = self.store.lock().await?;
        let mut active = self.store.active()?;
        let forced = limited_account == Some(active.name.as_str());
        if let Some(limited) = limited_account
            && !forced
            && !dry_run
            && let Some(mut account) = self.store.all()?.into_iter().find(|a| a.name == limited)
        {
            self.limit(&mut account).await?;
        }
        // Candidates must stay below this ceiling: any headroom after a limit, the threshold otherwise.
        let ceiling = if forced {
            100.0
        } else {
            match self.usage(&mut active).await {
                Ok(usage) if usage.exhausted() => 100.0,
                Ok(usage) => match usage.used() {
                    Some(used) if used >= self.switch_at => self.switch_at,
                    _ => return Ok(active),
                },
                Err(error) => {
                    self.notice(format!("{} usage unavailable: {error}", active.name));
                    return Ok(active);
                }
            }
        };
        if ceiling >= 100.0 && !dry_run {
            self.limit(&mut active).await?;
        }
        let mut candidates = Vec::new();
        // ponytail: serial account probes; use bounded concurrency if large account pools need it.
        for mut account in self.store.all()? {
            if account.name == active.name || account.blocked_until > now() {
                continue;
            }
            match self.usage(&mut account).await {
                Ok(usage) => candidates.extend(usage.available().map(|used| (account, used))),
                Err(error) => self.notice(format!("{} skipped: {error}", account.name)),
            }
        }
        // Highest tier first, lowest usage within it. Prefer accounts under the threshold so the
        // next check does not move again at once; at a real limit any headroom will do.
        let pick = |ceiling: f64| {
            candidates
                .iter()
                .filter(|(_, used)| *used < ceiling)
                .min_by(|(a, x), (b, y)| b.priority.cmp(&a.priority).then(x.total_cmp(y)))
                .map(|(account, _)| account.clone())
        };
        let Some(next) = pick(self.switch_at).or_else(|| pick(ceiling)) else {
            if ceiling >= 100.0 {
                self.notice("no available account; waiting for quota reset".to_owned());
            }
            return Ok(active);
        };
        if dry_run {
            self.notice(format!("would switch {} -> {}", active.name, next.name));
            return Ok(active);
        }
        if self.store.active()?.auth.identity()? != active.auth.identity()? {
            return self.store.active();
        }
        self.store.activate(&next)?;
        self.notice(format!("switched {} -> {}", active.name, next.name));
        Ok(next)
    }
    pub async fn list(&self, live: bool) -> Result<Value> {
        let _lock = self.store.lock().await?;
        let active = self
            .store
            .active_auth()?
            .map(|a| a.identity())
            .transpose()?;
        if active.is_some() {
            let _ = self.store.active();
        }
        let mut rows = Vec::new();
        for mut account in self.store.all()? {
            let mut row = json!({"name":account.name, "email":account.auth.email(), "plan":account.auth.plan(),
                "active":account.auth.identity().ok() == active, "blocked_until":account.blocked_until, "priority":account.priority});
            if live {
                match self.usage(&mut account).await {
                    Ok(usage) => row["usage"] = serde_json::to_value(usage)?,
                    Err(error) => row["error"] = json!(error.to_string()),
                }
            }
            rows.push(row);
        }
        Ok(json!({"accounts":rows}))
    }
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err("HTTP JSON response exceeds 1 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "invalid HTTP JSON response (contents omitted)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn monitor_lock_reports_owner_and_releases_without_deleting_file() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path().to_path_buf()).unwrap();
        let watch = store.watch_lock().unwrap();
        let error = store.watch_lock().unwrap_err().to_string();
        assert!(error.contains("codexmu watch"));
        assert!(error.contains(&format!("PID {}", std::process::id())));
        drop(watch);
        let _next_watch = store.watch_lock().unwrap();
        assert!(
            store
                .watch_lock()
                .unwrap_err()
                .to_string()
                .contains("codexmu watch")
        );
    }
    pub fn auth(user: &str) -> Auth {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"sub":user,"email":format!("{user}@example.test")}))
                .unwrap(),
        );
        Auth(
            json!({"tokens":{"access_token":format!("e30.{payload}.sig"),"id_token":format!("e30.{payload}.sig"),"account_id":user,"refresh_token":"secret"}}),
        )
    }
    #[tokio::test]
    async fn atomic_switch_preserves_tokens_and_rejects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        let _lock = store.lock().await.unwrap();
        store.add("a", auth("a")).unwrap();
        store.add("b", auth("b")).unwrap();
        assert!(store.add("duplicate", auth("a")).is_err());
        assert!(store.add("../escape", auth("c")).is_err());
        store.activate(&store.all().unwrap()[0]).unwrap();
        let mut rotated = auth("a");
        rotated.0["tokens"]["refresh_token"] = json!("rotated");
        atomic_json(&dir.path().join("auth.json"), &rotated.0).unwrap();
        store.activate(&store.all().unwrap()[1]).unwrap();
        assert_eq!(store.active().unwrap().name, "b");
        assert_eq!(
            store.all().unwrap()[0].auth.0["tokens"]["refresh_token"],
            "rotated"
        );
        assert!(store.remove("b").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join("auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn interrupted_refresh_is_recovered_before_reading_active_auth() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        store.add("a", auth("a")).unwrap();
        store.activate(&store.all().unwrap()[0]).unwrap();
        let before = auth("a");
        let mut updated = store.all().unwrap().remove(0);
        updated.auth.0["tokens"]["refresh_token"] = json!("new-single-use-token");
        atomic_json(
            &store.root.join("pending-refresh.json"),
            &PendingRefresh { before, updated },
        )
        .unwrap();
        let _lock = store.lock().await.unwrap();
        assert_eq!(
            store.active().unwrap().auth.0["tokens"]["refresh_token"],
            "new-single-use-token"
        );
        assert!(!store.root.join("pending-refresh.json").exists());
    }
    #[test]
    fn quota_windows_are_conservative() {
        let mut usage: Usage = serde_json::from_value(json!({"rate_limit":{"primary_window":{"used_percent":10,"reset_at":now()+100},"secondary_window":{"used_percent":100,"reset_at":now()+100}}})).unwrap();
        assert!(usage.exhausted());
        assert!(usage.available().is_none());
        usage.rate_limit.secondary_window = None;
        assert_eq!(usage.available(), Some(10.0));
        usage.rate_limit.primary_window.as_mut().unwrap().reset_at = Some(now() - 1);
        assert!(usage.available().is_none());
        usage.rate_limit.limit_reached = Some(true);
        assert!(usage.exhausted());
        assert!(endpoint("http://example.com/usage").is_err());
        assert!(endpoint("http://127.0.0.1/usage").is_ok());
    }
}
