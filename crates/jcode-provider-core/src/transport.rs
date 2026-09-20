//! Shared transport-level error classification for provider runtimes.
//!
//! Every provider (Anthropic, OpenAI, Gemini, Cursor, ...) needs to decide
//! whether a request failure is a transient transport fault worth retrying on
//! a fresh connection, or a real error to surface. Keeping the classifier here
//! ensures all providers recognize the same fault vocabulary.

use anyhow::Result;
use std::time::Duration;

/// Send a request while bounding the wait for response headers.
///
/// Reqwest's `send` future covers connection establishment and the wait for
/// response headers, but streaming body reads happen after it resolves. Provider
/// stream loops apply their own idle timeout to those body reads, so using the
/// same budget here closes the zero-response-bytes gap without imposing an
/// overall deadline on a legitimately long stream.
pub async fn send_with_initial_response_timeout(
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response> {
    match tokio::time::timeout(timeout, request.send()).await {
        Ok(response) => Ok(response?),
        Err(_) => anyhow::bail!(
            "Initial response timeout: no response headers received within {timeout:?}"
        ),
    }
}

/// [`send_with_initial_response_timeout`] that additionally invokes a progress
/// callback while the request is in flight (connect + upload + header wait).
///
/// The plain variant is an opaque `request.send()` that can appear to hang for a
/// long time: on a slow uplink the body upload drains for a while, and then the
/// server can sit without sending response headers for minutes (reasoning models
/// especially). The TUI footer shows nothing but a frozen "sending context"
/// phase with no way to tell an alive-but-slow send from a dead connection.
///
/// This polls the bounded send and, until it resolves, periodically calls the
/// async `on_progress` with a human-readable "still awaiting response, Ns
/// elapsed" message. Providers wire that into a `StatusDetail` stream event so
/// the footer visibly ticks and proves the phase is alive.
pub async fn send_with_initial_response_timeout_with_progress<Fut>(
    request: reqwest::RequestBuilder,
    timeout: Duration,
    heartbeat_secs: Duration,
    mut on_progress: impl FnMut(&str) -> Fut + Send,
) -> Result<reqwest::Response>
where
    Fut: std::future::Future<Output = ()> + Send,
{
    use futures::FutureExt;

    let started = std::time::Instant::now();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let send_task = tokio::spawn(async move {
        let result = tokio::time::timeout(timeout, request.send()).await;
        let _ = done_tx.send(result);
    });
    let mut done = done_rx.fuse();
    // The first tick must land after one full heartbeat period, not immediately.
    // `tokio::time::interval` fires its first tick as soon as it is polled, which
    // would emit a misleading "awaiting response headers (0s)" when the request
    // has barely started. Anchoring the first tick at `now + heartbeat_secs`
    // means the first progress word reports a real elapsed time (~heartbeat),
    // and subsequent words advance by the same period.
    let started_tick = std::time::Instant::now() + heartbeat_secs;
    let mut ticker = tokio::time::interval_at(started_tick.into(), heartbeat_secs);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            result = &mut done => {
                let _ = send_task.await;
                return match result {
                    Ok(Ok(response)) => Ok(response?),
                    Ok(Err(_)) => anyhow::bail!(
                        "Initial response timeout: no response headers received within {timeout:?}"
                    ),
                    Err(_) => anyhow::bail!(
                        "Request sender dropped unexpectedly while waiting for response headers"
                    ),
                };
            }
            _ = ticker.tick() => {
                let elapsed = started.elapsed().as_secs();
                on_progress(&format!("awaiting response headers ({elapsed}s)")).await;
            }
        }
    }
}

