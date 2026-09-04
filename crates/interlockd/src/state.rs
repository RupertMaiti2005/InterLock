//! Lease table, read sets, wait queues, cycle detection. Pure logic; the server drives it.

use crate::snapshot::Snapshotter;
use interlock_core::paths::{key_to_abs, repo_key};
use interlock_core::{now_ms, short_id, Event, EventKind, FileInfo, Harness, Response, SessionInfo, SessionState, StatusSnapshot, WhySnapshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};

pub const READ_SET_CAP: usize = 64;
pub const SESSION_TTL: Duration = Duration::from_secs(600);
pub const RECENT_CAP: usize = 200;

pub struct ReadEntry {
    pub hash: Option<String>,
    pub at: Instant,
}

pub struct Lease {
    pub holder: String,
    pub acquired_at: Instant,
}

pub struct Waiter {
    pub session: String,
    pub since: Instant,
    pub cap_ms: u64,
    pub tx: oneshot::Sender<Response>,
}

pub struct SessionMeta {
    pub harness: Harness,
    pub label: Option<String>,
    pub last_seen: Instant,
    pub last_tool: Option<String>,
    pub waiting_on: Option<String>,
}

pub enum AcquireOutcome {
    Immediate(Response),
    Park(oneshot::Receiver<Response>, Instant),
}

pub struct State {
    pub repo_root: PathBuf,
    pub fold_case: bool,
    pub started: Instant,
    pub last_activity: Instant,
    pub leases: HashMap<String, Lease>,
    pub queues: HashMap<String, VecDeque<Waiter>>,
    pub read_sets: HashMap<String, HashMap<String, ReadEntry>>,
    pub sessions: HashMap<String, SessionMeta>,
    pub hotspots: HashSet<String>,
    pub last_writer: HashMap<String, (String, Instant)>,
    pub snapshot_counts: HashMap<String, usize>,
    pub events: broadcast::Sender<Event>,
    pub recent: VecDeque<Event>,
    pub degraded: Vec<String>,
    pub snapshotter: Option<Snapshotter>,
}

impl State {
    pub fn new(repo_root: PathBuf, fold_case: bool, snapshotter: Option<Snapshotter>) -> State {
        let (events, _) = broadcast::channel(1024);
        State {
            repo_root,
            fold_case,
            started: Instant::now(),
            last_activity: Instant::now(),
            leases: HashMap::new(),
            queues: HashMap::new(),
            read_sets: HashMap::new(),
            sessions: HashMap::new(),
            hotspots: HashSet::new(),
            last_writer: HashMap::new(),
            snapshot_counts: HashMap::new(),
            events,
            recent: VecDeque::new(),
            degraded: Vec::new(),
            snapshotter,
        }
    }

    pub fn key(&self, cwd: &str, raw: &str) -> Option<String> {
        let cwd = if cwd.is_empty() { self.repo_root.clone() } else { PathBuf::from(cwd) };
        repo_key(&self.repo_root, &cwd, raw, self.fold_case)
    }

    pub fn abs(&self, key: &str) -> PathBuf {
        key_to_abs(&self.repo_root, key)
    }

    pub fn emit(&mut self, kind: EventKind) {
        let ev = Event { at_ms: now_ms(), kind };
        self.recent.push_back(ev.clone());
        while self.recent.len() > RECENT_CAP {
            self.recent.pop_front();
        }
        let _ = self.events.send(ev);
    }

    pub fn touch(&mut self, session: &str, harness: Option<Harness>) -> &mut SessionMeta {
        self.last_activity = Instant::now();
        if !self.sessions.contains_key(session) {
            let harness = harness.unwrap_or(Harness::Unknown);
            self.sessions.insert(
                session.to_string(),
                SessionMeta { harness, label: None, last_seen: Instant::now(), last_tool: None, waiting_on: None },
            );
            self.emit(EventKind::SessionStarted { session: session.to_string(), harness, label: None });
        }
        let m = self.sessions.get_mut(session).unwrap();
        m.last_seen = Instant::now();
        if let Some(h) = harness {
            if m.harness == Harness::Unknown {
                m.harness = h;
            }
        }
        m
    }

