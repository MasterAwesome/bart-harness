use std::{fs, os::unix::fs::PermissionsExt};

use serde_json::json;

use super::*;

impl Recorder {
    pub(crate) fn append(&self, message: &Message) -> PendingWrite {
        self.append_batch(vec![message.clone()])
    }

    pub(crate) fn read_only(path: &Path) -> Self {
        Self::from_writer(
            Writer::lock(File::open(path).unwrap()).unwrap(),
            String::new(),
            PathBuf::new(),
        )
    }
}

use crate::{
    history::{Assistant, Content, Part, ToolCall, ToolResult},
    tool::test_support::TempDir,
};

fn meta(cwd: &str, model: &str) -> Meta {
    Meta {
        cwd: cwd.into(),
        provider: "chatgpt".into(),
        model: model.into(),
        effort: None,
    }
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[tokio::test]
async fn records_privately_and_resumes_the_same_history() {
    let dir = TempDir::new("record");
    let sessions = Sessions::new(&dir.path().join("sessions"), Path::new("/work"));
    let (id, recorder) = sessions.create(&meta("/work", "m"), "").unwrap();
    let path = sessions.dir(&id).join(RECORD);
    let call = ToolCall {
        id: "c1".into(),
        name: "read".into(),
        arguments: json!({ "file_path": "a" }).into(),
    };
    let messages = vec![
        Message::User("hi".to_owned().into()),
        Message::Assistant(Assistant {
            protocol: crate::provider::Protocol::Chat,
            provider: "chatgpt".into(),
            model: "m".into(),
            parts: vec![Part {
                content: Content::ToolCall(call),
                replay: Some(json!({ "type": "function_call", "encrypted": "x" }).into()),
            }],
            usage: None,
            continues: false,
            refused: false,
            served_model: None,
        }),
        Message::ToolResult(ToolResult {
            activities: Default::default(),
            diffs: Vec::new(),
            notes: String::new(),
            call_id: "c1".into(),
            output: "text".to_owned().into(),
            is_error: false,
            jobs: vec![3],
            saved: Some(crate::history::SavedArtifact {
                path: dir.path().join("partial.log"),
                completeness: crate::history::Completeness::Incomplete(
                    crate::history::ArtifactOmission::FileLimit,
                ),
                chars: 0,
            }),
            cell: None,
        }),
    ];
    for message in &messages {
        recorder.append(message).await.unwrap();
    }
    assert!(
        sessions.resume(&id).is_err(),
        "a record open in one process is not continued by another"
    );
    drop(recorder);
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&path), mode(path.parent().unwrap())), (0o600, 0o700));
    assert!(
        Writer::create(&path, &meta("/work", "m"), "").is_err(),
        "an existing record is never overwritten"
    );

    append_bytes(&path, b"{\"message\": {\"role\": \"us");
    let resumed = sessions.resume(&id).unwrap();
    resumed
        .record
        .describe(&meta("/work", "other"))
        .await
        .unwrap();
    assert_eq!(resumed.previous, meta("/work", "m"));
    assert_eq!(
        resumed.conversation.history(),
        messages,
        "the history, provider items included, comes back as it was"
    );
    resumed
        .record
        .append(&Message::User("again".to_owned().into()))
        .await
        .unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(
        resumed.previous.model, "other",
        "the latest session line wins"
    );
    assert_eq!(resumed.conversation.history().len(), 4);
}

