//! Best-effort classification of a shell command into files it reads and files it writes.
//! Used for Codex (which reads through the shell) and for shell-write detection everywhere.
//! Misses piped and scripted access by construction; the watcher is the backstop (SPEC §6.6, §8).

const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "awk", "grep", "egrep", "fgrep", "rg", "wc", "sort", "diff", "nl",
    "cut", "tac", "od", "xxd", "hexdump", "stat", "file", "type", "sed",
];
/// Commands where the first non-flag argument is a pattern/program, not a file.
const PATTERN_FIRST: &[&str] = &["grep", "egrep", "fgrep", "rg", "awk", "sed"];

pub fn classify(command: &str) -> (Vec<String>, Vec<String>) {
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for seg in split_segments(command) {
        let toks = tokenize(&seg);
        if toks.is_empty() {
            continue;
        }
        // Redirections apply to any command.
        let mut i = 0;
        let mut args: Vec<String> = Vec::new();
        while i < toks.len() {
            let t = &toks[i];
            let is_redir_out = t == ">" || t == ">>" || t == "1>" || t == "2>" || t == "&>" || t == "1>>" || t == "2>>";
            if is_redir_out {
                if let Some(target) = toks.get(i + 1) {
                    if !target.starts_with('&') {
                        push(&mut writes, target);
                    }
                }
                i += 2;
                continue;
            }
            if let Some(rest) = t.strip_prefix(">>").or_else(|| t.strip_prefix('>')) {
                if !rest.is_empty() && !rest.starts_with('&') {
                    push(&mut writes, rest);
                }
                i += 1;
                continue;
            }
            if t == "<" {
                if let Some(src) = toks.get(i + 1) {
                    push(&mut reads, src);
                }
                i += 2;
                continue;
            }
            args.push(t.clone());
            i += 1;
        }
        // Skip leading env assignments and sudo/time wrappers.
        let mut start = 0;
        while start < args.len() && (args[start].contains('=') && !args[start].starts_with('-') || args[start] == "sudo" || args[start] == "time") {
            start += 1;
        }
        if start >= args.len() {
            continue;
        }
        // `cmd /c <command...>` and `powershell -Command <command...>` wrappers: unwrap.
        let mut start = start;
        let mut nested: Option<String> = None;
        loop {
            let c = base_name(&args[start]);
            if c == "cmd" && args.get(start + 1).map(|a| a.eq_ignore_ascii_case("/c")).unwrap_or(false) {
                start += 2;
            } else if (c == "powershell" || c == "pwsh") && args.len() > start + 1 {
                start += 1;
                while start < args.len() && args[start].starts_with('-') {
                    start += 1;
                }
            } else {
                break;
            }
            if start >= args.len() {
                break;
            }
            // A quoted inner command survives tokenizing as one token; classify it on its own.
            if args[start].contains(char::is_whitespace) {
                nested = Some(args[start..].join(" "));
                break;
            }
        }
        if let Some(inner) = nested {
            let (r, w) = classify(&inner);
            r.iter().for_each(|p| push(&mut reads, p));
            w.iter().for_each(|p| push(&mut writes, p));
            continue;
        }
        if start >= args.len() {
            continue;
        }
        let cmd = base_name(&args[start]);
        let rest = &args[start + 1..];
        // PowerShell cmdlets: file arguments come from -Path/-LiteralPath or the first positional.
        if let Some(kind) = powershell_kind(&cmd) {
            let mut files = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                let a = rest[i].to_lowercase();
                if a == "-path" || a == "-literalpath" || a == "-filepath" {
                    if let Some(v) = rest.get(i + 1) {
                        files.push(v.clone());
                    }
                    i += 2;
                    continue;
                }
                if !rest[i].starts_with('-') && files.is_empty() && !looks_like_glob(&rest[i]) {
                    files.push(rest[i].clone());
                }
                i += 1;
            }
            for f in files {
                match kind {
                    PsKind::Read => push(&mut reads, &f),
                    PsKind::Write => push(&mut writes, &f),
                }
            }
            continue;
        }
        let flags: Vec<&String> = rest.iter().filter(|a| a.starts_with('-')).collect();
        let positional: Vec<&String> = rest.iter().filter(|a| !a.starts_with('-') && !looks_like_glob(a)).collect();

        match cmd.as_str() {
            "tee" => positional.iter().for_each(|p| push(&mut writes, p)),
            "touch" | "truncate" | "rm" | "unlink" => positional.iter().for_each(|p| push(&mut writes, p)),
            "mv" | "cp" => {
                if positional.len() >= 2 {
                    for p in &positional[..positional.len() - 1] {
                        push(&mut reads, p);
                    }
                    push(&mut writes, positional[positional.len() - 1]);
                }
            }
            "sed" => {
                let inplace = flags.iter().any(|f| f.starts_with("-i") || *f == "--in-place");
                let has_e = flags.iter().any(|f| f.starts_with("-e") || f.starts_with("--expression"));
                let files: Vec<&String> = if has_e { positional.clone() } else { positional.iter().skip(1).cloned().collect() };
                for f in files {
                    if inplace {
                        push(&mut writes, f);
                    } else {
                        push(&mut reads, f);
                    }
                }
            }
            "dd" => {
                for a in rest {
                    if let Some(p) = a.strip_prefix("of=") {
                        push(&mut writes, p);
                    } else if let Some(p) = a.strip_prefix("if=") {
                        push(&mut reads, p);
                    }
                }
            }
            c if READERS.contains(&c) => {
                let skip = if PATTERN_FIRST.contains(&c) && !flags.iter().any(|f| f.starts_with("-e")) { 1 } else { 0 };
                for p in positional.iter().skip(skip) {
                    push(&mut reads, p);
                }
            }
            _ => {}
        }
    }
    (reads, writes)
}

