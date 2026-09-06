mod accounts;
mod bridge;
#[cfg(unix)]
mod dashboard;
#[cfg(unix)]
mod terminal;

use accounts::{Auth, Manager, Store};
use clap::{Parser, Subcommand};
use std::{ffi::OsString, path::PathBuf, process::ExitCode, time::Duration};
use tokio::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
#[command(version, about, subcommand_precedence_over_arg = true)]
struct Cli {
    #[arg(long, env = "CODEX_HOME", global = true)]
    codex_home: Option<PathBuf>,
    #[arg(
        long,
        env = "CODEXMU_CODEX_BIN",
        default_value = "codex",
        global = true
    )]
    codex_bin: PathBuf,
    #[arg(long, env="CODEXMU_INTERVAL", default_value_t=60, value_parser=clap::value_parser!(u64).range(5..), global=true)]
    interval: u64,
    /// Switch accounts without automatically sending a continuation turn.
    #[arg(long, env = "CODEXMU_NO_RESUME", global = true)]
    no_resume: bool,
    /// Switch early, between turns, once usage reaches this percent and a cooler account exists (100 = only at the limit).
    #[arg(long, env="CODEXMU_SWITCH_AT", default_value_t=100, value_parser=clap::value_parser!(u8).range(1..=100), global=true)]
    switch_at: u8,
    /// Use the unmodified official terminal layout without the codexmu status line.
    #[arg(long, global = true)]
    plain: bool,
    #[arg(
        long,
        env = "CODEXMU_USAGE_URL",
        default_value = "https://chatgpt.com/backend-api/wham/usage",
        global = true,
        hide = true
    )]
    usage_url: String,
    #[arg(
        long,
        env = "CODEXMU_TOKEN_URL",
        default_value = "https://auth.openai.com/oauth/token",
        global = true,
        hide = true
    )]
    token_url: String,
    #[command(subcommand)]
    command: Option<Action>,
    /// Prompt or native Codex CLI arguments. No command opens the Codex terminal.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(Subcommand)]
enum Action {
    /// Open the official Codex terminal with automatic account switching (also the default).
    Run {
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Save an existing ChatGPT auth.json (defaults to CODEX_HOME/auth.json).
    Add {
        name: String,
        #[arg(long)]
        auth_file: Option<PathBuf>,
    },
    /// Log in using official Codex in an isolated temporary home, then save the account.
    Login {
        name: String,
        #[arg(long)]
        device_auth: bool,
    },
    /// Show registered accounts, optionally with fresh usage from OpenAI.
    List {
        #[arg(long)]
        live: bool,
    },
    Switch {
        name: String,
    },
    /// Set an account's selection tier; higher tiers are used first (default 0).
    Priority {
        name: String,
        #[arg(allow_negative_numbers = true)]
        priority: i64,
    },
    Remove {
        name: String,
    },
    /// Poll usage and update auth.json. Use run or app-server for live authentication.
    Watch {
        #[arg(long)]
        once: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Stdio bridge to official Codex with live authentication and automatic recovery.
    AppServer {
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Launch the macOS Codex app with this binary as its app-server bridge.
    App {
        #[arg(long, default_value = "com.openai.codex")]
        id: String,
    },
}

fn home(cli: &Cli) -> Result<PathBuf> {
    std::path::absolute(
        cli.codex_home
            .clone()
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|p| PathBuf::from(p).join(".codex"))
            })
            .ok_or("set --codex-home or CODEX_HOME")?,
    )
    .map_err(Into::into)
}

pub fn native_command(binary: &PathBuf, store: &Store) -> Result<Command> {
    let resolved = resolve_binary(binary)?;
    if resolved == std::env::current_exe()?.canonicalize()? {
        return Err("--codex-bin must point to official Codex, not codexmu".into());
    }
    let mut cmd = Command::new(resolved);
    cmd.env("CODEX_HOME", &store.home)
        .env_remove("CODEX_CLI_PATH")
        .env_remove("CODEXMU_BRIDGE")
        .kill_on_drop(true);
    Ok(cmd)
}

