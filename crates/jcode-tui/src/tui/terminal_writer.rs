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
//! is not draining and the queue fills beyond the cap, the render thread
//! *drops the newest* chunk instead of blocking or growing memory. This is the
//! natural policy for an interactive TUI: the latest frame is what the user
//! should see, and once the pty drains again the next full frame supersedes
//! whatever was dropped. The render loop stays alive and responsive regardless
//! of the pty state.
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

/// Cap on queued (not-yet-written) output bytes. Once the render thread would
/// push the backlog past this the chunk is dropped rather than blocking. 256
/// KiB is comfortably larger than the kernel pty buffer and small enough that a
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
            if drained {
                if let Some(handle) = self.inner.handle.lock().unwrap().take() {
                    // Only join when the writer actually finished, so we never
                    // block on a wedged pty.
                    let _ = handle.join();
                }
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
        // Byte-bounded, drop-newest policy. Account for the chunk; if it would
        // push us past the cap, undo the accounting and drop it.
        let len = buf.len();
        let prev = self.inner.buffered.fetch_add(len, Ordering::Relaxed);
        if prev + len > QUEUE_CAPACITY_BYTES {
            self.inner.buffered.fetch_sub(len, Ordering::Relaxed);
            // The pty is wedged; remember that we dropped output so the app can
            // force a full re-emit once the pty drains (ratatui's model no
            // longer matches the real screen).
            RESYNC_REQUESTED.store(true, Ordering::Release);
            return Ok(len); // accepted-with-drop so the render loop never blocks
        }
        let chunk: Box<[u8]> = buf.to_vec().into_boxed_slice();
        match tx.send(Chunk::Data(chunk)) {
            Ok(()) => Ok(len),
            Err(_) => {
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

    /// A writer that parks forever on the first write, simulating a pty whose
    /// output buffer is full and never drains. The writer thread parks in
    /// `write` (like a kernel `write(2)` on a full pty). This thread is a
    /// detached daemon and is reaped when the test process exits.
    struct WedgedWriter;

    impl Write for WedgedWriter {
        fn write(&mut self, _b: &[u8]) -> io::Result<usize> {
            std::thread::park();
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn render_thread_never_blocks_when_the_pty_is_wedged() {
        let mut writer = TerminalWriter::new(WedgedWriter);
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
        assert!(
            take_resync_requested(),
            "expected a resync request after output was dropped"
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
        let mut writer = TerminalWriter::new(WedgedWriter);
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
}
