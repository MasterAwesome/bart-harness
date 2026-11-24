use std::{fs, path::Path, sync::Arc};

use serde_json::{Value, json};

use super::*;
use crate::{
    auth::{self, Credential},
    diagnostics::Log,
    history::Message,
    provider::{
        Custom, CustomUpdate, Delta, HttpProtocol, Provider,
        mock::{Reply, serve_catalog, serve_with},
    },
};

const ROTATE: &str = r#"echo run >> "$1/runs"; echo "key-$(wc -l < "$1/runs" | tr -d ' ')""#;

struct Fixture {
    dir: tempfile::TempDir,
    provider: Provider,
    transport: Transport,
    store: Store,
    client: Client,
    headers: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Fixture {
    async fn new(replies: Vec<Reply>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (provider, _, _, headers) = serve_catalog(dir.path(), replies, json!(null)).await;
        let provider = install(dir.path(), &provider).await;
        let store = Store::new(dir.path().join("auth.json"));
        Self {
            dir,
            provider,
            transport: Transport::default(),
            store,
            client: crate::http_client::new(),
            headers,
        }
    }

    async fn save(&self, credential: Credential) {
        self.store
            .update(async |stored| {
                stored
                    .providers
                    .insert(self.provider.name().into(), credential);
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn command(&self, script: &str) {
        let argv = ["sh", "-c", script, "sh", self.dir.path().to_str().unwrap()]
            .map(str::to_owned)
            .to_vec();
        self.save(Credential::Command {
            endpoint: self.provider.endpoint().unwrap().to_string(),
            argv,
        })
        .await;
    }

    fn runs(&self) -> usize {
        fs::read_to_string(self.dir.path().join("runs")).map_or(0, |text| text.lines().count())
    }

    async fn send(&self, attempts: Option<&mut Attempts<'_>>) -> Result<Assistant, Error> {
        self.transport
            .stream(
                &self.client,
                &self.store,
                &request(&self.provider, &[Message::User("hi".into())]),
                |_: Delta<'_>| {},
                attempts,
            )
            .await
    }

    fn authorizations(&self) -> Vec<String> {
        self.headers
            .lock()
            .unwrap()
            .iter()
            .filter_map(|head| {
                head.lines()
                    .find_map(|line| line.strip_prefix("authorization: ").map(str::to_owned))
            })
            .collect()
    }
}

fn request<'a>(provider: &'a Provider, history: &'a [Message]) -> Request<'a> {
    Request {
        provider,
        model: "m",
        history,
        instructions: "",
        tools: Tools::Functions(&[]),
        reasoning_effort: None,
        summaries: false,
        verbosity: None,
        max_output_tokens: None,
        cache: None,
        service_tier: Default::default(),
    }
}

async fn install(dir: &Path, endpoint: &Provider) -> Provider {
    let custom = Custom {
        protocol: HttpProtocol::Chat,
        url: endpoint.endpoint().unwrap().to_string(),
        key: true,
    };
    CustomUpdate::prepare(dir, endpoint.name(), Some(custom))
        .await
        .unwrap()
        .save()
        .unwrap();
    Provider::named(endpoint.name(), dir).unwrap()
}

fn trace_events(dir: &Path, event: &str) -> Vec<Value> {
    fs::read_to_string(dir.join("diagnostics.jsonl"))
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str::<Value>(row).unwrap())
        .filter(|row| row["event"] == event)
        .collect()
}

#[tokio::test]
async fn command_key_replay_finishes_two_linked_traces_and_http_prewarm_runs_no_command() {
    let fixture = Fixture::new(vec![
        Reply {
            authorization: Some("Bearer key-2"),
            ..Reply::text("unexpected first success")
        },
        Reply {
            authorization: Some("Bearer key-2"),
            ..Reply::text("ok")
        },
    ])
    .await;
    fixture.command(ROTATE).await;
    fixture
        .transport
        .prewarm(
            &fixture.client,
            &fixture.store,
            &request(&fixture.provider, &[]),
        )
        .await
        .unwrap();
    assert_eq!(fixture.runs(), 0);

    let dir = fixture.dir.path();
    let log = Arc::new(Log::open(dir).unwrap());
    fixture
        .send(Some(&mut Attempts::new(&log, 1)))
        .await
        .unwrap();
    assert_eq!(fixture.authorizations(), ["bearer key-1", "bearer key-2"]);
    assert_eq!(fixture.runs(), 2);
    let starts = trace_events(dir, "start");
    let [first, second] = starts.as_slice() else {
        panic!("expected two starts: {starts:?}")
    };
    assert_ne!(first["request_id"], second["request_id"]);
    assert_eq!(second["data"]["retry_of"], first["request_id"]);
    assert_eq!(second["data"]["attempt"], 2);
    let finishes = trace_events(dir, "finished");
    let [first_end, second_end] = finishes.as_slice() else {
        panic!("expected two finishes: {finishes:?}")
    };
    assert_eq!(first_end["request_id"], first["request_id"]);
    assert_eq!(first_end["data"]["http_status"], 401);
    assert_eq!(first_end["data"]["outcome"], "http_error");
    assert_eq!(second_end["request_id"], second["request_id"]);
    assert_eq!(second_end["data"]["http_status"], 200);
    assert_eq!(second_end["data"]["outcome"], "completed");
}

#[tokio::test]
async fn replay_fails_closed_after_repointing_logout_or_switching_to_a_static_key() {
    for change in ["repoint", "logout", "static key"] {
        let dir = tempfile::tempdir().unwrap();
        let (received, first_request) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        let mut gate = Some((received, resume));
        let (endpoint, requests) = serve_with(
            dir.path(),
            vec![
                Reply {
                    status: 401,
                    authorization: Some("Bearer key-A"),
                    ..Reply::text("")
                },
                Reply::text("unexpected replay"),
            ],
            move |index| {
                let gate = (index == Some(0)).then(|| gate.take()).flatten();
                async move {
                    if let Some((received, resume)) = gate {
                        received.send(()).unwrap();
                        resume.await.unwrap();
                    }
                }
            },
        )
        .await;
        let original = install(dir.path(), &endpoint).await;
        let store = Store::new(dir.path().join("auth.json"));
        let key_file = dir.path().join("key");
        fs::write(&key_file, "key-A").unwrap();
        let argv = [
            "sh",
            "-c",
            r#"echo run >> "$1/runs"; cat "$1/key""#,
            "sh",
            dir.path().to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec();
        let command = Credential::Command {
            endpoint: original.endpoint().unwrap().to_string(),
            argv: argv.clone(),
        };
        crate::provider::commit_identity(&store, original.name(), Some(command), None)
            .await
            .unwrap();
        let client = crate::http_client::new();
        let send = async |provider: &Provider| {
            let history = [Message::User("hi".into())];
            stream(
                &client,
                &store,
                &request(provider, &history),
                |_: Delta<'_>| {},
                None,
            )
            .await
        };
        let (result, expected_runs) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Box::pin(async {
                tokio::join!(send(&original), async {
                    first_request.await.unwrap();
                    let expected_runs = match change {
                        "repoint" => {
                            let replacement = Fixture::new(vec![Reply {
                                authorization: Some("Bearer key-B"),
                                ..Reply::text("replacement")
                            }])
                            .await;
                            fs::write(&key_file, "key-B").unwrap();
                            let provider = install(dir.path(), &replacement.provider).await;
                            crate::provider::commit_identity(
                                &store,
                                original.name(),
                                Some(Credential::Command {
                                    endpoint: provider.endpoint().unwrap().to_string(),
                                    argv,
                                }),
                                None,
                            )
                            .await
                            .unwrap();
                            assert_ne!(provider.endpoint(), original.endpoint());
                            send(&provider).await.unwrap();
                            assert_eq!(replacement.authorizations(), ["bearer key-b"]);
                            let stale = send(&original).await;
                            assert!(matches!(stale, Err(Error::Credential(_))), "{stale:?}");
                            2
                        }
                        "logout" => {
                            crate::provider::logout(dir.path(), &store, original.name())
                                .await
                                .unwrap();
                            1
                        }
                        _ => {
                            let key = auth::api_key("replacement-key").unwrap();
                            crate::provider::commit_identity(
                                &store,
                                original.name(),
                                Some(Credential::ApiKey { key }),
                                None,
                            )
                            .await
                            .unwrap();
                            1
                        }
                    };
                    release.send(()).unwrap();
                    expected_runs
                })
            }),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                result,
                Err(Error::Http {
                    status: reqwest::StatusCode::UNAUTHORIZED,
                    ..
                })
            ),
            "{change}: {result:?}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1, "{change}");
        assert_eq!(
            fs::read_to_string(dir.path().join("runs"))
                .unwrap()
                .lines()
                .count(),
            expected_runs,
            "{change}"
        );
    }
}

#[tokio::test]
async fn a_saved_command_for_another_endpoint_fails_before_running_or_sending() {
    let original = Fixture::new(vec![Reply::text("unexpected request")]).await;
    let replacement = Fixture::new(vec![Reply::text("unexpected request")]).await;
    let argv = [
        "sh",
        "-c",
        ROTATE,
        "sh",
        original.dir.path().to_str().unwrap(),
    ]
    .map(str::to_owned)
    .to_vec();
    original
        .save(Credential::Command {
            endpoint: replacement.provider.endpoint().unwrap().to_string(),
            argv,
        })
        .await;

    let result = original.send(None).await;
    assert!(matches!(result, Err(Error::Credential(_))), "{result:?}");
    assert_eq!(original.runs(), 0);
    assert!(original.headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn second_401_stops_replay_redacts_the_rejected_key_and_forgets_it() {
    let unauthorized = |key| Reply {
        status: 401,
        authorization: Some(key),
        chunks: vec![json!({"error": {"message": format!("expired {key}")}})],
        ..Reply::text("")
    };
    let fixture = Fixture::new(vec![
        unauthorized("Bearer key-1"),
        unauthorized("Bearer key-2"),
        Reply {
            authorization: Some("Bearer key-3"),
            ..Reply::text("ok")
        },
    ])
    .await;
    fixture.command(ROTATE).await;
    let error = fixture.send(None).await.unwrap_err().to_string();
    assert!(
        error.contains("401") && error.contains("expired Bearer [redacted]"),
        "{error}"
    );
    assert_eq!(fixture.authorizations(), ["bearer key-1", "bearer key-2"]);
    fixture.send(None).await.unwrap();
    assert_eq!(
        fixture.authorizations(),
        ["bearer key-1", "bearer key-2", "bearer key-3"]
    );
}

#[tokio::test]
async fn a_static_key_is_not_replayed() {
    let fixture = Fixture::new(vec![
        Reply {
            status: 401,
            ..Reply::text("")
        },
        Reply::text("ok"),
    ])
    .await;
    fixture
        .save(Credential::ApiKey {
            key: auth::api_key("static-key").unwrap(),
        })
        .await;
    let error = fixture.send(None).await.unwrap_err();
    assert!(matches!(
        error,
        Error::Http {
            status: reqwest::StatusCode::UNAUTHORIZED,
            ..
        }
    ));
    assert_eq!(fixture.authorizations(), ["bearer static-key"]);
}

#[tokio::test]
async fn failed_refresh_does_not_start_a_phantom_trace_or_disclose_stderr() {
    let fixture = Fixture::new(vec![Reply {
        status: 401,
        ..Reply::text("")
    }])
    .await;
    fixture.command(r#"if [ -f "$1/used" ]; then echo private-detail >&2; exit 3; fi; touch "$1/used"; echo key-1"#).await;
    let dir = fixture.dir.path();
    let log = Arc::new(Log::open(dir).unwrap());
    let error = fixture
        .send(Some(&mut Attempts::new(&log, 1)))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Credential(ref reason) if reason.ends_with("exited with status 3")),
        "{error}"
    );
    assert_eq!(trace_events(dir, "start").len(), 1);
    assert_eq!(
        trace_events(dir, "finished")[0]["data"]["outcome"],
        "http_error"
    );
}