    pub fn hello(&mut self, session: &str, harness: Harness, label: Option<String>) {
        self.last_activity = Instant::now();
        let label = label.map(|l| truncate_label(&l)).filter(|l| !l.is_empty());
        if let Some(m) = self.sessions.get_mut(session) {
            m.last_seen = Instant::now();
            m.harness = harness;
            if m.label.is_none() && label.is_some() {
                m.label = label.clone();
                self.emit(EventKind::SessionStarted { session: session.to_string(), harness, label });
            }
            return;
        }
        self.sessions.insert(
            session.to_string(),
            SessionMeta { harness, label: label.clone(), last_seen: Instant::now(), last_tool: None, waiting_on: None },
        );
        self.emit(EventKind::SessionStarted { session: session.to_string(), harness, label });
    }

    pub fn set_label(&mut self, session: &str, label: String) {
        let m = self.touch(session, None);
        m.label = Some(truncate_label(&label));
    }

    // ---------- leases ----------

    pub fn acquire(&mut self, session: &str, key: &str, blocking: bool, cap_ms: u64) -> AcquireOutcome {
        self.touch(session, None);
        if let Some(lease) = self.leases.get(key) {
            if lease.holder == session {
                return AcquireOutcome::Immediate(self.grant_response(session, key, 0));
            }
            let holder = lease.holder.clone();
            // Two-party cycle: holder is waiting on something we hold.
            if let Some(want) = self.sessions.get(&holder).and_then(|m| m.waiting_on.clone()) {
                if self.leases.get(&want).map(|l| l.holder == session).unwrap_or(false) {
                    self.emit(EventKind::Deadlock { session: session.to_string(), path: key.to_string(), other: holder.clone() });
                    let other_label = self.sessions.get(&holder).and_then(|m| m.label.clone());
                    return AcquireOutcome::Immediate(Response::Deadlock {
                        path: key.to_string(),
                        other: holder,
                        other_label,
                        other_wants: want,
                    });
                }
            }
            if !blocking {
                let holder_label = self.sessions.get(&holder).and_then(|m| m.label.clone());
                return AcquireOutcome::Immediate(Response::Blocked { path: key.to_string(), holder, holder_label, waited_ms: 0 });
            }
            let (tx, rx) = oneshot::channel();
            let since = Instant::now();
            self.queues.entry(key.to_string()).or_default().push_back(Waiter { session: session.to_string(), since, cap_ms, tx });
            if let Some(m) = self.sessions.get_mut(session) {
                m.waiting_on = Some(key.to_string());
            }
            self.emit(EventKind::Waiting { session: session.to_string(), path: key.to_string(), holder });
            return AcquireOutcome::Park(rx, since);
        }
        self.leases.insert(key.to_string(), Lease { holder: session.to_string(), acquired_at: Instant::now() });
        self.emit(EventKind::Acquired { session: session.to_string(), path: key.to_string() });
        AcquireOutcome::Immediate(self.grant_response(session, key, 0))
    }

    /// Build the reply for a granted lease: snapshot, then post-grant revalidation (SPEC §5).
    fn grant_response(&mut self, session: &str, key: &str, waited_ms: u64) -> Response {
        let snapshot = self.snapshot(session, key);
        if let Some(entry) = self.read_sets.get(session).and_then(|rs| rs.get(key)) {
            let current = interlock_core::hash_file(&self.abs(key));
            if current != entry.hash {
                let read_ago_ms = entry.at.elapsed().as_millis() as u64;
                let read_at = entry.at;
                let changed_by = self.changed_by(key, read_at);
                self.emit(EventKind::StaleBlocked {
                    session: session.to_string(),
                    target: key.to_string(),
                    changed: key.to_string(),
                    changed_by: changed_by.clone(),
                });
                return Response::Stale { path: key.to_string(), read_ago_ms, changed_by };
            }
        }
        Response::Granted { path: key.to_string(), waited_ms, snapshot }
    }

    fn changed_by(&self, key: &str, since: Instant) -> Option<String> {
        self.last_writer.get(key).filter(|(_, at)| *at > since).map(|(s, _)| self.display_name(s))
    }

