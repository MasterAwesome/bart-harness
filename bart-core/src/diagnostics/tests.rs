use super::*;
use crate::tool::test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn diagnostics_repair_torn_rows_rotate_and_record_cancellation() {
    let dir = TempDir::new("diagnostics");
    let path = dir.path().join("diagnostics.jsonl");
    fs::write(&path, b"{}\n{torn").unwrap();
    let log = Arc::new(Log::open(dir.path()).unwrap());
    assert_eq!(fs::read(&path).unwrap(), b"{}\n");
    drop(log);
    fs::remove_file(&path).unwrap();
    let log = Arc::new(Log::open(dir.path()).unwrap());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    {
        let mut trace = log.start("fixture", "offline", false, 1, None);
        trace.headers(200);
        trace.chunk(50);
        trace.generation(0, 0, 12);
        trace.provider_error(false, Some(429), true, true, false, RetryKind::Reset);
        trace.provider_error(true, Some(503), false, false, true, RetryKind::After);
        trace.provider_error(false, Some(400), false, false, false, RetryKind::No);
    }
    let rows: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    let retries: Vec<&str> = rows
        .iter()
        .filter(|row| row["event"] == "provider_error")
        .map(|row| row["data"]["retry"].as_str().unwrap())
        .collect();
    assert_eq!(retries, ["reset", "after", "no"]);
    let end = rows.last().unwrap();
    assert_eq!(end["data"]["outcome"], "cancelled");
    assert_eq!(end["data"]["chunks"], 1);
    assert_eq!(end["data"]["argument_bytes"], 12);
    assert_eq!(end["data"]["text_bytes"], 0);
    assert!(end["data"]["last_generation_ms"].is_number());
    drop(log);
    let mut padding = vec![b' '; usize::try_from(MAX_BYTES - 32).unwrap()];
    *padding.last_mut().unwrap() = b'\n';
    fs::write(&path, padding).unwrap();
    let log = Arc::new(Log::open(dir.path()).unwrap());
    drop(log.start("fixture", "offline", false, 1, None));
    assert!(path.with_extension("jsonl.1").exists());
    assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    assert!(fs::metadata(path.with_extension("jsonl.1")).unwrap().len() <= MAX_BYTES);
    for row in fs::read_to_string(&path).unwrap().lines() {
        let _: Value = serde_json::from_str(row).unwrap();
    }
}

#[test]
fn diagnostics_torn_tail_longer_than_chunk_preserves_prior_rows() {
    let dir = TempDir::new("diagnostics-long-torn");
    let path = dir.path().join("diagnostics.jsonl");
    let prefix = b"{\"event\":\"keep_1\"}\n{\"event\":\"keep_2\"}\n";
    let mut content = prefix.to_vec();
    content.extend(std::iter::repeat_n(
        b'x',
        usize::try_from(MAX_LINE_BYTES * 2).unwrap(),
    ));
    fs::write(&path, &content).unwrap();
    let _log = Log::open(dir.path()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), prefix);
}
