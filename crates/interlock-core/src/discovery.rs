//! Where the daemon for a repo lives: `~/.interlock/<repo-hash>/{port,pid,log}`.

use std::path::{Path, PathBuf};

pub fn repo_hash(repo_root: &Path) -> String {
    let s = repo_root.to_string_lossy().to_lowercase();
    blake3::hash(s.as_bytes()).to_hex()[..16].to_string()
}

pub fn interlock_home() -> PathBuf {
    if let Ok(p) = std::env::var("INTERLOCK_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".interlock")
}

pub fn state_dir(repo_root: &Path) -> PathBuf {
    interlock_home().join(repo_hash(repo_root))
}

pub fn port_file(repo_root: &Path) -> PathBuf {
    state_dir(repo_root).join("port")
}

pub fn read_port(repo_root: &Path) -> Option<u16> {
    std::fs::read_to_string(port_file(repo_root)).ok()?.trim().parse().ok()
}

/// Atomically publish the daemon's port.
pub fn write_port(repo_root: &Path, port: u16) -> std::io::Result<()> {
    let dir = state_dir(repo_root);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("port.{}.tmp", std::process::id()));
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, dir.join("port"))?;
    std::fs::write(dir.join("pid"), std::process::id().to_string())?;
    Ok(())
}

pub fn clear_port(repo_root: &Path) {
    let _ = std::fs::remove_file(port_file(repo_root));
    let _ = std::fs::remove_file(state_dir(repo_root).join("pid"));
}

/// Append a line to the shim/daemon log for this repo. Never fails.
pub fn log_line(repo_root: &Path, who: &str, line: &str) {
    use std::io::Write;
    let dir = state_dir(repo_root);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log")) {
        let _ = writeln!(f, "{} {who} {line}", crate::now_ms());
    }
}

/// Locate a sibling binary (same directory as the current executable), falling back to PATH.
pub fn sibling_binary(name: &str) -> PathBuf {
    let exe_name = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let cand = dir.join(&exe_name);
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from(exe_name)
}

/// Start the daemon for `repo_root` detached from the current process. Does not wait.
pub fn spawn_daemon(repo_root: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let bin = sibling_binary("interlockd");
    let mut cmd = Command::new(bin);
    cmd.arg("--repo").arg(repo_root);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        // Windows inherits every inheritable handle when stdio is redirected. The harness's
        // stdout/stderr pipes to this shim are inheritable, and a daemon holding them keeps the
        // harness waiting on the hook until the daemon exits. Mark ours non-inheritable first.
        windows_no_inherit_stdio();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(|_| ())
}

#[cfg(windows)]
fn windows_no_inherit_stdio() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n: u32) -> isize;
        fn SetHandleInformation(h: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 0x1;
    for n in [0xFFFF_FFF6u32, 0xFFFF_FFF5, 0xFFFF_FFF4] {
        // STD_INPUT_HANDLE (-10), STD_OUTPUT_HANDLE (-11), STD_ERROR_HANDLE (-12)
        unsafe {
            let h = GetStdHandle(n);
            if h != 0 && h != -1 {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}
