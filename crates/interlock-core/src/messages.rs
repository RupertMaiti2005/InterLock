//! Agent-facing wording. Treat every string here as a measured parameter (SPEC §6.3, §11).

pub fn stale(changed: &str, read_ago_ms: u64, changed_by: Option<&str>) -> String {
    let who = match changed_by {
        Some(w) => format!("rewritten by {w}"),
        None => "rewritten by another agent".to_string(),
    };
    format!(
        "{changed} changed since you read it ({who} {} ago).\n\
         Re-read it before editing. Do not implement elsewhere.",
        crate::fmt_ms(read_ago_ms)
    )
}

pub fn blocked(path: &str, holder_label: Option<&str>) -> String {
    let who = match holder_label {
        Some(l) => format!("another agent (\"{l}\")"),
        None => "another agent".to_string(),
    };
    format!(
        "{path} is being edited by {who} right now.\n\
         Do not implement this elsewhere or create a workaround file.\n\
         Wait, then retry this exact edit."
    )
}

pub fn deadlock(path: &str, other_wants: &str) -> String {
    format!(
        "Another agent needs {other_wants}, which you are editing, and is waiting on you.\n\
         Finish your current edit and end your turn so it can proceed, then continue with {path}."
    )
}
