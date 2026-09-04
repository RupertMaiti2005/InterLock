# interlock — Build Spec (rev 2, 2026-09-04)

Coordination for multiple coding agents working in one shared worktree, across harnesses.

**Status:** implementation in progress. Target ~5 weeks to v1.
**Language:** Rust (daemon + hook shim + CLI, one workspace).

Changes from rev 1 are marked **[rev2]**.

---

## 1. What this is

Agents in a shared directory overwrite each other's work and, more insidiously, write against file contents they read minutes ago. interlock is a daemon plus a hook shim that:

1. Serializes writes to the same file, blocking rather than denying.
2. Catches writes against stale reads via content-hash compare-and-swap across the session's whole read set, whether or not a lease was involved.
3. Snapshots prior file content into a hidden git ref so nothing is unrecoverable.
4. **[rev2]** Works across Claude Code, Codex CLI, Gemini CLI, Copilot CLI, and Cursor from one install, and shows the user what every agent is doing in a live dashboard.

Point 2 is the differentiator, with one correction from rev 1.

### [rev2] What the harness already does, and what it does not

Claude Code's native Edit tool refuses an edit if the target file's mtime changed since the session last Read it. So "C read `auth.ts`, A rewrote it, C edits `auth.ts`" is already caught for that one harness and that one file.

What nobody catches is the **cross-file** case: C read `auth.ts`, A changed an exported signature in it, and C now writes `routes.ts` against the old signature. No lock is violated. The target file has not changed. The native check is silent. interlock's read-set validation covers every file the session read, not just the one it is writing.

Secondary advantages over the native check: content hashes instead of mtime, reads made through Grep/Glob/shell are tracked, and it works for every harness with hooks, not only Claude Code.

### Why not worktrees

Worktrees isolate the tracked tree and nothing else. Each tree needs its own `npm install`, its own `.env`, its own build cache, its own dev-server port. interlock takes the other trade: zero environment duplication, coordination at write time instead of merge time. Where worktrees are in use, `init` detects them and says so.

### Why a daemon and not a lock directory

- **Blocking needs a rendezvous point.** A file-based scheme either polls or denies immediately. Holding a connection open and waking the waiter the instant a lease drops is the only way to get blocking under a sub-5ms hook budget.
- **Cycle detection needs a global view.**
- **[rev2] A dashboard needs an event stream.** The daemon already has the state; the TUI subscribes to it.

### Prior art on denial behavior

CircumEval (LessWrong, 2026) measured what agents do when a file is read-only: on source-locked FastAPI tasks, circumvention was 100% for Opus and Sonnet and 46% for GPT-5.4. Only an explicit instruction to stop and report reliably prevented it. Consequences: denial wording is an empirical parameter, and blocking beats denying because a blocked agent has nothing to route around.

---

## 2. Scope

**In v1:** file-level leases, turn-scoped holding, blocking waits, two-party cycle detection, cross-file stale-read validation, shadow-ref snapshots, fail-open, hotspot detection, filesystem watcher backstop, harness adapters (Claude Code and Codex as Tier 1 for the eval; Gemini, Copilot, Cursor after), `init` / dashboard / `status` / `watch` / `why` / `release` / `undo` / `demo` / `uninstall`.

**Cut:** co-change clustering, MCP server, `log` / `history` / `reindex` / `doctor` / `stop`, symbol granularity, human-presence leases, worktree merge analysis. Reasons in the appendix.

**[rev2] Promoted from "judgment call" to v1:** two-party deadlock detection. With the anti-workaround wording ("wait and retry this exact edit"), an A-holds-x-wants-y / B-holds-y-wants-x cycle becomes a livelock: both time out, both retry, both re-block. At N=2 the check is a few lines and it must ship with the wording that causes the problem.

---

## 3. Architecture