    pub fn display_name(&self, session: &str) -> String {
        match self.sessions.get(session).and_then(|m| m.label.clone()) {
            Some(l) => format!("{} \"{}\"", short_id(session), l),
            None => short_id(session),
        }
    }

    /// Remove a lease held by `session`. Returns true if it was held by them.
    pub fn release(&mut self, session: &str, key: &str) -> bool {
        match self.leases.get(key) {
            Some(l) if l.holder == session => {}
            _ => return false,
        }
        self.leases.remove(key);
        self.emit(EventKind::Released { session: session.to_string(), path: key.to_string() });
        self.hand_off(key);
        true
    }

    pub fn force_release(&mut self, key: &str) -> bool {
        let Some(l) = self.leases.remove(key) else { return false };
        self.emit(EventKind::Released { session: l.holder, path: key.to_string() });
        self.hand_off(key);
        true
    }

    /// Give a freed path to the next live waiter.
    fn hand_off(&mut self, key: &str) {
        loop {
            let Some(w) = self.queues.get_mut(key).and_then(|q| q.pop_front()) else { break };
            if w.tx.is_closed() {
                self.clear_waiting(&w.session, key);
                continue;
            }
            let waited_ms = w.since.elapsed().as_millis() as u64;
            self.leases.insert(key.to_string(), Lease { holder: w.session.clone(), acquired_at: Instant::now() });
            self.clear_waiting(&w.session, key);
            self.emit(EventKind::Granted { session: w.session.clone(), path: key.to_string(), waited_ms });
            let resp = self.grant_response(&w.session, key, waited_ms);
            if w.tx.send(resp).is_err() {
                // Waiter vanished between the check and the send; free it for the next one.
                self.leases.remove(key);
                continue;
            }
            break;
        }
        if self.queues.get(key).map(|q| q.is_empty()).unwrap_or(false) {
            self.queues.remove(key);
        }
    }

    fn clear_waiting(&mut self, session: &str, key: &str) {
        if let Some(m) = self.sessions.get_mut(session) {
            if m.waiting_on.as_deref() == Some(key) {
                m.waiting_on = None;
            }
        }
    }

    /// Called when a parked waiter gives up. `timed_out` carries the wait duration when the cap expired.
    pub fn abandon_wait(&mut self, session: &str, key: &str, timed_out: Option<u64>) -> Option<String> {
        if let Some(q) = self.queues.get_mut(key) {
            q.retain(|w| w.session != session);
            if q.is_empty() {
                self.queues.remove(key);
            }
        }
        self.clear_waiting(session, key);
        let holder = self.leases.get(key).map(|l| l.holder.clone());
        if let Some(waited_ms) = timed_out {
            let h = holder.clone().unwrap_or_default();
            self.emit(EventKind::WaitTimeout { session: session.to_string(), path: key.to_string(), holder: h, waited_ms });
        }
        holder
    }

    pub fn release_all(&mut self, session: &str, end_turn: bool) -> Vec<String> {
        let held: Vec<String> = self.leases.iter().filter(|(_, l)| l.holder == session).map(|(k, _)| k.clone()).collect();
        for k in &held {
            self.release(session, k);
        }
        if end_turn {
            self.read_sets.remove(session);
        }
        held
    }

    pub fn session_end(&mut self, session: &str) {
        self.release_all(session, true);
        let waiting: Vec<String> = self.queues.iter().filter(|(_, q)| q.iter().any(|w| w.session == session)).map(|(k, _)| k.clone()).collect();
        for k in waiting {
            self.abandon_wait(session, &k, None);
        }
        if self.sessions.remove(session).is_some() {
            self.emit(EventKind::SessionEnded { session: session.to_string() });
        }
        self.last_activity = Instant::now();
    }