fn resolve_binary(binary: &PathBuf) -> Result<PathBuf> {
    if binary.components().count() > 1 || binary.is_absolute() {
        return Ok(binary.canonicalize()?);
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = dir.join(binary);
        if path.is_file() {
            return Ok(path.canonicalize()?);
        }
        #[cfg(windows)]
        if path.with_extension("exe").is_file() {
            return Ok(path.with_extension("exe").canonicalize()?);
        }
    }
    Err("official Codex executable not found; set --codex-bin".into())
}

async fn run(cli: Cli) -> Result<u8> {
    let store = Store::new(home(&cli)?)?;
    let manager = Manager::new(store.clone(), &cli.usage_url, &cli.token_url, cli.switch_at)?;
    match cli.command.unwrap_or(Action::Run { args: cli.args }) {
        Action::Run { args } => {
            #[cfg(unix)]
            return terminal::run(
                manager,
                cli.codex_bin,
                args,
                cli.interval,
                !cli.no_resume,
                cli.plain,
            )
            .await;
            #[cfg(not(unix))]
            {
                let _ = args;
                return Err("interactive terminal mode currently requires macOS or Linux".into());
            }
        }
        Action::Add { name, auth_file } => {
            let auth = Auth::read(&auth_file.unwrap_or_else(|| store.home.join("auth.json")))?;
            let _lock = store.lock().await?;
            store.add(&name, auth)?;
            println!("Saved {name}");
        }
        Action::Login { name, device_auth } => {
            accounts::valid_name(&name)?;
            {
                let _lock = store.lock().await?;
                if store.all()?.iter().any(|a| a.name == name) {
                    return Err("account name already exists".into());
                }
            }
            let temp = tempfile::tempdir()?;
            let mut command = native_command(&cli.codex_bin, &store)?;
            command.env("CODEX_HOME", temp.path()).args([
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "login",
            ]);
            if device_auth {
                command.arg("--device-auth");
            }
            let mut child = command.spawn()?;
            let status = child.wait().await?;
            if !status.success() {
                return Err("Codex login failed; existing accounts were preserved".into());
            }
            let auth = Auth::read(&temp.path().join("auth.json"))?;
            let _lock = store.lock().await?;
            store.add(&name, auth)?;
            println!("Saved {name}. Activate with: codexmu switch {name}");
        }
        Action::List { live } => println!(
            "{}",
            serde_json::to_string_pretty(&manager.list(live).await?)?
        ),
        Action::Switch { name } => {
            manager.switch(&name).await?;
            println!("Switched to {name}");
        }
        Action::Priority { name, priority } => {
            let _lock = store.lock().await?;
            store.set_priority(&name, priority)?;
            println!("Set {name} priority to {priority}");
        }
        Action::Remove { name } => {
            let _lock = store.lock().await?;
            store.remove(&name)?;
            println!("Removed {name}");
        }
        Action::Watch { once, dry_run } => {
            let _monitor = store.watch_lock()?;
            if once {
                manager.prepare(None, dry_run).await?;
            } else {
                let mut tick = tokio::time::interval(Duration::from_secs(cli.interval));
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => return Ok(130),
                        _ = tick.tick() => if let Err(e) = manager.prepare(None, dry_run).await { eprintln!("codexmu: {e}"); },
                    }
                }
            }
        }
        Action::AppServer { args } => {
            let args = std::iter::once(OsString::from("app-server"))
                .chain(args)
                .collect::<Vec<_>>();
            return bridge::run(manager, cli.codex_bin, args, cli.interval, !cli.no_resume).await;
        }
        Action::App { id } => {
            #[cfg(target_os = "macos")]
            {
                // open does not update an already-running app's environment. Refuse that no-op.
                let script = format!("application id {} is running", serde_json::to_string(&id)?);
                let running = Command::new("/usr/bin/osascript")
                    .args(["-e", &script])
                    .output()
                    .await?;
                if String::from_utf8_lossy(&running.stdout).trim() == "true" {
                    return Err(
                        "quit the Codex app, then run codexmu app again to enable the bridge"
                            .into(),
                    );
                }
                let native = resolve_binary(&cli.codex_bin)?;
                let exe = std::env::current_exe()?.canonicalize()?;
                if native == exe {
                    return Err("--codex-bin cannot point to codexmu".into());
                }
                manager.credentials().await?;
                let mut launch = Command::new("/usr/bin/open");
                for (key, value) in [
                    ("CODEX_CLI_PATH", exe.to_string_lossy().into_owned()),
                    ("CODEXMU_CODEX_BIN", native.to_string_lossy().into_owned()),
                    ("CODEXMU_BRIDGE", "1".to_owned()),
                    ("CODEX_HOME", store.home.to_string_lossy().into_owned()),
                    ("CODEXMU_INTERVAL", cli.interval.to_string()),
                    ("CODEXMU_NO_RESUME", cli.no_resume.to_string()),
                    ("CODEXMU_SWITCH_AT", cli.switch_at.to_string()),
                    ("CODEXMU_USAGE_URL", cli.usage_url),
                    ("CODEXMU_TOKEN_URL", cli.token_url),
                ] {
                    launch.args(["--env", &format!("{key}={value}")]);
                }
                if !launch.args(["-b", &id]).status().await?.success() {
                    return Err("Codex app launch failed".into());
                }
                println!("Launched Codex with codexmu account switching");
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = id;
                return Err(
                    "app launcher currently supports macOS; use app-server for other clients"
                        .into(),
                );
            }
        }
    }
    Ok(0)
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("codexmu: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result: Result<u8> = runtime.block_on(async {
        // Desktop clients also invoke --version and other Codex commands through CODEX_CLI_PATH.
        if std::env::var_os("CODEXMU_BRIDGE").as_deref() == Some(std::ffi::OsStr::new("1")) {
            let cli = Cli::parse_from(["codexmu", "app-server"]);
            let store = Store::new(home(&cli)?)?;
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            if args.iter().any(|a| a == "app-server") {
                let manager = Manager::new(store, &cli.usage_url, &cli.token_url, cli.switch_at)?;
                return bridge::run(manager, cli.codex_bin, args, cli.interval, !cli.no_resume)
                    .await;
            }
            let status = native_command(&cli.codex_bin, &store)?
                .args(args)
                .status()
                .await?;
            return Ok(status.code().unwrap_or(1) as u8);
        }
        run(Cli::parse()).await
    });
    // Tokio's stdin reader can remain blocked after the child exits or Ctrl+C.
    runtime.shutdown_background();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("codexmu: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_terminal_and_native_arguments_preserve_management_commands() {
        let cli = Cli::try_parse_from(["codexmu"]).unwrap();
        assert!(cli.command.is_none() && cli.args.is_empty());
        let cli = Cli::try_parse_from(["codexmu", "--model", "gpt-5.1", "hello"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.args, ["--model", "gpt-5.1", "hello"]);
        let cli = Cli::try_parse_from(["codexmu", "list", "--live"]).unwrap();
        assert!(matches!(cli.command, Some(Action::List { live: true })));
        let cli = Cli::try_parse_from(["codexmu", "--switch-at", "80", "priority", "work", "-1"])
            .unwrap();
        assert!(
            cli.switch_at == 80
                && matches!(cli.command, Some(Action::Priority { ref name, priority: -1 }) if name == "work")
        );
        assert!(Cli::try_parse_from(["codexmu", "--switch-at", "0"]).is_err());
        let cli = Cli::try_parse_from(["codexmu", "run", "--", "resume", "--last"]).unwrap();
        assert!(matches!(cli.command, Some(Action::Run { args }) if args == ["resume", "--last"]));
    }
}
