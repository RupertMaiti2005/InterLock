//! Normalize hook payloads from every supported harness into one `HookEvent`.
//! See SPEC §8 for the per-harness matrix this encodes.

use crate::protocol::Harness;
use crate::shell;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum HookKind {
    PromptSubmit { prompt: String },
    PreRead(Vec<String>),
    PostRead(Vec<String>),
    PreWrite(Vec<String>),
    PostWrite(Vec<String>),
    PostTool,
    TurnEnd,
    SessionEnd,
    Ignore,
}

#[derive(Debug, Clone)]
pub struct HookEvent {
    pub harness: Harness,
    pub session: String,
    pub cwd: PathBuf,
    pub tool: Option<String>,
    pub kind: HookKind,
    /// True for events where the shim may block (pre-write on a Tier 1 harness).
    pub can_block: bool,
}

/// How the shim must answer a given harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyStyle {
    /// exit 0 allow; exit 2 + stderr deny (Claude Code, Codex)
    ExitCode,
    /// stdout `{"decision":"deny","reason":..}` (Gemini)
    GeminiJson,
    /// stdout `{"permissionDecision":"deny","permissionDecisionReason":..}`; never exit non-zero (Copilot)
    CopilotJson,
    /// stdout `{"permission":"allow"|"deny","user_message":..}` (Cursor)
    CursorJson,
}

pub fn reply_style(h: Harness) -> ReplyStyle {
    match h {
        Harness::Gemini => ReplyStyle::GeminiJson,
        Harness::Copilot => ReplyStyle::CopilotJson,
        Harness::Cursor => ReplyStyle::CursorJson,
        _ => ReplyStyle::ExitCode,
    }
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

fn obj<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(k)).filter(|x| !x.is_null())
}

pub fn detect_harness(v: &Value) -> Harness {
    if let Ok(h) = std::env::var("INTERLOCK_HARNESS") {
        return match h.to_lowercase().as_str() {
            "claude" => Harness::Claude,
            "codex" => Harness::Codex,
            "gemini" => Harness::Gemini,
            "copilot" => Harness::Copilot,
            "cursor" => Harness::Cursor,
            "mock" => Harness::Mock,
            _ => Harness::Unknown,
        };
    }
    if v.get("conversation_id").is_some() {
        return Harness::Cursor;
    }
    if v.get("sessionId").is_some() || v.get("toolName").is_some() {
        return Harness::Copilot;
    }
    let ev = s(v, &["hook_event_name"]).unwrap_or("");
    if matches!(
        ev,
        "BeforeTool" | "AfterTool" | "BeforeAgent" | "AfterAgent" | "BeforeModel" | "AfterModel" | "PreCompress"
    ) {
        return Harness::Gemini;
    }
    let tool = s(v, &["tool_name"]).unwrap_or("");
    if v.get("turn_id").is_some() || tool == "apply_patch" || tool == "unified_exec" {
        return Harness::Codex;
    }
    if v.get("session_id").is_some() {
        return Harness::Claude;
    }
    Harness::Unknown
}

fn session_id(v: &Value, harness: Harness) -> String {
    if let Some(id) = s(v, &["session_id", "sessionId", "conversation_id"]) {
        return id.to_string();
    }
    if let Ok(id) = std::env::var("INTERLOCK_SESSION") {
        return id;
    }
    // Last resort: stable per harness + cwd. Parent-PID identity is a follow-up.
    let cwd = s(v, &["cwd"]).unwrap_or("");
    format!("{}-{}", harness.short(), &blake3::hash(cwd.as_bytes()).to_hex()[..8])
}