#[derive(Clone, Copy)]
enum PsKind {
    Read,
    Write,
}

fn powershell_kind(cmd: &str) -> Option<PsKind> {
    match cmd {
        "get-content" | "gc" | "type" | "select-string" | "sls" | "get-item" | "gi" => Some(PsKind::Read),
        "set-content" | "sc" | "add-content" | "ac" | "out-file" | "new-item" | "ni" | "remove-item" | "ri" | "del" => {
            Some(PsKind::Write)
        }
        _ => None,
    }
}

fn push(v: &mut Vec<String>, p: &str) {
    let p = p.trim_matches(|c| c == '"' || c == '\'');
    if p.is_empty() || p == "/dev/null" || p == "-" || p.starts_with("/dev/") || p.starts_with("/proc/") {
        return;
    }
    if !v.iter().any(|x| x == p) {
        v.push(p.to_string());
    }
}

fn base_name(s: &str) -> String {
    let s = s.trim_matches(|c| c == '"' || c == '\'');
    let b = s.rsplit(['/', '\\']).next().unwrap_or(s);
    b.strip_suffix(".exe").unwrap_or(b).to_lowercase()
}

fn looks_like_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

fn split_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '|' | ';' | '\n' => {
                    out.push(std::mem::take(&mut cur));
                    if c == '|' && chars.get(i + 1) == Some(&'|') {
                        i += 1;
                    }
                }
                '&' if chars.get(i + 1) == Some(&'&') => {
                    out.push(std::mem::take(&mut cur));
                    i += 1;
                }
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    out.push(cur);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

fn tokenize(seg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = seg.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    // POSIX escape only before a quote, space, or backslash; otherwise it is a
                    // Windows path separator and stays literal.
                    match chars.peek() {
                        Some(&n) if matches!(n, '"' | '\'' | ' ' | '\\' | '$' | '`') => {
                            chars.next();
                            cur.push(n);
                        }
                        _ => cur.push('\\'),
                    }
                }
                ' ' | '\t' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                '>' | '<' => {
                    // split redirection operators into their own tokens, keeping fd prefixes like 2>
                    let fd_prefix = matches!(cur.as_str(), "1" | "2" | "&");
                    if !cur.is_empty() && !fd_prefix {
                        out.push(std::mem::take(&mut cur));
                    }
                    let mut op = std::mem::take(&mut cur);
                    op.push(c);
                    if c == '>' && chars.peek() == Some(&'>') {
                        chars.next();
                        op.push('>');
                    }
                    out.push(op);
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads() {
        let (r, w) = classify("cat src/a.rs && sed -n '1,20p' src/b.rs | head");
        assert_eq!(r, vec!["src/a.rs", "src/b.rs"]);
        assert!(w.is_empty());
    }

    #[test]
    fn grep_skips_pattern() {
        let (r, _) = classify("rg -n \"fn main\" src/main.rs");
        assert_eq!(r, vec!["src/main.rs"]);
        let (r, _) = classify("grep -rn TODO .");
        assert_eq!(r, vec!["."]);
    }

    #[test]
    fn writes() {
        let (_, w) = classify("echo x > out.txt; cat a >> log.txt");
        assert_eq!(w, vec!["out.txt", "log.txt"]);
        let (r, w) = classify("sed -i 's/a/b/' src/x.rs");
        assert_eq!(w, vec!["src/x.rs"]);
        assert!(r.is_empty());
        let (_, w) = classify("cargo test 2>&1 | tee build.log");
        assert_eq!(w, vec!["build.log"]);
        let (r, w) = classify("mv a.txt b.txt");
        assert_eq!(r, vec!["a.txt"]);
        assert_eq!(w, vec!["b.txt"]);
    }

    #[test]
    fn powershell_and_cmd_wrappers() {
        let (r, _) = classify("Get-Content -LiteralPath 'src/auth.ts' -Tail 3");
        assert_eq!(r, vec!["src/auth.ts"]);
        let (r, _) = classify("Get-Content -Raw -LiteralPath 'src/auth.ts'");
        assert_eq!(r, vec!["src/auth.ts"]);
        let (r, _) = classify("cmd /c type src\\auth.ts");
        assert_eq!(r, vec!["src\\auth.ts"]);
        let (r, _) = classify("powershell -NoProfile -Command \"cmd /c type src\\auth.ts\"");
        assert_eq!(r, vec!["src\\auth.ts"]);
        let (_, w) = classify("Set-Content -Path out.txt -Value 'x'");
        assert_eq!(w, vec!["out.txt"]);
        let (_, w) = classify("Add-Content notes.md 'line'");
        assert_eq!(w, vec!["notes.md"]);
    }

    #[test]
    fn dev_null_ignored() {
        let (_, w) = classify("cmd > /dev/null 2>&1");
        assert!(w.is_empty());
    }
}
