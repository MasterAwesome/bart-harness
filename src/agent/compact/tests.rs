use super::*;
use crate::conversation::Conversation;
use crate::history::{Assistant, Completeness, Part, SavedArtifact, ToolCall, ToolResult, Usage};
use serde_json::json;

fn synthetic_checkpoint(instructions: &str) -> Reduced {
    let window = vec![Message::Instruction("Synthetic historical summary".into())];
    let tokens = window
        .iter()
        .map(estimate)
        .fold(base(instructions), u64::saturating_add);
    Reduced {
        estimate: Estimate {
            messages: window.len(),
            tokens,
        },
        window,
    }
}

fn usage(input_tokens: u64, reasoning_tokens: Option<u64>) -> Usage {
    Usage {
        input_tokens,
        output_tokens: 10,
        reasoning_tokens,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost: None,
        cost_is_estimate: false,
    }
}

fn step(id: &str, code: &str, reasoning: Option<u64>, prose: Option<&str>) -> Message {
    let mut parts = Vec::new();
    if reasoning.is_some() {
        parts.push(Part {
            content: Content::Reasoning(String::new().into()),
            replay: Some(json!({"encrypted_content": "opaque", "signature": id}).into()),
        });
    }
    if let Some(prose) = prose {
        parts.push(Part {
            content: Content::Text(prose.into()),
            replay: None,
        });
    }
    parts.push(Part {
        content: Content::ToolCall(ToolCall {
            id: id.into(),
            name: "python".into(),
            arguments: json!({"code": code, "timeout": 12}).into(),
        }),
        replay: Some(json!({"type": "function_call", "id": id}).into()),
    });
    Message::Assistant(Assistant {
        protocol: crate::provider::Protocol::Responses,
        provider: "p".into(),
        model: "m".into(),
        parts,
        usage: reasoning.map(|tokens| usage(1, Some(tokens))),
        continues: false,
        refused: false,
        served_model: None,
    })
}

fn result(id: &str, cell: Option<u64>, text: &str, saved: Completeness, chars: usize) -> Message {
    Message::ToolResult(ToolResult {
        activities: Default::default(),
        diffs: Vec::new(),
        notes: String::new(),
        call_id: id.into(),
        output: text.into(),
        is_error: false,
        jobs: Vec::new(),
        saved: cell.map(|_| SavedArtifact {
            path: "unused".into(),
            completeness: saved,
            chars,
            bytes: text.len(),
        }),
        cell,
    })
}

fn full(id: &str, cell: u64, text: &str) -> Message {
    result(
        id,
        Some(cell),
        text,
        Completeness::Complete,
        text.chars().count(),
    )
}

#[test]
fn the_checkpoint_estimate_replaces_stale_usage_until_a_response_reports_again() {
    let mut stalled = step("step", "print(2)", None, None);
    let Message::Assistant(value) = &mut stalled else {
        unreachable!()
    };
    value.usage = Some(usage(90_000, None));
    let history = vec![
        Message::User("old".into()),
        step("old", "print(1)", None, None),
        full("old", 1, &"o".repeat(200_000)),
        Message::User("now".into()),
        stalled,
        full("step", 2, "2"),
    ];
    let measured = tokens(Conversation::of(history.clone()).context(), "");
    let Reduced {
        window, estimate, ..
    } = synthetic_checkpoint("");
    assert!(estimate.tokens <= 50_000 && estimate.tokens < measured - 40_000);
    let mut conversation = Conversation::default();
    conversation.compact(window, estimate);
    assert_eq!(tokens(conversation.context(), ""), estimate.tokens);
    conversation.push(Message::User("more".into()));
    assert!(tokens(conversation.context(), "") > estimate.tokens);
    let mut reply = step("reply", "print(3)", None, None);
    let Message::Assistant(value) = &mut reply else {
        unreachable!()
    };
    value.usage = Some(usage(30_000, None));
    conversation.push(reply);
    assert_eq!(tokens(conversation.context(), ""), 30_010);
}

#[test]
fn extreme_usage_saturates() {
    let mut response = step("x", "print(1)", Some(u64::MAX), None);
    let Message::Assistant(value) = &mut response else {
        panic!()
    };
    value.usage = Some(Usage {
        output_tokens: u64::MAX,
        ..usage(u64::MAX, Some(u64::MAX))
    });
    assert_eq!(
        tokens(
            Conversation::of(vec![response.clone(), response]).context(),
            ""
        ),
        u64::MAX
    );
}