#[test]
fn torn_tails_agree_across_consumers_and_complete_lines_must_parse() {
    let dir = TempDir::new("record-tails");
    let sessions = Sessions::new(dir.path(), Path::new("/work"));
    let user =
        "{\"event\":\"messages\",\"data\":[{\"role\":\"user\",\"content\":\"x\"}],\"time_ms\":0}";
    for (appended, kept) in [
        (format!("not json\n{user}\n").into_bytes(), None),
        (format!("{user}\nnot json\n").into_bytes(), None),
        (format!("{user}\n{user}").into_bytes(), Some(1)),
        (b"\xe2\x82".to_vec(), Some(0)),
        (
            [format!("{user}\n").as_bytes(), b"\xe2\x82"].concat(),
            Some(1),
        ),
        (
            format!("{user}\n{}", &user[..user.len() - 2]).into_bytes(),
            Some(1),
        ),
    ] {
        let (id, record) = sessions.create(&meta("/work", "m"), "").unwrap();
        drop(record);
        let path = sessions.dir(&id).join(RECORD);
        let complete = fs::metadata(&path).unwrap().len()
            + appended
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |at| at as u64 + 1);
        append_bytes(&path, &appended);
        if kept.is_some() {
            let listed = sessions.list_all().unwrap();
            assert!(
                listed
                    .iter()
                    .find(|summary| summary.id == id)
                    .unwrap()
                    .description
                    .is_ok()
            );
        }
        let resumed = sessions.resume(&id);
        assert_eq!(
            resumed
                .as_ref()
                .ok()
                .map(|resumed| resumed.conversation.history().len()),
            kept,
            "{appended:?}"
        );
        if let Some(kept) = kept {
            drop(resumed);
            assert_eq!(fs::metadata(&path).unwrap().len(), complete);
            assert_eq!(sessions.delete_if_empty(&id).unwrap(), kept != 0);
            assert_eq!(path.exists(), kept != 0);
        }
    }
}

#[test]
fn only_the_current_format_is_read_and_other_records_are_left_unchanged() {
    let dir = TempDir::new("record-format");
    let sessions = Sessions::new(dir.path(), Path::new("/work"));
    let (id, record) = sessions.create(&meta("/work", "m"), "").unwrap();
    drop(record);
    let path = sessions.dir(&id).join(RECORD);
    let header = fs::read_to_string(&path).unwrap();
    let user = r#"{"event":"messages","data":[{"role":"user","content":"x"}],"time_ms":0}"#;
    for contents in [
        header.replace("\"version\":1", "\"version\":2"),
        header.replace("\"version\":1,", ""),
        format!("{user}\n"),
        format!("{header}{header}"),
    ] {
        let torn = format!("{contents}{{\"event\"");
        fs::write(&path, &torn).unwrap();
        assert!(sessions.resume(&id).is_err(), "{contents}");
        let listed = sessions.list_all().unwrap();
        let summary = listed.iter().find(|summary| summary.id == id).unwrap();
        assert!(summary.description.is_err(), "{contents}");
        assert_eq!(fs::read_to_string(&path).unwrap(), torn, "{contents}");
    }
}

#[tokio::test]
async fn rewind_restores_the_latest_checkpoint_before_the_retained_history() {
    let dir = TempDir::new("compact-record");
    let sessions = Sessions::new(&dir.path().join("sessions"), Path::new("/work"));
    let (id, record) = sessions.create(&meta("/project", "m"), "").unwrap();
    let first = Message::User("Original constraint".to_owned().into());
    let next = Message::User("Continue".to_owned().into());
    record.append(&first).await.unwrap();
    let estimate = |messages, tokens| crate::conversation::Estimate { messages, tokens };
    record
        .compact(
            &[
                first.clone(),
                Message::Notification(crate::history::Notification {
                    text: "first handoff".to_owned().into(),
                    kind: crate::history::NotificationKind::Recovery,
                }),
            ],
            estimate(2, 900),
        )
        .await
        .unwrap();
    let result: Message = serde_json::from_value(json!({"role":"tool_result", "content":{
        "call_id":"c", "output":"original output", "is_error":false
    }}))
    .unwrap();
    record.append(&result).await.unwrap();
    record.append(&next).await.unwrap();
    let mut masked = result.clone();
    let Message::ToolResult(stub) = &mut masked else {
        panic!()
    };
    stub.output = "masked output".to_owned().into();
    record
        .compact(
            &[first.clone(), masked.clone(), next.clone()],
            estimate(3, 700),
        )
        .await
        .unwrap();
    drop(record);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(
        resumed.conversation.history(),
        [first.clone(), result.clone(), next.clone()]
    );
    assert_eq!(
        resumed.conversation.context().messages(),
        [first.clone(), masked, next]
    );

    let (Message::User(original), Message::User(selected)) = (
        &resumed.conversation.history()[0],
        &resumed.conversation.context().messages()[0],
    ) else {
        panic!("user message changed")
    };
    assert!(
        Arc::ptr_eq(original, selected),
        "replay shares equal immutable payloads"
    );
    resumed.record.rewind(2).await.unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(
        resumed.conversation.context().messages(),
        [
            first.clone(),
            Message::Notification(crate::history::Notification {
                text: "first handoff".into(),
                kind: crate::history::NotificationKind::Recovery,
            }),
            result,
        ],
        "rewinding across the newer summary restores the earlier checkpoint and original suffix"
    );
    let repair = Message::Instruction("repair after rewind".into());
    resumed
        .record
        .repair(vec![(2, repair.clone())], vec![(3, repair.clone())])
        .await
        .unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(
        resumed.conversation.context().messages().last(),
        Some(&repair)
    );
    assert_eq!(resumed.conversation.context().messages().len(), 4);
    resumed.record.rewind(1).await.unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history(), std::slice::from_ref(&first));
    assert_eq!(
        resumed.conversation.context().messages(),
        [
            first.clone(),
            Message::Notification(crate::history::Notification {
                text: "first handoff".into(),
                kind: crate::history::NotificationKind::Recovery,
            })
        ]
    );
    resumed
        .record
        .append(&Message::User("again".to_owned().into()))
        .await
        .unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    let kept = [first, Message::User("again".to_owned().into())];
    assert_eq!(resumed.conversation.history(), kept);
    assert!(resumed.record.rewind(3).await.is_err());
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history(), kept);
    assert_eq!(resumed.conversation.context().messages().len(), 3);
    resumed.record.rewind(0).await.unwrap();
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert!(resumed.conversation.history().is_empty());
    assert!(resumed.conversation.context().messages().is_empty());
}

