//! `interlock init` / `interlock uninstall`: hook installation per harness, hotspot mining,
//! worktree detection, self-test. Claude Code and Codex share the hooks.json shape.

use anyhow::{Context, Result};
use interlock_core::client::Client;
use interlock_core::{discovery, paths, Harness, Request, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Target {
    harness: Harness,
    file: PathBuf,
    present: bool,
}

fn targets(repo_root: &Path) -> Vec<Target> {
    let home = dirs_home();
    let has = |p: &Path| p.exists();
    vec![
        Target {
            harness: Harness::Claude,
            file: repo_root.join(".claude").join("settings.local.json"),
            present: has(&home.join(".claude")) || has(&repo_root.join(".claude")),
        },
        Target {
            harness: Harness::Codex,
            file: repo_root.join(".codex").join("hooks.json"),
            present: has(&home.join(".codex")) || has(&repo_root.join(".codex")),
        },
        Target {
            harness: Harness::Gemini,
            file: repo_root.join(".gemini").join("settings.json"),
            present: has(&home.join(".gemini")) || has(&repo_root.join(".gemini")),
        },
        Target {
            harness: Harness::Copilot,
            file: repo_root.join(".github").join("hooks").join("interlock.json"),
            present: has(&home.join(".copilot")),
        },
        Target {
            harness: Harness::Cursor,
            file: repo_root.join(".cursor").join("hooks.json"),
            present: has(&home.join(".cursor")) || has(&repo_root.join(".cursor")),
        },
    ]
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn hook_cmd() -> String {
    let p = discovery::sibling_binary("interlock-hook");
    let s = p.to_string_lossy().replace('\\', "/");
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s
    }
}

fn cmd_entry(cmd: &str, timeout_s: u64) -> Value {
    json!({"type": "command", "command": cmd, "timeout": timeout_s})
}

/// Claude Code and Codex: `{"hooks": {Event: [{matcher, hooks: [...]}]}}`
fn claude_style_hooks(cmd: &str, write_timeout: u64) -> Value {
    json!({
        "UserPromptSubmit": [{"hooks": [cmd_entry(cmd, 5)]}],
        "PreToolUse": [{"matcher": "Read|Edit|Write|MultiEdit|NotebookEdit|Bash|apply_patch", "hooks": [cmd_entry(cmd, write_timeout)]}],
        "PostToolUse": [{"matcher": "", "hooks": [cmd_entry(cmd, 5)]}],
        "Stop": [{"hooks": [cmd_entry(cmd, 5)]}],
        "SessionEnd": [{"hooks": [cmd_entry(cmd, 5)]}],
    })
}

fn gemini_hooks(cmd: &str) -> Value {
    json!({
        "BeforeAgent": [{"hooks": [cmd_entry(cmd, 5)]}],
        "BeforeTool": [{"matcher": "read_file|write_file|replace|run_shell_command", "hooks": [cmd_entry(cmd, 60)]}],
        "AfterTool": [{"matcher": ".*", "hooks": [cmd_entry(cmd, 5)]}],
        "AfterAgent": [{"hooks": [cmd_entry(cmd, 5)]}],
        "SessionEnd": [{"hooks": [cmd_entry(cmd, 5)]}],
    })
}

fn copilot_hooks(cmd: &str) -> Value {
    json!({
        "version": 1,
        "hooks": {
            "userPromptSubmitted": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 5}],
            "preToolUse": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 30}],
            "postToolUse": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 5}],
            "agentStop": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 5}],
            "sessionEnd": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 5}]
        }
    })
}

fn cursor_hooks(cmd: &str) -> Value {
    json!({
        "version": 1,
        "hooks": {
            "beforeSubmitPrompt": [{"command": cmd}],
            "beforeReadFile": [{"command": cmd}],
            "afterFileEdit": [{"command": cmd}],
            "beforeShellExecution": [{"command": cmd}],
            "afterShellExecution": [{"command": cmd}],
            "stop": [{"command": cmd}],
            "sessionEnd": [{"command": cmd}]
        }
    })
}

fn backup_dir(repo_root: &Path) -> PathBuf {
    discovery::state_dir(repo_root).join("backup")
}

