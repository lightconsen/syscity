//! PTY-level integration tests: the layer below every seam.
//!
//! The loop tests inject actions and fake backends, so the terminal itself is
//! always *assumed* to behave — raw mode entered and left, the cursor query
//! answered, signals caught. These tests run the real binary under a real
//! pseudo-terminal against a real in-process gateway, and assert exactly
//! those: that the TUI leaves a live terminal cooked, on ordinary exit and on
//! `SIGTERM`.
//!
//! The reader thread answers the cursor-position query (`ESC[6n`) the way an
//! actual terminal emulator would, because ratatui's inline viewport blocks
//! until it is. That answered path is asserted in one test rather than
//! assumed, and is what keeps the others from hanging.

#![cfg(unix)]
// Integration tests are a separate crate, so the `cfg_attr(test, allow(...))`
// in lib.rs does not reach here; a test asserts by panicking, which is exactly
// what expect/unwrap are for.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serial_test::serial;
use syscity::gateway::protocol::AuthMode;
use syscity::gateway::{Gateway, GatewayConfig};
use tokio::net::TcpStream;

/// Budget for every "the TUI should have done X by now" wait. Generous: a
/// debug build of a big binary starting under a pty is not fast.
const WAIT: Duration = Duration::from_secs(60);

/// A running `syscity tui` under a pty, plus the master end we drive.
struct Running {
    /// `None` once it has been moved into the waiter.
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Kept alive: dropping the master tears down the pty.
    _master: Box<dyn MasterPty + Send>,
    /// The slave's tty device — its termios is what the user's terminal
    /// looks like, and it survives the child while we hold the master.
    tty: PathBuf,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<Vec<u8>>>,
    /// Whether the child ever emitted a cursor-position query.
    queried_cursor: Arc<AtomicBool>,
}

impl Running {
    /// Wait until the stripped output contains `needle`.
    async fn expect_output(&self, needle: &str, why: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let text = plain(&self.snapshot());
            if text.contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "no {needle:?} in output ({why}): {text}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.output.lock().expect("output").clone()
    }

    /// Send keystrokes to the child's terminal.
    fn feed(&self, keys: &str) {
        let mut writer = self.writer.lock().expect("writer");
        writer.write_all(keys.as_bytes()).expect("feed");
        writer.flush().expect("feed flush");
    }

    async fn wait_exit(&mut self) -> portable_pty::ExitStatus {
        let child = self.child.take().expect("the child is still running");
        tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || {
                let mut child = child;
                child.wait().expect("wait")
            }),
        )
        .await
        .expect("the TUI exited within its budget")
        .expect("the waiter did not panic")
    }

    fn terminate(&self) {
        let pid = self.child.as_ref().expect("child").process_id();
        let pid = pid.expect("a pid");
        // SAFETY: signalling our own spawned child by pid.
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(rc, 0, "kill(SIGTERM) failed");
    }
}

/// Spawn `syscity tui --port <port>` under a fresh pty against `port`.
fn spawn_tui(port: u16) -> Running {
    let exe = std::env::var("CARGO_BIN_EXE_syscity").expect("the syscity bin is a test dep");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let tty = pair.master.tty_name().expect("the pty has a tty path");

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
    let output = Arc::new(Mutex::new(Vec::new()));
    let queried_cursor = Arc::new(AtomicBool::new(false));

    // Pump master → output, answering the cursor query like a terminal would.
    {
        let output = Arc::clone(&output);
        let writer = Arc::clone(&writer);
        let queried = Arc::clone(&queried_cursor);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if std::env::var("PTY_DEBUG").is_ok() {
                            eprintln!("MASTER CHUNK: {:?}", &chunk[..n.min(200)]);
                        }
                        if chunk.windows(3).any(|w| w == b"[6n") {
                            if std::env::var("PTY_DEBUG").is_ok() {
                                eprintln!("ANSWERING CURSOR QUERY");
                            }
                            queried.store(true, Ordering::SeqCst);
                            // Cursor at row 1, column 1.
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(b"\x1b[1;1R");
                                let _ = w.flush();
                            }
                        }
                        output.lock().expect("output").extend_from_slice(chunk);
                    }
                }
            }
        });
    }

    let mut cmd = CommandBuilder::new(&exe);
    let args: Vec<String> = vec!["tui", "--host", "127.0.0.1", "--port"]
        .into_iter()
        .map(String::from)
        .collect();
    let mut all = args;
    all.push(port.to_string());
    cmd.args(&all);
    cmd.env("TERM", "xterm-256color");
    cmd.env("RUST_LOG", "off");
    // Isolated so the test cannot touch a real user's config or state.
    cmd.env("SYSCITY_HOME", std::env::temp_dir().join(format!("syscity_pty_{port}")));
    let child = pair.slave.spawn_command(cmd).expect("spawn the TUI");

    Running {
        child: Some(child),
        _master: pair.master,
        tty,
        writer,
        output,
        queried_cursor,
    }
}

