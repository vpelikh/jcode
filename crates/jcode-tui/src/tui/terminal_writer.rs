//! A non-blocking terminal output writer.
//!
//! `ratatui::Terminal::flush` (and the drawing/cursor backend calls) write
//! synchronously to the underlying writer. When that writer is a pty (fd 1)
//! and the terminal stops draining the pty's output buffer (a backgrounded
//! tab, a dead multiplexer pane, a frozen SSH session, a `SIGSTOP`'d emulator),
//! the next `write(2)` parks in the kernel and never returns. Because the
//! interactive TUI renders on a single thread, that single blocking syscall
//! freezes the whole UI: cursor, input, animations, and scroll all stop.
//!
//! [`TerminalWriter`] fixes that by moving the real pty write onto a dedicated
//! writer thread. The render thread's `write`/`flush` only hand bytes over an
//! in-process channel and return immediately -- they never perform a blocking
//! pty syscall. This isolates the render loop from a wedged pty the way the
//! ratatui maintainers recommend ("provide `CrosstermBackend` with a custom
//! `Write` implementation ... that moves terminal output behind whatever
//! buffering, thread, channel, timeout, or cancellation policy the application
//! requires").
//!
//! ## Backpressure policy
//!
//! The channel is bounded in *bytes* by a shared atomic counter. When the pty
//! is not draining and the outstanding backlog reaches the cap, the render
//! thread *drops newer* chunks instead of blocking or growing memory. This is
//! the natural policy for an interactive TUI: the latest frame is what the user
//! should see, and once the pty drains again the next full frame supersedes
//! whatever was dropped. A single over-cap frame is still enqueued on a draining
//! consumer (so a large full-screen redraw is never spuriously lost), but once
//! the backlog saturates because the consumer is genuinely not draining, newer
//! chunks are dropped. The render loop stays alive and responsive regardless of
//! the pty state.
//!
//! ## Lifecycle
//!
//! The writer thread owns the real `Stdout`. When the last [`TerminalWriter`]
//! handle is dropped it sends a shutdown marker and waits (with a short
//! timeout) for the writer thread to drain remaining output and flush the pty.
//! If the pty is wedged and cannot drain within the timeout, the drop proceeds
//! without waiting so the session can exit and be resumed elsewhere; teardown
//! output on a wedged pty cannot be flushed anyway.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Cap on the *outstanding* (queued-but-not-yet-drained) output backlog in
/// bytes. Once the pty is not draining and this backlog saturates, the render
/// thread drops newer chunks rather than blocking or growing memory. 256 KiB is
/// comfortably larger than the kernel pty buffer and small enough that a
/// wedged-pty backlog can never balloon.
const QUEUE_CAPACITY_BYTES: usize = 256 * 1024;

/// How long `drop` may wait for the writer thread to drain before abandoning it
/// (the pty is wedged and would block forever).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// Set when a chunk is dropped because the pty is not draining, and cleared by
/// [`take_resync_requested`] once the app has forced a full re-emit.
///
/// When the writer drops output, ratatui's internal previous buffer diverges
/// from the real terminal: it still believes the dropped cells reached the
/// screen, so the next differential frame will not re-emit them. After the pty
/// drains, the app must force a soft full repaint so every cell is re-emitted
/// and ratatui's model matches the screen again. There is only ever one live
/// writer (the app terminal), so a crate-level flag is both safe and avoids
/// exposing unstable backend writer access.
static RESYNC_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether the app should force a full re-emit because the writer dropped
/// output while the pty was wedged. Clears the flag.
pub fn take_resync_requested() -> bool {
    RESYNC_REQUESTED.swap(false, Ordering::AcqRel)
}

/// Non-consuming peek at whether a resync is pending. Used by the idle-animation
/// fast path to decide whether to stand down, *without* clearing the flag so the
/// following full frame can still consume it and actually perform the heal.
pub fn resync_pending() -> bool {
    RESYNC_REQUESTED.load(Ordering::Acquire)
}

/// A registered live terminal writer that auxiliary output can enqueue into.
///
/// Auxiliary output (window titles, OSC-52 clipboard, turn notifications,
/// terminal-mode re-apply) must be serialized with the render writer thread's
/// frame bytes, otherwise its multi-byte escape sequences interleave with a
/// frame's cell writes on the same terminal and corrupt the stream — the real
/// screen then shows stray glyphs that a later full repaint clears. Routing
/// these writes through the *same* writer thread channel gives one ordered byte
/// stream and keeps them non-blocking (they drop on a wedged pty rather than
/// park). Registered in [`TerminalWriter::stdout`] (and the non-unix stdout
/// path) and cleared on shutdown.
struct LiveWriter {
    tx: Sender<Chunk>,
    inner: Arc<WriterInner>,
}

static LIVE_WRITER: Mutex<Option<LiveWriter>> = Mutex::new(None);

fn register_live_writer(tx: Sender<Chunk>, inner: Arc<WriterInner>) {
    *LIVE_WRITER.lock().unwrap() = Some(LiveWriter { tx, inner });
}

/// Outcome of an auxiliary terminal-write attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuxWriteResult {
    /// A live writer accepted (or was told to take) the bytes. It may still be
    /// applied asynchronously by the writer thread.
    Accepted,
    /// No live writer is registered; the caller should fall back to writing
    /// `io::stdout()` directly.
    Fallback,
    /// The bytes were dropped because the pty is wedged (backlog saturated) so
    /// they cannot be applied. The caller should report the failure rather than
    /// treating it as a successful delivery.
    Dropped,
}