#[test]
fn replayed_reasoning_costs_its_reported_tokens_not_its_encrypted_bytes() {
    let with = |encrypted: usize, reported: Option<u64>| {
        let mut message = step("x", "print(1)", Some(0), None);
        let Message::Assistant(value) = &mut message else {
            unreachable!()
        };
        value.parts[0].replay = Some(json!({"encrypted_content": "e".repeat(encrypted)}).into());
        value.usage = Some(usage(1, reported));
        estimate(&message)
    };
    assert_eq!(with(40_000, Some(50)), with(10, Some(50)));
    assert_eq!(with(40_000, Some(50)) - with(40_000, Some(0)), 50);
    assert!(
        with(40_000, None) > with(10, None) + 9_000,
        "unreported reasoning falls back to its bytes"
    );
}

#[tokio::test]
async fn rewind_preserves_safe_checkpoints_and_usage_live_and_after_resume() {
    use crate::{
        record::{Meta, Recorder, Sessions},
        tool::test_support::TempDir,
    };

    fn reopen(
        sessions: &Sessions,
        id: &str,
        record: Recorder,
        conversation: &Conversation,
    ) -> (Recorder, Conversation) {
        let expected = tokens(conversation.context(), "instructions");
        drop(record);
        let resumed = sessions.resume(id).unwrap();
        assert_eq!(resumed.conversation.history(), conversation.history());
        assert_eq!(
            resumed.conversation.context().messages(),
            conversation.context().messages()
        );
        assert_eq!(
            tokens(resumed.conversation.context(), "instructions"),
            expected
        );
        (resumed.record, resumed.conversation)
    }

    let dir = TempDir::new("rewind-estimate");
    let sessions = Sessions::new(dir.path(), std::path::Path::new("/work"));
    let (id, mut record) = sessions
        .create(
            &Meta {
                cwd: "/work".into(),
                provider: "p".into(),
                model: "m".into(),
                effort: None,
            },
            "",
        )
        .unwrap();
    let history = vec![
        Message::User("old".into()),
        step("old", "print(1)", None, None),
        full("old", 1, &"x".repeat(800_000)),
        Message::User("next".into()),
    ];
    let mut conversation = Conversation::of(history.clone());
    record.append_batch(history).await.unwrap();
    let reduced = synthetic_checkpoint("");
    record
        .compact(&reduced.window, reduced.estimate)
        .await
        .unwrap();
    conversation.compact(reduced.window, reduced.estimate);
    let mut reply = step("reply", "print(3)", None, None);
    let Message::Assistant(assistant) = &mut reply else {
        unreachable!()
    };
    assistant.usage = Some(usage(30_000, None));
    for message in [reply.clone(), Message::User("discard".into())] {
        record.append(&message).await.unwrap();
        conversation.push(message);
    }
    record.rewind(5).await.unwrap();
    conversation.rewind(5);
    (record, conversation) = reopen(&sessions, &id, record, &conversation);
    assert_eq!(tokens(conversation.context(), "instructions"), 30_010);
    assert_eq!(conversation.history()[4], reply, "usage remains intact");

    record.rewind(4).await.unwrap();
    conversation.rewind(4);
    (record, conversation) = reopen(&sessions, &id, record, &conversation);
    assert!(
        tokens(conversation.context(), "instructions") < 50_000,
        "rewind at the checkpoint boundary must not restore the large original output"
    );
    record.append(&reply).await.unwrap();
    conversation.push(reply.clone());
    (record, conversation) = reopen(&sessions, &id, record, &conversation);
    assert_eq!(
        tokens(conversation.context(), "instructions"),
        30_010,
        "fresh usage is authoritative"
    );

    record.rewind(4).await.unwrap();
    conversation.rewind(4);
    let reduced = synthetic_checkpoint("instructions");
    assert!(reduced.estimate.tokens < 50_000);
    record
        .compact(&reduced.window, reduced.estimate)
        .await
        .unwrap();
    conversation.compact(reduced.window, reduced.estimate);
    (record, conversation) = reopen(&sessions, &id, record, &conversation);
    assert!(
        tokens(conversation.context(), "instructions") < 50_000,
        "compacting restored history replaces its accounting again"
    );
    assert!(
        matches!(&conversation.history()[2], Message::ToolResult(result) if result.output.len() == 800_000)
    );
    record.append(&reply).await.unwrap();
    conversation.push(reply);
    let (_, conversation) = reopen(&sessions, &id, record, &conversation);
    assert_eq!(tokens(conversation.context(), "instructions"), 30_010);
}

