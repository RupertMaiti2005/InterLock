//! interlock CLI: dashboard (bare `interlock`), init, status, watch, why, release, undo, name,
//! reap, demo, uninstall.

mod dashboard;
mod demo;
mod init;
mod render;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use interlock_core::client::Client;
use interlock_core::{paths, Request, Response};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "interlock", version, about = "Coordinate multiple coding agents in one worktree")]
struct Cli {
    /// Repo path (defaults to the current directory's repo)
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Live dashboard of agents, files, and events (the default)
    Dash,
    /// Play the demo scenes with mock agents in a scratch repo
    Demo {
        /// Print events instead of opening the dashboard
        #[arg(long)]
        headless: bool,
        /// Speed multiplier (2 = twice as fast)
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
    /// Install hooks for every detected harness, mine hotspots, run a self-test
    Init {
        /// Only install for these harnesses (claude, codex, gemini, copilot, cursor)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Skip the self-test
        #[arg(long)]
        no_test: bool,
    },
    /// Remove interlock hooks and restore backed-up settings
    Uninstall,
    /// One-shot view of agents, files, and recent events
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stream events as they happen
    Watch {
        #[arg(long)]
        json: bool,
    },
    /// Explain who holds a file and who is waiting
    Why { path: String },
    /// Release a lease (yours, or anyone's with --force)
    Release {
        path: Option<String>,
        #[arg(long)]
        force: bool,
        /// Release every lease held by this session id
        #[arg(long)]
        session: Option<String>,
    },
    /// Restore a file from its snapshot history
    Undo {
        path: String,
        #[arg(long, default_value_t = 1)]
        steps: usize,
    },
    /// Give a session a human-readable label
    Name { session: String, label: String },
    /// Reap a session as if it had gone silent
    Reap { session: String },
    /// Is the daemon up?
    Ping,
    /// Ask the daemon to exit (it restarts on the next hook)
    Stop,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("interlock: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Demo { headless, speed }) = &cli.cmd {
        return demo::run(*headless, *speed);
    }
    let start = cli.repo.clone().unwrap_or(std::env::current_dir()?);
    let repo_root = paths::find_repo_root(&start).ok_or_else(|| anyhow!("not inside a git repository"))?;
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();

    match cli.cmd.unwrap_or(Cmd::Dash) {
        Cmd::Dash => dashboard::run(&repo_root, None),
        Cmd::Demo { .. } => unreachable!(),
        Cmd::Init { only, no_test } => init::run(&repo_root, &only, !no_test),
        Cmd::Uninstall => init::uninstall(&repo_root),
        Cmd::Status { json } => {
            let snap = match connect(&repo_root) {
                Ok(mut c) => match c.request(&Request::Status, Duration::from_secs(2))? {
                    Response::Status(s) => Some(s),
                    other => bail!("unexpected reply: {other:?}"),
                },
                Err(_) => None,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print!("{}", render::status(&repo_root, snap.as_ref()));
            }
            Ok(())
        }
        Cmd::Watch { json } => {
            let c = Client::connect_or_start(&repo_root, Duration::from_secs(3)).context("daemon unreachable")?;
            for ev in c.subscribe()? {
                let ev = ev?;
                if json {
                    println!("{}", serde_json::to_string(&ev)?);
                } else {
                    println!("{}", render::event_line(&ev));
                }
            }
            Ok(())
        }
        Cmd::Why { path } => {
            let mut c = connect(&repo_root)?;
            match c.request(&Request::Why { path, cwd }, Duration::from_secs(2))? {
                Response::Why(w) => print!("{}", render::why(&w)),
                Response::Error { message } => bail!(message),
                other => bail!("unexpected reply: {other:?}"),
            }
            Ok(())
        }
        Cmd::Release { path, force, session } => {
            let mut c = connect(&repo_root)?;
            let resp = match (path, session) {
                (Some(p), _) if force => c.request(&Request::ForceRelease { path: p, cwd }, Duration::from_secs(2))?,
                (Some(p), Some(s)) => c.request(&Request::Release { session: s, path: p, cwd }, Duration::from_secs(2))?,
                (None, Some(s)) => c.request(&Request::ReleaseAll { session: s }, Duration::from_secs(2))?,
                (Some(_), None) => bail!("pass --session <id> to release as that session, or --force to release regardless of holder"),
                (None, None) => bail!("give a path, or --session <id>"),
            };
            expect_ok(resp)
        }
        Cmd::Undo { path, steps } => {
            let mut c = connect(&repo_root)?;
            match c.request(&Request::Undo { path, cwd, steps }, Duration::from_secs(5))? {
                Response::Restored { path, blob_oid } => {
                    println!("restored {path} from snapshot {}", &blob_oid[..8]);
                    Ok(())
                }
                Response::Error { message } => bail!(message),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Name { session, label } => {
            let mut c = connect(&repo_root)?;
            expect_ok(c.request(&Request::SetLabel { session, label }, Duration::from_secs(2))?)
        }
        Cmd::Reap { session } => {
            let mut c = connect(&repo_root)?;
            expect_ok(c.request(&Request::Reap { session }, Duration::from_secs(2))?)
        }
        Cmd::Ping => {
            let mut c = connect(&repo_root)?;
            match c.request(&Request::Ping, Duration::from_secs(2))? {
                Response::Pong { pid, repo, uptime_ms } => {
                    println!("daemon ok  pid {pid}  up {}  {repo}", interlock_core::fmt_ms(uptime_ms));
                    Ok(())
                }
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Stop => {
            let mut c = connect(&repo_root)?;
            expect_ok(c.request(&Request::Shutdown, Duration::from_secs(2))?)
        }
    }
}

fn connect(repo_root: &std::path::Path) -> Result<Client> {
    Client::connect(repo_root, Duration::from_millis(200)).map_err(|_| anyhow!("daemon is not running for this repo (it starts on the first hook, or run `interlock init`)"))
}

fn expect_ok(resp: Response) -> Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!(message),
        other => bail!("unexpected reply: {other:?}"),
    }
}
