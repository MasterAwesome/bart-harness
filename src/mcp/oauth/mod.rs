use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use oauth2::TokenResponse as _;
use reqwest::{
    Client, Url,
    header::{HeaderName, HeaderValue},
};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, CredentialRefreshGuard, CredentialStore,
    OAuthState, StoredCredentials,
};
use rmcp::transport::streamable_http_client::{StreamableHttpClient, StreamableHttpError};
use tokio::net::TcpListener;

use crate::auth::{self, SecretValue, Store, callback};

const CALLBACK_PATH: &str = "/callback";
const REFRESH_MARGIN_SECS: u64 = 30;
type TransportError = StreamableHttpError<reqwest::Error>;

pub(super) fn stored_secrets(server: &str, store: &Store) -> Vec<String> {
    fn strings(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    strings(item, out);
                }
            }
            serde_json::Value::Object(fields) => {
                for (name, item) in fields {
                    match item.as_str() {
                        Some(text) if name.ends_with("token") || name.contains("secret") => {
                            out.push(text.to_owned());
                        }
                        _ => strings(item, out),
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(credentials) = store.load()
        && let Some(credentials) = credentials.mcp.get(server)
    {
        strings(&credentials.0, &mut out);
    }
    out
}

#[derive(Clone)]
struct Tokens {
    store: Store,
    key: String,
    held: Arc<Mutex<Option<Arc<LockedStore>>>>,
}

impl Tokens {
    fn new(store: &Store, server: &str) -> Self {
        Self {
            store: store.clone(),
            key: server.into(),
            held: Arc::default(),
        }
    }

    async fn change(
        &self,
        change: impl FnOnce(&mut auth::Credentials) + Send + 'static,
    ) -> Result<(), AuthError> {
        let held = self.held.lock().expect(crate::NOT_POISONED).clone();
        let locked = match held {
            Some(locked) => locked,
            None => Arc::new(LockedStore::new(
                self.store
                    .lock()
                    .await
                    .map_err(|error| credential_error(&error))?,
            )),
        };
        tokio::task::spawn_blocking(move || {
            let _serial = locked.serial.lock().expect(crate::NOT_POISONED);
            let mut credentials = locked.store.load()?;
            change(&mut credentials);
            locked.store.save(&credentials)
        })
        .await
        .map_err(|error| AuthError::CredentialStoreError(error.to_string()))?
        .map_err(|error| credential_error(&error))
    }
}

struct LockedStore {
    store: auth::Locked,
    serial: Mutex<()>,
}

impl LockedStore {
    fn new(store: auth::Locked) -> Self {
        Self {
            store,
            serial: Mutex::new(()),
        }
    }
}

fn credential_error(error: &auth::Error) -> AuthError {
    AuthError::CredentialStoreError(error.to_string())
}

struct Release(Arc<Mutex<Option<Arc<LockedStore>>>>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.lock().expect(crate::NOT_POISONED).take();
    }
}

#[async_trait::async_trait]
impl CredentialStore for Tokens {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let store = self.store.clone();
        let credentials = tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|error| AuthError::CredentialStoreError(error.to_string()))?
            .map_err(|error| credential_error(&error))?;
        match credentials.mcp.get(&self.key) {
            Some(credentials) => serde_json::from_value(credentials.0.clone())
                .map(Some)
                .map_err(|error| {
                    AuthError::CredentialStoreError(format!("stored MCP credentials: {error}"))
                }),
            _ => Ok(None),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let value = serde_json::to_value(credentials)
            .map_err(|error| AuthError::CredentialStoreError(error.to_string()))?;
        let key = self.key.clone();
        self.change(move |all| {
            let credentials = SecretValue(value);
            all.mcp.insert(key, credentials);
        })
        .await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let key = self.key.clone();
        self.change(move |all| {
            all.mcp.remove(&key);
        })
        .await
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        if self.held.lock().expect(crate::NOT_POISONED).is_some() {
            return Ok(Some(CredentialRefreshGuard::new(Release(Arc::clone(
                &self.held,
            )))));
        }
        let lock = self
            .store
            .lock()
            .await
            .map_err(|error| AuthError::CredentialStoreError(error.to_string()))?;
        *self.held.lock().expect(crate::NOT_POISONED) = Some(Arc::new(LockedStore::new(lock)));
        Ok(Some(CredentialRefreshGuard::new(Release(Arc::clone(
            &self.held,
        )))))
    }
}

