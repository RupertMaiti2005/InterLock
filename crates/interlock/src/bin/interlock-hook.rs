//! interlock-hook: the shim every harness invokes. Reads the hook payload on stdin, does one
//! round trip to the daemon, replies in the harness's format. Fails open on every error path.

use interlock_core::client::Client;
use interlock_core::hooks::{self, HookEvent, HookKind, ReplyStyle};
use interlock_core::{discovery, messages, paths, Harness, Request, Response};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);

enum Verdict {
    Allow,
    Deny(String),
}

fn main() {
    let started = Instant::now();
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        allow_and_exit(ReplyStyle::ExitCode);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        allow_and_exit(ReplyStyle::ExitCode);
    };
    let ev = hooks::normalize(&v);
    let style = hooks::reply_style(ev.harness);

    // Never let a panic turn into a non-zero exit (Copilot would treat that as deny).
    let result = std::panic::catch_unwind(|| run(&ev, started));
    match result {
        Ok(Verdict::Allow) | Err(_) => allow_and_exit(style),
        Ok(Verdict::Deny(msg)) => deny_and_exit(style, &msg),
    }
}

fn run(ev: &HookEvent, started: Instant) -> Verdict {
    if matches!(ev.kind, HookKind::Ignore) {
        return Verdict::Allow;
    }
    let repo_root = match repo_root_for(ev) {
        Some(r) => r,
        None => return Verdict::Allow,
    };
    let cwd = ev.cwd.to_string_lossy().to_string();

    let mut client = match Client::connect(&repo_root, CONNECT_TIMEOUT) {
        Ok(c) => c,
        Err(_) => {
            // Start the daemon for next time; this call passes through (SPEC §6.7).
            let _ = discovery::spawn_daemon(&repo_root);
            // Give it a moment on write paths so the very first contended write is still covered.
            if ev.can_block {
                match Client::connect_or_start(&repo_root, Duration::from_millis(400)) {
                    Ok(c) => c,
                    Err(_) => {
                        log(&repo_root, ev, "degraded: daemon unreachable, allowing", started);
                        return Verdict::Allow;
                    }
                }
            } else {
                log(&repo_root, ev, "degraded: daemon unreachable, allowing", started);
                return Verdict::Allow;
            }
        }
    };

    let session = ev.session.clone();
    let verdict = match &ev.kind {
        HookKind::PromptSubmit { prompt } => {
            let _ = client.request(
                &Request::Hello { session, harness: ev.harness, label: Some(prompt.clone()), cwd },
                REQUEST_TIMEOUT,
            );
            Verdict::Allow
        }
        HookKind::PreRead(paths) | HookKind::PostRead(paths) => {
            let _ = client.request(&Request::RecordRead { session, paths: paths.clone(), cwd }, REQUEST_TIMEOUT);
            Verdict::Allow
        }
        HookKind::PreWrite(paths) => pre_write(&mut client, ev, &session, &cwd, paths),
        HookKind::PostWrite(paths) => {
            let _ = client.request(&Request::WriteDone { session, paths: paths.clone(), cwd }, REQUEST_TIMEOUT);
            Verdict::Allow
        }
        HookKind::PostTool => {
            let _ = client.request(&Request::Heartbeat { session, tool: ev.tool.clone() }, REQUEST_TIMEOUT);
            Verdict::Allow
        }
        HookKind::TurnEnd => {
            let _ = client.request(&Request::ReleaseAll { session }, REQUEST_TIMEOUT);
            Verdict::Allow
        }
        HookKind::SessionEnd => {
            let _ = client.request(&Request::SessionEnd { session }, REQUEST_TIMEOUT);
            Verdict::Allow
        }
        HookKind::Ignore => Verdict::Allow,
    };
    let what = match &verdict {
        Verdict::Allow => "allow".to_string(),
        Verdict::Deny(m) => format!("deny: {}", m.lines().next().unwrap_or("")),
    };
    log(&repo_root, ev, &what, started);
    verdict
}