fn cwd_of(v: &Value) -> PathBuf {
    s(v, &["cwd"])
        .map(PathBuf::from)
        .or_else(|| {
            v.get("workspace_roots")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn one_path(input: &Value, keys: &[&str]) -> Vec<String> {
    s(input, keys).map(|p| vec![p.to_string()]).unwrap_or_default()
}

pub fn normalize(v: &Value) -> HookEvent {
    let harness = detect_harness(v);
    let session = session_id(v, harness);
    let cwd = cwd_of(v);
    let ev = s(v, &["hook_event_name", "hookEventName", "event"]).unwrap_or("").to_string();
    let tool = s(v, &["tool_name", "toolName"]).map(|t| t.to_string());
    let input = obj(v, &["tool_input", "toolArgs", "tool_args", "input"]).cloned().unwrap_or(Value::Null);
    let response = obj(v, &["tool_response", "toolResult", "tool_result", "output"]).cloned().unwrap_or(Value::Null);

    let kind = match harness {
        Harness::Claude => claude(&ev, tool.as_deref(), &input, &response, v),
        Harness::Codex => codex(&ev, tool.as_deref(), &input, v),
        Harness::Gemini => gemini(&ev, tool.as_deref(), &input, &response, v),
        Harness::Copilot => copilot(&ev, tool.as_deref(), &input, v),
        Harness::Cursor => cursor(&ev, v),
        Harness::Mock | Harness::Unknown => claude(&ev, tool.as_deref(), &input, &response, v),
    };
    let can_block = harness.tier() == 1 && matches!(kind, HookKind::PreWrite(_));
    HookEvent { harness, session, cwd, tool, kind, can_block }
}

// ---------- Claude Code ----------

fn claude(ev: &str, tool: Option<&str>, input: &Value, response: &Value, v: &Value) -> HookKind {
    match ev {
        "UserPromptSubmit" => HookKind::PromptSubmit { prompt: s(v, &["prompt"]).unwrap_or("").to_string() },
        "Stop" => HookKind::TurnEnd,
        "SessionEnd" => HookKind::SessionEnd,
        "PreToolUse" => match tool.unwrap_or("") {
            "Read" => HookKind::PreRead(one_path(input, &["file_path"])),
            "Edit" | "Write" | "MultiEdit" => HookKind::PreWrite(one_path(input, &["file_path"])),
            "NotebookEdit" => HookKind::PreWrite(one_path(input, &["notebook_path"])),
            "Bash" => shell_pre(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::Ignore,
        },
        "PostToolUse" => match tool.unwrap_or("") {
            "Read" => HookKind::PostRead(one_path(input, &["file_path"])),
            "Grep" | "Glob" => HookKind::PostRead(files_from_response(response)),
            "Edit" | "Write" | "MultiEdit" => HookKind::PostWrite(one_path(input, &["file_path"])),
            "NotebookEdit" => HookKind::PostWrite(one_path(input, &["notebook_path"])),
            "Bash" => shell_post(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::PostTool,
        },
        _ => HookKind::Ignore,
    }
}

/// Best-effort extraction of file paths from a Grep/Glob tool response.
fn files_from_response(resp: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = resp.get("filenames").and_then(Value::as_array) {
        out.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
        return out;
    }
    let text = match resp {
        Value::String(t) => t.clone(),
        Value::Object(_) => resp.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => String::new(),
    };
    for line in text.lines().take(500) {
        let cand = line.split(':').next().unwrap_or("").trim();
        if !cand.is_empty() && std::path::Path::new(cand).is_file() {
            if !out.iter().any(|x| x == cand) {
                out.push(cand.to_string());
            }
        }
    }
    out
}

fn shell_pre(command: &str) -> HookKind {
    let (reads, writes) = shell::classify(command);
    if !writes.is_empty() {
        HookKind::PreWrite(writes)
    } else if !reads.is_empty() {
        HookKind::PreRead(reads)
    } else {
        HookKind::Ignore
    }
}

fn shell_post(command: &str) -> HookKind {
    let (reads, writes) = shell::classify(command);
    if !writes.is_empty() {
        HookKind::PostWrite(writes)
    } else if !reads.is_empty() {
        HookKind::PostRead(reads)
    } else {
        HookKind::PostTool
    }
}

// ---------- Codex CLI ----------

fn codex(ev: &str, tool: Option<&str>, input: &Value, v: &Value) -> HookKind {
    match ev {
        "UserPromptSubmit" => HookKind::PromptSubmit { prompt: s(v, &["prompt"]).unwrap_or("").to_string() },
        "Stop" => HookKind::TurnEnd,
        "SessionEnd" => HookKind::SessionEnd,
        "PreToolUse" => match tool.unwrap_or("") {
            "apply_patch" => HookKind::PreWrite(patch_paths(input)),
            "Bash" | "unified_exec" | "shell" | "exec_command" => shell_pre(s(input, &["command", "cmd"]).unwrap_or("")),
            _ => HookKind::Ignore,
        },
        "PostToolUse" => match tool.unwrap_or("") {
            "apply_patch" => HookKind::PostWrite(patch_paths(input)),
            "Bash" | "unified_exec" | "shell" | "exec_command" => shell_post(s(input, &["command", "cmd"]).unwrap_or("")),
            _ => HookKind::PostTool,
        },
        _ => HookKind::Ignore,
    }
}

/// Paths touched by an `apply_patch` body (`*** Update File:` / `*** Add File:` / `*** Delete File:` / `*** Move to:`).
pub fn patch_paths(input: &Value) -> Vec<String> {
    let body = match input {
        Value::String(t) => t.as_str(),
        _ => s(input, &["command", "patch", "input"]).unwrap_or(""),
    };
    let mut out = Vec::new();
    for line in body.lines() {
        let l = line.trim_start();
        for prefix in ["*** Update File:", "*** Add File:", "*** Delete File:", "*** Move to:"] {
            if let Some(rest) = l.strip_prefix(prefix) {
                let p = rest.trim();
                if !p.is_empty() && !out.iter().any(|x| x == p) {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

// ---------- Gemini CLI ----------

fn gemini(ev: &str, tool: Option<&str>, input: &Value, response: &Value, v: &Value) -> HookKind {
    let path_keys = &["file_path", "absolute_path", "path"];
    match ev {
        "BeforeAgent" => HookKind::PromptSubmit { prompt: s(v, &["prompt"]).unwrap_or("").to_string() },
        "AfterAgent" => HookKind::TurnEnd,
        "SessionEnd" => HookKind::SessionEnd,
        "BeforeTool" => match tool.unwrap_or("") {
            "read_file" => HookKind::PreRead(one_path(input, path_keys)),
            "write_file" | "replace" | "edit" => HookKind::PreWrite(one_path(input, path_keys)),
            "run_shell_command" => shell_pre(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::Ignore,
        },
        "AfterTool" => match tool.unwrap_or("") {
            "read_file" => HookKind::PostRead(one_path(input, path_keys)),
            "read_many_files" | "glob" | "grep_search" | "search_file_content" => {
                HookKind::PostRead(files_from_response(response))
            }
            "write_file" | "replace" | "edit" => HookKind::PostWrite(one_path(input, path_keys)),
            "run_shell_command" => shell_post(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::PostTool,
        },
        _ => HookKind::Ignore,
    }
}

// ---------- GitHub Copilot CLI ----------

fn copilot(ev: &str, tool: Option<&str>, input: &Value, v: &Value) -> HookKind {
    let path_keys = &["path", "file_path", "filePath"];
    let t = tool.unwrap_or("").to_lowercase();
    match ev {
        "userPromptSubmitted" => HookKind::PromptSubmit { prompt: s(v, &["prompt"]).unwrap_or("").to_string() },
        "agentStop" => HookKind::TurnEnd,
        "sessionEnd" => HookKind::SessionEnd,
        "preToolUse" => match t.as_str() {
            "view" | "read" | "read_file" => HookKind::PreRead(one_path(input, path_keys)),
            "edit" | "create" | "write" | "str_replace_editor" | "edit_file" | "create_file" => {
                HookKind::PreWrite(one_path(input, path_keys))
            }
            "bash" | "shell" | "powershell" => shell_pre(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::Ignore,
        },
        "postToolUse" => match t.as_str() {
            "view" | "read" | "read_file" => HookKind::PostRead(one_path(input, path_keys)),
            "edit" | "create" | "write" | "str_replace_editor" | "edit_file" | "create_file" => {
                HookKind::PostWrite(one_path(input, path_keys))
            }
            "bash" | "shell" | "powershell" => shell_post(s(input, &["command"]).unwrap_or("")),
            _ => HookKind::PostTool,
        },
        _ => HookKind::Ignore,
    }
}

// ---------- Cursor (Tier 2: no pre-edit hook) ----------

fn cursor(ev: &str, v: &Value) -> HookKind {
    match ev {
        "beforeSubmitPrompt" => HookKind::PromptSubmit { prompt: s(v, &["prompt"]).unwrap_or("").to_string() },
        "stop" => HookKind::TurnEnd,
        "sessionEnd" => HookKind::SessionEnd,
        "beforeReadFile" => HookKind::PreRead(one_path(v, &["file_path"])),
        "afterFileEdit" => HookKind::PostWrite(one_path(v, &["file_path"])),
        "beforeShellExecution" => shell_pre(s(v, &["command"]).unwrap_or("")),
        "afterShellExecution" => shell_post(s(v, &["command"]).unwrap_or("")),
        _ => HookKind::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_edit_is_prewrite() {
        let v = json!({"session_id":"abc","cwd":"/r","hook_event_name":"PreToolUse","tool_name":"Edit",
                       "tool_input":{"file_path":"/r/src/a.ts","old_string":"x","new_string":"y"}});
        let e = normalize(&v);
        assert_eq!(e.harness, Harness::Claude);
        assert_eq!(e.kind, HookKind::PreWrite(vec!["/r/src/a.ts".into()]));
        assert!(e.can_block);
    }

    #[test]
    fn claude_stop_is_turn_end() {
        let v = json!({"session_id":"abc","cwd":"/r","hook_event_name":"Stop"});
        assert_eq!(normalize(&v).kind, HookKind::TurnEnd);
    }

    #[test]
    fn codex_apply_patch() {
        let v = json!({"session_id":"s","turn_id":"t","cwd":"/r","hook_event_name":"PreToolUse","tool_name":"apply_patch",
                       "tool_input":{"command":"*** Begin Patch\n*** Update File: src/a.ts\n@@\n-x\n+y\n*** Add File: src/b.ts\n+z\n*** End Patch"}});
        let e = normalize(&v);
        assert_eq!(e.harness, Harness::Codex);
        assert_eq!(e.kind, HookKind::PreWrite(vec!["src/a.ts".into(), "src/b.ts".into()]));
    }

    #[test]
    fn gemini_write_file() {
        let v = json!({"session_id":"s","cwd":"/r","hook_event_name":"BeforeTool","tool_name":"write_file",
                       "tool_input":{"file_path":"/r/x.py","content":"..."}});
        let e = normalize(&v);
        assert_eq!(e.harness, Harness::Gemini);
        assert_eq!(e.kind, HookKind::PreWrite(vec!["/r/x.py".into()]));
    }

    #[test]
    fn copilot_edit() {
        let v = json!({"sessionId":"s","cwd":"/r","hook_event_name":"preToolUse","toolName":"edit",
                       "toolArgs":{"path":"/r/x.py"}});
        let e = normalize(&v);
        assert_eq!(e.harness, Harness::Copilot);
        assert_eq!(e.kind, HookKind::PreWrite(vec!["/r/x.py".into()]));
        assert_eq!(reply_style(e.harness), ReplyStyle::CopilotJson);
    }

    #[test]
    fn cursor_is_detect_only() {
        let v = json!({"conversation_id":"c","workspace_roots":["/r"],"hook_event_name":"afterFileEdit",
                       "file_path":"/r/x.py","edits":[]});
        let e = normalize(&v);
        assert_eq!(e.harness, Harness::Cursor);
        assert_eq!(e.kind, HookKind::PostWrite(vec!["/r/x.py".into()]));
        assert!(!e.can_block);
    }

    #[test]
    fn bash_redirect_is_write() {
        let v = json!({"session_id":"s","cwd":"/r","hook_event_name":"PreToolUse","tool_name":"Bash",
                       "tool_input":{"command":"echo hi > out.txt"}});
        assert_eq!(normalize(&v).kind, HookKind::PreWrite(vec!["out.txt".into()]));
    }
}
