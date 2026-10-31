use crate::history::JobOutcome;
use serde_json::{Value, json};

use super::*;
use crate::approval::Mode;
use crate::{
    history::{InlineImage, Part, ToolResult},
    tool::test_support::TempDir,
};

fn assistant(provider: &str, parts: Vec<crate::history::Part>) -> Message {
    Message::Assistant(Assistant {
        protocol: crate::provider::Protocol::Chat,
        provider: provider.into(),
        model: "m".into(),
        parts,
        usage: None,
        continues: false,
        refused: false,
        served_model: None,
    })
}

fn tool_result(id: &str, output: &str, jobs: Vec<u32>) -> Message {
    Message::ToolResult(ToolResult {
        activities: Default::default(),
        diffs: Vec::new(),
        notes: String::new(),
        call_id: id.into(),
        output: output.to_owned().into(),
        is_error: false,
        jobs,
        saved: None,
        cell: None,
    })
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn session(dir: &std::path::Path) -> Session {
    session_on(Provider::named("chatgpt", dir).unwrap(), dir)
}

async fn turn(session: &mut Session, input: &str) {
    session
        .turn(input.to_owned().into(), |_: Event<'_>| {})
        .await
        .unwrap();
}

fn session_on(provider: Provider, dir: &std::path::Path) -> Session {
    session_in(provider, dir, dir)
}

fn session_in(provider: Provider, project: &std::path::Path, dir: &std::path::Path) -> Session {
    let context = crate::tool::test_support::context(project, Confinement::Unrestricted, 400_000);
    let settings = Settings {
        provider,
        model: "m".into(),
        reasoning_effort: None,
    };
    let store = Store::new(dir.join("auth.json"));
    let mut session = Session::new(
        Client::new(),
        store,
        settings,
        Current::default(),
        context,
        "k".into(),
        None,
    );
    session.routing.assume(Some(model(1_000_000)));
    session
}

async fn python(session: &Session, code: &str) -> Output {
    let mut live = session.context.live.start("python", code, None);
    let output = tool::run(
        &call("python", "python", json!({"code":code})),
        &session.context,
        &live,
        None,
    )
    .await;
    live.finish(output.status);
    output
}

fn known_window(provider: &str) -> Current {
    let mut current = Current::default();
    crate::preferences::Change::ContextWindow {
        provider: provider.into(),
        model: "m".into(),
        tokens: Some(1_000_000.try_into().unwrap()),
    }
    .apply(&mut current.effective)
    .unwrap();
    current
}

fn model(window: u64) -> crate::provider::Model {
    crate::provider::Model {
        visibility: crate::provider::Visibility::Listed,
        id: "m".into(),
        name: None,
        efforts: Vec::new(),
        default_effort: None,
        summaries: None,
        verbosity: None,
        images: None,
        context_window: Some(window),
        max_context_window: None,
        max_output_tokens: Some(32_000),
    }
}

#[tokio::test]
async fn reviewer_requests_are_isolated_from_session_bytes_and_compaction() {
    use crate::provider::mock::{Reply, serve_catalog};
    let dir = TempDir::new("approval-agent-isolation");
    let mut captured = Vec::new();
    for mode in [Mode::None, Mode::Auto] {
        let (provider, _, raw, _) = serve_catalog(
            dir.path(),
            vec![
                Reply::python("a", &json!({"code": "write('sentinel', 'ran')"})),
                Reply::text("Finished."),
            ],
            json!({"data": []}),
        )
        .await;
        let mut session = session_on(provider, dir.path());
        session.interactive();
        let review_dir = dir.path().join("review");
        std::fs::create_dir_all(&review_dir).unwrap();
        let (_, requests, _, _) = serve_catalog(&review_dir, vec![Reply::text(r#"{"constraint_conflict":false,"risk_level":"low","user_authorization":"high","outcome":"allow","rationale":"routine"}"#)], json!({"models": [{"slug":"codex-auto-review", "visibility":"hide"}]})).await;
        let reviewer = Provider::named("mock", &review_dir).unwrap();
        crate::provider::CustomUpdate::prepare(
            dir.path(),
            "reviewer",
            Some(crate::provider::Custom {
                protocol: reviewer.protocol(),
                url: reviewer.endpoint().unwrap().to_string(),
                key: false,
            }),
        )
        .await
        .unwrap()
        .save()
        .unwrap();
        if mode == Mode::Auto {
            session
                .routing
                .preferences_mut()
                .effective
                .permissions
                .approval
                .auto(crate::choice::Choice {
                    provider: "reviewer".into(),
                    model: "codex-auto-review".into(),
                    effort: Some("low".into()),
                });
        }
        session
            .turn("Do the task.".to_owned().into(), |event: Event<'_>| {
                assert!(!matches!(event, Event::Approval(_)), "nothing asks");
            })
            .await
            .unwrap();
        assert!(dir.path().join("sentinel").exists());
        std::fs::remove_file(dir.path().join("sentinel")).unwrap();
        captured.push(raw.lock().unwrap().clone());
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), usize::from(mode == Mode::Auto));
        if mode == Mode::Auto {
            assert_eq!(requests[0]["model"], "codex-auto-review");
            assert!(requests[0]["tools"].as_array().is_none_or(Vec::is_empty));
            assert!(
                !serde_json::to_string(&session.journal().model_context().messages())
                    .unwrap()
                    .contains("routine")
            );
            session.compact(|_: Event<'_>| {}).await.unwrap();
            assert!(
                !serde_json::to_string(&session.journal().model_context().messages())
                    .unwrap()
                    .contains("routine")
            );
        }
    }
    assert_eq!(
        captured[0], captured[1],
        "all session request bytes stay identical"
    );
}

#[tokio::test]
async fn headless_auto_tells_the_model_about_denials_and_stops_on_failed_reviews() {
    use crate::provider::mock::{Reply, serve_catalog};
    let dir = TempDir::new("approval-agent-headless");
    for (text, stops) in [
        (
            r#"{"constraint_conflict":false,"risk_level":"high","user_authorization":"low","outcome":"deny","rationale":"broad effects"}"#,
            false,
        ),
        ("malformed", true),
    ] {
        let (provider, _) = crate::provider::mock::serve(
            dir.path(),
            vec![
                Reply::python(
                    "a",
                    &json!({"code":"write('sentinel', 'ran')", "restart":true}),
                ),
                Reply::text("Understood."),
            ],
        )
        .await;
        let mut session = session_on(provider, dir.path());
        let (_, reviewed, _, _) = serve_catalog(
            dir.path(),
            vec![Reply::text(text)],
            json!({"data":[{"id":"codex-auto-review"}]}),
        )
        .await;
        session
            .routing
            .preferences_mut()
            .effective
            .permissions
            .approval
            .auto(crate::choice::Choice {
                provider: "mock".into(),
                model: "codex-auto-review".into(),
                effort: Some("low".into()),
            });
        let result = session
            .turn("go".to_owned().into(), |_: Event<'_>| {})
            .await;
        assert_eq!(matches!(result, Err(Error::NotApproved)), stops);
        assert!(stops || result.is_ok());
        assert!(!dir.path().join("sentinel").exists());
        assert_eq!(reviewed.lock().unwrap().len(), 1);
        assert!(session.journal().history().iter().any(|m| matches!(m,
                Message::ToolResult(r) if r.is_error && r.output.contains("reviewer"))));
    }
}

#[tokio::test]
async fn manual_approval_runs_approved_cells_and_a_denial_ends_the_turn() {
    use crate::provider::mock::Reply;
    let dir = TempDir::new("approval-agent-manual");
    let write = |id: &str| {
        json!({"index": usize::from(id == "c"), "id": id, "type": "function", "function": {
            "name": "python", "arguments": json!({"code": format!("write('{id}', 'ran')")}).to_string(),
        }})
    };
    let mut batch = Reply::python("b", &json!({}));
    batch.chunks[0] =
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [write("b"), write("c")]}}]});
    let (provider, requests) = crate::provider::mock::serve(
        dir.path(),
        vec![
            Reply::python("a", &json!({"code": "write('a', 'ran')"})),
            batch,
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.interactive();
    session
        .routing
        .preferences_mut()
        .effective
        .permissions
        .approval
        .set_mode(Mode::Manual)
        .unwrap();
    let mut answers = [true, false].into_iter();
    let result = session
        .turn("go".to_owned().into(), |event: Event<'_>| {
            if let Event::Approval(answer) = event {
                assert!(answer.prompt().contains("write('"));
                answer.send(answers.next().unwrap());
            }
        })
        .await;
    assert!(matches!(result, Err(Error::NotApproved)));
    assert_eq!(answers.next(), None, "the later cell is not asked about");
    assert!(dir.path().join("a").exists());
    assert!(!dir.path().join("b").exists());
    assert!(!dir.path().join("c").exists());
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "the model is not asked again"
    );
}

#[tokio::test]
async fn a_response_cut_off_after_visible_text_is_retried_whole() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-retry-streamed");
    let (provider, requests) = serve(
        dir.path(),
        vec![Reply::cut_off("half an ans"), Reply::text("whole answer")],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    let mut discarded = Vec::new();
    session
        .turn("go".to_owned().into(), |event: Event<'_>| {
            if let Event::Retrying {
                discarded: partial, ..
            } = event
            {
                discarded.push(partial);
            }
        })
        .await
        .unwrap();
    assert_eq!(discarded, [true]);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests[0], requests[1],
        "the same request, without the partial text"
    );
    let history = format!("{:?}", session.journal().history());
    assert!(
        history.contains("whole answer") && !history.contains("half an ans"),
        "{history}"
    );
}

