//! Newline-delimited JSON protocol between shim/CLI and daemon.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Gemini,
    Copilot,
    Cursor,
    Mock,
    Unknown,
}

impl Harness {
    pub fn short(&self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Gemini => "gemini",
            Harness::Copilot => "copilot",
            Harness::Cursor => "cursor",
            Harness::Mock => "mock",
            Harness::Unknown => "agent",
        }
    }

    /// Guarantee tier per SPEC §8.
    pub fn tier(&self) -> u8 {
        match self {
            Harness::Claude | Harness::Codex | Harness::Gemini | Harness::Copilot | Harness::Mock => 1,
            Harness::Cursor => 2,
            Harness::Unknown => 3,
        }
    }

    /// Default blocking wait cap: harness hook timeout minus a 15s margin.
    pub fn default_cap_ms(&self) -> u64 {
        match self {
            Harness::Copilot => 15_000,
            _ => 45_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        session: String,
        harness: Harness,
        #[serde(default)]
        label: Option<String>,
        cwd: String,
    },
    SetLabel {
        session: String,
        label: String,
    },
    Acquire {
        session: String,
        path: String,
        cwd: String,
        blocking: bool,
        cap_ms: u64,
    },
    Release {
        session: String,
        path: String,
        cwd: String,
    },
    ReleaseAll {
        session: String,
    },
    /// Force-release a lease regardless of holder (CLI / dashboard intervention).
    ForceRelease {
        path: String,
        cwd: String,
    },
    /// Reap a session as if it had timed out (CLI / dashboard intervention).
    Reap {
        session: String,
    },
    RecordRead {
        session: String,
        paths: Vec<String>,
        cwd: String,
    },
    /// Validate the whole read set of `session` before it writes `targets`.
    ValidateWrite {
        session: String,
        targets: Vec<String>,
        cwd: String,
    },
    WriteDone {
        session: String,
        paths: Vec<String>,
        cwd: String,
    },
    Heartbeat {
        session: String,
        #[serde(default)]
        tool: Option<String>,
    },
    SessionEnd {
        session: String,
    },
    Status,
    Why {
        path: String,
        cwd: String,
    },
    Undo {
        path: String,
        cwd: String,
        #[serde(default = "one")]
        steps: usize,
    },
    Subscribe,
    Ping,
    Shutdown,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    Ok,
    Pong {
        pid: u32,
        repo: String,
        uptime_ms: u64,
    },
    Granted {
        path: String,
        waited_ms: u64,
        #[serde(default)]
        snapshot: Option<String>,
    },
    Blocked {
        path: String,
        holder: String,
        holder_label: Option<String>,
        waited_ms: u64,
    },
    Stale {
        /// The file that changed since it was read (may differ from the write target).
        path: String,
        read_ago_ms: u64,
        changed_by: Option<String>,
    },
    Deadlock {
        path: String,
        other: String,
        other_label: Option<String>,
        /// The path `other` is waiting on, which this session holds.
        other_wants: String,
    },
    Fresh,
    Status(StatusSnapshot),
    Why(WhySnapshot),
    Restored {
        path: String,
        blob_oid: String,
    },
    Event(Event),
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state")]
pub enum SessionState {
    Idle,
    Editing {
        paths: Vec<String>,
    },
    Waiting {
        path: String,
        holder: String,
        waited_ms: u64,
        cap_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub harness: Harness,
    pub label: Option<String>,
    pub state: SessionState,
    pub last_seen_ms: u64,
    pub last_tool: Option<String>,
    pub held: Vec<String>,
    pub reads: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub holder: Option<String>,
    pub held_for_ms: u64,
    pub waiters: Vec<String>,
    pub hotspot: bool,
    pub snapshots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub repo: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub sessions: Vec<SessionInfo>,
    pub files: Vec<FileInfo>,
    pub hotspots: Vec<String>,
    pub degraded: Vec<String>,
    #[serde(default)]
    pub recent: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhySnapshot {
    pub path: String,
    pub holder: Option<String>,
    pub holder_label: Option<String>,
    pub held_for_ms: u64,
    pub waiters: Vec<String>,
    pub hotspot: bool,
    pub readers: Vec<String>,
    pub snapshots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum EventKind {
    SessionStarted { session: String, harness: Harness, label: Option<String> },
    SessionEnded { session: String },
    SessionReaped { session: String, released: Vec<String> },
    Acquired { session: String, path: String },
    Waiting { session: String, path: String, holder: String },
    Granted { session: String, path: String, waited_ms: u64 },
    WaitTimeout { session: String, path: String, holder: String, waited_ms: u64 },
    Released { session: String, path: String },
    StaleBlocked { session: String, target: String, changed: String, changed_by: Option<String> },
    Deadlock { session: String, path: String, other: String },
    Snapshot { session: String, path: String, oid: String },
    Restored { path: String, oid: String },
    ExternalWrite { path: String },
    Degraded { reason: String },
}

impl EventKind {
    /// One-line human rendering used by `status`, `watch`, and the dashboard.
    pub fn describe(&self) -> String {
        use EventKind::*;
        match self {
            SessionStarted { session, harness, label } => match label {
                Some(l) => format!("{session} ({}) started: \"{l}\"", harness.short()),
                None => format!("{session} ({}) started", harness.short()),
            },
            SessionEnded { session } => format!("{session} ended, leases released"),
            SessionReaped { session, released } => {
                format!("{session} silent too long, reaped ({} leases)", released.len())
            }
            Acquired { session, path } => format!("{session} acquired {path}"),
            Waiting { session, path, holder } => format!("{session} waiting on {path} (held by {holder})"),
            Granted { session, path, waited_ms } => {
                if *waited_ms > 0 {
                    format!("{session} granted {path} after {}", fmt_ms(*waited_ms))
                } else {
                    format!("{session} granted {path}")
                }
            }
            WaitTimeout { session, path, holder, waited_ms } => {
                format!("{session} timed out on {path} after {} (held by {holder}), told to wait", fmt_ms(*waited_ms))
            }
            Released { session, path } => format!("{session} released {path}"),
            StaleBlocked { session, target, changed, .. } => {
                if target == changed {
                    format!("{session} blocked: {changed} changed since read, told to re-read")
                } else {
                    format!("{session} blocked writing {target}: {changed} changed since read, told to re-read")
                }
            }
            Deadlock { session, path, other } => {
                format!("{session} would deadlock with {other} on {path}, told to finish its edit")
            }
            Snapshot { session, path, oid } => format!("{session} snapshot {path} {}", &oid[..oid.len().min(8)]),
            Restored { path, oid } => format!("restored {path} from {}", &oid[..oid.len().min(8)]),
            ExternalWrite { path } => format!("{path} written outside any hook flow"),
            Degraded { reason } => format!("degraded: {reason}"),
        }
    }
}

/// Short display form of a session id: the last dash-separated segment, at most 8 chars.
pub fn short_id(s: &str) -> String {
    let seg = s.rsplit('-').next().unwrap_or(s);
    let seg = if seg.is_empty() { s } else { seg };
    if seg.len() <= 8 {
        seg.to_string()
    } else {
        seg[seg.len() - 8..].to_string()
    }
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{}.{}s", s, (ms % 1000) / 100)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}