```
 agent tool call (any harness)
        │
        ▼
  interlock-hook  ── normalize payload ──▶  HookEvent { harness, session, kind, paths, cwd }
   (static bin)                                    │
        │                       TCP loopback, port file in ~/.interlock/<repo-hash>/
        ▼                                          ▼
   exit 0 / exit 2 / JSON     ◀──────────      interlockd
   (reply format per harness)                  in-memory lease table
                                               read-set map
                                               wait queues + oneshots
                                               wait-for graph (N=2)
                                               gix blob writer → refs/interlock/history
                                               fs watcher (notify) for Tier 3
                                               broadcast event stream
                                               append-only disk log
                                                   ▲
                                interlock (CLI/TUI) ┘   Subscribe
```

**Hook shim** (`interlock-hook`). Static binary, std-only, no async runtime. Reads hook JSON on stdin, detects the harness from the payload shape, normalizes it, does one round trip, and replies in the format that harness expects. The whole latency budget is process spawn.

**Daemon** (`interlockd`). Tokio, single-threaded runtime. Auto-started by the shim on first invocation (detached spawn; no fork on Windows). Exits after 30 min idle. Binding the listening port is the mutex against two shims racing to start it.

**[rev2] Transport.** TCP on 127.0.0.1 with an OS-assigned port written to `~/.interlock/<repo-hash>/port`, where repo-hash is a truncated blake3 of the canonicalized repo root. One transport for every platform. Unix sockets are not supported by tokio on Windows and the developer machine is Windows, so the platform split is removed rather than deferred.

**CLI / TUI** (`interlock`). Talks the same protocol. Bare `interlock` opens the dashboard.

---

## 4. Data model

```rust
type SessionId = String;   // harness session field, else parent PID of the hook process
type Hash = [u8; 32];      // blake3 of file content

struct Lease { path: RepoPath, holder: SessionId, acquired_at: Instant, last_renewed: Instant }

struct Waiter { session: SessionId, since: Instant, tx: oneshot::Sender<AcquireOutcome> }

struct SessionMeta {
    harness: Harness,            // [rev2]
    label: Option<String>,       // [rev2] first user prompt, truncated, or user-set
    last_seen: Instant,
    last_tool: Option<String>,   // [rev2]
    waiting_on: Option<RepoPath> // [rev2] for cycle detection
}

struct State {
    leases:    HashMap<RepoPath, Lease>,
    queues:    HashMap<RepoPath, VecDeque<Waiter>>,
    read_sets: HashMap<SessionId, HashMap<RepoPath, ReadEntry { hash, at: Instant }>>,
    sessions:  HashMap<SessionId, SessionMeta>,
    hotspots:  HashSet<RepoPath>,
    events:    broadcast::Sender<Event>,  // [rev2]
}
```

**[rev2] Path canonicalization runs in the daemon**, not the shim. The shim sends the raw path plus the hook `cwd`; the daemon resolves against the repo root through `std::fs::canonicalize`, strips the Windows verbatim prefix, makes it repo-relative, and lowercase-folds on case-insensitive filesystems. Different harnesses send different path shapes (Claude: absolute; Codex: patch-relative to `cwd`), and only the daemon can make them agree on a key.

---

## 5. Protocol

Newline-delimited JSON over the socket.

| Request | Response |
|---|---|
| `Hello { session, harness, label?, cwd }` | `Ok` — **[rev2]** registers session metadata |
| `Acquire { session, path, blocking, cap_ms }` | `Granted` \| `Blocked { holder, waited_ms }` \| `Stale { .. }` \| `Deadlock { other }` |
| `Release { session, path }` | `Ok` |
| `ReleaseAll { session }` | `Ok` — **[rev2]** turn end |
| `RecordRead { session, path, hash }` | `Ok` |
| `ValidateWrite { session, path }` | `Fresh` \| `Stale { path, read_at, changed_by }` |
| `WriteDone { session, path, hash }` | `Ok` — **[rev2]** refreshes the holder's own read entry |
| `Heartbeat { session, tool? }` | `Ok` |
| `SessionEnd { session }` | `Ok` |
| `Status` | `StatusSnapshot` |
| `Why { path }` | `WhySnapshot` |
| `Undo { path, steps }` | `Restored { blob_oid }` |
| `Subscribe` | stream of `Event` — **[rev2]** |