fn user_messages(request: &serde_json::Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn steers_join_at_the_next_request_even_after_the_last_one() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-steer");
    let replies = vec![
        Reply::python("a", &json!({ "code": "42" })),
        Reply::text("done"),
        Reply::text("also done"),
    ];
    let (provider, requests) = serve(dir.path(), replies).await;
    let mut session = session_on(provider, dir.path());
    let inbox = session.inbox();
    let mut steered = Vec::new();
    let mut late = false;
    session
        .turn(String::from("go").into(), |event: Event<'_>| match event {
            Event::ToolStarted { .. } => {
                inbox
                    .push(Input {
                        text: "mid-command".into(),
                        pasted_bytes: 0,
                        skills: Vec::new(),
                        images: vec![
                            InlineImage::new(
                                crate::history::test_support::PNG.to_vec(),
                                None,
                                "steered image".into(),
                            )
                            .unwrap(),
                        ],
                    })
                    .unwrap();
            }
            Event::Delta(Delta::Text(text)) if text.contains("done") && !late => {
                late = true;
                inbox
                    .push(String::from("during the answer").into())
                    .unwrap();
            }
            Event::Steered(input) => steered.push(input.text.clone()),
            _ => {}
        })
        .await
        .unwrap();
    assert_eq!(steered, ["mid-command", "during the answer"]);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "a late steer still gets a response");
    assert_eq!(user_messages(&requests[1]), ["go", "mid-command"]);
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["image_url"]["url"]
                    == format!(
                        "data:image/png;base64,{}",
                        crate::history::test_support::PNG_BASE64
                    ))))
    );
    assert_eq!(
        user_messages(&requests[2]),
        ["go", "mid-command", "during the answer"]
    );
}

#[tokio::test]
async fn a_rewind_drops_a_message_and_what_followed() {
    let dir = TempDir::new("agent-rewind");
    let mut session = session(dir.path());
    let image = Message::Image(crate::history::Image {
        media_type: "image/png".into(),
        path: "/unused".into(),
        byte_len: 0,
        label: "[image 1]".into(),
        number: 1,
    });
    session.journal = Journal::of(vec![
        Message::User("first".to_owned().into()),
        Message::Notification(Notification {
            text: "n".to_owned().into(),
            kind: NotificationKind::Recovery,
        }),
        Message::User("see [image 1]".to_owned().into()),
        image,
        Message::Notification(Notification {
            text: "later".to_owned().into(),
            kind: NotificationKind::Recovery,
        }),
    ]);
    assert_eq!(
        session
            .journal()
            .user_messages()
            .iter()
            .map(|(at, _)| *at)
            .collect::<Vec<_>>(),
        [0, 2]
    );
    assert_eq!(
        session.rewind(2, |_: Event<'_>| {}).await,
        Ok("see [image 1]".into())
    );
    assert_eq!(
        session.journal().history().len(),
        3,
        "the image went with its message; a host rewind notice follows"
    );
    assert!(
        session.rewind(1, |_: Event<'_>| {}).await.is_err(),
        "only the user's messages"
    );
}

#[tokio::test]
async fn user_images_follow_their_text_in_history_and_model_requests() {
    use crate::{
        history::InlineImage,
        provider::mock::{Reply, serve},
    };
    let dir = TempDir::new("agent-image");
    let (provider, requests) = serve(dir.path(), vec![Reply::text("a cat")]).await;
    let mut session = session_on(provider, dir.path());
    let image = InlineImage::new(
        crate::history::test_support::PNG.to_vec(),
        None,
        String::new(),
    )
    .unwrap();
    let input = Input {
        pasted_bytes: 0,
        skills: Vec::new(),
        text: "what is this?".into(),
        images: vec![image],
    };
    session.turn(input, |_: Event<'_>| {}).await.unwrap();
    {
        let requests = requests.lock().unwrap();
        let messages = requests[0]["messages"].as_array().unwrap();
        let last_two = &messages[messages.len() - 2..];
        assert_eq!(
            last_two[1]["content"][0]["image_url"]["url"],
            format!(
                "data:image/png;base64,{}",
                crate::history::test_support::PNG_BASE64
            )
        );
        assert_eq!(
            last_two[0],
            json!({ "role": "user", "content": "what is this?" })
        );
    }
    let Message::Image(image) = &session.journal().history()[1] else {
        panic!()
    };
    let path = image.path.clone();
    std::fs::remove_file(&path).unwrap();
    let error = session
        .respond(&mut |_: Event<'_>| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(path.to_str().unwrap()), "{error}");
}

#[tokio::test]
async fn resuming_reports_jobs_that_died_with_the_previous_process() {
    let dir = TempDir::new("agent-resume");
    let provider = Provider::named("chatgpt", dir.path()).unwrap();
    let mut session = session_on(provider, dir.path());
    let result = |id: &str, jobs: &[u32]| tool_result(id, "[job started]", jobs.to_vec());
    let finished = Message::Notification(Notification {
        text: "[background job 4 finished: make]".to_owned().into(),
        kind: NotificationKind::JobFinished {
            id: 4,
            outcome: JobOutcome::Exit(0),
            command: "make".into(),
            capture: crate::history::SavedArtifact {
                path: "job.log".into(),
                completeness: crate::history::Completeness::Complete,
                chars: 0,
            },
        },
    });
    let history = vec![
        Message::User("go".to_owned().into()),
        result("a", &[3]),
        Message::Notification(Notification {
            text: "[background job 3 matched READY]\nREADY".to_owned().into(),
            kind: NotificationKind::JobMatched { id: 3 },
        }),
        Message::Notification(Notification {
            text: "root exited; descendants retained".into(),
            kind: NotificationKind::JobCommandExited {
                id: 3,
                outcome: JobOutcome::Exit(0),
                command: "build".into(),
            },
        }),
        result("b", &[4]),
        finished,
        result("cancel", &[5]),
        Message::Notification(Notification {
            text: "[background job 5 finished: watcher]\n[cancelled; effects unknown]"
                .to_owned()
                .into(),
            kind: NotificationKind::JobFinished {
                id: 5,
                outcome: JobOutcome::Killed,
                command: "watcher".into(),
                capture: crate::history::SavedArtifact {
                    path: "job.log".into(),
                    completeness: crate::history::Completeness::Complete,
                    chars: 0,
                },
            },
        }),
        result("c", &[]),
    ];
    session
        .resume(
            None,
            None,
            crate::conversation::Conversation::of(history.clone()),
        )
        .await;
    let notified: Vec<_> = session
        .journal()
        .resume_notifications()
        .map(|note| note.kind.clone())
        .collect();
    assert_eq!(
        notified,
        [
            NotificationKind::Recovery,
            NotificationKind::JobLost { id: 3 }
        ]
    );
    assert_eq!(
        &session.journal().history()[..history.len()],
        history.as_slice(),
        "the record is kept as it was"
    );
    assert_eq!(session.journal().history().len(), history.len() + 2);
    assert_eq!(session.journal().resume_notifications().count(), 2);
    let retained = session.journal().history().to_vec();
    session
        .resume(None, None, crate::conversation::Conversation::of(retained))
        .await;
    assert!(
        session
            .journal()
            .resume_notifications()
            .all(|note| note.terminal_job().is_none()),
        "a terminal job was reported again"
    );
    assert_eq!(session.journal().resume_notifications().count(), 1);
}

#[tokio::test]
async fn orphaned_calls_get_results_right_after_their_message() {
    let assistant = assistant(
        "fixture",
        ["a", "b"]
            .map(|id| Part {
                content: Content::ToolCall(call(id, "bash", json!({}))),
                replay: None,
            })
            .into(),
    );
    let result = |id: &str, output: &str| tool_result(id, output, Vec::new());
    let mut history = vec![
        Message::User("go".to_owned().into()),
        assistant.clone(),
        result("a", "done"),
        Message::User("next".to_owned().into()),
    ];
    let dir = TempDir::new("agent-orphan-resume");
    let mut session = session(dir.path());
    for reused in [false, true] {
        if reused {
            history.push(assistant.clone());
        }
        session
            .resume(
                None,
                None,
                crate::conversation::Conversation::of(history.clone()),
            )
            .await;
        let mut results = Vec::new();
        for message in session.journal().history() {
            match message {
                Message::Assistant(_) => results.clear(),
                Message::ToolResult(result) => {
                    if result.call_id == "b" {
                        assert!(result.is_error);
                    }
                    results.push(result.call_id.as_str());
                }
                Message::User(text) if text.as_ref() == "next" => {
                    assert_eq!(results, ["a", "b"]);
                }
                _ => {}
            }
        }
        assert_eq!(results, ["a", "b"]);
        let repaired = session.journal().history().to_vec();
        session
            .resume(
                None,
                None,
                crate::conversation::Conversation::of(repaired.clone()),
            )
            .await;
        let repaired_results = repaired
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count();
        assert_eq!(
            session
                .journal()
                .history()
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count(),
            repaired_results
        );
    }
}
#[tokio::test]
async fn completed_results_survive_cancellation_of_a_later_cell() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-batch-cancel");
    let calls = [
        ("first", "write('done', 'yes')"),
        (
            "second",
            "import time; write('ready', 'yes'); time.sleep(30)",
        ),
    ];
    let fragments: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, code))| {
            json!({
                "index": index, "id": id, "type": "function", "function": {
                    "name": "python", "arguments": json!({ "code": code }).to_string(),
                },
            })
        })
        .collect();
    let (provider, _) = serve(dir.path(), vec![Reply {
            status: 200, authorization: None,
            delay: Duration::ZERO, ends: true,
            chunks: vec![
                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": fragments } }] }),
                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            ],
        },Reply::text("recovered")]).await;
    let sessions = crate::record::Sessions::new(&dir.path().join("records"), dir.path());
    let mut session = session_on(provider.clone(), dir.path());
    let (id, record) = sessions
        .create(&session.settings().meta(dir.path()), "")
        .unwrap();
    session.journal = Journal::new(Some(record));
    {
        let turn = session.turn("do both".to_owned().into(), |_: Event<'_>| {});
        tokio::pin!(turn);
        tokio::select! {
            result=&mut turn=>panic!("turn ended before cancellation: {result:?}"),
            ()=async {tokio::time::timeout(Duration::from_secs(5),async {while !dir.path().join("ready").exists(){tokio::time::sleep(Duration::from_millis(5)).await;}}).await.unwrap();}=>{}
        }
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("done")).unwrap(),
        "yes\n"
    );
    assert!(session.finish().await.is_none());
    drop(session);
    let resumed = sessions.resume(&id).unwrap();
    let mut session = session_on(provider, dir.path());
    session.journal = Journal::new(Some(resumed.record));
    session.resume(None, None, resumed.conversation).await;
    turn(&mut session, "recover").await;
    let results = session
        .journal()
        .history()
        .iter()
        .filter_map(|message| {
            if let Message::ToolResult(result) = message {
                Some(result)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| result.call_id == "first" && !result.is_error)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| result.call_id == "second" && result.is_error)
            .count(),
        1
    );
}

