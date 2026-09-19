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
    master: Box<dyn MasterPty + Send>,
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

    /// Wait until the TUI has written anything past `from`.
    ///
    /// The assertion a resize needs: something was drawn, without asking for a
    /// particular string — the TUI's own repaint is the evidence.
    async fn expect_growth(&self, from: usize, why: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            if self.snapshot().len() > from {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no output past byte {from} ({why}): {:?}",
                plain(&self.snapshot())
            );
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
        self.signal(libc::SIGTERM);
    }

    fn interrupt(&self) {
        self.signal(libc::SIGINT);
    }

    fn signal(&self, sig: libc::c_int) {
        let pid = self.child.as_ref().expect("child").process_id();
        let pid = pid.expect("a pid");
        // SAFETY: signalling our own spawned child by pid.
        let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
        assert_eq!(rc, 0, "kill({sig}) failed");
    }

    /// Resize the pty, which signals SIGWINCH to the child like a terminal
    /// emulator would.
    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the pty");
    }
}

/// Counts cursor-position queries (`ESC[6n`) in a stream that arrives in
/// chunks, remembering the tail so one split across a boundary is still seen.
///
/// Missing a query is not a small miss: ratatui's inline viewport blocks in
/// `get_cursor_position` until the reply arrives, and it re-queries on resize,
/// so a missed query is a TUI that stops drawing forever.
#[derive(Default)]
struct QueryScanner {
    /// The last two bytes of the previous chunk.
    carry: Vec<u8>,
}

impl QueryScanner {
    /// Feed one chunk; returns how many queries it completed.
    fn feed(&mut self, chunk: &[u8]) -> usize {
        let mut scanned = std::mem::take(&mut self.carry);
        scanned.extend_from_slice(chunk);
        let mut found = 0;
        while let Some(at) = scanned.windows(3).position(|w| w == b"[6n") {
            found += 1;
            scanned.drain(..at + 3);
        }
        self.carry = scanned[scanned.len().saturating_sub(2)..].to_vec();
        found
    }
}

/// Spawn `syscity tui --port <port>` under a fresh pty against `port`.
fn spawn_tui(port: u16) -> Running {
    spawn_tui_with(port, &[])
}

/// The same, with extra environment for the child (a debug drill, say).
fn spawn_tui_with(port: u16, extra_env: &[(&str, &str)]) -> Running {
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
            let mut scanner = QueryScanner::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if std::env::var("PTY_DEBUG").is_ok() {
                            eprintln!("MASTER CHUNK: {:?}", &chunk[..n.min(200)]);
                        }
                        for _ in 0..scanner.feed(chunk) {
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
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let child = pair.slave.spawn_command(cmd).expect("spawn the TUI");

    Running {
        child: Some(child),
        master: pair.master,
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

/// Typing `/` shows the command candidates — the completion state and the
/// Tab key existed for a while, but nothing rendered the list, so a slash
/// looked exactly like a dead key.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_slash_shows_command_hints() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    tui.feed("/");
    tui.expect_output("/new", "a candidate from the catalog")
        .await;

    // Backspace the `/` away before typing the real command.
    tui.feed("\x7f/quit\r");
    let status = tui.wait_exit().await;
    assert!(status.success(), "exit {status:?}");
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

/// What the TUI puts on the wire for CJK text: the raw bytes must contain the
/// characters back to back, with no padding between them.
///
/// "Chinese looks sparse" reports split into two causes — bytes the TUI
/// emitted (ours to fix) and how the terminal renders or copies wide cells
/// (not ours). This test pins the first half: if a space ever appears between
/// two hanzi on the wire, the bug is in our rendering path, not the terminal.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn cjk_text_is_written_without_padding() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    // The submitted line echoes into scrollback through `insert_before` — the
    // exact path a "sparse Chinese" complaint would come from.
    tui.feed("你好小王很稀疏吗\r");
    tui.expect_output("稀疏", "the user echo").await;

    let raw = tui.snapshot();
    // The composer repaints on every keystroke, so hanzi appear in the stream
    // several times; the echo through `insert_before` is the LAST one, written
    // after Enter is processed and the composer is cleared. A correct renderer
    // may move the cursor between cells (escape sequences), but must never put
    // a space between two hanzi — strip the escapes and read it like a user.
    let first = "你".as_bytes();
    let last_at = raw
        .windows(first.len())
        .rposition(|w| w == first)
        .expect("the echo reached the terminal at all");
    let end = (last_at + 200).min(raw.len());
    let echo = plain(&raw[last_at..end]);
    assert!(
        echo.starts_with("你好小王很稀疏吗"),
        "the scrollback echo must be tight, got: {echo:?}"
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

/// `SIGINT` from outside the process (not a Ctrl+C keystroke, which arrives
/// as a byte in raw mode) takes the same signal arm as SIGTERM.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn interrupt_restores_the_terminal() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    assert!(!is_cooked(&tui.tty), "raw while running");

    tui.interrupt();
    let status = tui.wait_exit().await;
    assert!(status.success(), "SIGINT must exit cleanly, got {status:?}");
    assert!(is_cooked(&tui.tty), "an interrupted TUI must still leave a working terminal");
}

/// A panic mid-run is the hook's whole job: the process dies (non-zero exit,
/// the panic message on the pty) but the terminal comes back cooked.
///
/// The drill is an env var checked in debug builds only, fired after the hook
/// is installed with the terminal raw — its worst case, not a gentle one.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_panic_restores_the_terminal() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || {
        spawn_tui_with(port, &[("SYSCITY_TUI_DEBUG_PANIC", "1")])
    })
    .await
    .expect("spawn");

    let status = tui.wait_exit().await;
    assert!(!status.success(), "a panic is not a clean exit: {status:?}");
    assert!(is_cooked(&tui.tty), "the panic hook must leave a working terminal");
    let text = plain(&tui.snapshot());
    assert!(
        text.contains("SYSCITY_TUI_DEBUG_PANIC drill"),
        "the original hook still reports the panic: {text:?}"
    );
}