`Acquire` with `blocking: true` parks the connection. **[rev2]** `cap_ms` is supplied by the shim, derived from the harness's hook timeout, because the cap differs per harness (see §8).

**[rev2] Acquire re-validates after grant.** Rev 1 cleared a session's read-set entry for a path the moment it acquired the lease. That drops the stale check at exactly the moment it matters: C read the file, waited on A's lease, A rewrote it, C is granted. Now: after grant, if C's recorded hash differs from current content, the daemon returns `Stale` instead of `Granted`. C keeps the lease, re-reads, retries, and succeeds. Read-set entries are cleared on `WriteDone`, not on acquire.

### Hook wiring (normalized)

| Normalized event | Shim action | Reply |
|---|---|---|
| `PromptSubmit` | `Hello` with label | allow |
| `PreRead(paths)` | hash each, `RecordRead` | allow |
| `PostRead(paths)` | same, for Grep/Glob whose matches are only known after | allow |
| `PreWrite(paths)` | `ValidateWrite` (all read-set entries), then `Acquire` blocking | allow / block with message |
| `PostWrite(paths)` | `WriteDone` | allow |
| `PostTool` | `Heartbeat` | allow |
| `TurnEnd` | `ReleaseAll` | allow |
| `SessionEnd` | `SessionEnd` | allow |

Validate freshness *before* queueing on the lock, so a stale reader is told to re-read instead of waiting out a lease it will then have to give up.

---

## 6. Core mechanisms

### 6.1 Leases — **[rev2] turn-scoped, not session-scoped**

Rev 1 held leases for the whole session. That made the happy path ("B waits, A releases, B proceeds") depend on A's session ending. In practice B waited out the cap, was denied, and was told to retry an edit that could not succeed. That is the CircumEval regime the tool exists to escape.

Now:
- Acquired on first write to a path within a turn.
- Released when the holder's turn ends (`Stop` / `AfterAgent` / `agentStop` hook, per harness), on `SessionEnd`, or by explicit `interlock release`.
- Renewed on every tool call within the turn.
- TTL is crash insurance: 10 minutes of total silence from a session reaps its leases. Note this also fires for a session parked on human input, which is the desired behavior now that leases are turn-scoped.

Cross-turn coherence comes from §6.2, which is why shortening the hold is safe.

### 6.2 Stale-read validation

- On read: `RecordRead` with blake3 of current content.
- On write: for every path in the session's read set, re-hash and compare. Any mismatch blocks with:

```
src/auth.ts changed since you read it (rewritten by another agent 90s ago).
Re-read it before editing. Do not implement elsewhere.
```

- **[rev2] False-block control.** A session that explored thirty files would be blocked on every write the moment any one of them changed. The read set is scoped to the current turn (cleared on `TurnEnd`) and capped at 64 entries, oldest evicted. §11 measures the false-block rate; if it is still high, the next knob is a time window.
- The holder's own writes refresh its read entry via `WriteDone`, so it does not block itself on its second edit.

### 6.3 Blocking

- The shim holds its connection open. The agent is idle, not looping.
- **[rev2]** Wait cap = that harness's hook timeout minus 15s margin, set explicitly by `init` in each harness config so the number is under interlock's control.
- On timeout, deny with:

```
src/routes/index.ts is being edited by another agent right now.
Do not implement this elsewhere or create a workaround file.
Wait, then retry this exact edit.
```

- **[rev2] Cycle detection.** Before parking a waiter on path P held by H, check whether H is itself waiting on a path this session holds. If so, do not park; return `Deadlock` to the later requester with different wording, because "retry" is wrong here:

```
Another agent needs src/auth.ts, which you are editing, and is waiting on you.
Finish your current edit and end your turn so it can proceed, then continue.
```

Wording is a measured parameter (§11).

### 6.4 Snapshots

**[rev2] On every write pre-check**, not only on first acquire. Rev 1 saved only the pre-lease image, so the holder's later edits had no pre-image and `undo` could only reach the pre-session state. Blobs dedupe by hash, so repeated snapshots of unchanged content cost one tree lookup.

