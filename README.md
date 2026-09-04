# interlock

Coordination for multiple coding agents working in **one shared worktree**. Works with Claude Code, Codex CLI, Gemini CLI, GitHub Copilot CLI, and Cursor from a single install.

```
npm i -g interlock          # coming with the first release; today: cargo build --release
cd my-repo && interlock init
interlock                   # live dashboard
```

## What it does

Agents sharing a directory overwrite each other's work and, more insidiously, write against file contents they read minutes ago. interlock is a small daemon plus a hook shim that:

1. **Serializes writes to the same file.** A second agent that wants a file another agent is editing *waits* (its tool call is paused, no tokens burned) until the holder's turn ends, then proceeds. It is blocked, not denied, so there is nothing to route around.
2. **Catches writes against stale reads, across files.** If agent C read `auth.ts`, then agent A changed a signature in it, C's next write to `routes.ts` is stopped with one instruction: re-read `auth.ts`. No lock was violated and the target file never changed. The harness's own "file modified since read" check does not see this case.
3. **Snapshots every pre-write image** into a hidden git ref (`refs/interlock/history`), so `interlock undo <file>` always works. Your index, branches, stash, and HEAD are never touched.
4. **Fails open.** If the daemon is down, writes pass through and the dashboard says so in red.

## The dashboard

```
 interlock · my-repo · 3 agents · daemon ok · 2 hotspots

 AGENTS
  ● claude  a3f1  "add rate limiting to auth"   EDITING  src/auth.ts
  ◐ codex   9c02  "fix login redirect"          WAITING  src/auth.ts  ← a3f1   0:08 / 0:45
  ○ claude  77be  "update route table"          IDLE     last seen 12s ago

 FILES
  src/auth.ts       held by a3f1 for 4m12s   1 waiting   ⚠ hotspot   3 snapshots

 EVENTS
  14:02:11  9c02 waiting on src/auth.ts (held by a3f1)
  14:01:40  77be blocked writing src/routes.ts: src/auth.ts changed since read, told to re-read
  13:58:03  a3f1 acquired src/auth.ts

 [Tab] pane  [↑↓] select  [r] release lease  [u] undo file  [k] reap session  [q] quit
```

Try it without any real agents: `interlock demo` plays four scenes with mock agents in a scratch repo.

## Commands

| Command | What it does |
|---|---|
| `interlock` | Live dashboard |
| `interlock init` | Detect harnesses, install hooks (with backup), mine hotspots, self-test |
| `interlock status [--json]` | One-shot view |
| `interlock watch [--json]` | Stream events |
| `interlock why <path>` | Who holds it, who is waiting, who read it |
| `interlock release <path> --force` | Break a lease by hand |
| `interlock undo <path> [--steps N]` | Restore from snapshot history |
| `interlock name <session> "label"` | Label an agent |
| `interlock demo [--headless]` | Scripted scenes with mock agents |
| `interlock uninstall` | Remove hooks, restore settings |

## Harness support

| Harness | Tier | How |
|---|---|---|
| Claude Code | 1 | PreToolUse blocks Edit/Write; Read/Grep/Glob tracked |
| Codex CLI | 1 | PreToolUse blocks `apply_patch`; reads parsed from shell and PowerShell commands (best effort). Codex ships with hooks off: `init` sets `features.hooks = true`, and you trust the hook once with `/hooks` (or `--dangerously-bypass-hook-trust` for `codex exec`) |
| Gemini CLI | 1 | BeforeTool blocks `write_file`/`replace` |
| Copilot CLI | 1 | preToolUse JSON deny |
| Cursor | 2 | No pre-edit hook exists; conflicts are detected after the fact and snapshotted |
| Shell writes, anything else | 3 | Detected by the daemon, snapshotted, shown in the dashboard; cannot block |

Tier 1 gives you blocking plus stale-read validation. Tier 2 gives detection and undo. Tier 3 gives undo.

## Why not worktrees

Worktrees isolate the tracked tree and nothing else: separate `npm install`, `.env`, build caches, dev-server ports. interlock takes the other trade, zero environment duplication and coordination at write time instead of merge time. Where worktrees are already in use, `init` says interlock adds little.

## Verified live

On 2026-09-04, with hooks installed by `interlock init`:

- Two Claude Code sessions launched six seconds apart on the same file. The second waited 18s on the first's lease, was granted when the first turn ended, and both sets of edits landed intact.
- One Claude Code session and one Codex CLI session on the same file. Codex waited 13s, was granted on Claude's turn end, and its patch applied on top of Claude's edits. Codex used PowerShell `Get-Content` to read, which the parser now tracks.

## Known gaps

- Shell writes (`sed -i`, `echo >`) from an agent's Bash tool are blocked only when the command is simple enough to parse. Piped or scripted writes are caught by the watcher after the fact.
- Session identity falls back to harness plus directory when a hook payload has no session field.
- Tested on Windows 11 and the daemon uses TCP loopback so it runs unchanged on macOS and Linux, but those have not been exercised yet.

## Building

```
cargo build --release
# binaries: target/release/interlock, interlockd, interlock-hook
```

`interlock init` writes the absolute path of `interlock-hook` into each harness's hook config, so keep the three binaries together.

See `SPEC.md` for the design, milestones, and the evaluation plan.