/// A *fatal* error takes the other exit: it returns an `Err` rather than
/// unwinding, so it is the `restore?` line after the loop that hands the
/// terminal back, not the panic hook. Both paths have to leave it cooked.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_fatal_error_restores_the_terminal() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || {
        spawn_tui_with(port, &[("SYSCITY_TUI_DEBUG_FATAL", "1")])
    })
    .await
    .expect("spawn");

    let status = tui.wait_exit().await;
    assert!(!status.success(), "a fatal error is not a clean exit: {status:?}");
    assert!(is_cooked(&tui.tty), "a fatal error must still leave a working terminal");
    let text = plain(&tui.snapshot());
    assert!(
        text.contains("SYSCITY_TUI_DEBUG_FATAL drill"),
        "the error is reported on the restored terminal: {text:?}"
    );
}

/// Resizing the window must not paint outside the new bounds.
///
/// After the pty goes from 80 to 40 columns, every explicit cursor move the
/// TUI writes has to stay within 40 — a draw at the old width would smear
/// cells into wrapped rows a real terminal never asked for.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_resize_redraws_within_the_new_bounds() {
    let port = start_gateway().await;
    let mut tui = tokio::task::spawn_blocking(move || spawn_tui(port))
        .await
        .expect("spawn");

    tui.expect_output("> ", "the composer").await;
    // Bytes written before this point were legitimately drawn at 80 columns;
    // only what the TUI emits after the resize may be judged against 40.
    let resized_at = tui.snapshot().len();
    tui.resize(24, 40);

    // The resize alone has to produce the frame: a resize arrives as SIGWINCH,
    // which the loop turns into a dirty mark and a draw. Waiting on *that* —
    // rather than on a keystroke drawn afterwards — keeps this test about
    // geometry. A key press would drag in crossterm's cursor-position query,
    // which shares an event queue with `event::poll` (crossterm documents that
    // they block each other) and made this test flaky on CI for reasons that
    // had nothing to do with the width.
    tui.expect_growth(resized_at, "a frame at the new width")
        .await;

    let raw = tui.snapshot();
    let overruns: Vec<u16> = cursor_columns(&raw[resized_at..])
        .into_iter()
        .filter(|c| *c > 40)
        .collect();
    assert!(overruns.is_empty(), "cursor moved past column 40: {overruns:?}");

    // The audit's criterion for a resize: the composer and the status row are
    // still there and still whole afterwards. A redraw that dropped either,
    // or painted them over each other, would show as a missing prompt.
    let after = plain(&raw[resized_at..]);
    assert!(
        after.contains('v') && after.contains("> "),
        "both the status row and the composer survived the resize: {after:?}"
    );

    tui.feed("\x7f/quit\r");
    let status = tui.wait_exit().await;
    assert!(status.success(), "exit {status:?}");
}

/// Every column a `CSI row;col H` cursor move targeted, in order.
fn cursor_columns(bytes: &[u8]) -> Vec<u16> {
    let mut cols = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'H' && j > start {
                let params = String::from_utf8_lossy(&bytes[start..j]);
                if let Some((_, col)) = params.split_once(';') {
                    if let Ok(col) = col.parse::<u16>() {
                        cols.push(col);
                    }
                }
            }
            i = j.max(i + 2);
        } else {
            i += 1;
        }
    }
    cols
}

/// The scanner has to survive a query split across two reads, because the
/// read boundary is the kernel's choice and the TUI blocks waiting for the
/// answer.
#[test]
fn a_cursor_query_split_across_chunks_is_still_seen() {
    let mut scanner = QueryScanner::default();
    assert_eq!(scanner.feed(b"\x1b[6"), 0, "not yet a query");
    assert_eq!(scanner.feed(b"n"), 1, "completed by the next chunk");
    assert_eq!(scanner.feed(b"rest"), 0, "and not counted twice");

    // Whole queries, several in one chunk.
    let mut scanner = QueryScanner::default();
    assert_eq!(scanner.feed(b"\x1b[6n\x1b[6n"), 2);

    // A split that leaves the carry holding a partial prefix which then turns
    // out not to be one.
    let mut scanner = QueryScanner::default();
    assert_eq!(scanner.feed(b"\x1b["), 0);
    assert_eq!(scanner.feed(b"31mhello"), 0);
}