1. Daemon reads current content (the daemon owns the gix handle; the shim never touches the repo).
2. Write a blob with `gix` directly into `.git/objects`. Never shell out to `git` on the hot path.
3. Commit a single-file tree onto `refs/interlock/history`, parented to the previous snapshot.

Never touches the user's index, branches, stash, or HEAD. Documented side effect: the ref appears in `git log --all` and in GUI clients.

### 6.5 Hotspots

On `init`: `git log --name-only --pretty=format:%H -n 5000`, count commits per file. **[rev2]** Take the top 10 by touch count, excluding lockfiles and changelogs, rather than a percentage threshold, which selected nothing in most repos. Surfaced in `status`, `why`, and the dashboard. `--preacquire-hotspots` exists, default off.

### 6.6 [rev2] Filesystem watcher

The daemon watches the repo with `notify`. A write to a leased path from a process that is not the holder's hook flow, or to any path the daemon did not just grant, produces a `ShellWrite` event: snapshot if a prior image is missing, flag in `status` and the dashboard, and update the content hash so the next `ValidateWrite` for other sessions catches it. This closes the shell-write gap to "detected and recoverable" rather than "unhandled". It cannot block.

### 6.7 Fail open, always

Daemon down, port file missing, connect timeout (50ms), malformed response, panic in the shim → allow the write. Log it, show it in `status` and the dashboard header as degraded.

**[rev2]** Copilot treats a non-zero exit or crash as deny. On JSON-reply harnesses the shim must exit 0 with an explicit allow on every error path. Exit code 2 is used only where the harness documents it.

---

## 7. [rev2] Install and user experience

### Install

```
npm i -g interlock            # or: curl -fsSL .../install.sh | sh, brew, winget
cd my-repo && interlock init
```

- Prebuilt binaries via cargo-dist: GitHub release, shell installer, npm package that fetches the platform binary, Homebrew tap, MSI. MSI and Homebrew are the first cuts if the schedule slips.
- `init` detects every harness configured in the repo or home directory, installs hooks for all of them with a backup, writes explicit hook timeouts, mines hotspots, detects worktrees, then runs a self-test with two mock clients so the user sees contention resolve before any real agent runs. Output is a checklist per harness with its tier.
- No `start` command. The shim auto-starts the daemon.

### Dashboard

Bare `interlock` opens a ratatui dashboard (crossterm, works on Windows). `interlock status` is the one-shot text form; `interlock watch --json` is the event stream for scripts.

```
 interlock · my-repo · 3 agents · daemon ok · 2 hotspots

 AGENTS
  ● claude  a3f1  "add rate limiting to auth"   EDITING  src/auth.ts          4m12s
  ◐ codex   9c02  "fix login redirect"          WAITING  src/auth.ts  ← a3f1   0:08 / 0:45
  ● claude  77be  "update route table"          IDLE                          last seen 12s

 FILES
  src/auth.ts       held by a3f1   1 waiting   ⚠ hotspot   3 snapshots
  src/routes.ts     free                        ⚠ hotspot

 EVENTS
  14:02:11  9c02 waiting on src/auth.ts (held by a3f1)
  14:01:40  77be blocked: read src/auth.ts 3m ago, changed since → told to re-read
  14:01:39  77be re-read src/auth.ts, write allowed
  13:58:03  a3f1 acquired src/auth.ts, snapshot 8f2a

 [r] release lease  [u] undo file  [k] reap session  [q] quit
```

- The WAITING row shows the timer against the cap and who it waits on; when granted, it flips to EDITING and the event log records total wait.
- Stale-read blocks have distinct wording from lease waits.
- Agents are labeled by their first prompt where the harness has a prompt hook; else harness plus short id. `interlock name <id> "label"` overrides.
- Human intervention: release, undo, reap, one key each.
- Degraded mode turns the header red and says writes are passing through unchecked.

### `interlock demo`

