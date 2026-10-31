use super::*;

#[test]
fn authorization_requests_carry_state_and_s256_without_exposing_the_verifier() {
    let pkce = Pkce::new().unwrap();
    let request = authorize_url(&pkce, "fixture-state");
    let query: Vec<_> = request.query_pairs().collect();
    let expected = [
        ("response_type", "code"),
        ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
        ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "fixture-state"),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex-tui"),
    ];
    assert_eq!(
        query
            .iter()
            .map(|(field, value)| (field.as_ref(), value.as_ref()))
            .collect::<Vec<_>>(),
        expected,
        "the authorize request matches Codex's field for field and in order"
    );
    assert_eq!(pkce.challenge, challenge(&pkce.verifier));
    assert!(!request.as_str().contains(&pkce.verifier));
}

#[test]
fn pkce_challenge_matches_rfc_7636_appendix_b() {
    assert_eq!(
        challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn refresh_keeps_what_the_server_does_not_reissue() {
    let jwt = |claims: Value| format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let response =
        |access: String, refresh: Option<&str>, id: Option<String>, expires_in| TokenResponse {
            access_token: access,
            refresh_token: refresh.map(Into::into),
            id_token: id,
            expires_in,
        };
    let id_token =
        jwt(serde_json::json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct" } }));
    let first = tokens_from(
        response(
            jwt(serde_json::json!({ "exp": 1000 })),
            Some("r1"),
            Some(id_token),
            None,
        ),
        None,
        0,
    )
    .unwrap();
    assert_eq!(
        (
            first.account_id.as_str(),
            first.refresh.expose(),
            first.expires_at
        ),
        ("acct", "r1", 1000)
    );

    let another_login = tokens_from(
        response(
            jwt(serde_json::json!({ "exp": 1000, "chatgpt_account_id": "acct" })),
            Some("r1"),
            None,
            None,
        ),
        None,
        0,
    )
    .unwrap();
    assert_ne!(another_login.installation_id, first.installation_id);
    assert_eq!(&first.installation_id[14..15], "4");

    let constrained = Tokens {
        residency: Some("eu".into()),
        ..first.clone()
    };
    let silent = tokens_from(
        response("opaque".into(), None, None, Some(60)),
        Some(&constrained),
        0,
    )
    .unwrap();
    assert_eq!(silent.residency.as_deref(), Some("eu"), "no claim keeps it");
    let lifted = tokens_from(
        response(
            jwt(serde_json::json!({ "exp": 9, "chatgpt_compute_residency": "no_constraint" })),
            None,
            None,
            None,
        ),
        Some(&constrained),
        0,
    )
    .unwrap();
    assert_eq!(lifted.residency, None, "no_constraint lifts it");

    let refreshed = tokens_from(
        response("opaque".into(), None, None, Some(3600)),
        Some(&first),
        500,
    )
    .unwrap();
    assert_eq!(
        refreshed.refresh, first.refresh,
        "an unrotated refresh token is kept"
    );
    assert_eq!(refreshed.installation_id, first.installation_id);
    assert_eq!(refreshed.account_id, "acct");
    assert_eq!(refreshed.expires_at, 4100);
    assert_eq!(refreshed.access.expose(), "opaque");
    let mut extreme = tokens_from(
        response("opaque".into(), None, None, Some(i64::MAX)),
        Some(&first),
        500,
    )
    .unwrap();
    assert_eq!(
        extreme.expires_at,
        i64::MAX,
        "no overflow on hostile metadata"
    );
    extreme.expires_at = i64::MIN;
    assert!(extreme.needs_refresh());

    let rotated = tokens_from(
        response("opaque".into(), Some("r2"), None, Some(60)),
        Some(&first),
        0,
    )
    .unwrap();
    assert_eq!(rotated.refresh.expose(), "r2");
    assert!(
        tokens_from(
            response("opaque".into(), Some("r"), None, Some(60)),
            None,
            0
        )
        .is_err(),
        "no account"
    );
    assert!(
        tokens_from(response("opaque".into(), None, None, None), Some(&first), 0).is_err(),
        "no expiry"
    );
}

#[tokio::test]
async fn dropped_refresh_saves_rotation_and_drain_excludes_lock_waiters() {
    use super::super::{Credential, Store, refresh};
    use std::sync::atomic::Ordering;
    let _serial = refresh::mock::SERIAL.lock().await;
    let server = refresh::mock::Server::new(
        r#"{"access_token":"access-B","refresh_token":"B","expires_in":3600}"#,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("auth.json"));
    store
        .update(async |all| {
            all.providers.insert(
                "chatgpt".into(),
                Credential::Chatgpt(Tokens {
                    access: Secret("access-A".into()),
                    refresh: Secret("A".into()),
                    expires_at: 0,
                    account_id: "acct".into(),
                    residency: None,
                    installation_id: "5b0f3c1e-7a2d-4e8b-9c61-2f4d8a7e9b13".into(),
                }),
            );
            Ok(())
        })
        .await
        .unwrap();
    let request = || {
        let (store, endpoint) = (store.clone(), server.url.clone());
        tokio::spawn(async move {
            credential_at(
                &store,
                &Client::new(),
                "chatgpt",
                "missing".into(),
                &endpoint,
            )
            .await
        })
    };
    let caller = request();
    server.started.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let competing = request();
    let waiting = request();
    tokio::task::yield_now().await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let draining = tokio::spawn(refresh::drain());
    tokio::task::yield_now().await;
    assert!(!draining.is_finished());
    server.release.notify_one();
    let Credential::Chatgpt(tokens) = competing.await.unwrap().unwrap() else {
        panic!()
    };
    assert_eq!(tokens.access.expose(), "access-B");
    draining.await.unwrap();
    let Credential::Chatgpt(saved) = &store.load().unwrap().providers["chatgpt"] else {
        panic!()
    };
    assert_eq!(saved.refresh.expose(), "B");
    assert_eq!(saved.installation_id, tokens.installation_id);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    let lock = store.lock().await.unwrap();
    let pending = request();
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(1), refresh::drain())
        .await
        .unwrap();
    assert!(!pending.is_finished());
    pending.abort();
    let _ = pending.await;
    drop(lock);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn detached_exchange_and_save_failures_settle_without_releasing_the_lock_early() {
    use super::super::{Credential, Store, refresh};
    let _serial = refresh::mock::SERIAL.lock().await;
    for fail_save in [false, true] {
        let response = if fail_save {
            r#"{"access_token":"access-B","refresh_token":"B","expires_in":3600}"#
        } else {
            "not a token response"
        };
        let server = refresh::mock::Server::new(response).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = Store::new(path.clone());
        store
            .update(async |all| {
                all.providers.insert(
                    "chatgpt".into(),
                    Credential::Chatgpt(Tokens {
                        access: Secret("access-A".into()),
                        refresh: Secret("A".into()),
                        expires_at: 0,
                        account_id: "acct".into(),
                        residency: None,
                        installation_id: "5b0f3c1e-7a2d-4e8b-9c61-2f4d8a7e9b13".into(),
                    }),
                );
                Ok(())
            })
            .await
            .unwrap();
        let caller = {
            let (store, endpoint) = (store.clone(), server.url.clone());
            tokio::spawn(async move {
                credential_at(
                    &store,
                    &Client::new(),
                    "chatgpt",
                    "missing".into(),
                    &endpoint,
                )
                .await
            })
        };
        server.started.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        if fail_save {
            std::fs::rename(&path, dir.path().join("original.json")).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        let waiting = {
            let store = store.clone();
            tokio::spawn(async move { store.lock().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        server.release.notify_one();
        refresh::drain().await;
        drop(waiting.await.unwrap().unwrap());
        if fail_save {
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(dir.path().join("original.json"), &path).unwrap();
        }
        let Credential::Chatgpt(saved) = &store.load().unwrap().providers["chatgpt"] else {
            panic!()
        };
        assert_eq!(saved.refresh.expose(), "A");
    }
}
