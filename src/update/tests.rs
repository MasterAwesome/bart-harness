use std::fs;

fn installed(candidate: &str) -> (Result<(), String>, String) {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("bart");
    fs::write(&binary, "original").unwrap();
    let result = super::install(
        &binary,
        format!("#!/bin/sh\n{candidate}\n").as_bytes(),
        "9.8.7",
    );
    (result, fs::read_to_string(&binary).unwrap())
}

#[test]
fn only_a_build_that_reports_the_exact_version_and_succeeds_replaces_the_binary() {
    let (result, contents) = installed("echo 'bart 9.8.7'; exit 42");
    assert!(result.is_err());
    assert_eq!(contents, "original");

    let (result, contents) = installed("echo 'bart 9.8.70'");
    assert!(result.is_err());
    assert_eq!(contents, "original");

    let (result, contents) = installed("exec sleep 30");
    assert!(result.is_err());
    assert_eq!(contents, "original");

    let (result, contents) = installed("echo 'bart 9.8.7'");
    assert_eq!(result, Ok(()));
    assert!(contents.contains("bart 9.8.7"));
}