/// Enqueue auxiliary terminal-output bytes through the live writer thread so
/// they are serialized with frame output. Returns the outcome distinguishing a
/// genuine hand-off from a fallback and from a wedged-pty drop (see
/// [`AuxWriteResult`]), so callers can report success/failure honestly and
/// fall back to `io::stdout()` only when no live writer routes there.
///
/// Never blocks: sending into the unbounded channel is non-blocking.
pub(crate) fn write_auxiliary(bytes: &[u8]) -> AuxWriteResult {
    if bytes.is_empty() {
        return AuxWriteResult::Accepted;
    }
    // Copy the live writer's handles out from under the lock so we never hold
    // the global registration lock across the CAS reservation, the allocation,
    // or the channel send. Those can be contended, and the lock should protect
    // registration state only, not the per-writer send path.
    let (tx, inner) = {
        let guard = LIVE_WRITER.lock().unwrap();
        let Some(live) = guard.as_ref() else {
            return AuxWriteResult::Fallback;
        };
        (live.tx.clone(), Arc::clone(&live.inner))
    };
    let len = bytes.len();
    let mut prev = inner.buffered.load(Ordering::Relaxed);
    loop {
        if prev >= QUEUE_CAPACITY_BYTES {
            // Wedged pty; drop the auxiliary bytes rather than block. Reported
            // as Dropped so the caller can surface a failure honestly instead
            // of reporting that the bytes reached the terminal.
            return AuxWriteResult::Dropped;
        }
        let Some(next) = prev.checked_add(len) else {
            return AuxWriteResult::Dropped;
        };
        match inner
            .buffered
            .compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(cur) => prev = cur,
        }
    }
    let body: Box<[u8]> = bytes.to_vec().into_boxed_slice();
    match tx.send(Chunk::Data(body)) {
        Ok(()) => AuxWriteResult::Accepted,
        Err(_) => {
            inner.buffered.fetch_sub(len, Ordering::Relaxed);
            AuxWriteResult::Fallback
        }
    }
}

/// Write auxiliary terminal-output bytes, serialized with the render writer
/// when a live writer is registered, else directly to stdout.
///
/// This is the single entry point for non-frame terminal output (window titles,
/// OSC-52 clipboard, turn notifications, mode re-apply). Using it guarantees
/// these escape sequences never interleave with frame bytes on the same
/// terminal. When no writer is live (startup, session picker, teardown) it
/// falls back to `io::stdout()`, where there is no concurrent renderer to race.
///
/// Returns whether the bytes were durably handed off. A live-writer `Dropped`
/// (wedged pty) is reported as failure, so callers that surface success/failure
/// (clipboard copy, turn notification fallback) do not report a dropped write
/// as though it reached the terminal.
pub(crate) fn write_serialized(bytes: &[u8]) -> bool {
    match write_auxiliary(bytes) {
        AuxWriteResult::Accepted => true,
        AuxWriteResult::Fallback => {
            let mut out = io::stdout();
            out.write_all(bytes).is_ok() && out.flush().is_ok()
        }
        AuxWriteResult::Dropped => false,
    }
}

enum Chunk {
    Data(Box<[u8]>),
    Shutdown,
}

struct WriterInner {
    /// Bytes currently queued (sent to the channel but not yet drained).
    buffered: AtomicUsize,
    /// Fires once the writer thread has fully drained and flushed (or exited).
    done: Mutex<Option<Receiver<()>>>,
    /// Thread handle, taken on shutdown so the last writer can join.
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// A `Write` that forwards bytes to a dedicated writer thread.
///
/// Safe to construct on the render thread and pass to `CrosstermBackend::new`.
/// The render thread never calls the real pty `write`; it only sends into the
/// channel and returns immediately.
pub struct TerminalWriter {
    tx: Option<Sender<Chunk>>,
    inner: Arc<WriterInner>,
    /// Coalescing scratch buffer. ratatui's `CrosstermBackend` calls `write` once
    /// per cell/command, so buffering here and sending to the channel on `flush`
    /// turns a per-frame burst of tiny allocations into one chunk (cheaper, and
    /// drops whole frames instead of individual cells on a wedged pty). This
    /// buffer lives only on the render thread (behind `&mut self`), never the
    /// writer thread.
    pending: Vec<u8>,
}

impl TerminalWriter {
    /// Spawn a writer thread that owns `writer` and serve writes for it.
    pub fn new<W>(writer: W) -> Self
    where
        W: Write + Send + 'static,
    {
        let (tx, rx): (Sender<Chunk>, Receiver<Chunk>) = mpsc::channel();
        let (done_tx, done_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let inner = Arc::new(WriterInner {
            buffered: AtomicUsize::new(0),
            done: Mutex::new(Some(done_rx)),
            handle: Mutex::new(None),
        });
        let thread_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("jcode-terminal-writer".into())
            .spawn(move || run_writer(writer, rx, done_tx, &thread_inner))
            .expect("failed to spawn terminal writer thread");
        *inner.handle.lock().unwrap() = Some(handle);
        Self { tx: Some(tx), inner, pending: Vec::new() }
    }

    /// Register this writer as the process's live terminal writer so auxiliary
    /// output (`write_auxiliary`/`write_serialized`) is serialized with its
    /// frame bytes. There is at most one live writer at a time; a later
    /// terminal's construction re-registers over it, and `shutdown` clears it.
    fn register_as_live(&self) {
        if let Some(tx) = self.tx.as_ref() {
            register_live_writer(tx.clone(), Arc::clone(&self.inner));
        }
    }

    /// Clear the global live writer only if `self` is the currently-registered
    /// one. Other writers (e.g. plain `new()`-constructed writers used in
    /// tests, or a superseded terminal) must not clobber the live writer's
    /// registration while it is still serving output.
    fn unregister_live_writer(&self) {
        let mut guard = LIVE_WRITER.lock().unwrap();
        if guard
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(&live.inner, &self.inner))
        {
            *guard = None;
        }
    }