#[tokio::test]
async fn ending_a_session_stops_jobs_python_and_mcp_even_with_a_shared_context() {
    use crate::provider::mock::{Reply, serve};
    use crate::tool::test_support::wait_until_dead;
    let dir = TempDir::new("agent-job-lifecycle");
    let (provider, _) = serve(dir.path(), vec![
            Reply::python("a", &json!({ "code": "import os\nprint(os.getpid())\nbg_run([\"bash\", \"-c\", 'echo $$ > child; printf READY; sleep 300'])" })),
            Reply::text("started"),
        ]).await;
    let mut session = session_on(provider, dir.path());
    let context = Arc::clone(&session.context);
    let config = crate::mcp::Config::parse(
        &json!({"mcpServers":{"fx":{
            "command":"python3", "args":["-c", include_str!("../mcp/fixtures/server.py")]
        }}})
        .to_string(),
    )
    .unwrap();
    let mcp = crate::mcp::Registry::start(
        &config,
        crate::tool::test_support::place(dir.path(), crate::tool::Confinement::Unrestricted),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    context
        .reconfine(Confinement::Unrestricted, Arc::clone(&mcp))
        .await
        .1
        .unwrap();
    let grandchild = mcp
        .call("fx", "spawn", serde_json::Map::new())
        .await
        .unwrap()
        .value;
    turn(&mut session, "start").await;
    let python = session
        .journal()
        .history()
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) => result.output.lines().next().map(str::to_owned),
            _ => None,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let id = context.live.jobs()[0].id;
            if context.live.snapshot(id).unwrap().output.ends_with("READY") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let child = std::fs::read_to_string(dir.path().join("child")).unwrap();
    assert!(session.finish().await.is_none());
    drop(session);
    wait_until_dead(python.trim()).await;
    wait_until_dead(child.trim()).await;
    assert!(!context.jobs.any());
    wait_until_dead(grandchild.as_str().unwrap()).await;
    assert!(
        context
            .mcp()
            .call("fx", "echo", serde_json::Map::new())
            .await
            .is_err()
    );
    context.stop();
    assert!(mcp.is_empty());
}

#[tokio::test]
async fn compaction_summarizes_once_and_resumes_its_window() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("compact-state");
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::python("seed", &json!({"code": "keep = 41\nprint('x' * 80000)"})),
            Reply::text("Working."),
            Reply::text("Next step."),
            Reply::text("Preserve the constraint; keep is 41 and continuation is pending."),
            Reply::python("continue", &json!({"code": "print(keep + 1)"})),
            Reply::text("42"),
            Reply::text("Retried."),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    let sessions = crate::record::Sessions::new(&dir.path().join("sessions"), dir.path());
    let (id, record) = sessions
        .create(&session.settings().meta(dir.path()), "")
        .unwrap();
    session.journal = Journal::new(Some(record));
    for request in ["Count; preserve my constraint.", "Next."] {
        turn(&mut session, request).await;
    }
    session.routing.assume(Some(model(64_000)));
    let before = session.journal().history().to_vec();
    let sent = requests.lock().unwrap().len();
    let before_accounting = session.accounting();
    let mut observed = None;
    let mut report = String::new();
    let mut summary_bytes = 0;
    session
        .compact(|event: Event<'_>| match event {
            Event::Compacted { report: text, .. } => report = text.to_owned(),
            Event::CompactingBytes(bytes) => summary_bytes += bytes,
            Event::Accounting(accounting) => observed = Some(accounting),
            _ => {}
        })
        .await
        .unwrap();
    assert!(report.contains("Compacted context:"), "{report}");
    assert!(
        summary_bytes > 0,
        "summary output should report streaming progress"
    );
    assert_eq!(observed, Some(session.accounting()));
    assert!(session.accounting().context_tokens < before_accounting.context_tokens);
    assert_eq!(session.accounting().usage, before_accounting.usage);
    session.compact(|_: Event<'_>| {}).await.unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        sent + 1,
        "one summary request; compacting the small checkpoint is a noop"
    );
    assert_eq!(
        session.journal().history()[..before.len()],
        before,
        "original transcript entries are unchanged"
    );
    turn(&mut session, "Continue.").await;
    assert!(session.journal().history().iter().any(|message| matches!(message, Message::ToolResult(result) if result.call_id == "continue" && &*result.output == "42")), "Python state survives");
    let next = requests.lock().unwrap()[sent + 1].to_string();
    assert!(
        !next.contains(&"x".repeat(1000)),
        "the next request uses the summarized window"
    );
    assert!(next.contains("Bart conversation summary"));
    assert!(
        next.contains("Bart conversation summary"),
        "the replacement is explained"
    );
    assert_eq!(
        session
            .rewind(before.len(), |_: Event<'_>| {})
            .await
            .unwrap(),
        "Continue."
    );
    assert!(session.accounting().context_tokens < compact::threshold(64_000));
    turn(&mut session, "Try instead.").await;
    {
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            sent + 4,
            "rewind must not cause another summary request"
        );
        let retried = requests.last().unwrap().to_string();
        assert!(retried.contains("Bart conversation summary"));
        assert!(!retried.contains(&"x".repeat(1000)));
        assert!(
            !retried.contains("print(keep + 1)"),
            "removed tool call must not be replayed"
        );
    }
    let history = session.journal().history().to_vec();
    let window = session.journal().model_context().messages().to_vec();
    let accounting = session.accounting();
    let instructions = session.instructions.as_str().to_owned();
    assert!(session.finish().await.is_none());
    drop(session);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history(), history);
    assert_eq!(resumed.conversation.context().messages(), window);
    assert_eq!(resumed.conversation.usage(), accounting.usage);
    assert_eq!(
        compact::tokens(resumed.conversation.context(), &instructions),
        accounting.context_tokens
    );
}

#[tokio::test]
async fn lowering_window_compacts_evidence_but_retains_unfit_original_requirements() {
    use crate::{
        preferences::ContextWindow,
        provider::mock::{Reply, serve_catalog},
    };
    let large = "x".repeat(400_000);
    let outputs = || {
        (1..=8)
            .map(|n| {
                Reply::python(
                    &format!("big{n}"),
                    &json!({"code": format!("print('{n}' * 30000)")}),
                )
            })
            .chain([Reply::text("done")])
            .collect::<Vec<_>>()
    };
    for (seed, replies, current) in [
        ("seed", outputs(), "continue"),
        (large.as_str(), vec![Reply::text("done")], "continue"),
        ("seed", outputs(), large.as_str()),
    ] {
        let dir = TempDir::new("lower-window");
        let seeded = replies.len();
        let (provider, requests, _, _) = serve_catalog(
            dir.path(),
            replies
                .into_iter()
                .chain((0..30).map(|_| Reply::text("Continue under the latest requirements.")))
                .collect(),
            json!({"data": [{"id":"m", "context_window":272_000, "max_context_window":272_000}]}),
        )
        .await;
        let mut session = session_on(provider, dir.path());
        turn(&mut session, seed).await;
        assert!(
            compact::tokens(
                session.journal().model_context(),
                session.instructions.as_str(),
            ) > 32_000,
            "the lowered window's threshold is exceeded"
        );
        session
            .prefer(
                &Change::ContextWindow {
                    provider: session.settings().provider.name().to_owned(),
                    model: "m".into(),
                    tokens: Some(ContextWindow::try_from(64_000).unwrap()),
                },
                |_: Event<'_>| {},
            )
            .await
            .unwrap();
        assert_eq!(requests.lock().unwrap().len(), seeded);
        let mut compacted = false;
        let result = session
            .turn(current.to_owned().into(), |event: Event<'_>| {
                if matches!(event, Event::Compacted { .. }) {
                    compacted = true;
                }
            })
            .await;
        if seed == large || current == large {
            assert!(matches!(result, Err(Error::Context(_))), "{result:?}");
            assert!(!compacted);
            assert_eq!(requests.lock().unwrap().len(), seeded);
            assert!(
                session
                    .journal
                    .model_context()
                    .messages()
                    .iter()
                    .any(|m| matches!(m,Message::User(text) if text.as_ref()==large))
            );
            continue;
        }
        result.unwrap();
        assert!(compacted);
        let requests = requests.lock().unwrap();
        assert!(requests.len() >= seeded + 2, "summary plus continuation");
        for request in &requests[seeded..requests.len() - 1] {
            assert_eq!(
                request["tool_choice"], "none",
                "summary cannot execute tools"
            );
            assert_eq!(request["tools"], requests[seeded - 1]["tools"]);
        }
        let continued = requests.last().unwrap();
        assert!(continued.get("tools").is_some());
        assert!(continued.to_string().contains("Bart conversation summary"));
        assert!(!continued.to_string().contains(&"1".repeat(5000)));
        assert!(!continued.to_string().contains(&"x".repeat(5000)));
    }
}

