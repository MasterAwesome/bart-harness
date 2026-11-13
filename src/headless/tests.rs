use super::*;
use crate::{
    agent::Settings,
    preferences::Current,
    provider::mock::{Reply, serve},
    tool::{
        BackgroundJobs, Confinement,
        test_support::{TempDir, context, wait_until_dead},
    },
};
use serde_json::json;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Helper {
    Run,
    Background,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalReply {
    Succeeds,
    Fails,
}

#[tokio::test]
async fn prompt_exit_stops_background_jobs() {
    check_prompt_lifetime(
        Helper::Background,
        BackgroundJobs::Stop,
        FinalReply::Succeeds,
    )
    .await;
}

#[tokio::test]
async fn prompt_exit_stops_adopted_descendants() {
    check_prompt_lifetime(Helper::Run, BackgroundJobs::Stop, FinalReply::Succeeds).await;
}

#[tokio::test]
async fn keep_prompt_releases_background_jobs_and_python() {
    check_prompt_lifetime(
        Helper::Background,
        BackgroundJobs::Keep,
        FinalReply::Succeeds,
    )
    .await;
}

#[tokio::test]
async fn keep_prompt_releases_adopted_descendants_and_python() {
    check_prompt_lifetime(Helper::Run, BackgroundJobs::Keep, FinalReply::Succeeds).await;
}

#[tokio::test]
async fn failed_keep_prompt_stops_jobs() {
    check_prompt_lifetime(Helper::Background, BackgroundJobs::Keep, FinalReply::Fails).await;
}

async fn check_prompt_lifetime(helper: Helper, jobs_policy: BackgroundJobs, reply: FinalReply) {
    let dir = TempDir::new("headless-retained");
    let keep = jobs_policy == BackgroundJobs::Keep;
    let fail = reply == FinalReply::Fails;
    let function = match helper {
        Helper::Run => "run",
        Helper::Background => "bg_run",
    };
    let command = "(while [ ! -e release ]; do sleep .01; done; echo AFTER; echo AFTER-ERR >&2; echo done > finished) & echo $! > daemon; exit 0";
    let code = format!(
        "{function}([\"bash\",\"-c\",{command:?}])\nimport os, time\nwhile not os.path.exists('daemon'): time.sleep(.01)"
    );
    let code = format!(
        "{code}\nimport socket\ns = socket.socket(socket.AF_UNIX)\ns.bind('client.sock')\ns.listen()\nb = bg_run(['bash', '-c', 'echo finite'])\nb.wait()\nassert b.return_code == 0"
    );
    let mut final_reply = Reply::text("done");
    if fail {
        final_reply.status = 400;
    }
    let (provider, requests) = serve(
        dir.path(),
        vec![Reply::python("start", &json!({"code":code})), final_reply],
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
            Mode::Prompt {
                prompt: "build".into(),
                jobs: jobs_policy,
            },
            Terminations::listen().unwrap(),
        ),
    )
    .await;
    assert!(
        result.as_ref().is_ok_and(|result| result.is_err() == fail),
        "{result:?}"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(dir.path().join("client.sock")).is_err(),
        "Python client socket remained open"
    );
    let pid = std::fs::read_to_string(dir.path().join("daemon")).unwrap();
    if keep && !fail {
        let pid = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
        assert!(rustix::process::test_kill_process(pid).is_ok());
        std::fs::write(dir.path().join("release"), "").unwrap();
    }
    let dead = tokio::time::timeout(Duration::from_secs(5), wait_until_dead(pid.trim())).await;
    jobs.stop_all();
    jobs.settled().await;
    assert!(dead.is_ok(), "the session left pid {pid} running");
    assert_eq!(dir.path().join("release").exists(), keep && !fail);
    if keep && !fail {
        assert_eq!(
            std::fs::read_to_string(dir.path().join("finished")).unwrap(),
            "done\n"
        );
        assert_no_detached_output(dir.path());
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let shown = serde_json::to_string(&*requests).unwrap();
    assert!(
        helper != Helper::Run || shown.contains("left processes running as job["),
        "{shown}"
    );
}

fn assert_no_detached_output(directory: &std::path::Path) {
    for suffix in ["stdout", "stderr"] {
        let outputs: Vec<_> = std::fs::read_dir(directory.join("spill"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(suffix))
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .collect();
        assert!(
            outputs
                .iter()
                .all(|bytes| !String::from_utf8_lossy(bytes).contains("AFTER")),
            "captured detached {suffix}"
        );
    }
}
