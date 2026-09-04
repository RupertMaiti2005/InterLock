//! `interlock demo`: a scratch repo, a real daemon, and scripted mock agents playing the
//! four demo scenes (SPEC §12) in the dashboard. No real agents, no API cost.

use anyhow::{Context, Result};
use interlock_core::client::Client;
use interlock_core::{discovery, Harness, Request, Response};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const T: Duration = Duration::from_secs(3);

struct Agent {
    id: String,
    client: Client,
    cwd: String,
}

impl Agent {
    fn new(repo: &Path, id: &str, harness: Harness, label: &str) -> Result<Agent> {
        let mut client = Client::connect(repo, Duration::from_millis(500)).context("connect mock agent")?;
        let cwd = repo.to_string_lossy().to_string();
        client.request(&Request::Hello { session: id.into(), harness, label: Some(label.into()), cwd: cwd.clone() }, T)?;
        Ok(Agent { id: id.into(), client, cwd })
    }
    fn read(&mut self, path: &str) -> Result<()> {
        self.client.request(&Request::RecordRead { session: self.id.clone(), paths: vec![path.into()], cwd: self.cwd.clone() }, T)?;
        Ok(())
    }
    /// Full write path as the shim does it: validate, acquire (blocking), then write and report.
    fn write(&mut self, repo: &Path, path: &str, content: &str, cap_ms: u64) -> Result<Response> {
        let v = self.client.request(&Request::ValidateWrite { session: self.id.clone(), targets: vec![path.into()], cwd: self.cwd.clone() }, T)?;
        if let Response::Stale { .. } = v {
            return Ok(v);
        }
        let r = self.client.request(
            &Request::Acquire { session: self.id.clone(), path: path.into(), cwd: self.cwd.clone(), blocking: true, cap_ms },
            Duration::from_millis(cap_ms + 2000),
        )?;
        if let Response::Granted { .. } = r {
            std::fs::write(repo.join(path), content)?;
            self.client.request(&Request::WriteDone { session: self.id.clone(), paths: vec![path.into()], cwd: self.cwd.clone() }, T)?;
        }
        Ok(r)
    }
    fn end_turn(&mut self) -> Result<()> {
        self.client.request(&Request::ReleaseAll { session: self.id.clone() }, T)?;
        Ok(())
    }
    fn end_session(&mut self) -> Result<()> {
        self.client.request(&Request::SessionEnd { session: self.id.clone() }, T)?;
        Ok(())
    }
}

fn make_repo() -> Result<PathBuf> {
    let dir = discovery::interlock_home().join("demo-repo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    let run = |args: &[&str]| -> Result<()> {
        let st = std::process::Command::new("git").args(args).current_dir(&dir).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
        anyhow::ensure!(st.success(), "git {:?} failed", args);
        Ok(())
    };
    run(&["init", "-q", "."])?;
    run(&["config", "user.email", "demo@interlock"])?;
    run(&["config", "user.name", "interlock demo"])?;
    std::fs::write(dir.join("src/auth.ts"), "export function auth(token: string) {\n  return verify(token);\n}\n")?;
    std::fs::write(dir.join("src/routes.ts"), "import { auth } from './auth';\napp.get('/me', (req) => auth(req.token));\n")?;
    std::fs::write(dir.join("src/db.ts"), "export const db = connect();\n")?;
    run(&["add", "-A"])?;
    run(&["commit", "-qm", "init"])?;
    Ok(interlock_core::paths::find_repo_root(&dir).unwrap_or(dir))
}