async fn manager(url: &str, tokens: Tokens, server: &str) -> Result<AuthorizationManager, String> {
    let failed = |error: AuthError| format!("OAuth setup for {server} failed: {error}");
    let mut manager = AuthorizationManager::new(url).await.map_err(failed)?;
    manager
        .with_client(crate::http_client::oauth())
        .map_err(failed)?;
    manager.set_credential_store(tokens);
    Ok(manager)
}

pub(super) async fn login(
    server: &str,
    url: &str,
    store: &Store,
    show_url: impl FnOnce(&str),
) -> Result<(), String> {
    let failed = |error: AuthError| format!("signing in to {server} failed: {error}");
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("cannot listen for the browser: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot listen for the browser: {error}"))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let mut state =
        OAuthState::Unauthorized(manager(url, Tokens::new(store, server), server).await?);
    state
        .start_authorization(AuthorizationRequest::new(&redirect).with_client_name("Bart"))
        .await
        .map_err(failed)?;
    let authorize = state.get_authorization_url().await.map_err(failed)?;
    let csrf = Url::parse(&authorize)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "state")
                .map(|(_, value)| value.into_owned())
        })
        .ok_or("the authorization URL carries no state")?;
    show_url(&authorize);
    let received = callback::receive(std::slice::from_ref(&listener), CALLBACK_PATH, &csrf)
        .await
        .map_err(|error| error.to_string())?;
    state
        .handle_callback_url(received.url.as_str())
        .await
        // The SDK's error may quote the PKCE verifier, which Bart never sees.
        .map_err(|_| format!("signing in to {server} failed: the server refused the authorization"))
}

pub(super) async fn logout(server: &str, store: &Store) -> Result<bool, String> {
    let unchanged =
        |error: auth::Error| format!("{error}; MCP credentials for {server} were not changed");
    let locked = store.lock().await.map_err(unchanged)?;
    let mut credentials = locked.load().map_err(unchanged)?;
    let before = credentials.mcp.remove(server);
    if before.is_none() {
        return Ok(false);
    }
    locked
        .save(&credentials)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

pub(super) async fn client(
    server: &str,
    url: &str,
    client: &Client,
    store: &Store,
) -> Result<Option<Authorized>, String> {
    let signed_in = store
        .load()
        .map_err(|error| error.to_string())?
        .mcp
        .contains_key(server);
    if !signed_in {
        return Ok(None);
    }
    let tokens = Tokens::new(store, server);
    let mut manager = manager(url, tokens.clone(), server).await?;
    let usable = manager
        .initialize_from_store()
        .await
        .map_err(|error| format!("stored sign-in for {server} is unusable: {error}"))?;
    Ok(usable.then(|| Authorized {
        http: client.clone(),
        manager: Arc::new(tokio::sync::Mutex::new(manager)),
        tokens,
    }))
}

#[derive(Clone)]
pub(super) struct Authorized {
    http: Client,
    manager: Arc<tokio::sync::Mutex<AuthorizationManager>>,
    tokens: Tokens,
}

impl Authorized {
    async fn token(&self, rejected: Option<String>) -> Result<String, AuthError> {
        if let Some(stored) = self.tokens.load().await?
            && let Some(response) = &stored.token_response
        {
            let access = response.access_token().secret();
            let expiring = response
                .expires_in()
                .zip(stored.token_received_at)
                .is_some_and(|(lifetime, received)| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |now| now.as_secs());
                    received.saturating_add(lifetime.as_secs())
                        < now.saturating_add(REFRESH_MARGIN_SECS)
                });
            if !expiring && rejected.as_deref() != Some(access.as_str()) {
                return Ok(access.clone());
            }
        }
        let manager = Arc::clone(&self.manager).lock_owned().await;
        let locked = self
            .tokens
            .store
            .lock()
            .await
            .map_err(|error| credential_error(&error))?;
        *self.tokens.held.lock().expect(crate::NOT_POISONED) =
            Some(Arc::new(LockedStore::new(locked)));
        let release = Release(Arc::clone(&self.tokens.held));
        auth::refresh::run(async move {
            let _release = release;
            let current = manager.get_access_token().await?;
            if rejected.as_deref() == Some(current.as_str()) {
                match manager.refresh_token().await {
                    Ok(token) => Ok(token.access_token().secret().to_owned()),
                    Err(AuthError::TokenRefreshRejected(_)) => {
                        Err(AuthError::AuthorizationRequired)
                    }
                    Err(error) => Err(error),
                }
            } else {
                Ok(current)
            }
        })
        .await
    }

    async fn call<T, F, Fut>(&self, token: Option<String>, call: F) -> Result<T, TransportError>
    where
        F: Fn(Option<String>) -> Fut,
        Fut: std::future::Future<Output = Result<T, TransportError>>,
    {
        let token = match token {
            Some(token) => Some(token),
            None => match self.token(None).await {
                Ok(token) => Some(token),
                Err(AuthError::AuthorizationRequired) => None,
                Err(error) => return Err(error.into()),
            },
        };
        match call(token.clone()).await {
            Err(StreamableHttpError::AuthRequired(challenge)) => {
                let Some(sent) = token else {
                    return Err(StreamableHttpError::AuthRequired(challenge));
                };
                match self.token(Some(sent.clone())).await {
                    Ok(fresh) if fresh != sent => call(Some(fresh)).await,
                    Ok(_) | Err(AuthError::AuthorizationRequired) => {
                        Err(StreamableHttpError::AuthRequired(challenge))
                    }
                    Err(error) => Err(error.into()),
                }
            }
            result => result,
        }
    }
}