#[tokio::test]
async fn a_resumed_rewind_keeps_only_reviews_of_surviving_calls() {
    let dir = TempDir::new("rewind-reviews");
    let sessions = Sessions::new(&dir.path().join("sessions"), Path::new("/work"));
    let (id, record) = sessions.create(&meta("/project", "m"), "").unwrap();
    record
        .append(&Message::User("first".to_owned().into()))
        .await
        .unwrap();
    let calls = |id: &str| {
        Message::Assistant(crate::history::Assistant {
            protocol: crate::provider::Protocol::Chat,
            provider: "p".into(),
            model: "m".into(),
            parts: vec![crate::history::Part {
                content: crate::history::Content::ToolCall(crate::history::ToolCall {
                    id: id.into(),
                    name: "python".into(),
                    arguments: json!({"code": "1"}).into(),
                }),
                replay: None,
            }],
            usage: None,
            continues: false,
            refused: false,
            served_model: None,
        })
    };
    let review = |id: &str| {
        serde_json::from_value(
            json!({"call_id": id, "decision": "unavailable", "reason": "offline"}),
        )
        .unwrap()
    };
    let later = 2;
    record.append(&calls("kept")).await.unwrap();
    record.review(&review("kept")).await.unwrap();
    let message = Message::User("later".to_owned().into());
    record.append(&message).await.unwrap();
    record.append(&calls("dropped")).await.unwrap();
    record.review(&review("dropped")).await.unwrap();
    record.rewind(later).await.unwrap();
    drop(record);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.history().len(), later);
    let reviews: Vec<_> = resumed
        .conversation
        .reviews()
        .iter()
        .map(crate::approval::Review::call_id)
        .collect();
    assert_eq!(
        reviews,
        ["kept"],
        "a rewind drops reviews of the calls it drops"
    );
}