pub fn run(headless: bool, speed: f64) -> Result<()> {
    let repo = make_repo()?;
    // Fresh daemon for the scratch repo.
    if let Ok(mut c) = Client::connect(&repo, Duration::from_millis(200)) {
        let _ = c.request(&Request::Shutdown, T);
        std::thread::sleep(Duration::from_millis(300));
    }
    Client::connect_or_start(&repo, Duration::from_secs(5)).context("start daemon for demo repo")?;

    if headless {
        // Print events from a background thread; the script runs here so the process exits when it ends.
        let printer_repo = repo.clone();
        std::thread::spawn(move || {
            if let Ok(c) = Client::connect(&printer_repo, Duration::from_millis(500)) {
                if let Ok(iter) = c.subscribe() {
                    for ev in iter.flatten() {
                        println!("{}", crate::render::event_line(&ev));
                    }
                }
            }
        });
        let outcome = scenes(&repo, speed, true);
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(mut c) = Client::connect(&repo, Duration::from_millis(200)) {
            let _ = c.request(&Request::Shutdown, T);
        }
        return outcome;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let script_stop = stop.clone();
    let script_repo = repo.clone();
    let script = std::thread::spawn(move || {
        let r = scenes(&script_repo, speed, false);
        script_stop.store(true, Ordering::Relaxed);
        r
    });
    println!("interlock demo: scratch repo at {}", repo.display());
    std::thread::sleep(Duration::from_millis(600));
    crate::dashboard::run(&repo, Some(stop.clone()))?;
    stop.store(true, Ordering::Relaxed);
    let outcome = script.join().unwrap_or_else(|_| Err(anyhow::anyhow!("demo script panicked")));
    if let Ok(mut c) = Client::connect(&repo, Duration::from_millis(200)) {
        let _ = c.request(&Request::Shutdown, T);
    }
    outcome?;
    println!("demo finished. Scenes: contention + hand-off, crash reap, cross-file stale read, mixed fleet.");
    Ok(())
}

fn pause(secs: f64, speed: f64) {
    std::thread::sleep(Duration::from_millis((secs * 1000.0 / speed.max(0.1)) as u64));
}

fn scenes(repo: &Path, speed: f64, headless: bool) -> Result<()> {
    let say = |s: &str| {
        if headless {
            println!("--- {s}");
        }
    };
    pause(1.0, speed);

    // Scene 1: contention and hand-off.
    say("scene 1: A edits auth.ts, B wants it too and waits; A's turn ends, B proceeds");
    let mut a = Agent::new(repo, "sess-a3f1", Harness::Claude, "add rate limiting to auth")?;
    let mut b = Agent::new(repo, "sess-9c02", Harness::Claude, "fix login redirect")?;
    a.read("src/auth.ts")?;
    a.write(repo, "src/auth.ts", "export function auth(token: string, limit = 100) {\n  return verify(token, limit);\n}\n", 1000)?;
    pause(1.5, speed);
    let repo_b = repo.to_path_buf();
    let b_thread = std::thread::spawn(move || -> Result<Agent> {
        b.read("src/auth.ts")?;
        let _ = b.write(&repo_b, "src/auth.ts", "export function auth(token: string, limit = 100) {\n  if (!token) redirect('/login');\n  return verify(token, limit);\n}\n", 20_000)?;
        Ok(b)
    });
    pause(4.0, speed);
    a.end_turn()?;
    let mut b = b_thread.join().unwrap()?;
    pause(1.5, speed);
    b.end_turn()?;

    // Scene 2: crash recovery.
    say("scene 2: A takes db.ts and goes silent; a human reaps it; B picks it up");
    a.write(repo, "src/db.ts", "export const db = connect({ pool: 10 });\n", 1000)?;
    pause(2.0, speed);
    {
        let mut c = Client::connect(repo, Duration::from_millis(500))?;
        c.request(&Request::Reap { session: a.id.clone() }, T)?;
    }
    pause(1.0, speed);
    b.write(repo, "src/db.ts", "export const db = connect({ pool: 20 });\n", 1000)?;
    pause(1.5, speed);
    b.end_turn()?;

    // Scene 3: cross-file stale read.
    say("scene 3: C reads auth.ts; A changes its signature; C writes routes.ts against the old one");
    let mut a = Agent::new(repo, "sess-a3f1", Harness::Claude, "add rate limiting to auth")?;
    let mut c = Agent::new(repo, "sess-77be", Harness::Codex, "update route table")?;
    c.read("src/auth.ts")?;
    c.read("src/routes.ts")?;
    pause(2.0, speed);
    a.write(repo, "src/auth.ts", "export function auth(req: Request, limit = 100) {\n  return verify(req.token, limit);\n}\n", 1000)?;
    a.end_turn()?;
    pause(2.0, speed);
    let r = c.write(repo, "src/routes.ts", "import { auth } from './auth';\napp.get('/me', (req) => auth(req.token));\napp.get('/admin', (req) => auth(req.token));\n", 1000)?;
    if headless {
        println!("    C's write -> {r:?}");
    }
    pause(3.0, speed);
    c.read("src/auth.ts")?;
    pause(0.5, speed);
    c.write(repo, "src/routes.ts", "import { auth } from './auth';\napp.get('/me', (req) => auth(req));\napp.get('/admin', (req) => auth(req));\n", 1000)?;
    pause(1.5, speed);
    c.end_turn()?;

    // Scene 4: mixed fleet contention with a timeout, then undo.
    say("scene 4: claude and codex contend; codex times out and is told to wait; then undo");
    a.write(repo, "src/auth.ts", "export function auth(req: Request, limit = 50) {\n  return verify(req.token, limit);\n}\n", 1000)?;
    pause(1.0, speed);
    let r = c.write(repo, "src/auth.ts", "// codex\n", (3.0 * 1000.0 / speed.max(0.1)) as u64)?;
    if headless {
        println!("    codex's write -> {r:?}");
    }
    pause(1.0, speed);
    a.end_turn()?;
    {
        let mut cli = Client::connect(repo, Duration::from_millis(500))?;
        cli.request(&Request::Undo { path: "src/auth.ts".into(), cwd: repo.to_string_lossy().to_string(), steps: 1 }, T)?;
    }
    pause(3.0, speed);
    a.end_session()?;
    b.end_session()?;
    c.end_session()?;
    pause(if headless { 0.5 } else { 6.0 }, speed);
    Ok(())
}
