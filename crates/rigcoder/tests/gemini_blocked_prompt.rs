//! A Gemini prompt the provider refuses (`promptFeedback.blockReason`) must
//! end the run with a failure that names the block reason, once: it is the
//! provider's verdict, not a transient fault to retry.
//!
//! The product session runs as it does live — streaming, the real Gemini
//! adapter, automatic provider retries — against a local server that answers
//! every `streamGenerateContent` request with Gemini's documented blocked-
//! prompt chunk and closes the stream.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bevy_app::App;
use rigcoder::{Conversation, Event, ModelChoice, RigcoderPlugin, Transcript};

const BLOCKED_CHUNK: &str = r#"{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT","safetyRatings":[{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","probability":"HIGH"}]},"usageMetadata":{"promptTokenCount":12,"totalTokenCount":12},"modelVersion":"gemini-2.5-flash","responseId":"blocked-1"}"#;

/// A one-thread HTTP/1.1 server: every request gets the blocked chunk as an
/// SSE body and a closed connection. Returns the base URL and the request
/// counter.
fn blocked_prompt_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let counter = counter.clone();
            std::thread::spawn(move || {
                // Read the head and the declared body so the client never
                // sees a reset before it finished sending.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut head_end = None;
                loop {
                    let n = match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if head_end.is_none()
                        && let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        head_end = Some(at + 4);
                    }
                    if let Some(at) = head_end {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= at + length {
                            break;
                        }
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let body = format!("data: {BLOCKED_CHUNK}\r\n\r\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (base, requests)
}

fn drive(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        app.update();
        // Provider backoff is a live-provider courtesy; the assertion is
        // about what is retried, not how long the wait is.
        app.world_mut()
            .resource_mut::<Conversation>()
            .expire_backoff();
        if !app.world().resource::<Conversation>().is_busy() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "the run did not end: {:?}",
        app.world().resource::<Transcript>().events
    );
}

#[test]
fn a_blocked_prompt_fails_once_with_the_block_reason() {
    let (base, requests) = blocked_prompt_server();
    let workspace = tempfile::tempdir().unwrap();
    let http = rig::http_client::ReqwestClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
    )
    .boxed();
    let mut app = App::new();
    app.add_plugins(RigcoderPlugin::live(
        workspace.path().to_owned(),
        ModelChoice::parse("gemini", Some("gemini-2.5-flash".to_owned())).unwrap(),
        8,
    ))
    .insert_resource(rigcoder::model::ModelConnection::new(
        base, "test-key", http,
    ));
    app.update();
    let run = rigcoder::submit(app.world_mut(), "Fix the vulnerability in app.py").unwrap();
    drive(&mut app);

    let events = &app.world().resource::<Transcript>().events;
    eprintln!(
        "provider requests: {}\ntranscript: {events:#?}",
        requests.load(Ordering::SeqCst)
    );
    let failures: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            Event::Failed { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 1, "one ending: {events:?}");
    let reason = failures[0];
    assert!(reason.contains("blocked the prompt"), "{reason}");
    assert!(reason.contains("PROHIBITED_CONTENT"), "{reason}");
    assert!(
        reason.contains("HARM_CATEGORY_DANGEROUS_CONTENT"),
        "{reason}"
    );
    assert!(
        !reason.contains("stream ended before its terminal record"),
        "a refusal is not a truncation: {reason}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Retrying { .. })),
        "a refusal is not retried: {events:?}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "exactly one request reached the provider"
    );

    // The record says the same: one completion, refused by the provider.
    let log = rigcoder::effect_log(app.world());
    assert_eq!(log.records.len(), 1, "{log:?}");
    let record = &log.records[0];
    let Err(report) = &record.outcome else {
        panic!("expected a failed completion: {record:?}");
    };
    assert_eq!(report.kind, rig::error::ErrorKind::Provider, "{report:?}");
    assert!(!report.retryable, "{report:?}");
    assert!(app.world().get::<rig_ecs::agent::Failed>(run).is_some());
}