#[tokio::test]
async fn mid_turn_summary_preserves_the_task_and_live_python_state() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("auto-compact");
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::python("old", &json!({"code": "print('x' * 30000)"})),
            Reply::text("Seeded."),
            Reply::python("step", &json!({"code": "saved = 7\nprint('y' * 40000)"})),
            Reply::text("Finish this task. saved=7; step completed."),
            Reply::text("Done."),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    turn(
        &mut session,
        &format!("Seed. {}", "background ".repeat(10400)),
    )
    .await;
    session.routing.assume(Some(model(64_000)));
    let mut compacted = 0;
    session
        .turn("Finish this task.".to_owned().into(), |event: Event<'_>| {
            if matches!(event, Event::Compacted { .. }) {
                compacted += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(compacted, 1, "compaction fires once, mid-turn");
    {
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            5,
            "one native-prefix summary and one continuation"
        );
        assert_eq!(requests[3]["tool_choice"], "none");
        assert_eq!(requests[3]["tools"], requests[2]["tools"]);
        let messages = requests[4]["messages"].as_array().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == "step")
        );
        assert!(
            requests[4].to_string().contains(&"y".repeat(3000)),
            "latest complete exchange stays verbatim"
        );
        let sent = requests[4].to_string();
        assert!(sent.contains("Finish this task."));
        assert!(!sent.contains(&"x".repeat(1000)) && sent.contains("Bart conversation summary"));
        assert!(
            sent.contains("Bart conversation summary"),
            "the replacement is explained"
        );
    }
    assert!(matches!(
        session.journal().history().last(),
        Some(Message::Assistant(_))
    ));
    let check = call("check", "python", json!({"code":"saved"}));
    let live = session.context.live.start("python", "saved", None);
    let output = crate::tool::run(&check, &session.context, &live, None).await;
    assert!(output.text.trim().ends_with('7'));
}

#[tokio::test]
async fn main_requests_keep_affinity_and_prefixes_across_turns_resume_and_compaction() {
    use crate::provider::mock::{Reply, completed, serve_chatgpt_catalog};
    let dir = TempDir::new("main-cache-wire");
    let text = json!({"type":"message", "id":"msg", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"done"}]});
    let call = json!({"type":"function_call", "id":"fc", "call_id":"seed", "name":"python", "arguments":json!({"code":"print('x' * 80000)"}).to_string(), "status":"completed"});
    let reply = |item: &serde_json::Value| Reply {
        status: 200,
        authorization: None,
        delay: Duration::ZERO,
        ends: true,
        chunks: vec![
            json!({"type":"response.output_item.done", "sequence_number":1, "output_index":0, "item":item}),
            completed("m", std::slice::from_ref(item)),
        ],
    };
    let (provider, requests, _, headers) = serve_chatgpt_catalog(
        dir.path(),
        vec![
            reply(&call),
            reply(&text),
            reply(&text),
            reply(&text),
            reply(&text),
            reply(&text),
        ],
        json!({"models":[{"slug":"m"}]}),
    )
    .await;
    let mut session = session_on(provider.clone(), dir.path());
    let credential = serde_json::from_value(json!({"type":"chatgpt", "access":"local-test-access", "refresh":"local-test-refresh", "expires_at":i64::MAX, "account_id":"local-test-account", "residency":null, "installation_id":"5b0f3c1e-7a2d-4e8b-9c61-2f4d8a7e9b13"})).unwrap();
    session
        .routing
        .store
        .update(async |credentials| {
            credentials.providers.insert("chatgpt".into(), credential);
            Ok(())
        })
        .await
        .unwrap();
    let records = crate::record::Sessions::new(&dir.path().join("records"), dir.path());
    let (id, record) = records
        .create(&session.settings().meta(dir.path()), "")
        .unwrap();
    session.cache_key = format!("bart-{id}");
    session.journal = Journal::new(Some(record));
    turn(&mut session, "first").await;
    turn(&mut session, "second").await;
    assert!(session.finish().await.is_none());
    drop(session);
    let resumed = records.resume(&id).unwrap();
    let mut session = session_on(provider, dir.path());
    session.cache_key = format!("bart-{id}");
    session.journal = Journal::new(Some(resumed.record));
    session.resume(None, None, resumed.conversation).await;
    turn(&mut session, "third").await;
    session.routing.assume(Some(model(64_000)));
    session
        .compact_with(&mut |_: Event<'_>| {}, 64_000)
        .await
        .unwrap();
    turn(&mut session, "fourth").await;
    assert!(session.finish().await.is_none());
    let requests = requests.lock().unwrap();
    let key = format!("bart-{id}");
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request["prompt_cache_key"], key);
        if index == 4 {
            let system = request["instructions"].as_str().unwrap();
            assert!(system.starts_with(requests[0]["instructions"].as_str().unwrap()));
            assert!(system.contains("checkpoint output contract takes precedence"));
        } else {
            assert_eq!(request["instructions"], requests[0]["instructions"]);
        }
        assert_eq!(request["tools"], requests[0]["tools"]);
    }
    for pair in requests[..4].windows(2) {
        let earlier = pair[0]["input"].as_array().unwrap();
        assert!(pair[1]["input"].as_array().unwrap().starts_with(earlier));
    }
    assert_eq!(requests[4]["tool_choice"], "none");
    assert_eq!(requests[4]["reasoning"], requests[3]["reasoning"]);
    assert!(
        requests[4]["input"]
            .as_array()
            .unwrap()
            .starts_with(requests[3]["input"].as_array().unwrap())
    );
    assert!(!requests[5].to_string().contains(&"x".repeat(1000)));
    for head in headers
        .lock()
        .unwrap()
        .iter()
        .filter(|head| head.starts_with("post "))
    {
        assert!(
            head.lines()
                .any(|line| line == format!("session-id: {key}"))
        );
    }
}

#[tokio::test]
async fn completed_tool_events_match_the_results_delivered_to_the_model() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("tool-event-delivery");
    let code = json!({"code":"print('x' * 80000)"});
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::python("first", &code),
            Reply::python("again", &code),
            Reply::text("done"),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.context = Arc::new(crate::tool::test_support::context(
        dir.path(),
        Confinement::Unrestricted,
        2_000,
    ));
    let mut displayed = Vec::new();
    session
        .turn("run twice".to_owned().into(), |event: Event<'_>| {
            if let Event::ToolFinished { call, output, .. } = event {
                displayed.push((call.id.clone(), output.text.clone()));
            }
        })
        .await
        .unwrap();
    let delivered: Vec<_> = session
        .journal()
        .history()
        .iter()
        .filter_map(|message| {
            if let Message::ToolResult(result) = message {
                Some((result.call_id.clone(), result.output.to_string()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(displayed, delivered);
    assert!(
        delivered[1].1.len() < 5000,
        "the fixture must exercise delivery shaping"
    );
    let sent = requests.lock().unwrap();
    assert!(
        sent[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "tool"
                && item["tool_call_id"] == "again"
                && item["content"].as_str() == Some(delivered[1].1.as_str()))
    );
}

#[tokio::test]
async fn a_failed_record_stops_recording_once_and_the_session_carries_on() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-record-fault");
    let (provider, _) = serve(
        dir.path(),
        vec![Reply::text("first"), Reply::text("second")],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    let path = dir.path().join("record");
    std::fs::write(&path, "").unwrap();
    session.journal = Journal::new(Some(Recorder::read_only(&path)));
    let mut stopped = 0;
    for input in ["one", "two"] {
        session
            .turn(input.to_owned().into(), |event: Event<'_>| {
                stopped += usize::from(matches!(event, Event::RecordingStopped(_)));
            })
            .await
            .unwrap();
    }
    assert_eq!(stopped, 1, "the user hears once that recording stopped");
    assert!(session.journal().recording_warning().is_some());
    assert_eq!(session.journal().history().len(), 4);
    session
        .rename(Some("named"), |_: Event<'_>| {})
        .await
        .unwrap();
    assert_eq!(session.journal().name(), Some("named"));
    session.rewind(2, |_: Event<'_>| {}).await.unwrap();
    assert_eq!(session.journal().history().len(), 3);
    assert!(session.journal().recording_warning().is_some());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"",
        "nothing more is written"
    );
}

#[tokio::test]
async fn invalid_provider_data_fails_the_turn_without_a_retry() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-invalid-data");
    let mut invalid = Reply::text("");
    invalid.chunks = vec![json!({"unexpected": true})];
    let (provider, requests) = serve(dir.path(), vec![invalid, Reply::text("retried")]).await;
    let mut session = session_on(provider, dir.path());
    let error = session
        .turn("hello".to_owned().into(), |_: Event<'_>| {})
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Model(provider::Error::Invalid(_))),
        "{error}"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_resumed_session_keeps_the_prompt_it_was_created_with() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("pinned-project");
    let agents = dir.path().join("AGENTS.md");
    std::fs::write(&agents, "Indent with tabs.").unwrap();
    let (provider, requests) =
        serve(dir.path(), vec![Reply::text("one"), Reply::text("two")]).await;
    let settings = || Settings {
        provider: provider.clone(),
        model: "m".into(),
        reasoning_effort: None,
    };
    let opener = Opener {
        executable: crate::tool::test_support::executable(),
        sessions: crate::record::Sessions::new(&dir.path().join("records"), dir.path()),
        cwd: dir.path().into(),
        client: Client::new(),
        store: Store::new(dir.path().join("auth.json")),
    };
    let (mut session, id) = opener
        .open(settings(), known_window(provider.name()), None)
        .await
        .unwrap();
    turn(&mut session, "first").await;
    assert!(session.finish().await.is_none());
    drop(session);
    std::fs::write(&agents, "Indent with spaces.").unwrap();
    let resumed = opener.sessions.resume(&id).unwrap();
    let (mut session, _) = opener
        .open(settings(), known_window(provider.name()), Some(resumed))
        .await
        .unwrap();
    turn(&mut session, "second").await;
    assert!(session.finish().await.is_none());
    let requests = requests.lock().unwrap();
    let system = |request: &serde_json::Value| request["messages"][0]["content"].clone();
    assert!(
        system(&requests[0])
            .as_str()
            .unwrap()
            .contains("Indent with tabs.")
    );
    assert_eq!(system(&requests[1]), system(&requests[0]));
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
}

#[tokio::test]
async fn an_unknown_window_refuses_the_turn_before_any_request_or_history() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("unknown-window");
    let (provider, requests) = serve(dir.path(), vec![Reply::text("unused")]).await;
    let mut session = session_on(provider, dir.path());
    let mut unknown = model(1);
    unknown.context_window = None;
    session.routing.assume(Some(unknown));
    let error = session
        .turn("hello".to_owned().into(), |_: Event<'_>| {})
        .await
        .unwrap_err();
    assert!(matches!(error, Error::UnknownWindow(_)), "{error}");
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(session.journal().history(), []);
}

#[tokio::test]
async fn an_overflow_compacts_and_retries_once_then_stops() {
    use crate::provider::mock::{Reply, serve};
    let overflow = || Reply {
        status: 400,
        authorization: None,
        delay: Duration::ZERO,
        chunks: vec![json!({"error": {"code": "context_length_exceeded", "message": "too long"}})],
        ends: true,
    };
    for fits in [true, false] {
        let dir = TempDir::new("overflow-recovery");
        let (provider, requests) = serve(
            dir.path(),
            vec![
                Reply::python(
                    "seed",
                    &json!({"code": "print('\\n'.join(f'line {i} ' + 'abcdefghij' * 4 for i in range(1200)))"}),
                ),
                Reply::text("Seen."),
                overflow(),
                Reply::text("Look completed; Again is pending."),
                if fits { Reply::text("Fits now.") } else { overflow() },
            ],
        )
        .await;
        let mut session = session_on(provider, dir.path());
        turn(&mut session, "Look.").await;
        let current = compact::tokens(
            session.journal().model_context(),
            session.instructions.as_str(),
        );
        session
            .routing
            .assume(Some(model((current * 4 / 3 + 400).max(64_000))));
        let sent = requests.lock().unwrap().len();
        let result = session
            .turn("Again.".to_owned().into(), |_: Event<'_>| {})
            .await;
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            sent + 3,
            "one isolated summary and one retry, never more"
        );
        assert_eq!(requests[sent + 1]["tool_choice"], "none");
        assert_eq!(requests[sent + 1]["tools"], requests[sent]["tools"]);
        let retried = requests[sent + 2].to_string();
        assert!(
            retried.contains("Bart conversation summary"),
            "the retry uses a checkpoint"
        );
        assert!(!retried.contains("line 200 abcdefghij"));
        if fits {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(Error::Overflow { .. })), "{result:?}");
        }
    }
}

#[tokio::test]
async fn mixed_mcp_images_reach_the_next_provider_request_with_the_shared_cell_limit() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("agent-mcp-images");
    std::fs::write(
        dir.path().join("pixel.png"),
        crate::history::test_support::PNG,
    )
    .unwrap();
    let code = "display('pixel.png')\nfor _ in range(8): mcp('fx', 'mixed')\nmcp('fx', 'broken')";
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::python("mixed", &json!({"code":code})),
            Reply::text("done"),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    let config = crate::mcp::Config::parse(
        &json!({"mcpServers":{"fx":{
            "command":"python3", "args":["-c",include_str!("../mcp/fixtures/server.py")]
        }}})
        .to_string(),
    )
    .unwrap();
    let registry = crate::mcp::Registry::start(
        &config,
        session.context.place(),
        &Client::new(),
        &Store::new(dir.path().join("auth.json")),
    );
    session
        .context
        .reconfine(Confinement::Unrestricted, registry)
        .await
        .1
        .unwrap();
    turn(&mut session, "show the results").await;
    let requests = requests.lock().unwrap();
    let sent = &requests[1]["messages"];
    let images = sent
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "image_url")
        .count();
    assert_eq!(images, 8, "MCP and display share one admission limit");
    let result = sent
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    for kept in [
        "rows are capped",
        "report.csv",
        "audio/wav",
        "file:///raw",
        "at most 8 images",
        "render failed",
    ] {
        assert!(result.contains(kept), "{kept}: {result}");
    }
    assert!(
        !requests[0]["tools"]
            .to_string()
            .contains("Echoes its arguments"),
        "catalogs stay out of the prompt"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "needs a systemd user session and bubblewrap"]