#[tokio::test]
async fn recovery_insertions_charge_content_and_never_revalidate_stale_usage() {
    use crate::{
        agent::journal::Journal,
        record::{Meta, Sessions},
        tool::test_support::TempDir,
    };

    fn response(id: &str, reported: u64, orphan: bool) -> Message {
        let mut message = step(id, "print(1)", None, None);
        let Message::Assistant(assistant) = &mut message else {
            unreachable!()
        };
        assistant.usage = Some(usage(reported, None));
        if !orphan {
            assistant.parts = vec![Part {
                content: Content::Text("seen".into()),
                replay: None,
            }];
        }
        message
    }

    for compacted in [false, true] {
        for (insertion, fresh) in [(3, false), (3, true), (4, false), (4, true), (6, false)] {
            let dir = TempDir::new("recovery-accounting");
            let sessions = Sessions::new(dir.path(), std::path::Path::new("/work"));
            let (id, record) = sessions
                .create(
                    &Meta {
                        cwd: "/work".into(),
                        provider: "p".into(),
                        model: "m".into(),
                        effort: None,
                    },
                    "",
                )
                .unwrap();
            let mut journal = Journal::new(Some(record));
            journal
                .append(vec![
                    Message::User("old".into()),
                    full("old", 1, &"x".repeat(800_000)),
                    if insertion == 3 {
                        response("orphan", 1, true)
                    } else {
                        Message::Instruction("filler".into())
                    },
                    response("retained", 1, insertion == 4),
                ])
                .await;
            if compacted {
                let mut checkpoint_window = journal.history().to_vec();
                checkpoint_window[1] = full("old", 1, "checkpoint_window");
                journal
                    .checkpoint(
                        checkpoint_window,
                        Estimate {
                            messages: 4,
                            tokens: 50_000,
                        },
                    )
                    .await;
            } else {
                journal.append(vec![Message::User("discard".into())]).await;
                journal.rewind(4).await.unwrap();
            }
            if fresh {
                journal.append(vec![response("later", 100, false)]).await;
            }
            if insertion == 6 {
                journal
                    .append(vec![
                        Message::Instruction("later".into()),
                        step("orphan", "print(1)", None, None),
                    ])
                    .await;
            }
            let usages = |messages: &[Message]| {
                messages
                    .iter()
                    .filter_map(|message| match message {
                        Message::Assistant(a) => a.usage,
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let before_usage = usages(journal.history());
            let output = "recovered output ".repeat(400);
            let suffix_cost = journal.model_context().messages()[4..]
                .iter()
                .map(estimate)
                .sum::<u64>();
            journal.recover(&output).await;
            journal.settle_recording().await;
            assert!(journal.recording_warning().is_none());
            assert_eq!(
                usages(journal.history()),
                before_usage,
                "recorded usage is never rewritten"
            );
            assert!(
                matches!(&journal.model_context().messages()[insertion], Message::ToolResult(result) if result.output.as_ref() == output)
            );
            let counted = tokens(journal.model_context(), "instructions");
            let inserted = estimate(&journal.model_context().messages()[insertion]);
            if compacted {
                assert_eq!(
                    counted,
                    50_000 + suffix_cost + inserted,
                    "insertion={insertion}, fresh={fresh}"
                );
            } else if insertion != 3 && !fresh {
                assert_eq!(
                    counted,
                    11 + suffix_cost + inserted,
                    "an insertion after the retained report preserves its measurement"
                );
            } else {
                assert!(
                    counted >= 200_000 + inserted,
                    "insertion={insertion}, fresh={fresh}: {counted}"
                );
            }
            let history = journal.history().to_vec();
            let snapshot = journal.model_context().snapshot();
            drop(journal);
            let resumed = sessions.resume(&id).unwrap();
            assert_eq!(resumed.conversation.history(), history);
            assert_eq!(
                resumed.conversation.context().messages(),
                snapshot.view().messages()
            );
            assert_eq!(
                tokens(resumed.conversation.context(), "instructions"),
                counted,
                "repair accounting must replay identically"
            );
            let mut journal = Journal::new(Some(resumed.record));
            journal
                .resume(
                    None,
                    crate::agent::journal::Restored {
                        conversation: resumed.conversation,
                        name: resumed.name,
                        prompt: resumed.prompt,
                    },
                )
                .await;
            journal.append(vec![response("new", 12_000, false)]).await;
            journal.recover("unused").await;
            assert_eq!(tokens(journal.model_context(), "instructions"), 12_010);
            journal
                .append(vec![response("latest-orphan", 90_000, true)])
                .await;
            journal.recover(&output).await;
            journal.settle_recording().await;
            assert!(journal.recording_warning().is_none());
            assert_eq!(
                tokens(journal.model_context(), "instructions"),
                90_010 + inserted,
                "inserting after authoritative usage preserves it and charges the suffix"
            );
            drop(journal);
            let resumed = sessions.resume(&id).unwrap();
            assert_eq!(&resumed.conversation.history()[..history.len()], history);
            assert_eq!(
                &resumed.conversation.context().messages()[..snapshot.view().messages().len()],
                snapshot.view().messages()
            );
            assert_eq!(
                tokens(resumed.conversation.context(), "instructions"),
                90_010 + inserted
            );
            resumed.record.rewind(history.len()).await.unwrap();
            drop(resumed);
            let resumed = sessions.resume(&id).unwrap();
            assert_eq!(resumed.conversation.history(), history);
            let expected = if compacted {
                counted
            } else {
                // The earlier report before the insertion is still valid, even
                // when the latest report was invalidated by that insertion.
                let after = if insertion == 3 { 3 } else { 4 };
                11 + history[after..].iter().map(estimate).sum::<u64>()
            };
            assert_eq!(
                tokens(resumed.conversation.context(), "instructions"),
                expected,
                "rewind uses only reports whose measured prompt is unchanged"
            );
        }
    }
}

#[tokio::test]
async fn repaired_checkpoints_charge_only_insertions_and_survive_usage_invalidation() {
    use crate::{
        record::{Meta, Recorder, Sessions},
        tool::test_support::TempDir,
    };

    fn reopen(
        sessions: &Sessions,
        id: &str,
        record: Recorder,
        conversation: &Conversation,
        expected: u64,
    ) -> (Recorder, Conversation) {
        assert_eq!(tokens(conversation.context(), "instructions"), expected);
        assert_eq!(
            tokens(conversation.context().snapshot().view(), "instructions"),
            expected
        );
        drop(record);
        let resumed = sessions.resume(id).unwrap();
        assert_eq!(resumed.conversation.history(), conversation.history());
        assert_eq!(
            resumed.conversation.context().messages(),
            conversation.context().messages()
        );
        assert_eq!(
            tokens(resumed.conversation.context(), "instructions"),
            expected
        );
        (resumed.record, resumed.conversation)
    }

    let dir = TempDir::new("checkpoint-insertions");
    let sessions = Sessions::new(dir.path(), std::path::Path::new("/work"));
    let (id, mut record) = sessions
        .create(
            &Meta {
                cwd: "/work".into(),
                provider: "p".into(),
                model: "m".into(),
                effort: None,
            },
            "",
        )
        .unwrap();
    let history: Vec<_> = (0..512)
        .map(|n| full(&format!("old-{n}"), n, &"x".repeat(1000)))
        .collect();
    record.append_batch(history.clone()).await.unwrap();
    let mut conversation = Conversation::of(history);
    let checkpoint_window: Vec<_> = (0..512)
        .map(|n| full(&format!("old-{n}"), n, &"x".repeat(250)))
        .collect();
    record
        .compact(
            &checkpoint_window,
            Estimate {
                messages: checkpoint_window.len(),
                tokens: 90_000,
            },
        )
        .await
        .unwrap();
    conversation.compact(
        checkpoint_window,
        Estimate {
            messages: 512,
            tokens: 90_000,
        },
    );
    let mut expected = 90_000;
    let mut boundary = 512;
    for positions in [vec![1], vec![0, 0], vec![1]] {
        let repairs: Vec<_> = positions
            .into_iter()
            .map(|at| {
                (
                    at,
                    result(
                        "repair",
                        None,
                        "interrupted; effects unknown",
                        Completeness::Complete,
                        0,
                    ),
                )
            })
            .collect();
        expected += repairs
            .iter()
            .map(|(_, message)| estimate(message))
            .sum::<u64>();
        boundary += repairs.len();
        record
            .repair(repairs.clone(), repairs.clone())
            .await
            .unwrap();
        conversation.repair(repairs.clone(), repairs);
        (record, conversation) = reopen(&sessions, &id, record, &conversation, expected);
        assert_eq!(
            tokens(conversation.context(), "instructions"),
            expected,
            "repair accounting retains the checkpoint anchor, not a fresh content estimate"
        );
    }
    for at in [boundary, boundary + 1] {
        let inserted = result("suffix", None, "done", Completeness::Complete, 0);
        expected += estimate(&inserted);
        let repairs = vec![(at, inserted)];
        record
            .repair(repairs.clone(), repairs.clone())
            .await
            .unwrap();
        conversation.repair(repairs.clone(), repairs);
        (record, conversation) = reopen(&sessions, &id, record, &conversation, expected);
    }

    let mut fresh = step("fresh", "print(1)", None, None);
    let Message::Assistant(assistant) = &mut fresh else {
        unreachable!()
    };
    assistant.usage = Some(usage(70_000, None));
    record.append(&fresh).await.unwrap();
    conversation.push(fresh);
    (record, conversation) = reopen(&sessions, &id, record, &conversation, 70_010);
    for at in [0, boundary + 1] {
        let inserted = result(
            "later-repair",
            None,
            "interrupted",
            Completeness::Complete,
            0,
        );
        expected += estimate(&inserted);
        let repairs = vec![(at, inserted)];
        record
            .repair(repairs.clone(), repairs.clone())
            .await
            .unwrap();
        conversation.repair(repairs.clone(), repairs);
        if at < boundary {
            boundary += 1;
        }
    }
    let fresh_cost = estimate(conversation.context().messages().last().unwrap());
    expected += fresh_cost;
    (record, conversation) = reopen(&sessions, &id, record, &conversation, expected);
    // This test adds results without matching tool calls. Save directly here;
    // the summarizer would reject that conversation.
    let window = conversation.context().messages().to_vec();
    let checkpoint = Estimate {
        messages: window.len(),
        tokens: expected,
    };
    record.compact(&window, checkpoint).await.unwrap();
    conversation.compact(window, checkpoint);
    (record, conversation) = reopen(&sessions, &id, record, &conversation, expected);
    let inserted = Message::Instruction("one more repair".into());
    expected += estimate(&inserted);
    let repairs = vec![(1, inserted)];
    record
        .repair(repairs.clone(), repairs.clone())
        .await
        .unwrap();
    conversation.repair(repairs.clone(), repairs);
    let _ = reopen(&sessions, &id, record, &conversation, expected);
}

#[test]
fn incremental_append_pricing_matches_the_context_estimator() {
    let mut conversation = Conversation::of(vec![Message::User("history".repeat(1000).into())]);
    let instructions = "instructions";
    let mut priced = tokens(conversation.context(), instructions);
    let mut reported = step("report", "print(1)", None, None);
    let Message::Assistant(assistant) = &mut reported else {
        unreachable!()
    };
    assistant.usage = Some(usage(100_000, None));
    let messages = [
        step("a", "print(1)", None, None),
        full("a", 1, &"x".repeat(32_000)),
        step("b", "print(2)", None, None),
        full("b", 2, &"y".repeat(32_000)),
        reported,
        full("report", 3, &"z".repeat(32_000)),
        step("last", "print(2)", None, None),
        full("last", 4, "2"),
    ];
    for message in messages {
        appended(&mut priced, std::slice::from_ref(&message));
        conversation.push(message);
        assert_eq!(priced, tokens(conversation.context(), instructions));
    }
    let reduced = synthetic_checkpoint(instructions);
    conversation.compact(reduced.window, reduced.estimate);
    priced = tokens(conversation.context(), instructions);
    let message = Message::User("after compact".into());
    appended(&mut priced, std::slice::from_ref(&message));
    conversation.push(message);
    assert_eq!(priced, tokens(conversation.context(), instructions));
}
