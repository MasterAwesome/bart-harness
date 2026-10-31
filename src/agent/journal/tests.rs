use super::*;
use crate::{
    history::{Assistant, Content, NotificationKind, Part, ToolCall},
    provider::Protocol,
    record::Sessions,
    tool::test_support::{TempDir, blocked_worker},
};
use futures_util::FutureExt as _;

fn meta() -> Meta {
    Meta {
        cwd: "/work".into(),
        provider: "mock".into(),
        model: "m".into(),
        effort: None,
    }
}

fn orphan() -> Message {
    Message::Assistant(Assistant {
        protocol: Protocol::Responses,
        provider: "mock".into(),
        model: "m".into(),
        parts: vec![Part {
            content: Content::ToolCall(ToolCall {
                id: "call".into(),
                name: "python".into(),
                arguments: serde_json::json!({"code":"print(1)"}).into(),
            }),
            replay: None,
        }],
        usage: None,
        continues: false,
        refused: false,
        served_model: None,
    })
}

fn review() -> Review {
    serde_json::from_value(
        serde_json::json!({"decision":"unavailable", "call_id":"call", "reason":"fixture"}),
    )
    .unwrap()
}

fn closing_note() -> Notification {
    Notification {
        text: "stopped".into(),
        kind: NotificationKind::JobLost { id: 7 },
    }
}

#[tokio::test]
async fn consecutive_mutations_persist_in_order_without_preparatory_flushes() {
    let dir = TempDir::new("journal-order");
    let sessions = Sessions::new(dir.path(), std::path::Path::new("/work"));
    let (id, record) = sessions.create(&meta(), "").unwrap();
    let mut journal = Journal::new(Some(record));
    journal
        .append(vec![
            Message::User("first".into()),
            orphan(),
            Message::User("discard".into()),
        ])
        .await;
    journal
        .checkpoint(
            journal.history().to_vec(),
            Estimate {
                messages: 3,
                tokens: 9000,
            },
        )
        .await;
    journal.recover("unknown effects").await;
    journal.review(&review()).await;
    journal.rename("before rewind".into()).await;
    let mut changed = meta();
    changed.model = "changed".into();
    journal.describe(&changed).await;
    assert_eq!(journal.rewind(3).await.unwrap().as_ref(), "discard");
    journal.append(vec![Message::User("kept".into())]).await;
    journal.rename("final".into()).await;
    journal.finish(|| vec![closing_note()]).await;
    assert!(journal.recording_warning().is_none());
    let resumed = sessions.resume(&id).unwrap();
    assert_eq!(resumed.name.as_deref(), Some("final"));
    assert_eq!(resumed.previous, changed);
    assert_eq!(resumed.conversation.history(), journal.history());
    assert_eq!(
        resumed.conversation.context().messages(),
        journal.model_context().messages()
    );
    assert_eq!(resumed.conversation.reviews(), journal.reviews());
    assert_eq!(resumed.conversation.reviews().len(), 1);
    assert_eq!(
        resumed
            .conversation
            .history()
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count(),
        1
    );
}

#[test]
fn cancellation_before_and_after_staging_retains_admissions_recovery_and_close() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = TempDir::new("journal-cancel");
        let sessions = Sessions::new(dir.path(), std::path::Path::new("/work"));
        let (id, record) = sessions.create(&meta(), "").unwrap();
        let mut journal = Journal::new(Some(record));
        let worker = blocked_worker().await;
        assert!(
            journal
                .append(vec![Message::User("first".into()), orphan()])
                .now_or_never()
                .is_some(),
            "admission cannot wait for its own write"
        );
        assert!(journal.recover("interrupted").now_or_never().is_none());
        assert!(journal.rename("not staged".into()).now_or_never().is_none());
        assert!(
            journal
                .append(vec![Message::User("not staged".into())])
                .now_or_never()
                .is_none()
        );
        assert_eq!(journal.history().len(), 2);
        assert!(journal.name().is_none());
        worker.release().await;
        journal.recover("interrupted").await;
        journal.settle_recording().await;
        let worker = blocked_worker().await;
        assert!(
            journal.review(&review()).now_or_never().is_some(),
            "completed review staging must not await its own write"
        );
        assert!(journal.settle_recording().now_or_never().is_none());
        worker.release().await;
        journal.append(vec![Message::User("second".into())]).await;
        journal.settle_recording().await;

        let worker = blocked_worker().await;
        let mut notes = vec![closing_note()];
        assert!(journal.rename("staged".into()).now_or_never().is_some());
        assert!(
            journal
                .finish(|| std::mem::take(&mut notes))
                .now_or_never()
                .is_none()
        );
        assert_eq!(
            notes.len(),
            1,
            "a pending earlier write cannot consume close notifications"
        );
        worker.release().await;
        journal.settle_recording().await;
        let worker = blocked_worker().await;
        assert!(
            journal
                .finish(|| std::mem::take(&mut notes))
                .now_or_never()
                .is_none()
        );
        assert_eq!(notes, []);
        assert_eq!(
            journal
                .history()
                .iter()
                .filter(|m| matches!(m, Message::Notification(_)))
                .count(),
            1
        );
        assert!(
            sessions.resume(&id).is_err(),
            "canceled close must retain the writer"
        );
        worker.release().await;
        journal.finish(|| std::mem::take(&mut notes)).await;
        let resumed = sessions.resume(&id).unwrap();
        assert_eq!(resumed.name.as_deref(), Some("staged"));
        assert_eq!(resumed.conversation.history(), journal.history());
        assert_eq!(resumed.conversation.reviews().len(), 1);
        assert_eq!(
            resumed
                .conversation
                .history()
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count(),
            1
        );
    });
}

