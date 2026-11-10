use super::*;
use crate::{
    agent::Settings,
    preferences::Current,
    provider::mock::{Reply, serve},
    tool::{
        Confinement,
        test_support::{TempDir, context, wait_until_dead},
    },
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn headless_finishes_after_root_exit_and_cleans_retained_descendants() {
    for foreground in [false, true] {
        let dir = TempDir::new("headless-retained");
        let helper = if foreground { "run" } else { "bg_run" };
        let command = "(while [ ! -e release ]; do sleep .01; done) >/dev/null 2>&1 & echo $! > daemon; exit 0";
        let code = format!("{helper}([\"bash\",\"-c\",{command:?}])");
        let (provider, requests) = serve(
            dir.path(),
            vec![
                Reply::python("start", &json!({"code":code})),
                Reply::text("done"),
                Reply::text("done"),
            ],
        )
        .await;
        let session = Session::new(
            reqwest::Client::new(),
            crate::auth::Store::new(dir.path().join("auth")),
            Settings {
                provider,
                model: "m".into(),
                reasoning_effort: None,
            },
            Current::default(),
            context(dir.path(), Confinement::Unrestricted, 1000),
            "fixture".into(),
            None,
        );
        let jobs = session.jobs();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run(
                session,
                Some("build".into()),
                Terminations::listen().unwrap(),
            ),
        )
        .await;
        jobs.stop_all();
        jobs.settled().await;
        assert!(matches!(result, Ok(Ok(()))), "{result:?}");
        let pid = std::fs::read_to_string(dir.path().join("daemon")).unwrap();
        wait_until_dead(pid.trim()).await;
        assert!(!dir.path().join("release").exists());
        let requests = requests.lock().unwrap();
        assert!((2..=3).contains(&requests.len()));
        let shown = serde_json::to_string(&*requests).unwrap();
        assert!(
            shown.contains(if foreground {
                "left processes running as job["
            } else {
                "its child processes still run"
            }),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn headless_yield_still_waits_for_an_active_command_and_delivers_its_result() {
    let dir = TempDir::new("headless-await-command");
    let command = "echo started > ready; while [ ! -e release ]; do sleep .01; done; echo actual-result > result";
    let (provider, requests) = serve(
        dir.path(),
        vec![
            Reply::python(
                "start",
                &json!({"code":format!("bg_run([\"bash\", \"-c\", {command:?}])")}),
            ),
            Reply::text("waiting"),
            Reply::text("delivered"),
        ],
    )
    .await;
    let session = Session::new(
        reqwest::Client::new(),
        crate::auth::Store::new(dir.path().join("auth")),
        Settings {
            provider,
            model: "m".into(),
            reasoning_effort: None,
        },
        Current::default(),
        context(dir.path(), Confinement::Unrestricted, 1000),
        "fixture".into(),
        None,
    );
    let jobs = session.jobs();
    let mut pending = Box::pin(run(
        session,
        Some("work".into()),
        Terminations::listen().unwrap(),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut pending => panic!("exited while command was pending: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            if requests.lock().unwrap().len() == 2 && dir.path().join("ready").exists() {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!dir.path().join("result").exists());
    std::fs::write(dir.path().join("release"), "").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), pending).await;
    jobs.stop_all();
    jobs.settled().await;
    assert!(matches!(result, Ok(Ok(()))), "{result:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result")).unwrap(),
        "actual-result\n"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].to_string().contains("[Bart job: job["));
}