/// Format a byte count compactly (e.g. `1.2 KB`, `3.4 MB`).
pub fn readable_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes >= MB as u64 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes >= KB as u64 {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Whether an error message describes a transient transport-level fault
/// (connection reset, DNS hiccup, TLS teardown, HTTP/2 stream error, ...)
/// that is likely to succeed on retry with a fresh connection.
pub fn is_transient_transport_error(error_str: &str) -> bool {
    let lower = error_str.to_ascii_lowercase();
    lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("connection refused")
        || lower.contains("connection aborted")
        || lower.contains("broken pipe")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("operation timed out")
        || lower.contains("error decoding")
        || lower.contains("error reading")
        || lower.contains("unexpected eof")
        // rustls reports an abrupt TCP close mid-stream as "peer closed
        // connection without sending TLS close_notify" (its docs URL spells
        // it "unexpected-eof", which the space-separated marker above does
        // not match).
        || lower.contains("close_notify")
        || lower.contains("peer closed connection")
        || lower.contains("tls handshake eof")
        // reqwest/hyper wrap connect-phase failures as "client error
        // (Connect)" and connection-level faults as "connection error: ...".
        || lower.contains("client error (connect)")
        || lower.contains("connection error")
        // hyper h1: connection closed while a response was still incomplete.
        || lower.contains("incomplete message")
        || lower.contains("request or response body error")
        || lower.contains("badrecordmac")
        || lower.contains("bad_record_mac")
        || lower.contains("fatal alert: badrecordmac")
        || lower.contains("fatal alert: bad_record_mac")
        || lower.contains("received fatal alert: badrecordmac")
        || lower.contains("received fatal alert: bad_record_mac")
        || lower.contains("decryption failed or bad record mac")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("failed to lookup address information")
        || lower.contains("dns error")
        || lower.contains("name or service not known")
        || lower.contains("no route to host")
        || lower.contains("network is unreachable")
        || lower.contains("host is unreachable")
        // HTTP/2 transport faults on reused/multiplexed connections. These are
        // transient: a fresh connection on retry typically succeeds. Seen as
        // "http2 error: stream error received: unspecific protocol error detected"
        // or RST_STREAM / GOAWAY frames from the server or an intermediary.
        || lower.contains("http2 error")
        || lower.contains("stream error")
        || lower.contains("stream_read_error")
        || lower.contains("protocol error")
        || lower.contains("refused_stream")
        || lower.contains("refused stream")
        || lower.contains("enhance_your_calm")
        || lower.contains("goaway")
        || lower.contains("go away")
        || lower.contains("sendrequest")
}

#[cfg(test)]
mod tests {
    use super::{
        is_transient_transport_error, send_with_initial_response_timeout,
        send_with_initial_response_timeout_with_progress,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn accepted_request_without_response_headers_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind blackhole server");
        let address = listener.local_addr().expect("read blackhole address");
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set request read timeout");
            let mut request = [0_u8; 4096];
            let bytes_read = stream.read(&mut request).expect("read request");
            assert!(bytes_read > 0, "client should send request bytes");
            request_seen_tx.send(()).expect("report request received");

            // Keep the socket open without sending status or response headers.
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });

        let request = reqwest::Client::new()
            .post(format!("http://{address}/chat/completions"))
            .body("{}");
        let timeout = Duration::from_millis(250);
        let started = Instant::now();
        let error = send_with_initial_response_timeout(request, timeout)
            .await
            .expect_err("blackholed response should time out");

        assert!(
            error.to_string().contains("Initial response timeout"),
            "unexpected error: {error:#}"
        );
        assert!(
            is_transient_transport_error(&error.to_string()),
            "initial response timeouts must enter provider retry machinery"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "initial response wait exceeded its timeout"
        );
        request_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server should receive the request before the timeout");

        let _ = release_tx.send(());
        server.join().expect("blackhole server should exit");
    }

    #[test]
    fn http2_stream_protocol_error_is_transient() {
        // Exact shape reqwest/h2 surfaces for a reset on a reused HTTP/2 connection.
        let msg = "error sending request for url (https://api.anthropic.com/v1/messages): \
                   client error (SendRequest): http2 error: stream error received: \
                   unspecific protocol error detected";
        assert!(is_transient_transport_error(msg));
    }

    #[test]
    fn http2_goaway_and_refused_stream_are_transient() {
        assert!(is_transient_transport_error("http2 error: GOAWAY received"));
        assert!(is_transient_transport_error("stream error: REFUSED_STREAM"));
    }

    #[test]
    fn auth_errors_are_not_transient() {
        assert!(!is_transient_transport_error("401 unauthorized"));
        assert!(!is_transient_transport_error("invalid x-api-key"));
    }

    /// Real transport-error shapes harvested from ~/.jcode/logs.
    #[test]
    fn real_world_transport_errors_are_transient() {
        let real_errors = [
            "client error (Connect): dns error: failed to lookup address information: \
             Name or service not known",
            "client error (SendRequest): http2 error: keep-alive timed out: operation timed out",
            "client error (SendRequest): connection error: peer closed connection without \
             sending TLS close_notify: https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof",
            "client error (Connect): operation timed out",
            "client error (SendRequest): connection error: timed out",
            "client error (Connect): tls handshake eof",
            "error decoding response body: request or response body error: operation timed out",
        ];
        for error in real_errors {
            assert!(
                is_transient_transport_error(error),
                "should be transient: {error}"
            );
        }
    }

    /// OpenAI-compatible stream failure with structured upstream_error payload.
    /// Regression test for issue #885: stream_read_error should be retried.
    #[test]
    fn openai_compatible_stream_read_error_is_transient() {
        // Formatted output from extract_error_with_retry when receiving:
        // { "type": "error", "error": { "type": "upstream_error", "code": "stream_read_error" } }
        assert!(is_transient_transport_error(
            "upstream_error: stream_read_error"
        ));
        // Also test the identifier in isolation (case-insensitive)
        assert!(is_transient_transport_error("stream_read_error"));
    }

    #[test]
    fn readable_bytes_formats_compact_units() {
        assert_eq!(super::readable_bytes(512), "512 B");
        assert_eq!(super::readable_bytes(2048), "2.0 KB");
        assert_eq!(super::readable_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[tokio::test]
    async fn progress_heartbeat_fires_before_slow_request_resolves() {
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow server");
        let address = listener.local_addr().expect("read slow server address");
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read request");
            request_seen_tx.send(()).expect("report request received");
            // Hold the connection open without sending headers so the heartbeat
            // has a chance to fire, then respond and let the client resolve.
            release_rx.recv_timeout(Duration::from_secs(2)).ok();
            let body = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
            let _ = stream.write_all(body.as_bytes()).expect("write response");
        });

        let beacons = Arc::new(Mutex::new(Vec::new()));
        let beacons_clone = Arc::clone(&beacons);
        let request = reqwest::Client::new()
            .post(format!("http://{address}/chat/completions"))
            .body("{}");

        let send_task = tokio::spawn(send_with_initial_response_timeout_with_progress(
            request,
            Duration::from_secs(5),
            Duration::from_millis(100),
            move |msg: &str| {
                beacons_clone
                    .lock()
                    .unwrap()
                    .push(msg.to_string());
                async move {}
            },
        ));

        // Wait until at least one heartbeat message has been emitted before
        // releasing the server, to prove the callback fires while in flight.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !beacons.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "heartbeat did not fire before request resolved"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        request_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server should receive request");
        let _ = release_tx.send(());
        send_task.await.expect("send task to complete").expect("response ok");
        server.join().expect("server thread to exit");

        let beacons = beacons.lock().unwrap();
        assert!(
            !beacons.is_empty(),
            "expected at least one heartbeat message"
        );
        assert!(
            beacons[0].contains("awaiting response headers"),
            "unexpected heartbeat: {:?}",
            beacons[0]
        );
    }
}