#[tokio::test]
async fn a_failure_discovered_inside_a_canceled_mutation_remains_reportable_once() {
    let dir = TempDir::new("journal-fault");
    let path = dir.path().join("read-only-record");
    std::fs::write(&path, "").unwrap();
    let mut journal = Journal::new(Some(Recorder::read_only(&path)));
    journal
        .append(vec![Message::User("first".into()), orphan()])
        .await;
    let (staged, ready) = tokio::sync::oneshot::channel();
    {
        let canceled = async {
            journal.rename("staged after failure".into()).await;
            staged.send(()).unwrap();
            std::future::pending::<()>().await;
        };
        tokio::pin!(canceled);
        tokio::select! { () = &mut canceled => panic!("must be canceled"), _ = ready => {} }
    }
    assert_eq!(journal.name(), Some("staged after failure"));
    assert!(journal.recording_warning().is_some());
    journal.recover("interrupted").await;
    journal.review(&review()).await;
    assert!(journal.take_recording_notice().is_some());
    assert!(journal.take_recording_notice().is_none());
    journal.append(vec![Message::User("discard".into())]).await;
    journal.rewind(3).await.unwrap();
    journal.rename("still mutable".into()).await;
    journal.finish(Vec::new).await;
    assert_eq!(journal.history().len(), 3);
    assert_eq!(journal.name(), Some("still mutable"));
    assert!(
        journal.recording_warning().is_some(),
        "closing preserves fault status"
    );
    assert!(journal.take_recording_notice().is_none());
    assert_eq!(std::fs::read(path).unwrap(), b"");
}

#[test]
fn canceling_approval_before_prior_persistence_finishes_never_loses_a_completed_review() {
    use crate::{
        approval::{Approval, Gate, Notice, Reviewer},
        provider::mock::{Reply, serve_catalog},
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let dir = TempDir::new("approval-journal-order");
        let (_, requests, _, _) = serve_catalog(dir.path(), vec![Reply::text(r#"{"constraint_conflict":false,"risk_level":"low","user_authorization":"high","outcome":"allow","rationale":"routine"}"#)], serde_json::json!({"models":[{"slug":"codex-auto-review","visibility":"hide"}]})).await;
        let client = reqwest::Client::new();
        let store = crate::auth::Store::new(dir.path().join("auth.json"));
        let mut approval = Approval::default();
        approval.auto(crate::choice::Choice { provider: "mock".into(), model: "codex-auto-review".into(), effort: None });
        let sessions = Sessions::new(&dir.path().join("records"), dir.path());
        let (id, record) = sessions.create(&meta(), "").unwrap();
        let mut journal = Journal::new(Some(record));
        journal.append(vec![Message::User("do the task".into())]).await;
        let worker = blocked_worker().await;
        let mut reviewer = Reviewer::new("fixture");
        let mut reviews = 0;
        let mut report = |notice: Notice<'_>| { reviews += usize::from(matches!(notice, Notice::Review(_))); };
        {
            let mut gate = Gate { client: &client, store: &store, state: &mut reviewer, journal: &mut journal, interactive: false, report: &mut report, stopped: false, approval: &approval };
            assert!(gate.check("call", serde_json::json!({"code":"print(1)"}), "permission").now_or_never().is_none());
            assert!(requests.lock().unwrap().is_empty(), "settle before obtaining a verdict that cannot be discarded");
            assert!(gate.journal.reviews().is_empty());
            worker.release().await;
            gate.check("call", serde_json::json!({"code":"print(1)"}), "permission").await.unwrap();
        }
        assert_eq!(reviews, 1);
        assert_eq!(requests.lock().unwrap().len(), 1);
        journal.finish(Vec::new).await;
        let resumed = sessions.resume(&id).unwrap();
        assert_eq!(resumed.conversation.reviews(), journal.reviews());
        assert_eq!(resumed.conversation.reviews().len(), 1);
    });
}
