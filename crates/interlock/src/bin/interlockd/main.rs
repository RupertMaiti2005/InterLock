//! interlockd: per-repo coordination daemon. Started by the hook shim on demand.

mod server;
mod snapshot;
mod state;

use interlock_core::discovery;
use interlock_core::paths;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

const IDLE_EXIT: Duration = Duration::from_secs(30 * 60);

fn main() {
    let mut args = std::env::args().skip(1);
    let mut repo: Option<PathBuf> = None;
    let mut idle_exit = IDLE_EXIT;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => repo = args.next().map(PathBuf::from),
            "--idle-exit-secs" => idle_exit = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or(1800)),
            "-h" | "--help" => {
                eprintln!("usage: interlockd --repo <path> [--idle-exit-secs N]");
                return;
            }
            _ => {}
        }
    }
    let repo_root = repo
        .or_else(|| std::env::current_dir().ok())
        .and_then(|p| paths::find_repo_root(&p))
        .unwrap_or_else(|| {
            eprintln!("interlockd: not inside a git repository");
            std::process::exit(2);
        });

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(run(repo_root, idle_exit));
}

async fn run(repo_root: PathBuf, idle_exit: Duration) {
    // Another daemon already serving this repo?
    if let Ok(mut c) = interlock_core::client::Client::connect(&repo_root, Duration::from_millis(100)) {
        if c.request(&interlock_core::Request::Ping, Duration::from_millis(500)).is_ok() {
            return;
        }
    }

    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("interlockd: bind failed: {e}");
            std::process::exit(1);
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if let Err(e) = discovery::write_port(&repo_root, port) {
        eprintln!("interlockd: cannot write port file: {e}");
        std::process::exit(1);
    }
    discovery::log_line(&repo_root, "daemon", &format!("started pid={} port={port}", std::process::id()));

    let fold_case = paths::is_case_insensitive(&repo_root);
    let snapshotter = match snapshot::Snapshotter::open(&repo_root) {
        Ok(s) => Some(s),
        Err(e) => {
            discovery::log_line(&repo_root, "daemon", &format!("snapshots disabled: {e:#}"));
            None
        }
    };
    let mut st = state::State::new(repo_root.clone(), fold_case, snapshotter);
    if st.snapshotter.is_none() {
        st.degraded.push("snapshots disabled: could not open git repository".into());
    }
    st.load_hotspots(&discovery::state_dir(&repo_root));
    let state: server::Shared = Arc::new(Mutex::new(st));

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Reaper + idle exit.
    {
        let state = state.clone();
        let repo_root = repo_root.clone();
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let (reaped, idle_for, empty) = {
                    let mut st = state.lock().unwrap();
                    let reaped = st.reap_idle();
                    (reaped, st.last_activity.elapsed(), st.sessions.is_empty())
                };
                for s in reaped {
                    discovery::log_line(&repo_root, "daemon", &format!("reaped {s}"));
                }
                if empty && idle_for > idle_exit {
                    let _ = shutdown_tx.send(()).await;
                    return;
                }
            }
        });
    }

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    stream.set_nodelay(true).ok();
                    tokio::spawn(server::handle(stream, state.clone(), shutdown_tx.clone()));
                }
            }
            _ = shutdown_rx.recv() => break,
            _ = &mut ctrl_c => break,
        }
    }
    discovery::clear_port(&repo_root);
    discovery::log_line(&repo_root, "daemon", "exiting");
}
