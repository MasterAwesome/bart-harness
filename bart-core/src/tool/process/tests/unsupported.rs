use super::*;
use crate::tool::{
    Escape, Network,
    test_support::{TempDir, context},
};

#[tokio::test]
async fn a_jail_is_refused_without_starting_commands() {
    let project = TempDir::new("unsupported-jail");
    for network in [Network::On, Network::Off] {
        let context = context(
            project.path(),
            Confinement::Jailed(network, Escape::Withheld),
            1000,
        );
        assert!(confined("bash", &context).is_err());
        assert!(
            Process::spawn(
                &crate::tool::execution::Argv::parse(vec!["touch".into(), "started".into()])
                    .unwrap(),
                &context,
                super::super::Launch::Session,
            )
            .await
            .is_err()
        );
        assert!(!project.path().join("started").exists());
    }
}
