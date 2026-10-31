use super::*;
use crate::tool::test_support::TempDir;

#[test]
fn invalid_settings_are_reported_not_silently_replaced() {
    let dir = TempDir::new("preferences-input");
    for bytes in [
        b"{broken".as_slice(),
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
