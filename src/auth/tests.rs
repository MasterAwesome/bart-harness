use std::fs;

use super::*;
use crate::tool::test_support::TempDir;

fn key(value: &str) -> Credential {
    Credential::ApiKey {
        key: Secret(value.into()),
    }
}

#[tokio::test]
async fn updates_are_private_atomic_and_serialized() {
    let dir = TempDir::new("auth-store");
    let store = Store::new(dir.path().join("config/auth.json"));
    assert!(store.load().unwrap().providers.is_empty());

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = Store::new(store.path.clone());
            tokio::spawn(async move {
                store
                    .update(async |credentials| {
                        let count = credentials.providers.len();
                        tokio::task::yield_now().await;
                        credentials
                            .providers
                            .insert(format!("k{count}"), key("secret"));
                        Ok(())
                    })
                    .await
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        store.load().unwrap().providers.len(),
        8,
        "no update was lost"
    );
    let mode = fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        fs::metadata(dir.path().join("config"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!format!("{:?}", store.load().unwrap()).contains("secret"));

    fs::set_permissions(&store.path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(store.load(), Err(Error::NotPrivate(_))));
    fs::write(
        &store.path,
        "{\"x\": {\"type\": \"api_key\", \"key\": \"leak\"",
    )
    .unwrap();
    fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600)).unwrap();
    let error = store.load().unwrap_err();
    assert!(!error.to_string().contains("leak"), "{error}");
}

#[tokio::test]
async fn independent_writers_preserve_keys_and_killed_owners_release_locks() {
    const CHILD: &str = "BART_TEST_AUTH_WRITER";
    if let Ok(directory) = std::env::var(CHILD) {
        let root = PathBuf::from(directory);
        let name = std::env::var("BART_TEST_AUTH_KEY").unwrap();
        let store = Store::new(root.join("auth.json"));
        let update = store.update(async |credentials| {
            credentials.providers.insert(name.clone(), key("synthetic"));
            if name != "second" {
                fs::write(root.join("ready"), b"").unwrap();
                while !root.join("release").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            Ok(())
        });
        tokio::pin!(update);
        if name == "second" {
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(update.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            fs::write(root.join("waiting"), []).unwrap();
        }
        update.await.unwrap();
        return;
    }
    let dir = TempDir::new("auth-process-writers");
    let spawn = |name| {
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "auth::tests::independent_writers_preserve_keys_and_killed_owners_release_locks",
            ])
            .env(CHILD, dir.path())
            .env("BART_TEST_AUTH_KEY", name)
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let ready = async |marker| {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join(marker).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    };
    let mut first = spawn("first");
    ready("ready").await;
    let mut second = spawn("second");
    ready("waiting").await;
    fs::write(dir.path().join("release"), b"").unwrap();
    for child in [&mut first, &mut second] {
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    let store = Store::new(dir.path().join("auth.json"));
    let loaded = store.load().unwrap();
    assert_eq!(loaded.providers.len(), 2);
    for name in ["first", "second"] {
        assert_eq!(loaded.providers[name], key("synthetic"));
    }
    fs::remove_file(dir.path().join("ready")).unwrap();
    fs::remove_file(dir.path().join("release")).unwrap();
    let mut owner = spawn("killed");
    ready("ready").await;
    owner.kill().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.update(async |credentials| {
            credentials
                .providers
                .insert("after".into(), key("synthetic"));
            Ok(())
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.providers.len(), 3);
    for name in ["after", "first", "second"] {
        assert_eq!(loaded.providers[name], key("synthetic"));
    }
}
