use super::*;

#[tokio::test(start_paused = true)]
async fn cleanup_deadline_does_not_claim_reserved_scopes_stopped() {
    let jobs = Arc::new(Jobs::after(0));
    let reserved = jobs
        .reserve(&super::super::execution::Argv::parse(vec!["unsettled scope".into()]).unwrap())
        .unwrap();
    jobs.stop_all();
    assert!(reserved.cancel.is_cancelled());
    let cleanup = jobs.settled().await;
    assert_eq!(cleanup, Cleanup::Unconfirmed);
    assert!(cleanup.notice().contains("some may remain"));
    drop(reserved);
    assert_eq!(jobs.settled().await, Cleanup::Settled);
}

#[tokio::test]
async fn finished_jobs_keep_their_result_when_background_jobs_are_released() {
    use crate::tool::{
        Confinement, Escape,
        process::Launch,
        test_support::{TempDir, context},
    };
    let dir = TempDir::new("finished-job-handoff");
    let context = context(dir.path(), Confinement::Unrestricted, 1000);
    let request = Request::parse(
        serde_json::json!({"argv": ["sh", "-c", "printf finished; exit 7"]}),
        Escape::Withheld,
    )
    .unwrap();
    let started = context
        .jobs
        .start_command(request, &context, Launch::Session)
        .await
        .unwrap();
    assert_eq!(context.jobs.settled().await, Cleanup::Settled);
    context.jobs.keep_background().await.unwrap();
    assert_eq!(
        context.jobs.snapshot(started.id, &context).await.unwrap()["return_code"],
        7
    );
    let notices = context.jobs.take_notifications();
    assert!(notices.iter().any(|notice| matches!(notice.kind,
        NotificationKind::JobFinished { id, .. } if id == started.id)));
    assert!(
        !notices
            .iter()
            .any(|notice| matches!(notice.kind, NotificationKind::JobDetached { .. }))
    );
    let path = dir.path().join(format!("spill/job-{}.stdout", started.id));
    assert_eq!(std::fs::read(path).unwrap(), b"finished");
}

#[tokio::test]
async fn running_jobs_are_detached_without_a_completion_notification() {
    use crate::tool::{
        Confinement, Escape,
        process::Launch,
        test_support::{TempDir, context},
    };
    let dir = TempDir::new("running-job-handoff");
    let context = context(dir.path(), Confinement::Unrestricted, 1000);
    let request = Request::parse(
        serde_json::json!({"argv": ["sh", "-c", "while [ ! -e release ]; do sleep .01; done; echo done > finished"]}),
        Escape::Withheld,
    ).unwrap();
    let started = context
        .jobs
        .start_command(request, &context, Launch::Session)
        .await
        .unwrap();
    let result = context.jobs.keep_background().await;
    std::fs::write(dir.path().join("release"), "").unwrap();
    result.unwrap();
    assert_eq!(context.jobs.running_count(), 0);
    let notices = context.jobs.take_notifications();
    assert!(notices.iter().any(|notice| matches!(notice.kind,
        NotificationKind::JobDetached { id } if id == started.id)));
    assert!(
        !notices
            .iter()
            .any(|notice| matches!(notice.kind, NotificationKind::JobFinished { .. }))
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !dir.path().join("finished").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
