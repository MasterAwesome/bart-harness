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
async fn a_one_turn_session_stops_what_its_commands_left_running() {
    for foreground in [false, true] {
        let dir = TempDir::new("headless-retained");
        let helper = if foreground { "run" } else { "bg_run" };
        let command = "(while [ ! -e release ]; do sleep .01; done) >/dev/null 2>&1 & echo $! > daemon; exit 0";
        let code = format!(
            "{helper}([\"bash\",\"-c\",{command:?}])\nimport os, time\nwhile not os.path.exists('daemon'): time.sleep(.01)"
        );
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
                Mode::Prompt("build".into()),
                Terminations::listen().unwrap(),
            ),
        )
        .await;
        assert!(matches!(result, Ok(Ok(()))), "{result:?}");
        let pid = std::fs::read_to_string(dir.path().join("daemon")).unwrap();
        let dead = tokio::time::timeout(Duration::from_secs(5), wait_until_dead(pid.trim())).await;
        jobs.stop_all();
        jobs.settled().await;
        assert!(dead.is_ok(), "the session left pid {pid} running");
        assert!(!dir.path().join("release").exists());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let shown = serde_json::to_string(&*requests).unwrap();
        assert!(
            !foreground || shown.contains("left processes running as job["),
            "{shown}"
        );
    }
}