    pub fn reap_idle(&mut self) -> Vec<String> {
        let dead: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, m)| m.waiting_on.is_none() && m.last_seen.elapsed() > SESSION_TTL)
            .map(|(s, _)| s.clone())
            .collect();
        for s in &dead {
            let released = self.release_all(s, true);
            self.sessions.remove(s);
            self.emit(EventKind::SessionReaped { session: s.clone(), released });
        }
        dead
    }

    pub fn reap(&mut self, session: &str) -> bool {
        if !self.sessions.contains_key(session) {
            return false;
        }
        let released = self.release_all(session, true);
        let waiting: Vec<String> = self.queues.iter().filter(|(_, q)| q.iter().any(|w| w.session == session)).map(|(k, _)| k.clone()).collect();
        for k in waiting {
            self.abandon_wait(session, &k, None);
        }
        self.sessions.remove(session);
        self.emit(EventKind::SessionReaped { session: session.to_string(), released });
        true
    }

    // ---------- read sets ----------

    pub fn record_read(&mut self, session: &str, keys: &[String]) {
        self.touch(session, None);
        let rs = self.read_sets.entry(session.to_string()).or_default();
        for k in keys {
            let hash = interlock_core::hash_file(&key_to_abs(&self.repo_root, k));
            rs.insert(k.clone(), ReadEntry { hash, at: Instant::now() });
        }
        while rs.len() > READ_SET_CAP {
            let oldest = rs.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    rs.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Validate the session's entire read set. Returns the first stale entry.
    pub fn validate_write(&mut self, session: &str, targets: &[String]) -> Response {
        self.touch(session, None);
        let Some(rs) = self.read_sets.get(session) else { return Response::Fresh };
        let mut stale: Option<(String, Instant)> = None;
        let mut entries: Vec<(&String, &ReadEntry)> = rs.iter().collect();
        entries.sort_by_key(|(_, e)| e.at);
        for (k, e) in entries {
            if self.leases.get(k).map(|l| l.holder == session).unwrap_or(false) {
                continue;
            }
            let current = interlock_core::hash_file(&self.abs(k));
            if current != e.hash {
                stale = Some((k.clone(), e.at));
                break;
            }
        }
        match stale {
            None => Response::Fresh,
            Some((k, at)) => {
                let changed_by = self.changed_by(&k, at);
                self.emit(EventKind::StaleBlocked {
                    session: session.to_string(),
                    target: targets.first().cloned().unwrap_or_default(),
                    changed: k.clone(),
                    changed_by: changed_by.clone(),
                });
                Response::Stale { path: k, read_ago_ms: at.elapsed().as_millis() as u64, changed_by }
            }
        }
    }

    pub fn write_done(&mut self, session: &str, keys: &[String]) {
        self.touch(session, None);
        for k in keys {
            let hash = interlock_core::hash_file(&key_to_abs(&self.repo_root, k));
            if let Some(rs) = self.read_sets.get_mut(session) {
                rs.insert(k.clone(), ReadEntry { hash, at: Instant::now() });
            }
            self.last_writer.insert(k.clone(), (session.to_string(), Instant::now()));
        }
    }

    pub fn heartbeat(&mut self, session: &str, tool: Option<String>) {
        let m = self.touch(session, None);
        if tool.is_some() {
            m.last_tool = tool;
        }
    }

    // ---------- snapshots ----------

    fn snapshot(&mut self, session: &str, key: &str) -> Option<String> {
        let abs = self.abs(key);
        let snap = self.snapshotter.as_mut()?;
        match snap.snapshot(key, &abs) {
            Ok(Some(oid)) => {
                *self.snapshot_counts.entry(key.to_string()).or_default() += 1;
                self.emit(EventKind::Snapshot { session: session.to_string(), path: key.to_string(), oid: oid.clone() });
                Some(oid)
            }
            Ok(None) => None,
            Err(e) => {
                let reason = format!("snapshot failed for {key}: {e}");
                if !self.degraded.iter().any(|d| d.starts_with("snapshot failed")) {
                    self.degraded.push(reason.clone());
                }
                self.emit(EventKind::Degraded { reason });
                None
            }
        }
    }

    pub fn undo(&mut self, key: &str, steps: usize) -> Result<String, String> {
        let abs = self.abs(key);
        let snap = self.snapshotter.as_mut().ok_or("snapshots unavailable (not a git repo?)")?;
        let oid = snap.restore(key, &abs, steps).map_err(|e| e.to_string())?;
        self.last_writer.insert(key.to_string(), ("undo".to_string(), Instant::now()));
        self.emit(EventKind::Restored { path: key.to_string(), oid: oid.clone() });
        Ok(oid)
    }

    // ---------- views ----------

    pub fn status(&self) -> StatusSnapshot {
        let mut sessions: Vec<SessionInfo> = self
            .sessions
            .iter()
            .map(|(id, m)| {
                let held: Vec<String> = self.leases.iter().filter(|(_, l)| &l.holder == id).map(|(k, _)| k.clone()).collect();
                let state = if let Some(w) = &m.waiting_on {
                    let (waited_ms, cap_ms) = self
                        .queues
                        .get(w)
                        .and_then(|q| q.iter().find(|x| &x.session == id))
                        .map(|x| (x.since.elapsed().as_millis() as u64, x.cap_ms))
                        .unwrap_or((0, 0));
                    let holder = self.leases.get(w).map(|l| l.holder.clone()).unwrap_or_default();
                    SessionState::Waiting { path: w.clone(), holder, waited_ms, cap_ms }
                } else if !held.is_empty() {
                    SessionState::Editing { paths: held.clone() }
                } else {
                    SessionState::Idle
                };
                SessionInfo {
                    id: id.clone(),
                    harness: m.harness,
                    label: m.label.clone(),
                    state,
                    last_seen_ms: m.last_seen.elapsed().as_millis() as u64,
                    last_tool: m.last_tool.clone(),
                    held,
                    reads: self.read_sets.get(id).map(|r| r.len()).unwrap_or(0),
                }
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));

        let mut paths: HashSet<String> = self.leases.keys().cloned().collect();
        paths.extend(self.queues.keys().cloned());
        paths.extend(self.hotspots.iter().cloned());
        let mut files: Vec<FileInfo> = paths
            .into_iter()
            .map(|p| FileInfo {
                holder: self.leases.get(&p).map(|l| l.holder.clone()),
                held_for_ms: self.leases.get(&p).map(|l| l.acquired_at.elapsed().as_millis() as u64).unwrap_or(0),
                waiters: self.queues.get(&p).map(|q| q.iter().map(|w| w.session.clone()).collect()).unwrap_or_default(),
                hotspot: self.hotspots.contains(&p),
                snapshots: self.snapshot_counts.get(&p).copied().unwrap_or(0),
                path: p,
            })
            .collect();
        files.sort_by(|a, b| b.holder.is_some().cmp(&a.holder.is_some()).then(a.path.cmp(&b.path)));

        let mut hotspots: Vec<String> = self.hotspots.iter().cloned().collect();
        hotspots.sort();
        StatusSnapshot {
            repo: self.repo_root.to_string_lossy().to_string(),
            pid: std::process::id(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
            sessions,
            files,
            hotspots,
            degraded: self.degraded.clone(),
            recent: self.recent.iter().rev().take(50).cloned().collect(),
        }
    }

    pub fn why(&self, key: &str) -> WhySnapshot {
        let lease = self.leases.get(key);
        WhySnapshot {
            path: key.to_string(),
            holder: lease.map(|l| l.holder.clone()),
            holder_label: lease.and_then(|l| self.sessions.get(&l.holder)).and_then(|m| m.label.clone()),
            held_for_ms: lease.map(|l| l.acquired_at.elapsed().as_millis() as u64).unwrap_or(0),
            waiters: self.queues.get(key).map(|q| q.iter().map(|w| w.session.clone()).collect()).unwrap_or_default(),
            hotspot: self.hotspots.contains(key),
            readers: self.read_sets.iter().filter(|(_, rs)| rs.contains_key(key)).map(|(s, _)| s.clone()).collect(),
            snapshots: self.snapshot_counts.get(key).copied().unwrap_or(0),
        }
    }

    pub fn load_hotspots(&mut self, dir: &Path) {
        if let Ok(text) = std::fs::read_to_string(dir.join("hotspots.json")) {
            if let Ok(list) = serde_json::from_str::<Vec<String>>(&text) {
                self.hotspots = list.into_iter().map(|p| if self.fold_case { p.to_lowercase() } else { p }).collect();
            }
        }
    }
}

fn truncate_label(l: &str) -> String {
    let one_line = l.lines().next().unwrap_or("").trim();
    let mut out: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        out.push('…');
    }
    out
}
