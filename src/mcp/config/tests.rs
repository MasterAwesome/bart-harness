use super::*;

#[test]
fn configured_servers_resolve_or_report_why_not() {
    let config = Config::parse(
            r#"{ "mcpServers": {
                "local": { "command": "srv", "args": ["--a"], "env": { "K": "v" } },
                "remote": { "type": "http", "url": "https://h.example/mcp", "headers": { "X": "y" } },
                "cursor": { "url": "https://c.example/mcp" },
                "off": { "command": "x", "disabled": true },
                "stringly": { "command": "x", "disabled": "true" },
                "legacy": { "type": "sse", "url": "https://s.example/sse" },
                "both": { "command": "x", "url": "https://b.example/" }
            } }"#)
        .unwrap();
    let resolved: BTreeMap<&str, Result<Server, String>> = config
        .entries()
        .filter(|(_, entry)| !entry.disabled())
        .map(|(name, entry)| (name, resolve(entry)))
        .collect();
    assert_eq!(
        resolved["local"],
        Ok(Server::Stdio {
            command: "srv".into(),
            args: vec!["--a".into()],
            env: BTreeMap::from([("K".into(), "v".into())]),
        })
    );
    assert!(matches!(&resolved["remote"], Ok(Server::Http { headers, .. }) if headers["X"] == "y"));
    assert!(matches!(&resolved["cursor"], Ok(Server::Http { .. })));
    assert!(!resolved.contains_key("off"));
    assert!(resolved["legacy"].is_err());
    assert!(resolved["both"].is_err());
    assert!(
        resolved["stringly"].is_err(),
        "an unclear switch never launches"
    );
}

#[test]
fn server_configuration_expands_environment_references() {
    let home = std::env::var("HOME").unwrap();
    for (source, expected) in [
        ("${HOME}/x", Some(format!("{home}/x"))),
        ("a${BART_TEST_UNSET:-d}b", Some("adb".into())),
        ("${BART_TEST_UNSET}", None),
        ("${HOME", None),
        ("plain $HOME", Some("plain $HOME".into())),
    ] {
        let result = resolve(&Entry::new(json!({"command":source})));
        match expected {
            Some(expected) => {
                assert!(matches!(result, Ok(Server::Stdio {command, ..}) if command == expected));
            }
            None => assert!(result.is_err()),
        }
    }
}
