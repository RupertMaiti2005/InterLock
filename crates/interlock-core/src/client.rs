//! Synchronous std-only client. Used by the shim (must not pull an async runtime) and the CLI.

use crate::discovery;
use crate::protocol::{Request, Response};
use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

pub struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    /// Connect to the daemon for `repo_root`. Fails fast if no port file or no listener.
    pub fn connect(repo_root: &Path, timeout: Duration) -> Result<Client> {
        let port = discovery::read_port(repo_root).ok_or_else(|| anyhow!("no daemon port file"))?;
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let stream = TcpStream::connect_timeout(&addr, timeout).context("connect")?;
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client { stream, reader })
    }

    /// Connect, or spawn the daemon and retry for up to `wait`.
    pub fn connect_or_start(repo_root: &Path, wait: Duration) -> Result<Client> {
        if let Ok(c) = Client::connect(repo_root, Duration::from_millis(50)) {
            return Ok(c);
        }
        discovery::spawn_daemon(repo_root).context("spawn daemon")?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Ok(c) = Client::connect(repo_root, Duration::from_millis(50)) {
                return Ok(c);
            }
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("daemon did not come up"));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        Ok(())
    }

    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<Response> {
        self.stream.set_read_timeout(timeout)?;
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(anyhow!("daemon closed connection"));
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub fn request(&mut self, req: &Request, timeout: Duration) -> Result<Response> {
        self.send(req)?;
        self.recv(Some(timeout))
    }

    /// Turn this connection into an event stream.
    pub fn subscribe(mut self) -> Result<impl Iterator<Item = Result<crate::protocol::Event>>> {
        self.send(&Request::Subscribe)?;
        self.stream.set_read_timeout(None)?;
        Ok(std::iter::from_fn(move || match self.recv(None) {
            Ok(Response::Event(e)) => Some(Ok(e)),
            Ok(Response::Ok) => Some(Err(anyhow!("unexpected ok"))),
            Ok(other) => Some(Err(anyhow!("unexpected {:?}", other))),
            Err(e) => {
                if e.to_string().contains("closed") {
                    None
                } else {
                    Some(Err(e))
                }
            }
        }))
    }
}
