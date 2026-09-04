//! Plain-text rendering for `status`, `watch`, `why`.

use interlock_core::{fmt_ms, Event, SessionState, StatusSnapshot, WhySnapshot};
use std::path::Path;

fn short(s: &str) -> String {
    interlock_core::short_id(s)
}

pub fn status(repo_root: &Path, snap: Option<&StatusSnapshot>) -> String {
    let name = repo_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = String::new();
    let Some(s) = snap else {
        out.push_str(&format!("interlock · {name} · daemon not running\n"));
        out.push_str("  No agents are being coordinated. The daemon starts on the first hooked tool call.\n");
        out.push_str("  Run `interlock init` if hooks are not installed yet.\n");
        return out;
    };
    let degraded = if s.degraded.is_empty() { "daemon ok".to_string() } else { format!("DEGRADED: {}", s.degraded.join("; ")) };
    out.push_str(&format!(
        "interlock · {name} · {} agent{} · {degraded} (pid {}, up {}) · {} hotspot{}\n\n",
        s.sessions.len(),
        if s.sessions.len() == 1 { "" } else { "s" },
        s.pid,
        fmt_ms(s.uptime_ms),
        s.hotspots.len(),
        if s.hotspots.len() == 1 { "" } else { "s" },
    ));

    out.push_str("AGENTS\n");
    if s.sessions.is_empty() {
        out.push_str("  (none)\n");
    }
    for a in &s.sessions {
        let label = a.label.as_deref().map(|l| format!("\"{l}\"")).unwrap_or_default();
        let (mark, state) = match &a.state {
            SessionState::Editing { paths } => ("●", format!("EDITING  {}", paths.join(", "))),
            SessionState::Waiting { path, holder, waited_ms, cap_ms } => (
                "◐",
                format!("WAITING  {path}  ← {}   {} / {}", short(holder), fmt_ms(*waited_ms), fmt_ms(*cap_ms)),
            ),
            SessionState::Idle => ("○", format!("IDLE     last seen {} ago", fmt_ms(a.last_seen_ms))),
        };
        out.push_str(&format!("  {mark} {:<8} {:<8} {:<40} {state}\n", a.harness.short(), short(&a.id), label));
    }

    out.push_str("\nFILES\n");
    if s.files.is_empty() {
        out.push_str("  (nothing leased)\n");
    }
    for f in &s.files {
        let holder = match &f.holder {
            Some(h) => format!("held by {} for {}", short(h), fmt_ms(f.held_for_ms)),
            None => "free".to_string(),
        };
        let waiting = if f.waiters.is_empty() { String::new() } else { format!("  {} waiting", f.waiters.len()) };
        let hot = if f.hotspot { "  ⚠ hotspot" } else { "" };
        let snaps = if f.snapshots > 0 { format!("  {} snapshot{}", f.snapshots, if f.snapshots == 1 { "" } else { "s" }) } else { String::new() };
        out.push_str(&format!("  {:<40} {holder}{waiting}{hot}{snaps}\n", f.path));
    }

    out.push_str("\nEVENTS\n");
    if s.recent.is_empty() {
        out.push_str("  (none yet)\n");
    }
    for ev in s.recent.iter().take(12) {
        out.push_str(&format!("  {}\n", event_line(ev)));
    }
    out
}

pub fn event_line(ev: &Event) -> String {
    let secs = ev.at_ms / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}  {}", ev.kind.describe())
}

pub fn why(w: &WhySnapshot) -> String {
    let mut out = format!("{}\n", w.path);
    match &w.holder {
        Some(h) => {
            let label = w.holder_label.as_deref().map(|l| format!(" \"{l}\"")).unwrap_or_default();
            out.push_str(&format!("  held by {}{label} for {}\n", short(h), fmt_ms(w.held_for_ms)));
        }
        None => out.push_str("  not leased\n"),
    }
    if !w.waiters.is_empty() {
        out.push_str(&format!("  waiting: {}\n", w.waiters.iter().map(|s| short(s)).collect::<Vec<_>>().join(", ")));
    }
    if !w.readers.is_empty() {
        out.push_str(&format!("  read this turn by: {}\n", w.readers.iter().map(|s| short(s)).collect::<Vec<_>>().join(", ")));
    }
    if w.hotspot {
        out.push_str("  ⚠ hotspot: changes in many commits, expect contention\n");
    }
    out.push_str(&format!("  snapshots this session: {}\n", w.snapshots));
    out
}