/// The tty's current line discipline: cooked means ICANON and ECHO are on.
fn is_cooked(tty: &std::path::Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(tty)
        .expect("open the pty slave");
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: zeroed termios + valid fd is the documented tcgetattr shape.
    let rc = unsafe { libc::tcgetattr(file.as_raw_fd(), &mut termios) };
    assert_eq!(rc, 0, "tcgetattr failed");
    const COOKED: libc::tcflag_t = libc::ICANON | libc::ECHO;
    termios.c_lflag & COOKED == COOKED
}

/// Drop the escape sequences, keep what a user would read.
fn plain(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // final byte of the CSI sequence
            } else {
                i += 1; // two-character escape (or truncated)
            }
        } else {
            // Decode one UTF-8 sequence at a time so CJK survives intact.
            let rest = &bytes[i..];
            let len = utf8_len(rest[0]);
            text.push_str(&String::from_utf8_lossy(&rest[..len.min(rest.len())]));
            i += len;
        }
    }
    text
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Start a real gateway on a free port and wait for it to accept.
// The nested fields (storage.*, security.*) cannot be set in one struct
// literal, so the reassignments after `default()` are the shape this takes.
#[allow(clippy::field_reassign_with_default)]
async fn start_gateway() -> u16 {
    let port = free_port();
    let db = std::env::temp_dir().join(format!("syscity_pty_test_{port}.db"));
    let _ = std::fs::remove_file(&db);
    let mut config = GatewayConfig::default();
    config.host = "127.0.0.1".to_string();
    config.port = port;
    config.storage.storage_type = "sqlite".to_string();
    config.storage.database_url = Some(format!("sqlite:{}", db.display()));
    config.security.auth_mode = AuthMode::None;
    config.security.local_scopes = vec!["chat".into(), "read".into(), "write".into()];
    config.plugins.enabled = false;
    config.channels.clear();
    config.vector_memory.enabled = false;

    let gateway = Gateway::new(config, None).await.expect("gateway");
    tokio::spawn(async move {
        let _ = gateway.start().await;
    });
    let deadline = Instant::now() + WAIT;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return port;
        }
        assert!(Instant::now() < deadline, "the gateway did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ask for a port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Startup asks the terminal where the cursor is, then paints.
///
/// ratatui's inline viewport cannot exist without the `ESC[6n]` answer, and
/// every other test relies on that answer arriving — so this pins the request
/// going out and the connected UI coming back, through the real crossterm,
/// not the TestBackend stub.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn startup_queries_the_cursor_and_paints_the_composer() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("connected", "the greeting").await;
    assert!(
        tui.queried_cursor.load(Ordering::SeqCst),
        "an inline viewport must ask the terminal for the cursor position"
    );
    tui.feed("/quit\r");
    let status = tui.wait_exit().await;
    assert!(status.success(), "exit {status:?}");
}

/// An ordinary `/quit` restores the terminal it borrowed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn quit_restores_the_terminal() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    assert!(!is_cooked(&tui.tty), "the running TUI owns the tty — it must be raw");

    tui.feed("/quit\r");
    let status = tui.wait_exit().await;
    assert!(status.success(), "exit {status:?}");
    assert!(is_cooked(&tui.tty), "/quit must hand back a working terminal");
}

/// A `SIGTERM` also restores it — the regression the signal handling exists
/// for, asserted end to end: signals bypass the event loop and the panic hook,
/// and before `SignalWatch` they left the user's terminal in raw mode.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn terminate_restores_the_terminal() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    assert!(!is_cooked(&tui.tty), "raw while running");

    tui.terminate();
    let status = tui.wait_exit().await;
    assert!(status.success(), "SIGTERM must exit cleanly, got {status:?}");
    assert!(is_cooked(&tui.tty), "a terminated TUI must still leave a working terminal");
}
