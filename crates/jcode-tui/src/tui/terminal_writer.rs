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
        Self { tx: Some(tx), inner }
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
        Ok(Self::new(file))
    }

    fn shutdown(&mut self) {
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
        let Some(tx) = self.tx.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer shut down"));
        };
        let len = buf.len();

        // Reserve `len` bytes in the byte-bounded backlog, dropping only when the
        // *current* backlog is already saturated (the consumer is not draining).
        //
        // A single large chunk (e.g. a full-screen redraw diff) must NOT be
        // dropped on a healthy consumer just because `len > cap`; we allow it to
        // be enqueued and rely on the consumer to drain it. We only stop
        // enqueueing once the outstanding (queued-but-not-drained) backlog has
        // already reached `QUEUE_CAPACITY_BYTES`. This bounds sustained memory
        // when the pty is wedged while never dropping a legitimate fresh frame on
        // a draining pty.
        //
        // We use `compare_exchange` so the reservation is atomic: `buffered` is
        // mutated when the writer drains (`fetch_sub`) and by concurrent `write`s.
        let mut prev = self.inner.buffered.load(Ordering::Relaxed);
        loop {
            if prev >= QUEUE_CAPACITY_BYTES {
                // Backlog already saturated (wedged). Drop this chunk and flag a
                // resync; ratatui's model no longer matches the real screen.
                RESYNC_REQUESTED.store(true, Ordering::Release);
                return Ok(len); // accepted-with-drop so the render loop never blocks
            }
            // `checked_add` guards against a pathological `len` (or cumulative
            // backlog) that would overflow `usize` and wrap the reservation to a
            // tiny value, which would otherwise defeat the cap entirely.
            let Some(next) = prev.checked_add(len) else {
                RESYNC_REQUESTED.store(true, Ordering::Release);
                return Ok(len);
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

        let chunk: Box<[u8]> = buf.to_vec().into_boxed_slice();
        match tx.send(Chunk::Data(chunk)) {
            Ok(()) => Ok(len),
            Err(_) => {
                // Writer exited; undo the reservation.
                self.inner.buffered.fetch_sub(len, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer thread exited"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // No-op on the render thread: bytes are buffered in the channel and the
        // writer thread flushes the real pty. Returning Ok keeps the ratatui
        // backend happy without blocking.
        Ok(())
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
        }
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
        let mut writer = TerminalWriter::new(wedge);
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
            .flat_map(|c| c)
            .filter(|&b| b == b';')
            .count();
        assert_eq!(semicolons, 8 * 200, "healthy-path concurrent delivery lost data");
    }
}
