//! The real product provider path honors a host's transport and run budgets.

use bevy_app::App;
use rigcoder::{Conversation, Event, ModelChoice, RigcoderPlugin, RunSettings, Transcript};

#[tokio::test]
async fn all_providers_use_host_transport_tokens_and_stream_mode_without_retries() {
    for provider in ["anthropic", "openai", "gemini"] {
        for stream in [false, true] {
            let server = httpmock::MockServer::start_async().await;
            let expected = server
                .mock_async(|when, then| {
                    let when = when.method("POST");
                    let when = match provider {
                        "gemini" => when
                            .query_param("key", "host-owned-key")
                            .path_includes(if stream {
                                "streamGenerateContent"
                            } else {
                                "generateContent"
                            })
                            .json_body_includes(r#"{"generationConfig":{"maxOutputTokens":512}}"#),
                        "anthropic" => when
                            .header("x-api-key", "host-owned-key")
                            .json_body_includes(r#"{"max_tokens":512}"#),
                        _ => when
                            .header("authorization", "Bearer host-owned-key")
                            .json_body_includes(r#"{"max_output_tokens":512}"#),
                    };
                    if provider != "gemini" {
                        if stream {
                            when.json_body_includes(r#"{"stream":true}"#);
                        } else {
                            // Anthropic omits false; OpenAI may encode false.
                            when.body_excludes(r#""stream":true"#);
                        }
                    }
                    then.status(503).json_body(serde_json::json!({
                        "error": {"type":"overloaded_error", "message":"temporarily unavailable"}
                    }));
                })
                .await;
            let workspace = assert_fs::TempDir::new().unwrap();
            let http = rig::http_client::ReqwestClient::new(
                reqwest::Client::builder().no_proxy().build().unwrap(),
            )
            .boxed();
            let connection =
                rigcoder::model::ModelConnection::new(server.base_url(), "host-owned-key", http);
            assert!(!format!("{connection:?}").contains("host-owned-key"));
            let mut app = App::new();
            app.insert_resource(connection);
            app.insert_resource(RunSettings {
                stream,
                max_tokens: 512,
                provider_retries: 0,
            });
            app.add_plugins(RigcoderPlugin::live(
                workspace.path().to_owned(),
                ModelChoice::parse(provider, Some("test-model".into())).unwrap(),
                8,
            ));
            app.update();
            let run =
                rigcoder::submit(app.world_mut(), "Say hello").expect("product agent installed");
            assert_eq!(
                app.world()
                    .get::<rigcoder::RunConfiguration>(run)
                    .unwrap()
                    .0
                    .provider_retries,
                0
            );
            // A later host default must not expand an in-flight run's budget.
            app.world_mut()
                .resource_mut::<RunSettings>()
                .provider_retries = 3;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while app.world().resource::<Conversation>().is_busy() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{provider}/{stream}: product run did not settle"
                );
                app.update();
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            expected.assert_calls_async(1).await;
            let transcript = app.world().resource::<Transcript>();
            assert!(
                transcript
                    .events
                    .iter()
                    .any(|e| matches!(e, Event::Failed { .. }))
            );
            assert!(
                !transcript
                    .events
                    .iter()
                    .any(|e| matches!(e, Event::Retrying { .. }))
            );
        }
    }
}