    /// Create a writer thread over a `dup` of fd 1 (stdout) that does not
    /// participate in the process-wide [`Stdout`] lock.
    ///
    /// Why this matters: a `Stdout` handle acquires the global stdout reentrant
    /// mutex for the duration of each `write`. If the pty is wedged, the writer
    /// thread blocks inside `write(2)` while *holding that lock*, so any other
    /// `io::stdout()` caller on the render loop (mode re-application on
    /// `FocusGained`, OSC‑52 clipboard, window title) would block waiting for the
    /// lock — reintroducing the exact render-loop freeze the shim removes. Using a
    /// raw duplicated fd means the wedged `write(2)` holds no user-space lock, so
    /// concurrent `io::stdout()` calls proceed independently.
    ///
    /// [`Stdout`]: std::io::Stdout
    #[cfg(unix)]
    pub fn stdout() -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        // Use `dup` so we own a private descriptor to the terminal; closing it on
        // teardown never affects the real fd 1.
        let dup = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // `File` has no user-space locking, so the writer thread's blocking
        // `write(2)` holds no process-wide lock.
        let file = unsafe { std::fs::File::from_raw_fd(dup) };
        let writer = Self::new(file);
        writer.register_as_live();
        Ok(writer)
    }

    fn shutdown(&mut self) {
        // Stop routing auxiliary output to this writer (it is shutting down);
        // a later terminal's `new` will re-register its own writer.
        self.unregister_live_writer();
        // Flush any coalesced-but-not-yet-enqueued bytes so Drop loses nothing.
        let _ = self.flush();
        let tx = self.tx.take();
        if let Some(tx) = tx {
            // Signal the writer to drain+flush and stop.
            let _ = tx.send(Chunk::Shutdown);
            // Wait (bounded) for it to finish. On a wedged pty the writer cannot
            // finish within the timeout, so abandon it rather than hang the
            // teardown thread; the session is resumable elsewhere.
            let drained = {
                let done = self.inner.done.lock().unwrap();
                if let Some(done) = done.as_ref() {
                    matches!(done.recv_timeout(SHUTDOWN_TIMEOUT), Ok(()))
                } else {
                    false
                }
            };
            if drained
                && let Some(handle) = self.inner.handle.lock().unwrap().take()
            {
                // Only join when the writer actually finished, so we never
                // block on a wedged pty.
                let _ = handle.join();
            }
        }
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Buffer on the render thread. Actual enqueue to the channel happens on
        // `flush`, so a frame's per-cell writes coalesce into one chunk. Never
        // perform the real pty `write` (blocking) here; the writer thread does.
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Enqueue whatever has accumulated since the last flush (usually one
        // full frame) to the channel, which the writer thread writes+flushes to
        // the real pty. This never blocks: sending to an unbounded channel is
        // non-blocking, and drops happen here if the backlog is saturated.
        if self.pending.is_empty() {
            return Ok(());
        }
        let len = self.pending.len();
        let body: Box<[u8]> = std::mem::take(&mut self.pending).into_boxed_slice();

        let Some(tx) = self.tx.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer shut down"));
        };
        // Reserve `len` bytes, dropping the whole frame only when the *current*
        // backlog is already saturated (the consumer is not draining). This bounds
        // memory on a wedged pty and never drops a legitimate fresh frame on a
        // draining consumer.
        let mut prev = self.inner.buffered.load(Ordering::Relaxed);
        loop {
            if prev >= QUEUE_CAPACITY_BYTES {
                // Backlog already saturated (wedged). Drop this frame and flag a
                // resync; ratatui's model no longer matches the real screen.
                RESYNC_REQUESTED.store(true, Ordering::Release);
                return Ok(()); // accepted-with-drop so the render loop never blocks
            }
            let Some(next) = prev.checked_add(len) else {
                RESYNC_REQUESTED.store(true, Ordering::Release);
                return Ok(());
            };
            match self.inner.buffered.compare_exchange(
                prev,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }

        match tx.send(Chunk::Data(body)) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Writer exited; undo the reservation.
                self.inner.buffered.fetch_sub(len, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer thread exited"))
            }
        }
    }
}

/// Drains `rx`, writes each chunk to `writer`, and maintains the byte counter.
fn run_writer<W: Write>(
    mut writer: W,
    rx: Receiver<Chunk>,
    done_tx: Sender<()>,
    inner: &WriterInner,
) {
    while let Ok(chunk) = rx.recv() {
        match chunk {
            Chunk::Shutdown => break,
            Chunk::Data(chunk) => {
                let _ = writer.write_all(&chunk);
                let _ = writer.flush();
                inner.buffered.fetch_sub(chunk.len(), Ordering::Relaxed);
            }
        }
    }
    // Final best-effort flush so teardown output stays coherent.
    let _ = writer.flush();
    let _ = done_tx.send(());
}

