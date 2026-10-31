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
    }
    let rows: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
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