fn backup_name(h: Harness) -> String {
    format!("{}.json", h.short())
}

fn is_ours(v: &Value) -> bool {
    let text = v.to_string();
    text.contains("interlock-hook")
}

/// Merge our hook groups into an existing `{"hooks": {...}}` document, replacing prior interlock entries.
fn merge_hooks(existing: &mut Value, ours: &Value) {
    if !existing.is_object() {
        *existing = json!({});
    }
    let hooks = existing.as_object_mut().unwrap().entry("hooks").or_insert(json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    // Strip any prior interlock groups everywhere.
    for (_, groups) in hooks.iter_mut() {
        if let Some(arr) = groups.as_array_mut() {
            arr.retain(|g| !is_ours(g));
        }
    }
    for (event, groups) in ours.as_object().unwrap() {
        let arr = hooks.entry(event.clone()).or_insert(json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        arr.as_array_mut().unwrap().extend(groups.as_array().unwrap().iter().cloned());
    }
}

fn strip_hooks(existing: &mut Value) {
    let Some(hooks) = existing.get_mut("hooks").and_then(Value::as_object_mut) else { return };
    let mut empty = Vec::new();
    for (ev, groups) in hooks.iter_mut() {
        if let Some(arr) = groups.as_array_mut() {
            arr.retain(|g| !is_ours(g));
            if arr.is_empty() {
                empty.push(ev.clone());
            }
        }
    }
    for ev in empty {
        hooks.remove(&ev);
    }
    if hooks.is_empty() {
        existing.as_object_mut().unwrap().remove("hooks");
    }
}

fn install_one(repo_root: &Path, t: &Target, cmd: &str) -> Result<String> {
    let existing_bytes = std::fs::read(&t.file).ok();
    // Keep the first backup as the pristine pre-install copy.
    let bdir = backup_dir(repo_root);
    std::fs::create_dir_all(&bdir)?;
    let bpath = bdir.join(backup_name(t.harness));
    if !bpath.exists() {
        match &existing_bytes {
            Some(b) => std::fs::write(&bpath, b)?,
            None => std::fs::write(&bpath, b"")?, // empty marker: file did not exist
        }
    }
    let mut doc: Value = existing_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok())
        .unwrap_or(json!({}));

    match t.harness {
        Harness::Claude | Harness::Codex => merge_hooks(&mut doc, &claude_style_hooks(cmd, 60)),
        Harness::Gemini => merge_hooks(&mut doc, &gemini_hooks(cmd)),
        Harness::Copilot => doc = copilot_hooks(cmd),
        Harness::Cursor => doc = cursor_hooks(cmd),
        _ => {}
    }
    if let Some(parent) = t.file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&t.file, serde_json::to_string_pretty(&doc)? + "\n")?;
    Ok(t.file.strip_prefix(repo_root).unwrap_or(&t.file).to_string_lossy().replace('\\', "/"))
}

fn restore_one(repo_root: &Path, t: &Target) -> Result<&'static str> {
    let bpath = backup_dir(repo_root).join(backup_name(t.harness));
    if !t.file.exists() {
        return Ok("not installed");
    }
    let mut current: Value = serde_json::from_slice(&std::fs::read(&t.file)?).unwrap_or(json!({}));
    if !is_ours(&current) {
        return Ok("not installed");
    }
    match t.harness {
        Harness::Copilot | Harness::Cursor => current = json!({}),
        _ => strip_hooks(&mut current),
    }
    if let Ok(backup) = std::fs::read(&bpath) {
        if backup.is_empty() {
            // File did not exist before install.
            if current == json!({}) || current.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                std::fs::remove_file(&t.file)?;
                return Ok("removed (did not exist before install)");
            }
        } else if let Ok(bdoc) = serde_json::from_slice::<Value>(&backup) {
            if bdoc == current {
                std::fs::write(&t.file, &backup)?;
                return Ok("restored byte-identical from backup");
            }
        }
    }
    std::fs::write(&t.file, serde_json::to_string_pretty(&current)? + "\n")?;
    Ok("interlock entries removed (file changed since install, kept your edits)")
}