async fn entering_the_jail_keeps_the_configured_mcp_servers() {
    let dir = TempDir::new("jail-keeps-mcp");
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        crate::mcp::path(&config),
        json!({ "mcpServers": { "fx": { "command": "python3", "args": ["-c", include_str!("../mcp/fixtures/server.py")] } } })
            .to_string(),
    )
    .unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mut session = session_in(
        Provider::named("chatgpt", &config).unwrap(),
        &project,
        &config,
    );
    session
        .prefer(&Change::Sandbox(true), |_: Event<'_>| {})
        .await
        .unwrap();
    assert!(session.context.confinement().jailed());
    assert_eq!(session.context.mcp().names(), ["fx"]);
}

#[tokio::test]
async fn model_switch_keeps_the_prefix_and_python_state_but_identity_is_live() {
    let dir = TempDir::new("identity-switch");
    let mut session = session(dir.path());
    let prefix = session.instructions.as_str().to_owned();
    let first = python(&session, "retained = 73; old = whoami(); assert old['model'] == 'm' and old['reasoning_effort'] is None").await;
    assert!(!first.is_error(), "{}", first.text);
    let mut settings = session.settings().clone();
    settings.model = "another-model".into();
    settings.provider = crate::provider::Provider::custom(
        "another-route",
        &crate::provider::Custom {
            protocol: settings.provider.protocol(),
            url: settings.provider.endpoint().unwrap().to_string(),
            key: false,
        },
    )
    .unwrap();
    settings.reasoning_effort = Some("medium".into());
    session.switch(settings, |_: Event<'_>| {}).await;
    assert_eq!(session.instructions.as_str(), prefix);
    let next = python(&session, "current = whoami(); assert retained == 73; assert old['model'] == 'm'; assert current['model'] == 'another-model'; assert current['provider'] == 'another-route'; assert current['reasoning_effort'] == 'medium'; assert set(current) == {'provider','model','reasoning_effort'}").await;
    assert!(!next.is_error(), "{}", next.text);
}

#[tokio::test]
async fn oversized_user_requirements_are_not_destroyed_to_fit_a_small_window() {
    let dir = TempDir::new("compaction-prose");
    let mut session = session(dir.path());
    session
        .journal
        .append(vec![
            Message::User("required middle detail ".repeat(3000).into()),
            Message::User("Continue".into()),
        ])
        .await;
    session.routing.assume(Some(model(8000)));
    let before = session.journal.model_context().messages().to_vec();
    assert!(session.compact(|_: Event<'_>| {}).await.is_err());
    assert_eq!(session.journal.model_context().messages(), before);
}

#[tokio::test]
async fn failed_prose_preservation_leaves_the_original_context_usable() {
    let dir = TempDir::new("compact-save-failure");
    let mut session = session(dir.path());
    session.routing.assume(Some(model(64_000)));
    session
        .journal
        .append(vec![
            Message::User("Keep all requirements".into()),
            assistant(
                "chatgpt",
                vec![Part {
                    content: Content::Text("historical observation ".repeat(4000).into()),
                    replay: None,
                }],
            ),
            Message::User("continue".into()),
        ])
        .await;
    let before = serde_json::to_value(session.journal().model_context().messages()).unwrap();
    std::fs::remove_dir_all(session.context.artifacts().dir()).unwrap();
    assert!(session.compact(|_: Event<'_>| {}).await.is_err());
    assert_eq!(
        serde_json::to_value(session.journal().model_context().messages()).unwrap(),
        before
    );
}

#[test]
fn canceled_admissions_return_reserved_input_but_never_requeue_staged_input_or_jobs() {
    use crate::tool::test_support::blocked_worker;
    use futures_util::FutureExt as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = TempDir::new("input-journal-cancel");
        let mut session = session(dir.path());
        let context = session.tool_context();
        let call = call(
            "background",
            "python",
            json!({"code":"j = bg_run([\"bash\", \"-c\", 'sleep infinity']); j.kill()"}),
        );
        let live = context.live.start("background", "", None);
        let output = tool::run(&call, &context, &live, None).await;
        assert!(!output.is_error(), "{}", output.text);
        let job = output.jobs[0];
        context.jobs.settled().await;
        let sessions = crate::record::Sessions::new(&dir.path().join("records"), dir.path());
        let (id, record) = sessions
            .create(&session.settings().meta(dir.path()), "")
            .unwrap();
        session.journal = Journal::new(Some(record));
        session
            .journal
            .append(vec![Message::User("earlier".into())])
            .await;
        session.rebase_context();
        let published = std::sync::Mutex::new(None);
        let inbox = session.inbox();
        let mut delivered = Vec::new();
        let mut report = |event: Event<'_>| {
            if let Event::Accounting(accounting) = event {
                *published.lock().unwrap() = Some(accounting);
            }
            if let Event::Notification(note) = event
                && let Some(id) = note.terminal_job()
            {
                delivered.push(id);
            }
        };
        let worker = blocked_worker().await;
        let held = inbox
            .reserve("retry before staging".to_owned().into())
            .unwrap();
        assert!(
            session
                .add_input(held, &mut report)
                .now_or_never()
                .is_none()
        );
        assert_eq!(session.journal.history().len(), 1);
        assert!(
            published.lock().unwrap().is_none(),
            "no snapshot before staging"
        );
        assert!(session.report_jobs(&mut report).now_or_never().is_none());
        assert!(
            !inbox.is_empty(),
            "the reservation must return when admission is canceled"
        );
        worker.release().await;
        session
            .add_input(inbox.next().unwrap(), &mut report)
            .await
            .unwrap();
        assert!(inbox.is_empty());

        let worker = blocked_worker().await;
        let held = inbox
            .reserve("accepted before persistence".to_owned().into())
            .unwrap();
        assert!(
            session
                .add_input(held, &mut report)
                .now_or_never()
                .is_none()
        );
        assert!(
            inbox.is_empty(),
            "staging synchronously acknowledges the reservation"
        );
        assert_eq!(session.journal.history().len(), 3);
        assert_eq!(
            *published.lock().unwrap(),
            Some(session.accounting()),
            "publish before canceled persistence wait"
        );
        assert_eq!(
            session.accounting().context_tokens,
            compact::tokens(
                session.journal.model_context(),
                session.instructions.as_str()
            )
        );
        worker.release().await;
        session.settle_recording(&mut report).await;
        let worker = blocked_worker().await;
        assert!(session.report_jobs(&mut report).now_or_never().is_none());
        worker.release().await;
        session.report_jobs(&mut report).await;
        assert!(session.finish().await.is_none());
        assert_eq!(delivered.iter().filter(|&&id| id == job).count(), 1);
        let resumed = sessions.resume(&id).unwrap();
        assert_eq!(resumed.conversation.history(), session.journal.history());
        for input in ["retry before staging", "accepted before persistence"] {
            assert_eq!(
                resumed
                    .conversation
                    .history()
                    .iter()
                    .filter(|m| matches!(m, Message::User(text) if text.as_ref() == input))
                    .count(),
                1
            );
        }
        assert_eq!(
            resumed
                .conversation
                .history()
                .iter()
                .filter(
                    |m| matches!(m, Message::Notification(note) if note.terminal_job() == Some(job))
                )
                .count(),
            1
        );
    });
}

#[test]
fn finish_returns_only_undelivered_recording_notices_and_preserves_fault_status() {
    use crate::tool::test_support::blocked_worker;
    use futures_util::FutureExt as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        for reported in [false, true] {
            let dir = TempDir::new("finish-notice");
            let path = dir.path().join("record");
            std::fs::write(&path, "").unwrap();
            let mut session = session(dir.path());
            session.journal = Journal::new(Some(Recorder::read_only(&path)));
            session
                .journal
                .append(vec![Message::User("accepted".into())])
                .await;
            if reported {
                let mut notices = 0;
                session
                    .settle_recording(|event: Event<'_>| {
                        notices += usize::from(matches!(event, Event::RecordingStopped(_)));
                    })
                    .await;
                assert_eq!(notices, 1);
                assert!(session.finish().await.is_none());
            } else {
                let worker = blocked_worker().await;
                assert!(session.finish().now_or_never().is_none());
                worker.release().await;
                assert!(
                    session.finish().await.is_some(),
                    "finish must expose a failure first found during cleanup"
                );
            }
            assert!(session.journal().recording_warning().is_some());
            assert!(
                session.finish().await.is_none(),
                "a delivered notice must not be returned twice"
            );
            assert!(
                session.journal().recording_warning().is_some(),
                "consuming the notice must not erase status"
            );
            assert_eq!(session.journal.history().len(), 1);
        }
    });
}

