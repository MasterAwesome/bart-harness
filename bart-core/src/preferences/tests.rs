use super::*;
use crate::tool::test_support::TempDir;

#[test]
fn invalid_settings_are_reported_not_silently_replaced() {
    let dir = TempDir::new("preferences-input");
    for bytes in [
        b"{broken".as_slice(),
        br#"{"service_tier":"flex"}"#,
        b"{\"wait_for_reset\":\"no\"}",
        b"{\"diffs\":\"off\"}",
        b"{\"models\":{\"p\":{\"m\":{\"context_window\":9999}}}}",
        b"{\"permissions\":{\"approval\":{\"mode\":\"auto\"}}}",
        &vec![b' '; 4097],
    ] {
        std::fs::write(dir.path().join("settings.json"), bytes).unwrap();
        assert!(Preferences::load(dir.path()).is_err());
    }
}

#[test]
fn service_tier_is_ephemeral_and_not_saved() {
    let dir = TempDir::new("tier-settings");
    std::fs::write(dir.path().join("settings.json"), b"{}").unwrap();
    let mut current = Current::load(dir.path()).unwrap();
    assert_eq!(current.effective.service_tier, ServiceTier::Auto);
    current
        .apply(&Change::ServiceTier(ServiceTier::Flex))
        .unwrap();
    assert_eq!(current.effective.service_tier, ServiceTier::Flex);
    assert_eq!(current.saved.service_tier, ServiceTier::Auto);
    current.saved.save(dir.path()).unwrap();
    let saved = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
    assert!(!saved.contains("service_tier"));
    assert_eq!(
        Current::load(dir.path()).unwrap().effective.service_tier,
        ServiceTier::Auto
    );
}
