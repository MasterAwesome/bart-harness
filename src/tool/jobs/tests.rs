use super::*;

#[tokio::test(start_paused = true)]
async fn cleanup_deadline_does_not_claim_reserved_scopes_stopped() {
    let jobs = Arc::new(Jobs::after(0));
    let reserved = jobs.reserve("unsettled scope").unwrap();
    jobs.stop_all();
    assert!(reserved.cancel.is_cancelled());
    let cleanup = jobs.settled().await;
    assert_eq!(cleanup, Cleanup::Unconfirmed);
    assert!(cleanup.notice().contains("some may remain"));
    drop(reserved);
    assert_eq!(jobs.settled().await, Cleanup::Settled);
}
