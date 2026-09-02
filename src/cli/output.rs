pub const QUIET_ENV: &str = "JCODE_QUIET";

pub fn set_quiet_enabled(enabled: bool) {
    if enabled {
        crate::env::set_var(QUIET_ENV, "1");
    } else {
        crate::env::remove_var(QUIET_ENV);
    }
}

pub fn quiet_enabled() -> bool {
    std::env::var(QUIET_ENV)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn stderr_info(message: impl AsRef<str>) {
    if !quiet_enabled() {
        // Match `eprintln!` semantics: always write a full, newline-terminated
        // line. Callers pass the message without a trailing newline.
        tolerant_write_line(
            &mut std::io::stderr(),
            &crate::output_style::terminal_text(message.as_ref()),
        );
    }
}

pub fn terminal_title(title: impl AsRef<str>) -> String {
    crate::output_style::terminal_text(title.as_ref()).into_owned()
}

pub fn stderr_blank_line() {
    if !quiet_enabled() {
        tolerant_write(&mut std::io::stderr(), "\n");
    }
}

/// Write `text` as a newline-terminated line, tolerating a dead/broken writer.
///
/// `eprintln!` panics (via `std::io::stdio::_eprint`) when the write fails,
/// which is exactly what happens on a closed terminal (dropped SSH, closed
/// window) where stderr raises EIO or a broken pipe. That panic can cascade
/// into a panic hook that itself writes to the same dead stderr, producing a
/// double-panic abort (SIGABRT).
///
/// Always prefer this over `eprintln!` on teardown, error-reporting, and panic
/// paths. See issues #599 and #129.
pub(crate) fn tolerant_write_line(out: &mut impl std::io::Write, text: &str) {
    let mut line = text.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    tolerant_write(out, &line);
}

/// Write `text` to a writer, tolerating a dead/broken writer.
///
/// This writes `text` verbatim (no added newline). Pair with [`tolerant_write_line`]
/// when a newline-terminated line is intended. A dead stderr must never turn a
/// simple write into a panicking `eprintln!` and a downstream double-panic abort.
pub(crate) fn tolerant_write(out: &mut impl std::io::Write, text: &str) {
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn tolerant_write_ignores_a_broken_writer_without_panicking() {
        struct BrokenWriter;

        impl io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "stderr closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::Other, "flush failed"))
            }
        }

        // Must not panic: a broken stderr on a closed terminal must never turn a
        // simple error report into a double-panic abort.
        tolerant_write(&mut BrokenWriter, "hello\n");
        tolerant_write(&mut BrokenWriter, "\n");
    }

    #[test]
    fn tolerant_write_writes_through_to_a_healthy_writer() {
        let mut buf = Vec::new();
        tolerant_write(&mut buf, "line one\n");
        tolerant_write(&mut buf, "line two\n");
        assert_eq!(buf, b"line one\nline two\n");
    }

    #[test]
    fn tolerant_write_line_adds_a_terminating_newline() {
        let mut buf = Vec::new();
        tolerant_write_line(&mut buf, "hello");
        tolerant_write_line(&mut buf, "world");
        // `eprintln!`-equivalent: each call yields a newline-terminated line.
        assert_eq!(buf, b"hello\nworld\n");
    }

    #[test]
    fn tolerant_write_line_is_idempotent_on_an_existing_newline() {
        let mut buf = Vec::new();
        tolerant_write_line(&mut buf, "already\n");
        assert_eq!(buf, b"already\n");
    }

    #[test]
    fn tolerant_write_line_ignores_a_broken_writer_without_panicking() {
        struct BrokenWriter;
        impl io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "stderr closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        tolerant_write_line(&mut BrokenWriter, "line");
    }
}