#[tokio::test]
async fn replay_refuses_out_of_context_accounting_and_repair_positions_without_mutating_the_record()
{
    let dir = TempDir::new("invalid-context-record");
    let sessions = Sessions::new(dir.path(), Path::new("/work"));
    let (id, record) = sessions.create(&meta("/work", "m"), "").unwrap();
    let user = Message::User("kept".into());
    record.append(&user).await.unwrap();
    record
        .compact(
            std::slice::from_ref(&user),
            crate::conversation::Estimate {
                messages: 1,
                tokens: 3,
            },
        )
        .await
        .unwrap();
    drop(record);
    let path = sessions.dir(&id).join(RECORD);
    let prefix = fs::read_to_string(&path).unwrap();
    for entries in [
        vec![Entry::Compact {
            window: vec![user.clone()],
            estimate: crate::conversation::Estimate {
                messages: 2,
                tokens: 3,
            },
        }],
        vec![Entry::Repair {
            history: vec![(2, user.clone())],
            window: vec![],
        }],
        vec![Entry::Repair {
            history: vec![],
            window: vec![(2, user.clone())],
        }],
        vec![
            Entry::Rewind(0),
            Entry::Repair {
                history: vec![],
                window: vec![(0, user.clone())],
            },
        ],
    ] {
        let mut bytes = prefix.clone();
        for entry in entries {
            bytes.push_str(&serde_json::to_string(&Timed { entry, time_ms: 0 }).unwrap());
            bytes.push('\n');
        }
        fs::write(&path, &bytes).unwrap();
        assert!(sessions.resume(&id).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn retained_usage_survives_compaction_and_replay_but_rewinds_with_history() {
    use crate::conversation::{Conversation, Estimate};
    use crate::history::Usage;
    let dir = TempDir::new("retained-usage");
    let sessions = Sessions::new(dir.path(), Path::new("/work"));
    let (id, record) = sessions.create(&meta("/work", "m"), "").unwrap();
    let mut conversation = Conversation::default();
    let response = |usage| {
        Message::Assistant(Assistant {
            protocol: crate::provider::Protocol::Chat,
            provider: "p".into(),
            model: "m".into(),
            parts: Vec::new(),
            usage,
            continues: false,
            refused: false,
            served_model: None,
        })
    };
    let usage = Usage {
        input_tokens: 911_348,
        output_tokens: 42,
        cost: Some(1.5),
        cost_is_estimate: false,
        cache_read_tokens: Some(900_000),
        cache_write_tokens: None,
        reasoning_tokens: None,
    };
    for message in [Message::User("first".into()), response(Some(usage))] {
        record.append(&message).await.unwrap();
        conversation.push(message);
    }
    let original = conversation.usage();
    assert_eq!(original.output_tokens, Some(42));
    assert_eq!(original.cost, Some(1.5));
    let masked = vec![Message::Instruction("masked history".into())];
    let checkpoint = Estimate {
        messages: 1,
        tokens: 100,
    };
    record.compact(&masked, checkpoint).await.unwrap();
    conversation.compact(masked, checkpoint);
    assert_eq!(conversation.usage(), original);
    for message in [Message::User("second".into()), response(None)] {
        record.append(&message).await.unwrap();
        conversation.push(message);
    }
    assert!(
        conversation.usage().last.is_none(),
        "missing usage must not leave a stale cache ratio"
    );
    assert_eq!(conversation.usage().output_tokens, original.output_tokens);
    drop(record);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.usage(), conversation.usage());
    resumed.record.rewind(2).await.unwrap();
    conversation.rewind(2);
    assert!(
        matches!(conversation.context().anchor(), crate::conversation::Anchor::Checkpoint(estimate) if estimate.tokens == 100)
    );
    assert_eq!(conversation.usage(), original);
    drop(resumed);
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.conversation.usage(), original);
    resumed.record.rewind(1).await.unwrap();
    drop(resumed);
    assert_eq!(
        sessions.resume(&id).unwrap().conversation.usage(),
        Default::default()
    );
}

#[tokio::test]
async fn automatic_names_use_the_highest_project_suffix() {
    let dir = TempDir::new("automatic-name");
    let sessions = Sessions::new(dir.path(), Path::new("/work"));
    let (_, current) = sessions.create(&meta("/work", "m"), "").unwrap();
    assert_eq!(
        current.automatic_name("review-fold").await.unwrap(),
        "review-fold"
    );
    let mut records = Vec::new();
    for name in [
        "review-fold",
        "review-fold-2",
        "review-fold-7",
        "review-fold-other",
    ] {
        let (_, record) = sessions.create(&meta("/work", "m"), "").unwrap();
        record.rename(name).await.unwrap();
        records.push(record);
    }
    assert_eq!(
        current.automatic_name("review-fold").await.unwrap(),
        "review-fold-8"
    );
    let elsewhere = Sessions::new(dir.path(), Path::new("/elsewhere"));
    let (_, other) = elsewhere.create(&meta("/elsewhere", "m"), "").unwrap();
    assert_eq!(
        other.automatic_name("review-fold").await.unwrap(),
        "review-fold"
    );
}