/// Application-wide concrete terminal type.
///
/// Ratatui's `DefaultTerminal` is `Terminal<CrosstermBackend<Stdout>>` where the
/// writes go straight to the real pty (and can block forever on a full one).
/// This alias routes the backend's writes through [`TerminalWriter`] so the
/// render loop never performs a blocking pty syscall.
pub type AppTerminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<TerminalWriter>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that install a live writer into the process-global
    /// [`LIVE_WRITER`] (the auxiliary-write test and the unix `stdout()` dup
    /// test), so they cannot race each other's global registration.
    static LIVE_WRITER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the live-writer test lock, tolerating a poison from a sibling
    /// test's panic (the state is just a guard; the guard's panic must not
    /// cascade into `PoisonError` on every other test).
    fn live_writer_test_guard() -> std::sync::MutexGuard<'static, ()> {
        LIVE_WRITER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A writer that forwards each write to a channel so a test can read the
    /// exact ordered bytes the real pty would receive.
    struct ChannelWriter {
        tx: Sender<Vec<u8>>,
    }

    impl Write for ChannelWriter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let _ = self.tx.send(b.to_vec());
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that blocks on the first write until released, simulating a pty
    /// whose output buffer is full and never drains. The writer thread blocks in
    /// `write` (like a kernel `write(2)` on a full pty). The test holds the
    /// [`Sender`] side and can release the thread before returning so no OS thread
    /// is leaked across test runs.
    struct WedgedWriter {
        release: Receiver<()>,
    }

    impl WedgedWriter {
        /// Create a wedge gate. `release` is the sender the test holds; the writer
        /// thread blocks until the test drops/sends it.
        fn new() -> (Self, Sender<()>) {
            let (tx, rx) = mpsc::channel();
            (Self { release: rx }, tx)
        }
    }

    impl Write for WedgedWriter {
        fn write(&mut self, _b: &[u8]) -> io::Result<usize> {
            // Block until released, modeling write(2) blocked on a full pty.
            let _ = self.release.recv();
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn render_thread_never_blocks_when_the_pty_is_wedged() {
        let (wedge, release) = WedgedWriter::new();
        let mut writer = TerminalWriter::new(wedge);
        let start = std::time::Instant::now();
        for i in 0..5000 {
            let data = vec![b'x'; 1024];
            assert!(writer.write_all(&data).is_ok(), "write {i} failed");
            // Flush periodically, as the render loop flushes a frame each tick.
            // This is where the buffered writer enqueues (and, once the wedged
            // consumer saturates, drops) the accumulated bytes.
            if i % 8 == 0 {
                let _ = writer.flush();
            }
        }
        let _ = writer.flush();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "render thread blocked on a wedged pty: {elapsed:?}"
        );
        // Dropped output must flag a resync so the app re-emits the screen once
        // the pty drains, and reading it clears the flag for the next frame.
        // A peek (used by the idle-animation gate) must not consume it, so the
        // full frame's take still sees it and performs the heal.
        assert!(
            resync_pending(),
            "expected a resync request after output was dropped"
        );
        assert!(
            take_resync_requested(),
            "peek must not consume the resync request"
        );
        assert!(
            !take_resync_requested(),
            "resync request should be cleared after reading"
        );
        // Drop must return promptly too: the writer thread is wedged in `write`,
        // so the bounded shutdown drain abandons it rather than joining forever.
        let drop_start = std::time::Instant::now();
        drop(writer);
        assert!(
            drop_start.elapsed() < std::time::Duration::from_secs(1),
            "drop blocked on a wedged pty"
        );
        // Release the wedged writer thread so the test leaves no leaked thread.
        drop(release);
    }

    #[test]
    fn drains_in_order_on_a_normal_writer() {
        let (w, wrx) = mpsc::channel::<Vec<u8>>();
        let mut writer = TerminalWriter::new(ChannelWriter { tx: w });
        writer.write_all(b"hello ").unwrap();
        writer.write_all(b"world").unwrap();
        drop(writer);
        let mut got = String::new();
        while let Ok(chunk) = wrx.try_recv() {
            got.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert_eq!(got, "hello world");
    }

    /// End-to-end: a real `Terminal<CrosstermBackend<TerminalWriter>>` — the exact
    /// `AppTerminal` shape the app runs on — must complete `draw`+`flush` without
    /// blocking even when the underlying pty is wedged and never drains.
    #[test]
    fn app_terminal_draw_never_blocks_on_a_wedged_pty() {
        let (wedge, release) = WedgedWriter::new();
        let writer = TerminalWriter::new(wedge);
        let backend = ratatui::backend::CrosstermBackend::new(writer);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let start = std::time::Instant::now();
        for _ in 0..20 {
            terminal
                // Draw a non-trivial frame (a full-width paragraph) so the backend
                // actually emits a meaningful diff, not a no-op.
                .draw(|frame| {
                    let p = ratatui::widgets::Paragraph::new(
                        ratatui::text::Text::from("the quick brown fox jumps over the lazy dog"),
                    );
                    frame.render_widget(p, frame.area());
                })
                .expect("draw");
            terminal.flush().expect("flush");
        }
        let elapsed = start.elapsed();
        // If the render loop were blocked on the pty, the very first draw would
        // hang forever. 20 full draws completing comfortably distinguishes
        // "non-blocking" from that. A generous 5s bound absorbs test-load jitter
        // while staying far under a multi-frame blocking write would never
        // return at all.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "draw pipeline blocked on a wedged pty: {elapsed:?}"
        );
        // Drop drains with a bounded timeout and abandons a wedged pty; it must
        // not hang the caller.
        let drop_start = std::time::Instant::now();
        drop(terminal);
        assert!(
            drop_start.elapsed() < std::time::Duration::from_secs(1),
            "drop hung on a wedged pty"
        );
        // Release the wedged writer thread so the test leaves no leaked thread.
        drop(release);
        // (The resync-after-drop behavior is covered by the dedicated
        // `render_thread_never_blocks_when_the_pty_is_wedged` test, which
        // forces the byte cap.)
    }

    /// Acceptance-aligned: use a *real* OS pipe whose buffer is full, so the
    /// underlying write is a genuine blocking `write(2)` (exactly how a full pty
    /// wedges). The shim must absorb the real syscall block so the caller never
    /// parks.
    #[cfg(unix)]
    #[test]
    fn real_full_pipe_write_never_blocks_the_caller() {
        use std::os::fd::FromRawFd;
        // Create an OS pipe. Keep the read end open but never read from it, so
        // once the kernel buffer fills, the write end becomes a blocking fd.
        let mut fds = [0i32; 2];
        let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);
        let pipe_writer = unsafe { std::fs::File::from_raw_fd(write_fd) };

        let mut shim = TerminalWriter::new(pipe_writer);
        // Feed far more than the kernel pipe buffer (typically ~64 KiB).
        let big = vec![b'x'; 1024 * 1024];
        let start = std::time::Instant::now();
        for _ in 0..8 {
            // write_all through the shim; the shim must not block even though the
            // underlying pipe write would.
            assert!(shim.write_all(&big).is_ok());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "caller blocked on a real full pipe: {elapsed:?}"
        );

        // Drop: the writer thread is genuinely stuck in write(2); shutdown's
        // bounded drain must abandon it rather than hang.
        let drop_start = std::time::Instant::now();
        drop(shim);
        assert!(
            drop_start.elapsed() < std::time::Duration::from_secs(1),
            "drop hung on a real full pipe"
        );
        // Clean up the read end so the blocking writer thread can be reaped.
        unsafe { libc::close(read_fd) };
    }

    #[test]
    fn zero_length_write_is_a_noop() {
        let (w, _wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: w });
        // Zero-length writes must return Ok(0) without erroring.
        assert_eq!(shim.write(&[]).unwrap(), 0);
        // Normal writes still work after a zero-length no-op.
        assert!(shim.write_all(b"ok").is_ok());
    }

    #[test]
    fn shutdown_drains_writes_queued_before_drop() {
        // Data queued before the handle is dropped must still reach the consumer
        // (the writer thread drains in FIFO order before exiting). This matters
        // for teardown: output issued just before drop is preserved, not lost.
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        {
            let mut shim = TerminalWriter::new(ChannelWriter { tx: t });
            shim.write_all(b"first ").unwrap();
            shim.write_all(b"second").unwrap();
            // Drop shuts down; remaining queued bytes must drain before exit.
        }
        let mut got = String::new();
        while let Ok(chunk) = wrx.try_recv() {
            got.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert_eq!(got, "first second");
    }

    #[test]
    fn no_data_loss_when_the_consumer_keeps_up() {
        // With a fast consumer that drains faster than we write, no chunk may be
        // dropped and ordering must be exact.
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });
        // Write well under the queue cap so nothing is dropped.
        for i in 0..10 {
            let msg = format!("chunk-{i};");
            assert!(shim.write_all(msg.as_bytes()).is_ok());
        }
        drop(shim);
        let mut got = String::new();
        while let Ok(chunk) = wrx.try_recv() {
            got.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        let expected = (0..10).map(|i| format!("chunk-{i};")).collect::<String>();
        assert_eq!(got, expected);
        // With no drops, resync is not requested.
    }

    #[test]
    fn single_large_chunk_is_not_dropped_on_a_healthy_consumer() {
        // Regression guard: a frame larger than the byte cap must still be
        // enqueued on a *draining* consumer. The old code dropped any chunk where
        // `len > cap`, even when the pty was healthy, which would corrupt a
        // legitimate full-screen redraw and force a spurious resync.
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });
        let big = vec![b'x'; QUEUE_CAPACITY_BYTES + 1]; // larger than the cap
        assert!(shim.write_all(&big).is_ok());
        // Drop waits for the writer to drain everything queued before shutting
        // down, so after this returns `wrx` holds the complete payload.
        drop(shim);
        let delivered: usize = wrx.try_iter().map(|c| c.len()).sum();
        assert_eq!(delivered, QUEUE_CAPACITY_BYTES + 1, "large frame was dropped");
    }

    /// Regression guard for the round-D fix: a writer over a raw `File` (e.g. the
    /// `dup` of fd 1 used by [`TerminalWriter::stdout`]) must not hold the global
    /// `Stdout` lock while blocked in `write(2)`. If it did, a wedged pty would
    /// block *every other* `io::stdout()` caller on the render loop — reintroducing
    /// the freeze we removed.
    ///
    /// We model the wedge with a pipe whose reader is never read: the raw `File`
    /// writer blocks in `write(2)`, but a concurrent `io::stdout()` call must still
    /// complete (they do not share a lock).
    #[cfg(unix)]
    #[test]
    fn raw_file_writer_does_not_block_io_stdout_when_wedged() {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Wrap the blocking pipe write end in a raw File (no user-space lock).
        let file = unsafe { std::fs::File::from_raw_fd(write_fd) };
        let mut shim = TerminalWriter::new(file);

        // Saturate the pipe so further write(2) on the writer thread blocks.
        let big = vec![b'x'; 1024 * 1024];
        for _ in 0..4 {
            assert!(shim.write_all(&big).is_ok());
        }
        // The writer thread is now blocked in write(2) on the raw File.
        // A concurrent io::stdout() write must still succeed (proving it does not
        // contend on the same lock).
        let mut out = std::io::stdout();
        let start = std::time::Instant::now();
        assert!(out.write_all(b"\x1b]0;\x07").is_ok()); // harmless OSC0 (clear title)
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "io::stdout() blocked on a wedged raw-file writer"
        );

        drop(shim);
        unsafe { libc::close(read_fd) };
    }

    /// Concurrent stress: many threads write through the shim simultaneously and
    /// the writer is dropped mid-flight. The shim must never hang, crash, or lose
    /// the ordering guarantee on a healthy consumer.
    #[test]
    fn concurrent_writers_and_shutdown_do_not_deadlock() {
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let shim = Arc::new(Mutex::new(TerminalWriter::new(ChannelWriter { tx: t })));

        let mut handles = Vec::new();
        for tid in 0..8 {
            let shim = Arc::clone(&shim);
            handles.push(thread::spawn(move || {
                for i in 0..200 {
                    let msg = format!("t{tid}-{i};");
                    let mut g = shim.lock().unwrap();
                    assert!(g.write_all(msg.as_bytes()).is_ok());
                }
            }));
        }

        let start = std::time::Instant::now();
        for h in handles {
            h.join().unwrap();
        }
        // All writers finished quickly (no deadlock between writers).
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        drop(shim); // shutdown drains + joins

        // The consumer received every message. Each message ends in ';', so counting
        // ';' tells us exactly how many chunks arrived. 8 threads * 200 writes.
        let semicolons: usize = wrx
            .try_iter()
            .flatten()
            .filter(|&b| b == b';')
            .count();
        assert_eq!(semicolons, 8 * 200, "healthy-path concurrent delivery lost data");
    }

    /// Writes coalesce until `flush`, then drain in order as one unit. This is
    /// the buffered contract: the render loop flushes a frame each tick, turning
    /// many per-cell `write`s into one channel chunk.
    #[test]
    fn writes_coalesce_until_flush_then_deliver_in_order() {
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });

        // Writes without flush are not yet delivered.
        shim.write_all(b"a").unwrap();
        shim.write_all(b"b").unwrap();
        assert!(
            wrx.try_recv().is_err(),
            "writes should not be delivered until flush"
        );

        // A flush delivers all accumulated bytes as a unit.
        shim.flush().unwrap();
        // The writer thread forwards the chunk to `wrx` asynchronously; wait for
        // it with a timeout rather than racing `try_recv`.
        let mut got = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while got.len() < 2 && std::time::Instant::now() < deadline {
            match wrx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(chunk) => got.push_str(std::str::from_utf8(&chunk).unwrap()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("recv failed: {e:?}"),
            }
        }
        assert_eq!(got, "ab", "coalesced bytes lost: {got:?}");
        drop(shim);
    }

    /// Byte-exact stream integrity under coalescing: varying-size writes
    /// interleaved with flushes must deliver an exactly-ordered, lossless byte
    /// stream on a healthy consumer. This guards against reordering or partial
    /// loss from the buffered write+flush path.
    #[test]
    fn mixed_size_writes_with_interleaved_flushes_are_exact_and_ordered() {
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });

        // A deterministic sequence of writes of varying byte lengths, with flush
        // boundaries between groups (mirroring frames of per-cell writes).
        let mut expected: Vec<u8> = Vec::new();
        let mut group = |shim: &mut TerminalWriter, parts: &[&[u8]], flush_after: bool| {
            for p in parts {
                shim.write_all(p).unwrap();
                expected.extend_from_slice(p);
            }
            if flush_after {
                shim.flush().unwrap();
            }
        };

        group(&mut shim, &[b"\x1b[2;1H", b"abcdef"], true);
        group(&mut shim, &[b"\x1b[3;1H", b"x", b"\x1b[4;1H", b"longer-tail-"], false);
        group(&mut shim, &[b"ZZ"], true);
        group(&mut shim, &[b""], false); // empty write, no-op
        group(&mut shim, &[b"\x1b[5;1Hfinal"], true);

        // Drop flushes any remaining pending bytes.
        drop(shim);

        // Collect everything delivered to the consumer.
        let mut got: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while got.len() < expected.len() && std::time::Instant::now() < deadline {
            match wrx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(chunk) => got.extend_from_slice(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("recv failed: {e:?}"),
            }
        }
        assert_eq!(got, expected, "buffered stream corrupted; expected {expected:?}, got {got:?}");
    }

    /// `flush()` with nothing pending (or called repeatedly) must be a safe no-op,
    /// and writing after that still works normally.
    #[test]
    fn empty_and_repeated_flush_are_noops_and_write_still_works() {
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });

        // Empty flush before any writes: no-op, no error.
        shim.flush().unwrap();

        // Write + flush delivers.
        shim.write_all(b"x").unwrap();
        shim.flush().unwrap();

        // A second flush with nothing new pending is also a no-op.
        shim.flush().unwrap();

        // More writes still work.
        shim.write_all(b"y").unwrap();
        shim.flush().unwrap();

        drop(shim);
        let mut got = String::new();
        while let Ok(chunk) = wrx.recv_timeout(std::time::Duration::from_millis(200)) {
            got.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert_eq!(got, "xy", "writes after empty/repeated flush lost: {got:?}");
    }

    /// The production `TerminalWriter::stdout()` (a `dup` of fd 1) must build a
    /// working writer: writes succeed, flushes don't block, and dropping doesn't
    /// hang. This exercises the actual construction path used by
    /// `build_app_terminal`, not just the generic `new()`.
    #[cfg(unix)]
    #[test]
    fn stdout_dup_constructor_writes_and_drops_cleanly() {
        let _guard = live_writer_test_guard();
        let mut shim = TerminalWriter::stdout().expect("stdout() dup failed");
        // Write a harmless, zero-width terminal sequence (a Bell). Writing to the
        // real stdout is safe here and verifies the dup'd fd actually carries bytes.
        assert!(shim.write_all(b"\x07").is_ok());
        assert!(shim.flush().is_ok());
        // Dropping flushes pending + shuts down the writer thread; must not hang.
        let start = std::time::Instant::now();
        drop(shim);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "stdout() writer drop hung"
        );
    }

    /// Auxiliary output (`write_auxiliary`) must be serialized with frame writes
    /// through the *same* writer thread channel, arriving in enqueue order as
    /// atomic chunks. This is the contract that prevents escape sequences written
    /// from the event loop (window title, OSC-52 clipboard, turn notification,
    /// mode re-apply) from interleaving with render frame bytes on the terminal.
    #[test]
    fn auxiliary_writes_are_serialized_in_order_with_frames() {
        let _guard = live_writer_test_guard();
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let mut shim = TerminalWriter::new(ChannelWriter { tx: t });
        shim.register_as_live();

        // A frame chunk: coalesced writes flushed as one atomic unit.
        shim.write_all(b"\x1b[1;1Hcell").unwrap();
        shim.write_all(b"-bytes").unwrap();
        shim.flush().unwrap();

        // Auxiliary bytes enqueued while a writer is registered must land in the
        // same ordered stream, not on a side channel.
        assert_eq!(
            write_auxiliary(b"\x1b]0;title\x07"),
            AuxWriteResult::Accepted,
            "auxiliary write should be accepted while a writer is live"
        );

        // Another frame chunk.
        shim.write_all(b"\x1b[2;1Hnext").unwrap();
        shim.flush().unwrap();

        // Unregister happens on drop; after that, no live writer routes there so
        // auxiliary writes report Fallback (caller should use io::stdout()).
        drop(shim);
        #[cfg(unix)]
        {
            assert_eq!(
                write_auxiliary(b"\x07"),
                AuxWriteResult::Fallback,
                "auxiliary write must fall back when no live writer is registered"
            );
        }

        let mut got = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match wrx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(chunk) => got.push_str(std::str::from_utf8(&chunk).unwrap()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Frame 1 (one chunk), aux title, frame 2 (one chunk) — byte-exact order.
        assert_eq!(
            got, "\x1b[1;1Hcell-bytes\x1b]0;title\x07\x1b[2;1Hnext",
            "frame+aux stream must be serialized and ordered: {got:?}"
        );
    }

    /// Integration boundary: drive the real `Terminal<CrosstermBackend<TerminalWriter>>`
    /// (the production `AppTerminal` type), interleave an auxiliary `write_serialized`
    /// escape between two full draws, and assert the auxiliary sequence reaches the
    /// downstream writer intact — never split by a frame's cell bytes.
    ///
    /// Before the fix, auxiliary output went to `io::stdout()` (fd 1) while the
    /// render writer drained a lock-free `dup` into the same terminal, so an aux
    /// multi-byte escape could be cut by a concurrently-emitted frame's `MoveTo`
    /// / cell sequence, leaving garbage on screen. Now the aux bytes go through
    /// the same single writer thread channel and are always emitted as one
    /// contiguous run between frame chunks.
    #[cfg(unix)]
    #[test]
    fn real_terminal_keeps_interleaved_aux_write_intact() {
        let _guard = live_writer_test_guard();
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let writer = TerminalWriter::new(ChannelWriter { tx: t });
        writer.register_as_live();
        let backend = ratatui::backend::CrosstermBackend::new(writer);
        // This is the production AppTerminal shape: ratatui over the writer.
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        // Draw a first frame.
        terminal
            .draw(|frame| {
                let p = ratatui::widgets::Paragraph::new(ratatui::text::Text::from("alpha line"));
                frame.render_widget(p, frame.area());
            })
            .expect("draw 1");
        terminal.flush().expect("flush 1");

        // Auxiliary write interleaved on the event thread (window title).
        crate::tui::terminal_writer::write_serialized(b"\x1b]0;jcode-title\x07");

        // Draw a second frame.
        terminal
            .draw(|frame| {
                let p = ratatui::widgets::Paragraph::new(ratatui::text::Text::from("beta line"));
                frame.render_widget(p, frame.area());
            })
            .expect("draw 2");
        terminal.flush().expect("flush 2");

        drop(terminal); // drains the writer channel to `t`.

        // Reconstruct the full downstream byte stream.
        let mut got = String::new();
        while let Ok(chunk) = wrx.recv_timeout(std::time::Duration::from_millis(300)) {
            got.push_str(std::str::from_utf8(&chunk).expect("utf8 on test bytes"));
        }

        // ratatui emits a paragraph's cells as separate MoveTo+text runs (not one
        // contiguous line), so assert on the individual cell runs reaching the
        // downstream writer...
        assert!(got.contains("alpha"), "frame 1 cells lost: {got:?}");
        assert!(got.contains("line"), "frame 1 cells lost: {got:?}");
        assert!(got.contains("beta"), "frame 2 cells lost: {got:?}");
        // ...and — the actual regression — the auxiliary OSC-0 window-title escape
        // arrives byte-intact as one contiguous run, never split by a frame's
        // cell/command bytes.
        assert!(
            got.contains("\x1b]0;jcode-title\x07"),
            "auxiliary escape was split or lost: {got:?}"
        );
    }

    /// Honest wedge-drop reporting: once the wedged-pty backlog is saturated, an
    /// auxiliary write must report `Dropped` (not `Accepted`), so callers that
    /// surface success/failure (clipboard copy, turn-notification fallback) do not
    /// report a dropped write as though it reached the terminal.
    #[test]
    fn auxiliary_write_reports_dropped_when_backlog_saturated() {
        let _guard = live_writer_test_guard();
        let (wedge, release) = WedgedWriter::new();
        let mut writer = TerminalWriter::new(wedge);
        writer.register_as_live();

        // Saturate the wedged-pty backlog with frame writes (each flush reserves
        // the bytes in `buffered`; the wedged consumer never drains them).
        for i in 0..(QUEUE_CAPACITY_BYTES / 1024 + 2) {
            let data = vec![b'x'; 1024];
            assert!(writer.write_all(&data).is_ok(), "write {i} failed");
            let _ = writer.flush();
        }

        // The backlog is saturated, so an auxiliary write is dropped, not accepted.
        assert_eq!(
            write_auxiliary(b"\x1b]0;title\x07"),
            AuxWriteResult::Dropped,
            "auxiliary write must report Dropped when the backlog is saturated"
        );

        // Release the wedged writer so the test leaves no leaked thread.
        drop(release);
        drop(writer);
    }

    /// Validation of the core fix guarantee under the exact concurrency model
    /// that caused the bug: many threads call `write_serialized` concurrently
    /// while a live registered writer drains the channel. Every auxiliary escape
    /// must reach the downstream writer intact and complete — never split by
    /// bytes from another concurrent writer (that interleaving is what left
    /// stray characters on the real terminal before routing aux output through
    /// the single writer thread).
    #[test]
    fn concurrent_auxiliary_writes_are_never_split() {
        let _guard = live_writer_test_guard();
        let (t, wrx) = mpsc::channel::<Vec<u8>>();
        let writer = TerminalWriter::new(ChannelWriter { tx: t });
        writer.register_as_live();

        const THREADS: usize = 8;
        const PER_THREAD: usize = 100;
        let mut handles = Vec::new();
        for tid in 0..THREADS {
            handles.push(thread::spawn(move || {
                for i in 0..PER_THREAD {
                    // A distinct, well-delimited OSC-0 style escape per write so
                    // any split (or byte interleaving) is detectable downstream.
                    let seq = format!("\x1b]0;t{tid}-{i}\x07");
                    assert!(
                        write_serialized(seq.as_bytes()),
                        "write_serialized must accept while a live writer is registered"
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        drop(writer); // drains the channel fully to `wrx`.

        // Concatenate the whole stream and assert every escape arrived whole and
        // in a contiguous run. Any interleaving would produce a fragment that is
        // not a complete "\x1b]0;...\x07".
        let mut got = String::new();
        while let Ok(chunk) = wrx.recv_timeout(std::time::Duration::from_millis(300)) {
            got.push_str(std::str::from_utf8(&chunk).expect("payload is ascii"));
        }
        let total_escapes = THREADS * PER_THREAD;
        // Every escape appears verbatim and unbroken.
        for tid in 0..THREADS {
            for i in 0..PER_THREAD {
                let needle = format!("\x1b]0;t{tid}-{i}\x07");
                assert!(
                    got.contains(&needle),
                    "escape {needle:?} was split or lost (stream len {})",
                    got.len()
                );
            }
        }
        // No intra-escape interleaving: the pool contains only valid escapes, so
        // its total byte count must exactly equal the sum of the escapes' lengths
        // (compute them directly since `i` varies in digit count).
        let expected_len: usize = (0..THREADS)
            .flat_map(|tid| (0..PER_THREAD).map(move |i| format!("\x1b]0;t{tid}-{i}\x07")))
            .map(|s| s.len())
            .sum();
        assert_eq!(got.len(), expected_len, "unexpected bytes interleaved in stream");
        // Provenance is exact: the OSC start marker and the BEL terminator each
        // appear exactly once per escape, so the stream is exactly the set of
        // escaped payloads with nothing added, dropped, or split.
        assert_eq!(got.matches("\x1b]0;").count(), total_escapes);
        assert_eq!(got.matches('\x07').count(), total_escapes);
    }
}
