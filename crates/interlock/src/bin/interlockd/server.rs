//! Connection handling: one task per client, newline-delimited JSON, parked acquires.

use crate::state::{AcquireOutcome, State};
use interlock_core::{Request, Response};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub type Shared = Arc<Mutex<State>>;

pub async fn handle(stream: TcpStream, state: Shared, shutdown: tokio::sync::mpsc::Sender<()>) {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                let _ = write(&mut wr, &Response::Error { message: format!("bad request: {e}") }).await;
                continue;
            }
        };
        match req {
            Request::Subscribe => {
                subscribe(&mut wr, &state).await;
                return;
            }
            Request::Acquire { session, path, cwd, blocking, cap_ms } => {
                let key_opt = {
                    let st = state.lock().unwrap();
                    st.key(&cwd, &path)
                };
                let key = match key_opt {
                    Some(k) => k,
                    None => {
                        // Outside the repo: nothing to coordinate.
                        let _ = write(&mut wr, &Response::Granted { path, waited_ms: 0, snapshot: None }).await;
                        continue;
                    }
                };
                let outcome = {
                    let mut st = state.lock().unwrap();
                    st.acquire(&session, &key, blocking, cap_ms)
                };
                let resp = match outcome {
                    AcquireOutcome::Immediate(r) => r,
                    AcquireOutcome::Park(rx, since) => {
                        let cap = Duration::from_millis(cap_ms.max(1));
                        let mut peek = String::new();
                        tokio::select! {
                            r = tokio::time::timeout(cap, rx) => match r {
                                Ok(Ok(resp)) => resp,
                                Ok(Err(_)) => Response::Error { message: "daemon dropped waiter".into() },
                                Err(_) => timeout_reply(&state, &session, &key, since),
                            },
                            // Client hung up while parked (harness killed the hook): drop the wait.
                            _ = reader.read_line(&mut peek) => {
                                abandon(&state, &session, &key);
                                return;
                            }
                        }
                    }
                };
                let granted = matches!(resp, Response::Granted { .. } | Response::Stale { .. });
                if write(&mut wr, &resp).await.is_err() {
                    // Reply failed: the shim is gone, so the lease it was just granted is orphaned.
                    if granted {
                        let mut st = state.lock().unwrap();
                        st.release(&session, &key);
                    }
                    return;
                }
            }
            Request::Shutdown => {
                let _ = write(&mut wr, &Response::Ok).await;
                let _ = shutdown.send(()).await;
                return;
            }
            other => {
                let resp = dispatch(other, &state);
                if write(&mut wr, &resp).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn timeout_reply(state: &Shared, session: &str, key: &str, since: std::time::Instant) -> Response {
    let mut st = state.lock().unwrap();
    let holder = st.abandon_wait(session, key, Some(since.elapsed().as_millis() as u64)).unwrap_or_default();
    let holder_label = st.sessions.get(&holder).and_then(|m| m.label.clone());
    Response::Blocked { path: key.to_string(), holder, holder_label, waited_ms: since.elapsed().as_millis() as u64 }
}

fn abandon(state: &Shared, session: &str, key: &str) {
    let mut st = state.lock().unwrap();
    st.abandon_wait(session, key, None);
    // If we were granted in the race, give it back.
    st.release(session, key);
}

fn dispatch(req: Request, state: &Shared) -> Response {
    let mut st = state.lock().unwrap();
    match req {
        Request::Hello { session, harness, label, cwd: _ } => {
            st.hello(&session, harness, label);
            Response::Ok
        }
        Request::SetLabel { session, label } => {
            st.set_label(&session, label);
            Response::Ok
        }
        Request::Release { session, path, cwd } => {
            if let Some(k) = st.key(&cwd, &path) {
                st.release(&session, &k);
            }
            Response::Ok
        }
        Request::ReleaseAll { session } => {
            st.release_all(&session, true);
            Response::Ok
        }
        Request::ForceRelease { path, cwd } => match st.key(&cwd, &path) {
            Some(k) if st.force_release(&k) => Response::Ok,
            Some(k) => Response::Error { message: format!("{k} is not leased") },
            None => Response::Error { message: "path is outside the repo".into() },
        },
        Request::Reap { session } => {
            if st.reap(&session) {
                Response::Ok
            } else {
                Response::Error { message: format!("no session {session}") }
            }
        }
        Request::RecordRead { session, paths, cwd } => {
            let keys: Vec<String> = paths.iter().filter_map(|p| st.key(&cwd, p)).collect();
            st.record_read(&session, &keys);
            Response::Ok
        }
        Request::ValidateWrite { session, targets, cwd } => {
            let keys: Vec<String> = targets.iter().filter_map(|p| st.key(&cwd, p)).collect();
            st.validate_write(&session, &keys)
        }
        Request::WriteDone { session, paths, cwd } => {
            let keys: Vec<String> = paths.iter().filter_map(|p| st.key(&cwd, p)).collect();
            st.write_done(&session, &keys);
            Response::Ok
        }
        Request::Heartbeat { session, tool } => {
            st.heartbeat(&session, tool);
            Response::Ok
        }
        Request::SessionEnd { session } => {
            st.session_end(&session);
            Response::Ok
        }
        Request::Status => Response::Status(st.status()),
        Request::Why { path, cwd } => match st.key(&cwd, &path) {
            Some(k) => Response::Why(st.why(&k)),
            None => Response::Error { message: "path is outside the repo".into() },
        },
        Request::Undo { path, cwd, steps } => match st.key(&cwd, &path) {
            Some(k) => match st.undo(&k, steps) {
                Ok(oid) => Response::Restored { path: k, blob_oid: oid },
                Err(e) => Response::Error { message: e },
            },
            None => Response::Error { message: "path is outside the repo".into() },
        },
        Request::Ping => Response::Pong {
            pid: std::process::id(),
            repo: st.repo_root.to_string_lossy().to_string(),
            uptime_ms: st.started.elapsed().as_millis() as u64,
        },
        Request::Subscribe | Request::Acquire { .. } | Request::Shutdown => unreachable!(),
    }
}

async fn subscribe(wr: &mut tokio::net::tcp::OwnedWriteHalf, state: &Shared) {
    let (mut rx, backlog) = {
        let st = state.lock().unwrap();
        (st.events.subscribe(), st.recent.iter().cloned().collect::<Vec<_>>())
    };
    for ev in backlog {
        if write(wr, &Response::Event(ev)).await.is_err() {
            return;
        }
    }
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if write(wr, &Response::Event(ev)).await.is_err() {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        }
    }
}

async fn write(wr: &mut tokio::net::tcp::OwnedWriteHalf, resp: &Response) -> std::io::Result<()> {
    let mut s = serde_json::to_string(resp).map_err(std::io::Error::other)?;
    s.push('\n');
    wr.write_all(s.as_bytes()).await?;
    wr.flush().await
}
