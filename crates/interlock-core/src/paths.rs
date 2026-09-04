//! Repo discovery and canonical repo-relative path keys.

use std::path::{Component, Path, PathBuf};

/// Walk up from `start` to find the directory containing `.git` (file or dir).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    if cur.is_file() {
        cur.pop();
    }
    loop {
        if cur.join(".git").exists() {
            return Some(strip_verbatim(&std::fs::canonicalize(&cur).unwrap_or(cur)));
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// True if the `.git` entry is a file (worktree or submodule) rather than a directory.
pub fn is_worktree(repo_root: &Path) -> bool {
    repo_root.join(".git").is_file()
}

/// Remove Windows' `\\?\` verbatim prefix that `canonicalize` adds.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// Whether the filesystem at `root` ignores case (probe-based, cached by caller).
pub fn is_case_insensitive(root: &Path) -> bool {
    // Probe: does the root exist under a case-flipped spelling?
    let s = root.to_string_lossy();
    let flipped: String = s
        .chars()
        .map(|c| if c.is_lowercase() { c.to_uppercase().next().unwrap_or(c) } else { c.to_lowercase().next().unwrap_or(c) })
        .collect();
    if flipped == s {
        return cfg!(any(windows, target_os = "macos"));
    }
    Path::new(&flipped).exists()
}

/// Canonical repo-relative key for `raw` as sent by a hook, resolved against `cwd`.
/// Returns `None` if the path is outside the repo.
pub fn repo_key(repo_root: &Path, cwd: &Path, raw: &str, fold_case: bool) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    let abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    // Canonicalize the deepest existing ancestor so new files still get a stable key.
    let abs = canonicalize_lenient(&abs);
    let rel = abs.strip_prefix(repo_root).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut key = parts.join("/");
    if fold_case {
        key = key.to_lowercase();
    }
    Some(key)
}

fn canonicalize_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return strip_verbatim(&c);
    }
    // Resolve the longest existing prefix, then append the remainder lexically normalized.
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(n) => tail.push(n.to_os_string()),
            None => break,
        }
        if !existing.pop() {
            break;
        }
    }
    let mut out = std::fs::canonicalize(&existing).map(|c| strip_verbatim(&c)).unwrap_or(existing);
    for t in tail.iter().rev() {
        if t == ".." {
            out.pop();
        } else if t != "." {
            out.push(t);
        }
    }
    out
}

/// Absolute filesystem path for a repo key.
pub fn key_to_abs(repo_root: &Path, key: &str) -> PathBuf {
    let mut p = repo_root.to_path_buf();
    for part in key.split('/') {
        p.push(part);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_absolute_agree() {
        let dir = std::env::temp_dir().join(format!("interlock-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "x").unwrap();
        let root = strip_verbatim(&std::fs::canonicalize(&dir).unwrap());
        let k1 = repo_key(&root, &root, "src/a.rs", false).unwrap();
        let k2 = repo_key(&root, &root.join("src"), "a.rs", false).unwrap();
        let k3 = repo_key(&root, &root, root.join("src").join("a.rs").to_str().unwrap(), false).unwrap();
        let k4 = repo_key(&root, &root.join("src"), "../src/./a.rs", false).unwrap();
        assert_eq!(k1, "src/a.rs");
        assert_eq!(k1, k2);
        assert_eq!(k1, k3);
        assert_eq!(k1, k4);
        // new file, not yet on disk
        let k5 = repo_key(&root, &root, "src/new/b.rs", false).unwrap();
        assert_eq!(k5, "src/new/b.rs");
        // outside the repo
        assert!(repo_key(&root, &root, "../../etc/passwd", false).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