The §10 mock-client harness with a preset script that plays the three demo scenes in the dashboard. It is the onboarding, the README recording, and the self-test.

---

## 8. [rev2] Harness matrix

| Harness | Tier | Pre-write block | Read hook | Turn end | Session field | Hook timeout | Config |
|---|---|---|---|---|---|---|---|
| Claude Code | 1 | PreToolUse Edit/Write/MultiEdit/NotebookEdit, exit 2 | PreToolUse Read; PostToolUse Grep/Glob | Stop | `session_id` | 60s default | `.claude/settings.json` |
| Codex CLI | 1 | PreToolUse `apply_patch`, exit 2 or JSON deny | none; parse `cat`/`sed`/`rg` from Bash hook | Stop | `session_id` | 600s default | `.codex/hooks.json` |
| Gemini CLI | 1 | BeforeTool `write_file`/`replace`, JSON deny | BeforeTool `read_file`, grep | AfterAgent | `session_id` | 60s default | `.gemini/settings.json` |
| Copilot CLI | 1 | preToolUse, JSON deny only; fail-closed on crash | preToolUse `view` | agentStop | `sessionId` | 30s default | `.github/hooks/*.json` |
| Cursor | 2 | **none**; `afterFileEdit` only | `beforeReadFile` | stop | `conversation_id` | undisclosed | `.cursor/hooks.json` |
| anything else, shell writes | 3 | watcher | — | — | — | — | — |

**Tiers.** 1: blocking leases plus CAS. 2: detect after the fact, snapshot, message delivered on the next turn. 3: watcher only, snapshot and flag, no agent-facing message. The README states the tier per harness.

**Session identity fallback.** When the payload has no session field, use the parent PID of the hook process. This also collapses subagents to their parent on every harness, which answers rev 1 open question 1 with a rule.

**Codex read tracking** is best effort by construction: the Bash hook's command string is parsed for file arguments to `cat`, `sed`, `head`, `tail`, `rg`, `grep`, `less`, `bat`. Piped or scripted reads are missed. The same parser gives best-effort shell-write detection (`>`, `>>`, `tee`, `sed -i`, `mv`, `cp`) on every harness, backed by the watcher.

---

## 9. Milestones

**M0 — Spike (2 days).** Open questions in §13. Deliverable: the harness matrix above confirmed by running each harness once, plus measured hook latency.

**M1 — Mechanism (week 1).** Core crate, daemon, shim with Claude Code adapter, TCP transport and port file, acquire/block/release, turn-scoped holding, heartbeat, TTL reaping, cycle detection, fail-open, `status`, `release`.
*Accept:* two real Claude Code sessions in one directory contest one file; one visibly waits; `status` shows EDITING and WAITING; A's turn ends and B proceeds; killing the daemon lets both proceed.

**M2 — Coherence (week 2).** `RecordRead`, `ValidateWrite`, `WriteDone`, post-grant revalidation, read-set lifecycle and cap, block messages. Snapshots and `undo`.
*Accept:* C reads `auth.ts`, never locks it, A rewrites it, C's write to `routes.ts` is blocked with a re-read instruction naming `auth.ts`. `interlock undo auth.ts` restores A's overwritten version. `git status` clean, `git stash list` untouched.

**M3 — Harnesses and watcher (week 3).** Normalizer, Codex adapter with shell-command parser, Gemini and Copilot adapters, Cursor detect-only adapter, `notify` watcher, `Subscribe` event stream, `watch --json`.
*Accept:* one Claude Code session and one Codex session contest a file; both tiers behave per the matrix. A `sed -i` from a plain shell is shown in `status` within one second and is undoable.

**M4 — Install and dashboard (week 4).** `init` for all harnesses with backup, hotspot mining, worktree detection, self-test, `uninstall`, dashboard, `demo`, cargo-dist packaging with npm and shell installer.
*Accept:* `npm i -g` then `init` on a fresh machine reaches a working dashboard in under two minutes. `init` completes under 60s on a 3k-commit repo. `uninstall` restores Claude Code settings byte-identical; other harnesses restore from backup. p99 hook latency recorded.