pub fn run(repo_root: &Path, only: &[String], self_test: bool) -> Result<()> {
    let name = repo_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    println!("interlock init · {name}");
    println!("  repo      {}", repo_root.display());
    if paths::is_worktree(repo_root) {
        println!("  note      this is a git worktree. interlock adds little where worktrees already isolate agents.");
    }
    let cmd = hook_cmd();
    println!("  hook      {cmd}");
    let daemon = discovery::sibling_binary("interlockd");
    if !daemon.exists() && daemon.components().count() > 1 {
        println!("  warning   interlockd not found next to this binary; hooks will fail open");
    }

    println!("\nHARNESSES");
    let mut installed = 0;
    for t in targets(repo_root) {
        let wanted = only.is_empty() || only.iter().any(|o| o.eq_ignore_ascii_case(t.harness.short()));
        if !wanted {
            continue;
        }
        if !t.present && only.is_empty() {
            println!("  - {:<8} not found, skipped (use --only {} to force)", t.harness.short(), t.harness.short());
            continue;
        }
        match install_one(repo_root, &t, &cmd) {
            Ok(f) => {
                installed += 1;
                let tier = match t.harness.tier() {
                    1 => "tier 1: blocks conflicting writes, catches stale reads",
                    2 => "tier 2: detect-only (no pre-edit hook in this harness)",
                    _ => "tier 3",
                };
                println!("  ✓ {:<8} {f}  ({tier})", t.harness.short());
                if t.harness == Harness::Codex {
                    match enable_codex_hooks_feature() {
                        Ok(true) => println!("             enabled `hooks` feature in ~/.codex/config.toml (it is off by default)"),
                        Ok(false) => {}
                        Err(e) => println!("             note: could not enable the Codex `hooks` feature automatically ({e}); add `[features]\\nhooks = true` to ~/.codex/config.toml"),
                    }
                    println!("             in interactive Codex run /hooks once to trust the interlock hook; for `codex exec` pass --dangerously-bypass-hook-trust");
                }
            }
            Err(e) => println!("  ✗ {:<8} {e:#}", t.harness.short()),
        }
    }
    if installed == 0 {
        println!("  nothing installed. Pass --only claude,codex,... to install for a harness that was not detected.");
    }

    println!("\nHOTSPOTS");
    match mine_hotspots(repo_root) {
        Ok(list) if list.is_empty() => println!("  none (repo has too little history)"),
        Ok(list) => {
            for (p, n) in list.iter().take(10) {
                println!("  {:<48} {n} commits", p);
            }
            let keys: Vec<&String> = list.iter().map(|(p, _)| p).collect();
            let dir = discovery::state_dir(repo_root);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("hotspots.json"), serde_json::to_string(&keys)?)?;
        }
        Err(e) => println!("  skipped: {e:#}"),
    }

    println!("\nDAEMON");
    match Client::connect_or_start(repo_root, Duration::from_secs(3)) {
        Ok(mut c) => match c.request(&Request::Ping, Duration::from_secs(2)) {
            Ok(Response::Pong { pid, .. }) => println!("  ✓ running (pid {pid})"),
            _ => println!("  ? started but not answering"),
        },
        Err(e) => println!("  ✗ could not start: {e:#}"),
    }

    if self_test {
        println!("\nSELF-TEST");
        match self_test_run(repo_root) {
            Ok(lines) => lines.iter().for_each(|l| println!("  {l}")),
            Err(e) => println!("  ✗ {e:#}"),
        }
    }
    println!("\nDone. Run `interlock status` any time, or `interlock watch` to follow events live.");
    Ok(())
}

/// Codex ships with the `hooks` feature off. Turn it on in the user config. Returns true if changed.
fn enable_codex_hooks_feature() -> Result<bool> {
    let path = dirs_home().join(".codex").join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    // Already enabled anywhere under [features]?
    let mut in_features = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_features = l == "[features]";
        } else if in_features && l.starts_with("hooks") && l.contains("true") {
            return Ok(false);
        }
    }
    let mut out = String::new();
    let mut inserted = false;
    for line in text.lines() {
        out.push_str(line);
        out.push('\n');
        if line.trim() == "[features]" && !inserted {
            out.push_str("hooks = true\n");
            inserted = true;
        }
    }
    if !inserted {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n[features]\nhooks = true\n");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, out)?;
    Ok(true)
}

