use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::{
    approval::{Approval, Gate, Mode, Notice, Reviewer},
    auth::Store,
    history::Message,
    provider::mock::{Reply, serve_catalog},
    tool::{
        Confinement, Escape, Network,
        test_support::{TempDir, context},
    },
};

fn verdict(risk: &str) -> Reply {
    let outcome = if risk == "low" { "allow" } else { "deny" };
    Reply::text(&json!({"constraint_conflict":false,"risk_level": risk, "user_authorization": "high", "outcome": outcome, "rationale": "test action"}).to_string())
}

fn catalog() -> Value {
    json!({"models": [{"slug": "reviewer"}]})
}

fn approval(mode: Mode) -> Approval {
    let mut approval = Approval::default();
    if mode == Mode::Auto {
        approval.auto(crate::choice::Choice {
            provider: "mock".into(),
            model: "reviewer".into(),
            effort: None,
        });
    } else {
        approval.set_mode(mode).unwrap();
    }
    approval
}

async fn ask(
    project: &TempDir,
    mode: Mode,
    interactive: bool,
    report: &mut dyn crate::approval::Report,
) -> Result<(), String> {
    let context = Arc::new(context(
        project.path(),
        Confinement::Jailed(Network::On, Escape::Offered),
        1000,
    ));
    let client = reqwest::Client::new();
    let store = Store::new(project.path().join("auth.json"));
    let mut reviewer = Reviewer::new("test");
    let mut journal =
        crate::agent::Journal::of(vec![Message::User("commit the fix".to_owned().into())]);
    let approval = approval(mode);
    let mut gate = Gate {
        client: &client,
        store: &store,
        state: &mut reviewer,
        journal: &mut journal,
        interactive,
        report,
        stopped: false,
        approval: &approval,
    };
    let mut review = CellReview {
        id: "test",
        context: &context,
        gate: Some(&mut gate),
    };
    approve(
        &mut review,
        "sh",
        "git commit -m fix",
        "untrusted \u{202e} reason",
    )
    .await
    .map(drop)
}

#[cfg(target_os = "linux")]
#[test]
fn escape_is_offered_exactly_in_a_jail_whose_approval_someone_gives() {
    for (enabled, mode, escape) in [
        (false, Mode::Manual, None),
        (true, Mode::None, Some(Escape::Withheld)),
        (true, Mode::Manual, Some(Escape::Offered)),
        (true, Mode::Auto, Some(Escape::Offered)),
    ] {
        let permissions = crate::preferences::Permissions {
            approval: approval(mode),
            sandbox: crate::preferences::Sandbox {
                enabled,
                network: Network::Off,
            },
        };
        let confinement = permissions.confinement().unwrap();
        assert_eq!(
            confinement,
            escape.map_or(Confinement::Unrestricted, |escape| {
                Confinement::Jailed(Network::Off, escape)
            }),
            "{enabled} {mode:?}"
        );
    }
}

#[test]
fn sandbox_and_reason_exist_only_where_escape_is_offered() {
    use crate::tool::{execution, jobs};
    let foreground = |escape, extra: Value| {
        let mut arguments = json!({"argv": ["true"], "timeout": 5});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        execution::Request::parse(arguments, escape).map(|request| request.outside)
    };
    let job = |escape, extra: Value| {
        let mut arguments = json!({"argv": ["true"], "notify": null});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        jobs::Request::parse(arguments, escape).map(|request| request.outside)
    };
    assert_eq!(foreground(Escape::Withheld, json!({})), Ok(None));
    assert!(
        foreground(
            Escape::Withheld,
            json!({"sandbox": false, "reason": "commit"})
        )
        .is_err()
    );
    assert!(
        job(
            Escape::Withheld,
            json!({"sandbox": false, "reason": "commit"})
        )
        .is_err()
    );
    for parse in [foreground, job] {
        let offered = |sandbox: bool, reason: Value| {
            parse(
                Escape::Offered,
                json!({"sandbox": sandbox, "reason": reason}),
            )
        };
        assert_eq!(offered(false, json!("commit")), Ok(Some("commit".into())));
        assert_eq!(offered(true, Value::Null), Ok(None));
        assert!(offered(false, Value::Null).is_err());
        assert!(offered(false, json!("  ")).is_err());
        assert!(offered(true, json!("commit")).is_err());
    }
}

#[tokio::test]
async fn a_reviewer_that_allows_it_lets_it_run_and_otherwise_the_user_decides() {
    let project = TempDir::new("escape-auto");
    let (_, requests, _, _) = serve_catalog(
        project.path(),
        vec![verdict("low"), verdict("high"), verdict("high")],
        catalog(),
    )
    .await;
    let mut reviewed = |notice: Notice<'_>| assert!(matches!(notice, Notice::Review(_)));
    assert_eq!(
        ask(&project, Mode::Auto, false, &mut reviewed).await,
        Ok(())
    );
    let headless = ask(&project, Mode::Auto, false, &mut reviewed).await;
    assert!(headless.is_err_and(|error| error.contains("headless")));
    let mut asked = None;
    let mut user = |notice: Notice<'_>| {
        if let Notice::Ask(answer) = notice {
            asked = Some(answer.prompt().to_owned());
            answer.send(false);
        }
    };
    let denied = ask(&project, Mode::Auto, true, &mut user).await;
    assert_eq!(denied, Err("[not run: user denied]".into()));
    let prompt = asked.expect("the user was asked");
    assert!(prompt.contains("git commit -m fix") && prompt.contains("reason"));
    assert!(
        !prompt.contains('\u{202e}'),
        "the reason cannot reorder the prompt"
    );
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn manual_approval_asks_the_user_without_a_reviewer() {
    let project = TempDir::new("escape-manual");
    let mut user = |notice: Notice<'_>| {
        let Notice::Ask(answer) = notice else {
            panic!("manual approval asks directly");
        };
        answer.send(true);
    };
    assert_eq!(ask(&project, Mode::Manual, true, &mut user).await, Ok(()));
}