**M5 — Evidence (week 5).** Mock-client harness, evaluation (§11), write-up.
*Accept:* route-around rate, stale-read incidents, and false-block rate on ≥20 paired runs, at least one arm Claude versus Codex.

---

## 10. Testing

Mock-client harness driving N clients through scripted sequences with injected delays and deterministic seeds. No real agents. Also powers `demo` and the `init` self-test.

Cover: two clients one file; two clients circular wait (must return `Deadlock`, not two timeouts); holder crash; daemon restart with leases outstanding; daemon absent; wait-cap boundary; cross-file stale read; stale read *and* lock contention on the same path with post-grant revalidation; turn-end release; read-set cap eviction; subagent identity collapse; path canonicalization across symlink, case-fold, Windows verbatim prefix, and Codex relative paths; every harness payload shape into the normalizer; Copilot fail-closed reply on every shim error path.

Snapshot layer: scratch repo, assert index, branches, stash, and reflog untouched after 100 snapshots.

Then real end-to-end runs, because the one thing that cannot be simulated is how a model responds to being blocked.

---

## 11. Evaluation

Two agents, tasks engineered to contend on a shared file, 20 paired runs with and without interlock. At least one arm is Claude Code versus Codex.

Measure: lost writes; stale-read incidents (cross-file counted separately from same-file); route-around incidents; **[rev2] false blocks** (stale-read blocks where the changed file was irrelevant to the write); wasted agent-minutes.

Secondary arm varies the denial wording across three variants, plus the deadlock wording, to test whether the CircumEval finding reproduces under contention-blocking.

If blocking frequently produces workaround files, or false blocks exceed a few percent of writes, that needs to be known before building further.

---

## 12. Demo

1. **Contention.** A holds `auth.ts`. B blocks. Dashboard shows the wait timer. A's turn ends; B proceeds.
2. **Crash recovery.** A is killed. TTL reaps, B picks up the lease.
3. **[rev2] Cross-file stale read.** C read `auth.ts` at minute 2. A changes a signature in it. C's write to `routes.ts` is stopped: `auth.ts` changed since you read it. No lock was violated, the target file was untouched, and the harness's own check would have let it through.
4. **[rev2] Mixed fleet.** Scene 1 again, with Claude Code and Codex.

---

## 13. Open questions

Resolve in M0.

1. Which Codex dispatch path for `apply_patch` fires PreToolUse: the function tool, the shell form, or both?
2. How reliably do `Stop` / `AfterAgent` / `agentStop` fire on each harness? Turn-scoped leases depend on them. If unreliable on a harness, that harness falls back to TTL and the tier note says so.
3. Does a 45s held connection surface as a hang in each harness's UI?
4. Do Claude Code subagents share the parent's `session_id`, and does the parent-PID fallback agree?
5. Real end-to-end p99 hook latency per harness.
6. Does an agent that waits out a lease and is then told `Stale` re-read and succeed, or route around?
7. Does turn-end release cause a holder to lose a file it still needs across turns in practice? CAS should catch it; confirm it fires.
8. What does Cursor's `afterFileEdit` offer for delivering a message on the next turn?

---

## Appendix: what was cut and why

| Cut | Reason |
|---|---|
| Co-change clustering, Louvain, thresholds | Clusters were never lock units. Hotspot frequency gives the benefit in ~20 lines. |
| Soft reservation | Advisory-only; served by hotspot flagging. |
| MCP server | Voluntary tools undercut an enforcement guarantee. The watcher is the honest fallback for hook-less harnesses. |
| Shell-write blocking | Impossible by construction; detection plus watcher plus snapshots is the honest answer. |
| `log`, `history`, `reindex`, `doctor`, `stop` | Surface area without demo value. `watch --json` covers `log`. |
| Symbol granularity, presence leases, merge analysis | Post-v1. |
| Unix sockets / named pipes | One TCP loopback transport for all platforms. |
| Session-scoped leases | Replaced by turn-scoped; see §6.1. |