impl StreamableHttpClient for Authorized {
    type Error = reqwest::Error;
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), TransportError> {
        self.call(auth_token, |token| {
            let uri = uri.clone();
            let session_id = session_id.clone();
            let custom_headers = custom_headers.clone();
            async move {
                self.http
                    .delete_session(uri, session_id, token, custom_headers)
                    .await
            }
        })
        .await
    }
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<rmcp::transport::common::client_side_sse::BoxedSseResponse, TransportError> {
        self.call(auth_token, |token| {
            let uri = uri.clone();
            let session_id = session_id.clone();
            let last_event_id = last_event_id.clone();
            let custom_headers = custom_headers.clone();
            async move {
                self.http
                    .get_stream(uri, session_id, last_event_id, token, custom_headers)
                    .await
            }
        })
        .await
    }
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<rmcp::transport::common::client_side_sse::BoxedSseResponse, TransportError> {
        self.call(auth_token, |token| {
            let uri = uri.clone();
            let session_id = session_id.clone();
            let last_event_id = last_event_id.clone();
            let custom_headers = custom_headers.clone();
            async move {
                self.http
                    .get_stream_with_max_sse_event_size(
                        uri,
                        session_id,
                        last_event_id,
                        token,
                        custom_headers,
                        max_sse_event_size,
                    )
                    .await
            }
        })
        .await
    }
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<rmcp::transport::streamable_http_client::StreamableHttpPostResponse, TransportError>
    {
        self.call(auth_token, |token| {
            let uri = uri.clone();
            let message = message.clone();
            let session_id = session_id.clone();
            let custom_headers = custom_headers.clone();
            async move {
                self.http
                    .post_message(uri, message, session_id, token, custom_headers)
                    .await
            }
        })
        .await
    }
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<rmcp::transport::streamable_http_client::StreamableHttpPostResponse, TransportError>
    {
        self.call(auth_token, |token| {
            let uri = uri.clone();
            let message = message.clone();
            let session_id = session_id.clone();
            let custom_headers = custom_headers.clone();
            async move {
                self.http
                    .post_message_with_max_sse_event_size(
                        uri,
                        message,
                        session_id,
                        token,
                        custom_headers,
                        max_sse_event_size,
                    )
                    .await
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
