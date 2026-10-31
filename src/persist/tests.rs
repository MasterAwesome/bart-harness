use std::{fs, os::unix::fs::PermissionsExt as _};

use super::*;
use crate::tool::test_support::TempDir;

#[test]
fn concurrent_readers_see_only_complete_replacements() {
    let dir = TempDir::new("persist-visible");
    let path = dir.path().join("value");
    let old = vec![b'a'; 512 * 1024];
    let new = vec![b'b'; 512 * 1024];
    replace(&path, &old).unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let (ready, started) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut ready = Some(ready);
            loop {
                let bytes = fs::read(&path).unwrap();
                assert!(
                    bytes == old || bytes == new,
                    "reader observed a torn replacement"
                );
                if let Some(ready) = ready.take() {
                    ready.send(()).unwrap();
                }
                if done.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
            }
        });
        started.recv().unwrap();
        for index in 0..20 {
            replace(&path, if index % 2 == 0 { &new } else { &old }).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        reader.join().unwrap();
    });
}

#[test]
fn replacement_is_private_and_leaves_no_temporary_files() {
    let dir = TempDir::new("persist");
    let path = dir.path().join("sub").join("kept.json");
    fs::create_dir(dir.path().join("sub")).unwrap();
    replace(&path, b"one").unwrap();
    replace(&path, b"two").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let names: Vec<String> = fs::read_dir(dir.path().join("sub"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["kept.json"], "no temporary file is left");
    let refused = dir.path().join("refused");
    fs::create_dir(&refused).unwrap();
    assert!(matches!(
        replace(&refused, b"not written"),
        Err(ReplaceError::Unchanged(_))
    ));
    assert!(refused.is_dir());
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        2,
        "failed replacement left no temporary file"
    );
}
