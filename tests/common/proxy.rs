//! A listener that stands where the proxy of a test server is, to see what a process in it tries to
//! reach: the HTTP client of the product reads `ALL_PROXY` from its environment, so a fetch from
//! herdr.dev comes to it as `CONNECT herdr.dev:443 HTTP/1.1`, and it is answered by closing the
//! connection, which makes the fetch fail. Nothing leaves the machine either way.

// Each test file that includes `common` uses a subset of it; unused-item warnings would otherwise
// fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct Proxy {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn start() -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut line = String::new();
                            let _ = BufReader::new(&stream).read_line(&mut line);
                            seen.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(line.trim_end().to_string());
                            // Dropped here: the connection closes, and the fetch behind it fails.
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            })
        };
        Proxy {
            port,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    /// What goes in `ALL_PROXY` to reach this listener.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The first line of every connection that has been made to it, so far.
    pub fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
