//! End-to-end coverage for live tool output streaming.
//!
//! Drives a real server + client over a unix socket with a scripted bash tool
//! call and asserts the client observes `ServerEvent::ToolOutput` chunks plus a
//! terminal (done) sentinel. This exercises the full path that unit tests stub:
//! bash publisher -> bus -> client_lifecycle forwarding -> wire event.

use crate::test_support::*;

#[tokio::test]
async fn running_bash_tool_streams_live_output_to_client() -> Result<()> {
    let _env = setup_test_env()?;
    let runtime_dir = short_runtime_dir(format!(
        "jcode-live-output-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir)?;
    let socket_path = runtime_dir.join("jcode.sock");
    let debug_socket_path = runtime_dir.join("jcode-debug.sock");

    let provider = MockProvider::new();
    // Round 1: the model calls bash, which prints a marker then stays alive long
    // enough for at least one throttled live chunk to be published.
    provider.queue_response(vec![
        StreamEvent::ToolUseStart {
            id: "call-live-1".to_string().into(),
            name: "bash".to_string(),
        },
        StreamEvent::ToolInputDelta(
            serde_json::json!({"command": "printf 'live-marker-out\\n'; sleep 0.4"})
                .to_string(),
        ),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".to_string()),
        },
    ]);
    // Round 2: the model wraps up after seeing the tool result.
    provider.queue_response(vec![
        StreamEvent::TextDelta("done".to_string()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".to_string()),
        },
    ]);

    let provider: Arc<dyn Provider> = Arc::new(provider);
    let server_instance =
        server::Server::new_with_paths(provider, socket_path.clone(), debug_socket_path.clone());
    let server_handle = tokio::spawn(async move { server_instance.run().await });

    let result = async {
        wait_for_server_ready(&socket_path, &debug_socket_path).await?;
        let mut client = server::Client::connect_with_path(socket_path.clone()).await?;
        let subscribe_id = client.subscribe().await?;
        let _ = collect_until_done_unix(&mut client, subscribe_id).await?;

        let message_id = client
            .send_message("run the command")
            .await?;

        let mut saw_text_chunk = false;
        let mut saw_done_sentinel = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            // A 2s lull between events is not a failure (the server may be
            // between chunks); keep waiting until the overall deadline. Only a
            // real read error aborts.
            let event = match timeout(Duration::from_secs(2), client.read_event()).await {
                Ok(Ok(event)) => event,
                Ok(Err(error)) => return Err(error),
                Err(_elapsed) => continue,
            };
            let is_terminal = matches!(
                &event,
                ServerEvent::Done { id } if *id == message_id
            ) || matches!(&event, ServerEvent::Error { id, .. } if *id == message_id);
            if let ServerEvent::ToolOutput {
                name, text, done, ..
            } = &event
                && *name == "bash"
            {
                if *done {
                    saw_done_sentinel = true;
                } else if text.contains("live-marker-out") {
                    saw_text_chunk = true;
                }
            }
            if is_terminal {
                break;
            }
        }

        assert!(
            saw_text_chunk,
            "client should observe the running command's live output"
        );
        assert!(
            saw_done_sentinel,
            "client should observe the terminal sentinel that clears the live view"
        );
        Ok(())
    }
    .await;

    abort_server_and_cleanup(&server_handle, &socket_path, &debug_socket_path);
    result
}

/// A command whose only progress output uses carriage returns (like cargo or
/// curl) must reach the client live, while the command is still running, and
/// each update must be marked as an in-place overwrite.
#[tokio::test]
async fn running_bash_carriage_return_progress_streams_live_output() -> Result<()> {
    let _env = setup_test_env()?;
    let runtime_dir = short_runtime_dir(format!(
        "jcode-live-cr-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&runtime_dir)?;
    let socket_path = runtime_dir.join("jcode.sock");
    let debug_socket_path = runtime_dir.join("jcode-debug.sock");

    let provider = MockProvider::new();
    provider.queue_response(vec![
        StreamEvent::ToolUseStart {
            id: "call-cr-1".to_string().into(),
            name: "bash".to_string(),
        },
        // Progress uses `\r` only, then the command stays alive so the tail must
        // surface from a periodic flush rather than at exit.
        StreamEvent::ToolInputDelta(
            serde_json::json!({
                "command": "printf 'cr-live-1\\rcr-live-2\\r'; sleep 0.6"
            })
            .to_string(),
        ),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".to_string()),
        },
    ]);
    provider.queue_response(vec![
        StreamEvent::TextDelta("done".to_string()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".to_string()),
        },
    ]);

    let provider: Arc<dyn Provider> = Arc::new(provider);
    let server_instance =
        server::Server::new_with_paths(provider, socket_path.clone(), debug_socket_path.clone());
    let server_handle = tokio::spawn(async move { server_instance.run().await });

    let result = async {
        wait_for_server_ready(&socket_path, &debug_socket_path).await?;
        let mut client = server::Client::connect_with_path(socket_path.clone()).await?;
        let subscribe_id = client.subscribe().await?;
        let _ = collect_until_done_unix(&mut client, subscribe_id).await?;

        let message_id = client.send_message("run the cr command").await?;

        let mut saw_cr_live = false;
        let mut saw_overwrite = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            // A 2s lull between events is not a failure; keep waiting until the
            // overall deadline. Only a real read error aborts.
            let event = match timeout(Duration::from_secs(2), client.read_event()).await {
                Ok(Ok(event)) => event,
                Ok(Err(error)) => return Err(error),
                Err(_elapsed) => continue,
            };
            let is_terminal = matches!(
                &event,
                ServerEvent::Done { id } if *id == message_id
            ) || matches!(&event, ServerEvent::Error { id, .. } if *id == message_id);
            if let ServerEvent::ToolOutput {
                name,
                text,
                replace,
                done,
                ..
            } = &event
                && *name == "bash"
                && !*done
                && text.contains("cr-live")
            {
                saw_cr_live = true;
                if *replace {
                    saw_overwrite = true;
                }
            }
            if is_terminal {
                break;
            }
        }

        assert!(
            saw_cr_live,
            "carriage-return progress must reach the client while the command runs"
        );
        assert!(
            saw_overwrite,
            "the second carriage-return update must be marked as an in-place overwrite"
        );
        Ok(())
    }
    .await;

    abort_server_and_cleanup(&server_handle, &socket_path, &debug_socket_path);
    result
}