pub fn uninstall(repo_root: &Path) -> Result<()> {
    println!("interlock uninstall");
    for t in targets(repo_root) {
        match restore_one(repo_root, &t) {
            Ok(what) => println!("  {:<8} {what}", t.harness.short()),
            Err(e) => println!("  {:<8} ✗ {e:#}", t.harness.short()),
        }
    }
    if let Ok(mut c) = Client::connect(repo_root, Duration::from_millis(200)) {
        let _ = c.request(&Request::Shutdown, Duration::from_secs(2));
        println!("  daemon   stopped");
    }
    println!("  note     snapshots stay on refs/interlock/history; delete with: git update-ref -d refs/interlock/history");
    Ok(())
}

/// Top files by commit touch count over recent history (SPEC §6.5).
pub fn mine_hotspots(repo_root: &Path) -> Result<Vec<(String, usize)>> {
    let out = std::process::Command::new("git")
        .args(["log", "--name-only", "--pretty=format:", "-n", "5000"])
        .current_dir(repo_root)
        .output()
        .context("run git log")?;
    if !out.status.success() {
        anyhow::bail!("git log failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || is_noise(l) {
            continue;
        }
        *counts.entry(l.to_string()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().filter(|(_, n)| *n >= 3).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(10);
    Ok(v)
}

fn is_noise(p: &str) -> bool {
    let base = p.rsplit('/').next().unwrap_or(p).to_lowercase();
    matches!(
        base.as_str(),
        "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "cargo.lock" | "poetry.lock" | "go.sum" | "changelog.md" | "changelog" | "gemfile.lock" | "composer.lock"
    )
}

/// Two mock sessions contend on one path through the real daemon.
fn self_test_run(repo_root: &Path) -> Result<Vec<String>> {
    let cwd = repo_root.to_string_lossy().to_string();
    let t = Duration::from_secs(2);
    let mut a = Client::connect(repo_root, Duration::from_millis(300)).context("connect A")?;
    let mut b = Client::connect(repo_root, Duration::from_millis(300)).context("connect B")?;
    let path = ".interlock-selftest".to_string();
    let mut lines = Vec::new();
    a.request(&Request::Hello { session: "selftest-A".into(), harness: Harness::Mock, label: Some("self-test A".into()), cwd: cwd.clone() }, t)?;
    b.request(&Request::Hello { session: "selftest-B".into(), harness: Harness::Mock, label: Some("self-test B".into()), cwd: cwd.clone() }, t)?;
    match a.request(&Request::Acquire { session: "selftest-A".into(), path: path.clone(), cwd: cwd.clone(), blocking: true, cap_ms: 1000 }, t)? {
        Response::Granted { .. } => lines.push("✓ A acquired the test file".into()),
        other => anyhow::bail!("A expected Granted, got {other:?}"),
    }
    match b.request(&Request::Acquire { session: "selftest-B".into(), path: path.clone(), cwd: cwd.clone(), blocking: false, cap_ms: 0 }, t)? {
        Response::Blocked { .. } => lines.push("✓ B was told A holds it".into()),
        other => anyhow::bail!("B expected Blocked, got {other:?}"),
    }
    // B blocks for real while A releases from another connection.
    b.send(&Request::Acquire { session: "selftest-B".into(), path: path.clone(), cwd: cwd.clone(), blocking: true, cap_ms: 3000 })?;
    std::thread::sleep(Duration::from_millis(150));
    a.request(&Request::ReleaseAll { session: "selftest-A".into() }, t)?;
    match b.recv(Some(Duration::from_secs(4)))? {
        Response::Granted { waited_ms, .. } => lines.push(format!("✓ B waited {} and was handed the lease when A's turn ended", interlock_core::fmt_ms(waited_ms))),
        other => anyhow::bail!("B expected Granted after release, got {other:?}"),
    }
    a.request(&Request::SessionEnd { session: "selftest-A".into() }, t)?;
    b.request(&Request::SessionEnd { session: "selftest-B".into() }, t)?;
    lines.push("✓ blocking, hand-off, and turn-end release all work".into());
    Ok(lines)
}