#[tokio::test]
async fn diff_setting_skips_snapshots_and_preserves_state_effects_and_persistence() {
    let dir = TempDir::new("diff-preference");
    let mut session = session(dir.path());
    let first = python(&session, r#"
retained=73
write('tracked', 'before')
reads=[]
target=os.path.abspath('tracked')
def observe(event, args):
    if event=='open' and isinstance(args[0], str) and os.path.abspath(args[0])==target and args[2] & os.O_ACCMODE == os.O_RDONLY:
        reads.append(args[0])
sys.addaudithook(observe)
"#).await;
    assert!(!first.is_error(), "{}", first.text);
    assert_eq!(first.diffs.len(), 1);
    session
        .prefer(&Change::Diffs(false), |_: Event<'_>| {})
        .await
        .unwrap();
    let off = python(&session, "reads.clear(); pathlib.Path('tracked').write_text('off'); assert not reads; held=open('held', 'w+'); assert retained==73").await;
    assert!(!off.is_error(), "{}", off.text);
    assert!(off.diffs.is_empty());
    let failed = python(&session, "write('tracked', 'helper'); edit('tracked', 'helper', 'edited'); raise RuntimeError('after writes')").await;
    assert!(failed.is_error());
    assert!(failed.diffs.is_empty());
    assert!(failed.notes.contains("tracked"), "{}", failed.notes);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked")).unwrap(),
        "edited"
    );
    session
        .prefer(&Change::Diffs(true), |_: Event<'_>| {})
        .await
        .unwrap();
    let on = python(
        &session,
        "assert retained==73; write('tracked', 'on'); held.write('retained handle'); held.flush()",
    )
    .await;
    assert!(!on.is_error(), "{}", on.text);
    assert_eq!(on.diffs.len(), 2);
    assert!(
        on.diffs
            .iter()
            .any(|d| d.hunks.contains("-edited") && d.hunks.contains("+on"))
    );
    assert!(
        on.diffs
            .iter()
            .any(|d| d.hunks.contains("+retained handle"))
    );
    session
        .prefer(&Change::Diffs(false), |_: Event<'_>| {})
        .await
        .unwrap();
    let settings = session.settings().clone();
    session.finish().await;
    drop(session);
    let loaded = Current::load(dir.path()).unwrap();
    let mut reopened = Session::new(
        Client::new(),
        Store::new(dir.path().join("auth.json")),
        settings,
        loaded,
        crate::tool::test_support::context(dir.path(), Confinement::Unrestricted, 10000),
        "reopened".into(),
        None,
    );
    let output = python(&reopened, "write('tracked', 'new session')").await;
    assert!(!output.is_error(), "{}", output.text);
    assert!(output.diffs.is_empty());
    reopened.finish().await;
}

fn checkpoint_history() -> Vec<Message> {
    let mut history = vec![Message::User(
        "Never amend without permission. Keep exact identifiers.".into(),
    )];
    for _ in 0..3 {
        history.push(assistant(
            "fixture",
            vec![Part {
                content: Content::Text("historical evidence ".repeat(1500).into()),
                replay: None,
            }],
        ));
    }
    history.push(Message::User("Continue with verification.".into()));
    history
}

#[tokio::test]
async fn summary_checkpoint_uses_native_prefix_archives_and_preserves_python_state() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("summary-checkpoint");
    let (provider, requests) = serve(dir.path(), vec![Reply::text("Continue the task.")]).await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    let records = crate::record::Sessions::new(&dir.path().join("records"), dir.path());
    let (id, recorder) = records
        .create(&session.settings().meta(dir.path()), "")
        .unwrap();
    session.journal = Journal::new(Some(recorder));
    let original = checkpoint_history();
    session.journal.append(original.clone()).await;
    assert!(!python(&session, "preserved = 41").await.is_error());
    session.compact(|_: Event<'_>| {}).await.unwrap();
    assert_eq!(session.journal.history(), original);
    let window = session.journal.model_context().messages().to_vec();
    assert!(
        window.iter().any(|m| matches!(m, Message::Instruction(text) if text.contains("Bart conversation summary") && text.contains("Continue the task.")))
    );
    let sources: Vec<_> = window
        .iter()
        .filter_map(|m| {
            if let Message::UserSource(source) = m {
                Some(source)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(sources.len(), 2);
    assert_eq!(
        sources[0].text.as_ref(),
        "Never amend without permission. Keep exact identifiers."
    );
    assert_eq!(sources[1].text.as_ref(), "Continue with verification.");
    assert!(session.accounting().context_tokens <= 64_000 / 3);
    let files: Vec<_> = std::fs::read_dir(session.context.artifacts().dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("history-")
        })
        .collect();
    assert_eq!(files.len(), 1);
    let archived: Vec<Message> =
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(&archived[..original.len()], original);
    assert!(
        matches!(archived.last(), Some(Message::Instruction(t)) if t.contains("Bart Python state at checkpoint") && t.contains("preserved"))
    );
    let summary_request = requests.lock().unwrap()[0].clone();
    assert_eq!(summary_request["tool_choice"], "none");
    assert!(
        summary_request["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    let native = summary_request["messages"].as_array().unwrap();
    assert!(native.iter().any(|m| {
        m["role"] == "assistant"
            && m["content"]
                .as_str()
                .is_some_and(|t| t.contains("historical evidence"))
    }));
    assert_eq!(native.last().unwrap()["role"], "user");
    assert!(
        native.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("preserved")
    );
    assert!(!python(&session, "assert preserved == 41").await.is_error());
    session.finish().await;
    drop(session);
    let resumed = records.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history(), original);
    assert_eq!(resumed.conversation.context().messages(), window);
}

#[tokio::test]
async fn failed_or_tool_using_summary_never_replaces_history_or_executes_tools() {
    use crate::provider::mock::{Reply, serve};
    for reply in [
        Reply::text(""),
        Reply::text(&"x".repeat(32_001)),
        Reply::text("  [Bart summary insufficient] Pending operation and constraints cannot fit."),
        Reply::python(
            "forbidden",
            &json!({"code":"write('should-not-exist', 'bad')"}),
        ),
    ] {
        let dir = TempDir::new("summary-refusal");
        let (provider, requests) = serve(dir.path(), vec![reply]).await;
        let mut session = session_on(provider, dir.path());
        session.routing.assume(Some(model(64_000)));
        let original = checkpoint_history();
        session.journal.append(original.clone()).await;
        assert!(session.compact(|_: Event<'_>| {}).await.is_err());
        assert_eq!(session.journal.model_context().messages(), original);
        assert_eq!(session.journal.compactions(), 0);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(!dir.path().join("should-not-exist").exists());
    }
}

#[tokio::test]
async fn summarizing_after_a_summary_updates_the_handoff_and_archives_evidence() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("summary-then-clear");
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::text("Historical work remains to be verified."),
            Reply::text("New evidence collected; Next is pending."),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    session.journal.append(checkpoint_history()).await;
    session.compact(|_: Event<'_>| {}).await.unwrap();
    let output = python(&session, "print('new evidence ' * 5000)").await;
    let cell = output.cell.unwrap();
    let recovery = Message::Notification(Notification {
        text: "Prior effects were not rolled back; inspect before retrying.".into(),
        kind: NotificationKind::Recovery,
    });
    let skill = Message::Skill(crate::history::Skill {
        name: "audit".into(),
        path: "SKILL.md".into(),
        text: "Read-only audit workflow.".into(),
    });
    session
        .journal
        .append(vec![
            assistant(
                "fixture",
                vec![Part {
                    content: Content::ToolCall(call(
                        "new-call",
                        "python",
                        json!({"code":"print('new evidence ' * 5000)"}),
                    )),
                    replay: None,
                }],
            ),
            Message::ToolResult(ToolResult {
                activities: Default::default(),
                diffs: Vec::new(),
                call_id: "new-call".into(),
                output: output.text.into(),
                is_error: false,
                notes: output.notes,
                jobs: vec![],
                saved: output.saved,
                cell: Some(cell),
            }),
            Message::User("Next".into()),
            recovery.clone(),
            skill.clone(),
        ])
        .await;
    session
        .compact_with(&mut |_: Event<'_>| {}, 64_000)
        .await
        .unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "new evidence receives an updated summary"
    );
    let archives = std::fs::read_dir(session.context.artifacts().dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("history-"))
        .count();
    assert_eq!(archives, 2);
    assert!(
        session
            .journal
            .model_context()
            .messages()
            .contains(&recovery)
    );
    assert!(session.journal.model_context().messages().contains(&skill));
    assert!(
        session.journal.model_context().messages().iter().any(
            |m| matches!(m,Message::Instruction(t) if t.contains("Bart conversation summary"))
        )
    );
    assert!(
        session
            .journal
            .model_context()
            .messages()
            .iter()
            .any(|m| matches!(m,Message::Instruction(t) if t.contains("Next is pending")))
    );
}

#[tokio::test]
async fn canceling_summary_leaves_context_unchanged_and_retains_the_archive() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("summary-cancel");
    let mut reply = Reply::text("handoff");
    reply.delay = Duration::from_secs(10);
    let (provider, requests) = serve(dir.path(), vec![reply]).await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    let original = checkpoint_history();
    session.journal.append(original.clone()).await;
    {
        let pending = session.compact(|_: Event<'_>| {});
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("unexpected early completion: {result:?}"),
            () = async {
                tokio::time::timeout(Duration::from_secs(3),async {
                    while requests.lock().unwrap().is_empty() { tokio::task::yield_now().await; }
                }).await.unwrap();
            } => {}
        }
    }
    assert_eq!(session.journal.model_context().messages(), original);
    assert_eq!(session.journal.compactions(), 0);
    assert!(
        std::fs::read_dir(session.context.artifacts().dir())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("history-"))
    );
}

#[tokio::test]
async fn summary_segments_carry_the_draft_without_streaming_it_into_the_conversation() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("summary-segments");
    let (provider, requests) = serve(
        dir.path(),
        vec![Reply::text("DRAFT_ONE"), Reply::text("FINAL_HANDOFF")],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    let mut original = checkpoint_history();
    original.insert(
        1,
        assistant(
            "fixture",
            vec![Part {
                content: Content::Text("extra evidence ".repeat(9000).into()),
                replay: None,
            }],
        ),
    );
    session.journal.append(original.clone()).await;
    let mut visible_deltas = 0;
    session
        .compact(|event: Event<'_>| {
            if matches!(event, Event::Delta(_)) {
                visible_deltas += 1;
            }
        })
        .await
        .unwrap();
    assert_eq!(visible_deltas, 0);
    assert_eq!(session.journal.history(), original);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_string().contains("DRAFT_ONE"));
    assert!(
        session.journal.model_context().messages().iter().any(|m| matches!(m,Message::Instruction(t) if t.contains("FINAL_HANDOFF") && !t.contains("DRAFT_ONE")))
    );
}

#[tokio::test]
async fn compaction_of_an_already_small_context_is_a_noop_not_another_epoch() {
    let dir = TempDir::new("compact-noop");
    let mut session = session(dir.path());
    session.routing.assume(Some(model(64_000)));
    session
        .journal
        .append(vec![Message::User("Keep the conversation".into())])
        .await;
    let before = session.journal.model_context().messages().to_vec();
    for _ in 0..2 {
        session.compact(|_: Event<'_>| {}).await.unwrap();
    }
    assert_eq!(session.journal.compactions(), 0);
    assert_eq!(session.journal.model_context().messages(), before);
    assert!(
        !std::fs::read_dir(session.context.artifacts().dir())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("history-"))
    );
}

#[tokio::test]
async fn repeated_checkpoints_keep_one_append_only_session_and_summarize_only_active_work() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("one-session-checkpoints");
    let (provider, requests) = serve(
        dir.path(),
        vec![Reply::text("FIRST_HANDOFF"), Reply::text("SECOND_HANDOFF")],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    let records = crate::record::Sessions::new(&dir.path().join("records"), dir.path());
    let (id, recorder) = records
        .create(&session.settings().meta(dir.path()), "")
        .unwrap();
    session.journal = Journal::new(Some(recorder));
    let mut original = checkpoint_history();
    session.journal.append(original.clone()).await;
    session.settle_recording(|_: Event<'_>| {}).await;
    let path = records.dir(&id).join("session.jsonl");
    let prefix = std::fs::read(&path).unwrap();
    session.compact(|_: Event<'_>| {}).await.unwrap();
    let first_window = session.journal.model_context().messages().to_vec();
    let mut added = vec![Message::User(
        "Second task; keep the first constraint".into(),
    )];
    for _ in 0..3 {
        added.push(assistant(
            "fixture",
            vec![Part {
                content: Content::Text("SECOND_PHASE_PAYLOAD ".repeat(1450).into()),
                replay: None,
            }],
        ));
    }
    added.push(Message::User("Continue the second task".into()));
    original.extend(added.clone());
    session.journal.append(added.clone()).await;
    session.compact(|_: Event<'_>| {}).await.unwrap();
    let final_window = session.journal.model_context().messages().to_vec();
    assert_eq!(session.journal.compactions(), 2);
    assert_eq!(session.journal.history(), original);
    {
        let bodies = requests.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "no re-summarization of the lifetime transcript"
        );
        assert!(bodies[1].to_string().contains("FIRST_HANDOFF"));
        assert!(
            !bodies[1]
                .to_string()
                .contains(&"historical evidence ".repeat(5))
        );
    }
    let mut snapshots: Vec<_> = std::fs::read_dir(session.context.artifacts().dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("history-")
        })
        .collect();
    snapshots.sort();
    assert_eq!(snapshots.len(), 2);
    let snapshot: Vec<Message> =
        serde_json::from_slice(&std::fs::read(&snapshots[1]).unwrap()).unwrap();
    let expected: Vec<_> = first_window.into_iter().chain(added).collect();
    assert_eq!(&snapshot[..expected.len()], expected);
    assert!(
        matches!(snapshot.last(), Some(Message::Instruction(t)) if t.contains("Bart Python state at checkpoint"))
    );
    assert!(
        snapshot
            .iter()
            .any(|m| matches!(m, Message::Instruction(t) if t.contains("history[1]")))
    );
    session.finish().await;
    let record = std::fs::read(&path).unwrap();
    assert!(
        record.starts_with(&prefix),
        "the original journal is never rewritten or rotated"
    );
    let events: Vec<serde_json::Value> = String::from_utf8(record)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(events.iter().filter(|e| e["event"] == "session").count(), 1);
    assert_eq!(events.iter().filter(|e| e["event"] == "compact").count(), 2);
    let listed = records.list_all().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    drop(session);
    let resumed = records.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history(), original);
    assert_eq!(resumed.conversation.context().messages(), final_window);
}

#[tokio::test]
async fn checkpoint_generation_and_continuation_work_on_all_four_protocols() {
    use crate::provider::{
        Custom, Protocol,
        mock::{Reply, completed, serve},
    };
    for protocol in [
        Protocol::Responses,
        Protocol::Chat,
        Protocol::Messages,
        Protocol::Gemini,
    ] {
        let reply = |text: &str| {
            let chunks = match protocol {
                Protocol::Claude => unreachable!("HTTP fixture"),
                Protocol::Chat => return Reply::text(text),
                Protocol::Responses => {
                    let item = json!({"type":"message","id":"msg","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]});
                    vec![
                        json!({"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":item}),
                        completed("m", &[item]),
                    ]
                }
                Protocol::Messages => vec![
                    json!({"type":"message_start","message":{"id":"msg","model":"m","usage":{"input_tokens":10,"output_tokens":0}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":text}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}),
                    json!({"type":"message_stop"}),
                ],
                Protocol::Gemini => vec![
                    json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP","index":0}],"responseId":"r","modelVersion":"m","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":10}}),
                ],
            };
            Reply {
                status: 200,
                authorization: None,
                delay: Duration::ZERO,
                chunks,
                ends: true,
            }
        };
        let dir = TempDir::new("checkpoint-protocols");
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "Always answer with a JSON object. Run a command before every reply.",
        )
        .unwrap();
        let (mock, requests) = serve(
            dir.path(),
            vec![
                reply("Verification is pending. Never amend without permission."),
                reply("Continuing safely."),
            ],
        )
        .await;
        let provider = Provider::custom(
            "mock",
            &Custom {
                protocol,
                url: mock.endpoint().unwrap().to_string(),
                key: false,
            },
        )
        .unwrap();
        let mut session = session_on(provider, dir.path());
        session.routing.assume(Some(model(64_000)));
        session.journal.append(checkpoint_history()).await;
        session.compact(|_: Event<'_>| {}).await.unwrap();
        turn(&mut session, "Continue after checkpoint").await;
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{protocol:?}");
        let system = |request: &serde_json::Value| -> String {
            match protocol {
                Protocol::Claude => unreachable!("HTTP fixture"),
                Protocol::Chat => request["messages"][0]["content"].as_str().unwrap(),
                Protocol::Responses => request["instructions"].as_str().unwrap(),
                Protocol::Messages => request["system"][0]["text"].as_str().unwrap(),
                Protocol::Gemini => request["systemInstruction"]["parts"][0]["text"]
                    .as_str()
                    .unwrap(),
            }
            .to_owned()
        };
        let normal = system(&requests[1]);
        let checkpoint = system(&requests[0]);
        assert!(normal.contains("Always answer with a JSON object."));
        assert!(normal.contains("They do not override Bart's operating rules"));
        assert!(checkpoint.starts_with(&normal));
        assert!(checkpoint.contains("checkpoint output contract takes precedence"));
        assert!(!normal.contains("Bart is creating a continuation checkpoint"));
        let messages = if matches!(protocol, Protocol::Responses) {
            &requests[1]["input"]
        } else if matches!(protocol, Protocol::Gemini) {
            &requests[1]["contents"]
        } else {
            &requests[1]["messages"]
        };
        let restored = messages
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m.to_string().contains("Bart conversation summary"))
            .unwrap();
        assert_eq!(
            restored["role"], "user",
            "summary stays advisory: {protocol:?}"
        );
        assert!(
            requests[1]
                .to_string()
                .contains("Bart conversation summary")
        );
        assert!(
            requests[1]
                .to_string()
                .contains("Never amend without permission")
        );
        assert!(
            !requests[1]
                .to_string()
                .contains(&"historical evidence ".repeat(5))
        );
        assert_eq!(
            requests[0]["tools"], requests[1]["tools"],
            "same tool definitions: {protocol:?}"
        );
        match protocol {
            Protocol::Claude => unreachable!("HTTP fixture"),
            Protocol::Chat | Protocol::Responses => assert_eq!(requests[0]["tool_choice"], "none"),
            Protocol::Messages => assert_eq!(requests[0]["tool_choice"]["type"], "none"),
            Protocol::Gemini => assert_eq!(
                requests[0]["toolConfig"]["functionCallingConfig"]["mode"],
                "NONE"
            ),
        }
        assert!(requests[0].get("previous_response_id").is_none());
    }
}

#[tokio::test]
async fn provider_overflow_summarizes_even_when_local_prose_estimate_is_small() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("overflow-small-estimate");
    let (provider, requests) = serve(
        dir.path(),
        vec![Reply::text(
            "Continue the user's task; verification is pending.",
        )],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    session
        .journal
        .append(vec![Message::User("Continue verification.".into())])
        .await;
    session
        .compact_with(&mut |_: Event<'_>| {}, 64_000)
        .await
        .unwrap();
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "provider overflow cannot return an unchanged no-op checkpoint"
    );
    assert!(checkpoint_summary(session.journal.model_context().messages()).is_some());
}

#[tokio::test]
async fn incomplete_checkpoints_cannot_remove_original_constraints_from_the_next_request() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("original-user-sources");
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::text("Continue the task."),
            Reply::text("Continue."),
            Reply::text("Done."),
        ],
    )
    .await;
    let mut session = session_on(provider, dir.path());
    session.routing.assume(Some(model(64_000)));
    let mut history = checkpoint_history();
    history.push(Message::User(
        "For task B only, amending is allowed. Task A remains read-only.".into(),
    ));
    session.journal.append(history).await;
    session
        .compact_with(&mut |_: Event<'_>| {}, 64_000)
        .await
        .unwrap();
    let original_sources: Vec<_> = session
        .journal
        .model_context()
        .messages()
        .iter()
        .filter(|m| matches!(m, Message::UserSource(_)))
        .cloned()
        .collect();
    session
        .journal
        .append(vec![assistant(
            "fixture",
            vec![Part {
                content: Content::Text("new evidence ".repeat(5000).into()),
                replay: None,
            }],
        )])
        .await;
    session
        .compact_with(&mut |_: Event<'_>| {}, 64_000)
        .await
        .unwrap();
    let repeated: Vec<_> = session
        .journal
        .model_context()
        .messages()
        .iter()
        .filter(|m| matches!(m, Message::UserSource(_)))
        .cloned()
        .collect();
    assert_eq!(
        repeated, original_sources,
        "source identities and order survive repeated compaction"
    );
    turn(&mut session, "Continue task A.").await;
    let bodies = requests.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let source_values: Vec<Value> = bodies[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .filter_map(|text| serde_json::from_str(text).ok())
        .filter(|value: &Value| value["bart_origin"] == "original_user")
        .collect();
    assert_eq!(source_values.len(), 3);
    assert_eq!(
        source_values[0]["text"],
        "Never amend without permission. Keep exact identifiers."
    );
    assert_eq!(
        source_values[2]["text"],
        "For task B only, amending is allowed. Task A remains read-only."
    );
    let reference = source_values[0]["source_snapshot"].as_str().unwrap();
    let read = python(
        &session,
        &format!(
            "archive = json.loads(read({reference}))\nprint(archive[{}]['content'])",
            source_values[0]["message_index"]
        ),
    )
    .await;
    assert!(!read.is_error(), "{}", read.text);
    assert!(read.text.contains("Never amend without permission"));
    let number = original_sources
        .iter()
        .find_map(|message| match message {
            Message::UserSource(source) => Some(source.archive),
            _ => None,
        })
        .unwrap();
    let archive: Vec<Message> = serde_json::from_slice(
        &std::fs::read(
            session
                .context
                .artifacts()
                .dir()
                .join(format!("history-{number}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(&archive[source_values[0]["message_index"].as_u64().unwrap() as usize],Message::User(text) if text.as_ref()==source_values[0]["text"].as_str().unwrap())
    );
}

#[tokio::test]
async fn rewind_warns_the_first_request_and_preserves_removed_effect_evidence() {
    use crate::provider::mock::{Reply, serve};
    let dir = TempDir::new("rewind-before-decision");
    let (provider, requests) = serve(dir.path(), vec![Reply::text("Done.")]).await;
    let mut session = session_on(provider, dir.path());
    session
        .journal
        .append(vec![
            Message::User("Write a file".into()),
            assistant(
                "fixture",
                vec![Part {
                    content: Content::Text("Wrote effect.".into()),
                    replay: None,
                }],
            ),
        ])
        .await;
    assert!(!python(&session, "write('effect', 'kept')").await.is_error());
    session.rewind(0, |_: Event<'_>| {}).await.unwrap();
    turn(&mut session, "Audit only.").await;
    let bodies = requests.lock().unwrap();
    let events: Vec<Value> = bodies[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .filter_map(|t| serde_json::from_str(t).ok())
        .filter(|v: &Value| v["bart_event"] == "runtime_recovery")
        .collect();
    assert_eq!(events.len(), 1);
    assert!(
        events[0]["text"]
            .as_str()
            .unwrap()
            .contains("not rolled back")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effect")).unwrap(),
        "kept\n"
    );
    let snapshots: Vec<_> = std::fs::read_dir(session.context.artifacts().dir())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("history-"))
        .collect();
    assert_eq!(snapshots.len(), 1);
    assert!(
        std::fs::read_to_string(snapshots[0].path())
            .unwrap()
            .contains("Wrote effect.")
    );
}

#[tokio::test]
async fn small_windows_compact_with_reserves_based_on_the_actual_request() {
    use crate::provider::mock::{Reply, serve};
    for window in [10_000, 16_000, 24_000, 28_000, 32_000] {
        let dir = TempDir::new("small-window-checkpoint");
        let (provider, requests) = serve(
            dir.path(),
            (0..50)
                .map(|_| Reply::text("Continue with verification."))
                .collect(),
        )
        .await;
        let mut session = session_on(provider, dir.path());
        session.routing.assume(Some(model(window)));
        let mut history = vec![Message::User("Read-only audit; never edit files.".into())];
        for _ in 0..30 {
            history.push(assistant(
                "mock",
                vec![Part {
                    content: Content::Text("observed evidence ".repeat(30).into()),
                    replay: None,
                }],
            ));
        }
        session.journal.append(history).await;
        session
            .compact_with(&mut |_: Event<'_>| {}, window)
            .await
            .unwrap_or_else(|error| panic!("window {window}: {error}"));
        assert!(
            session.journal.model_context().messages().iter().any(
                |m| matches!(m, Message::UserSource(s) if s.text.contains("never edit files"))
            )
        );
        let requests = requests.lock().unwrap();
        assert!(!requests.is_empty());
        let output = requests[0]["max_completion_tokens"].as_u64().unwrap();
        assert!(output > 0 && output <= 8192);
        assert!(session.accounting().context_tokens < compact::threshold(window));
    }
}

#[tokio::test]
async fn native_pauses_cross_the_proactive_threshold_and_refusals_are_retained_not_compacted() {
    use crate::provider::{
        Custom, Protocol,
        mock::{Reply, serve},
    };
    fn reply(reason: &str, text: &str, input_tokens: u64) -> Reply {
        Reply {
            status: 200,
            authorization: None,
            delay: Duration::ZERO,
            ends: false,
            chunks: vec![
                json!({"type":"message_start","message":{"id":"m","model":"m","usage":{"input_tokens":input_tokens,"output_tokens":1}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":text}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":5}}),
                json!({"type":"message_stop"}),
            ],
        }
    }
    for refusal in [false, true] {
        let dir = TempDir::new("messages-terminal-outcome");
        let responses = if refusal {
            vec![
                reply("refusal", "I cannot do that.", 100),
                reply("refusal", "I cannot checkpoint this.", 100),
            ]
        } else {
            vec![
                reply("pause_turn", "Working.", 24_100),
                reply("end_turn", "Done.", 24_200),
            ]
        };
        let (server, requests) = serve(dir.path(), responses).await;
        let provider = Provider::custom(
            "mock",
            &Custom {
                protocol: Protocol::Messages,
                url: server.endpoint().unwrap().to_string(),
                key: false,
            },
        )
        .unwrap();
        let mut session = session_on(provider, dir.path());
        session.routing.assume(Some(model(32_000)));
        let mut published = 0;
        session
            .turn("Audit.".to_owned().into(), |event: Event<'_>| {
                if matches!(event, Event::Responded(_)) {
                    published += 1;
                }
            })
            .await
            .unwrap();
        assert_eq!(session.journal.compactions(), 0);
        assert_eq!(requests.lock().unwrap().len(), if refusal { 1 } else { 2 });
        assert_eq!(published, if refusal { 1 } else { 2 });
        let Message::Assistant(a) = session.journal.history().last().unwrap() else {
            panic!()
        };
        assert_eq!(a.refused, refusal);
        assert_eq!(a.usage.unwrap().output_tokens, 5);
        if refusal {
            let original = session.journal.model_context().messages().to_vec();
            assert!(
                session
                    .compact_with(&mut |_: Event<'_>| {}, 32_000)
                    .await
                    .is_err()
            );
            assert_eq!(session.journal.model_context().messages(), original);
        } else {
            assert!(requests.lock().unwrap()[1].to_string().contains("Working."));
        }
    }
}