fn pre_write(client: &mut Client, ev: &HookEvent, session: &str, cwd: &str, paths: &[String]) -> Verdict {
    // Tier 2 harnesses cannot block; record intent only.
    if !ev.can_block {
        let _ = client.request(&Request::Heartbeat { session: session.into(), tool: ev.tool.clone() }, REQUEST_TIMEOUT);
        return Verdict::Allow;
    }
    // 1. Freshness first, so a stale reader is told to re-read instead of waiting on a lease.
    match client.request(
        &Request::ValidateWrite { session: session.into(), targets: paths.to_vec(), cwd: cwd.into() },
        REQUEST_TIMEOUT,
    ) {
        Ok(Response::Stale { path, read_ago_ms, changed_by }) => {
            return Verdict::Deny(messages::stale(&path, read_ago_ms, changed_by.as_deref()));
        }
        Ok(Response::Fresh) | Ok(Response::Ok) => {}
        Ok(_) | Err(_) => return Verdict::Allow,
    }
    // 2. Acquire each target, blocking up to the harness cap.
    let cap_ms = std::env::var("INTERLOCK_WAIT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(ev.harness.default_cap_ms());
    for p in paths {
        let req = Request::Acquire { session: session.into(), path: p.clone(), cwd: cwd.into(), blocking: true, cap_ms };
        match client.request(&req, Duration::from_millis(cap_ms + 2000)) {
            Ok(Response::Granted { .. }) => {}
            Ok(Response::Stale { path, read_ago_ms, changed_by }) => {
                return Verdict::Deny(messages::stale(&path, read_ago_ms, changed_by.as_deref()));
            }
            Ok(Response::Blocked { path, holder_label, .. }) => {
                return Verdict::Deny(messages::blocked(&path, holder_label.as_deref()));
            }
            Ok(Response::Deadlock { path, other_wants, .. }) => {
                return Verdict::Deny(messages::deadlock(&path, &other_wants));
            }
            Ok(_) | Err(_) => return Verdict::Allow,
        }
    }
    Verdict::Allow
}

fn repo_root_for(ev: &HookEvent) -> Option<PathBuf> {
    let first_path = match &ev.kind {
        HookKind::PreRead(p) | HookKind::PostRead(p) | HookKind::PreWrite(p) | HookKind::PostWrite(p) => p.first().cloned(),
        _ => None,
    };
    if let Some(p) = first_path {
        let pp = Path::new(&p);
        let abs = if pp.is_absolute() { pp.to_path_buf() } else { ev.cwd.join(pp) };
        if let Some(r) = paths::find_repo_root(&abs) {
            return Some(r);
        }
    }
    paths::find_repo_root(&ev.cwd)
}

fn log(repo_root: &Path, ev: &HookEvent, what: &str, started: Instant) {
    let kind = match &ev.kind {
        HookKind::PromptSubmit { .. } => "prompt".to_string(),
        HookKind::PreRead(p) => format!("pre-read {}", p.join(",")),
        HookKind::PostRead(p) => format!("post-read {}", p.join(",")),
        HookKind::PreWrite(p) => format!("pre-write {}", p.join(",")),
        HookKind::PostWrite(p) => format!("post-write {}", p.join(",")),
        HookKind::PostTool => "post-tool".to_string(),
        HookKind::TurnEnd => "turn-end".to_string(),
        HookKind::SessionEnd => "session-end".to_string(),
        HookKind::Ignore => "ignore".to_string(),
    };
    discovery::log_line(
        repo_root,
        "hook",
        &format!("{} {} {} -> {} ({}us)", ev.harness.short(), ev.session, kind, what, started.elapsed().as_micros()),
    );
}

fn allow_and_exit(style: ReplyStyle) -> ! {
    match style {
        ReplyStyle::ExitCode => {}
        ReplyStyle::GeminiJson => println!("{{}}"),
        ReplyStyle::CopilotJson => println!("{{\"permissionDecision\":\"allow\"}}"),
        ReplyStyle::CursorJson => println!("{{\"permission\":\"allow\"}}"),
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

fn deny_and_exit(style: ReplyStyle, msg: &str) -> ! {
    match style {
        ReplyStyle::ExitCode => {
            eprintln!("{msg}");
            let _ = std::io::stderr().flush();
            std::process::exit(2)
        }
        ReplyStyle::GeminiJson => {
            println!("{}", serde_json::json!({"decision": "deny", "reason": msg}));
        }
        ReplyStyle::CopilotJson => {
            println!("{}", serde_json::json!({"permissionDecision": "deny", "permissionDecisionReason": msg}));
        }
        ReplyStyle::CursorJson => {
            println!("{}", serde_json::json!({"permission": "deny", "user_message": msg}));
        }
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

#[allow(dead_code)]
fn _harness_unused(_: Harness) {}
