use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, time};

use super::{Error, Secret, callback};

const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_mins(15);
use crate::http_client::TOKEN_REQUEST_TIMEOUT;
const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CALLBACK_PORT: u16 = 1455;
const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub(crate) const ORIGINATOR: &str = "codex-tui";
pub(crate) const USER_AGENT: &str = "bart";

const REFRESH_MARGIN_SECS: i64 = 300;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub(crate) access: Secret,
    refresh: Secret,
    expires_at: i64,
    pub(crate) account_id: String,
    pub(crate) residency: Option<String>,
    pub(crate) installation_id: String,
}

impl Tokens {
    #[must_use]
    pub(crate) fn needs_refresh(&self) -> bool {
        self.expires_at.saturating_sub(now()) < REFRESH_MARGIN_SECS
    }
}

struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn new() -> Result<Self, Error> {
        let verifier = random_token()?;
        let challenge = challenge(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_token() -> Result<String, Error> {
    Ok(URL_SAFE_NO_PAD.encode(crate::random::bytes::<32>()?))
}

fn authorize_url(pkce: &Pkce, state: &str) -> Url {
    Url::parse_with_params(
        &format!("{ISSUER}/oauth/authorize"),
        [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("scope", SCOPES),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", ORIGINATOR),
        ],
    )
    .expect("the issuer URL is valid")
}

pub(crate) async fn login(client: &Client, show_url: impl FnOnce(&str)) -> Result<Tokens, Error> {
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|error| Error::Login(format!("cannot listen on port {CALLBACK_PORT}: {error}")))?;
    let mut listeners = vec![listener];
    match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, CALLBACK_PORT)).await {
        Ok(listener) => listeners.push(listener),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) => {}
        Err(error) => {
            return Err(Error::Login(format!(
                "cannot listen on IPv6 loopback port {CALLBACK_PORT}: {error}"
            )));
        }
    }
    let pkce = Pkce::new()?;
    let state = random_token()?;
    show_url(authorize_url(&pkce, &state).as_str());
    let code = callback::receive(&listeners, "/auth/callback", &state)
        .await?
        .code;
    exchange(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &pkce.verifier),
        ],
        None,
        &format!("{ISSUER}/oauth/token"),
    )
    .await
}

pub(crate) async fn login_device(
    client: &Client,
    show: impl FnOnce(&str, &str),
) -> Result<Tokens, Error> {
    #[derive(Deserialize)]
    struct Issued {
        device_auth_id: String,
        #[serde(alias = "usercode")]
        user_code: String,
        #[serde(default)]
        interval: Value,
    }
    #[derive(Deserialize)]
    struct Approved {
        authorization_code: String,
        code_verifier: String,
    }
    let accounts = format!("{ISSUER}/api/accounts/deviceauth");
    let (status, body) = post_json(
        client,
        &format!("{accounts}/usercode"),
        &serde_json::json!({ "client_id": CLIENT_ID }),
    )
    .await?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::Login(
            "this server offers no device-code login; use the browser login".into(),
        ));
    }
    if !status.is_success() {
        return Err(Error::Login(format!(
            "the device-code request failed ({status})"
        )));
    }
    let code: Issued = serde_json::from_slice(&body).map_err(|_| {
        Error::Login("the device-code response was not in the expected form".into())
    })?;
    let interval = code
        .interval
        .as_u64()
        .or_else(|| {
            code.interval
                .as_str()
                .and_then(|text| text.trim().parse().ok())
        })
        .unwrap_or(5)
        .clamp(1, 60);
    show(&format!("{ISSUER}/codex/device"), &code.user_code);
    let poll =
        serde_json::json!({ "device_auth_id": code.device_auth_id, "user_code": code.user_code });
    let started = std::time::Instant::now();
    let approved: Approved = loop {
        let (status, body) = post_json(client, &format!("{accounts}/token"), &poll).await?;
        if status.is_success() {
            break serde_json::from_slice(&body).map_err(|_| {
                Error::Login("the device-code approval was not in the expected form".into())
            })?;
        }
        if !matches!(
            status,
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
        ) {
            return Err(Error::Login(format!(
                "the device-code login failed ({status})"
            )));
        }
        if started.elapsed() >= DEVICE_LOGIN_TIMEOUT {
            return Err(Error::Login(
                "the code was not entered within fifteen minutes".into(),
            ));
        }
        time::sleep(Duration::from_secs(interval)).await;
    };
    exchange(
        client,
        &[
            ("grant_type", "authorization_code"),
            ("code", &approved.authorization_code),
            ("redirect_uri", &format!("{ISSUER}/deviceauth/callback")),
            ("client_id", CLIENT_ID),
            ("code_verifier", &approved.code_verifier),
        ],
        None,
        &format!("{ISSUER}/oauth/token"),
    )
    .await
}

async fn post_json(
    client: &Client,
    url: &str,
    body: &Value,
) -> Result<(reqwest::StatusCode, Vec<u8>), Error> {
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(Error::Transport)?;
    let status = response.status();
    Ok((status, bounded_body(response).await?))
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, Error> {
    use crate::http_body::{self, Mode};
    http_body::read(
        response,
        MAX_TOKEN_RESPONSE_BYTES,
        Mode::Complete,
        time::Instant::now() + TOKEN_REQUEST_TIMEOUT,
    )
    .await
    .complete()
    .map_err(|error| match error {
        http_body::Error::Transport(error) => Error::Transport(error),
        http_body::Error::Overflow => Error::Login("the response is too large".into()),
        http_body::Error::Deadline => Error::Login("the response body timed out".into()),
    })
}

pub(crate) async fn credential(
    store: &super::Store,
    client: &Client,
    key: &str,
    missing: String,
) -> Result<super::Credential, Error> {
    credential_at(
        store,
        client,
        key,
        missing,
        &format!("{ISSUER}/oauth/token"),
    )
    .await
}

async fn credential_at(
    store: &super::Store,
    client: &Client,
    key: &str,
    missing: String,
    endpoint: &str,
) -> Result<super::Credential, Error> {
    let locked = store.lock().await?;
    let client = client.clone();
    let key = key.to_owned();
    let endpoint = endpoint.to_owned();
    super::refresh::run(async move {
        locked
            .update(async |credentials| {
                let credential = credentials
                    .providers
                    .get_mut(&key)
                    .ok_or(Error::Login(missing))?;
                if let super::Credential::Chatgpt(tokens) = credential
                    && tokens.needs_refresh()
                {
                    *tokens = refresh(&client, tokens, &endpoint).await?;
                }
                Ok(credential.clone())
            })
            .await
    })
    .await
}

async fn refresh(client: &Client, tokens: &Tokens, endpoint: &str) -> Result<Tokens, Error> {
    exchange(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens.refresh.expose()),
            ("client_id", CLIENT_ID),
        ],
        Some(tokens),
        endpoint,
    )
    .await
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

async fn exchange(
    client: &Client,
    form: &[(&str, &str)],
    previous: Option<&Tokens>,
    endpoint: &str,
) -> Result<Tokens, Error> {
    let response = client
        .post(endpoint)
        .form(form)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(Error::Transport)?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        let message = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|error| {
                error["error_description"]
                    .as_str()
                    .or_else(|| error["error"].as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "no details".into());
        // A server that echoes what it was sent would print a credential.
        let secrets: Vec<&str> = form
            .iter()
            .filter(|(key, _)| matches!(*key, "code" | "code_verifier" | "refresh_token"))
            .map(|(_, value)| *value)
            .collect();
        let message = crate::provider::printable(&message, &secrets);
        return Err(Error::Http { status, message });
    }
    let tokens: TokenResponse = serde_json::from_slice(&body)
        .map_err(|_| Error::Login("the token response was not in the expected form".into()))?;
    tokens_from(tokens, previous, now())
}

fn tokens_from(
    tokens: TokenResponse,
    previous: Option<&Tokens>,
    now: i64,
) -> Result<Tokens, Error> {
    let access_claims = claims(&tokens.access_token);
    let id_claims = tokens.id_token.as_deref().and_then(claims);
    let account_id = id_claims
        .as_ref()
        .and_then(account_id)
        .or_else(|| access_claims.as_ref().and_then(account_id))
        .or_else(|| previous.map(|tokens| tokens.account_id.clone()))
        .ok_or_else(|| Error::Login("the tokens name no ChatGPT account".into()))?;
    let expires_at = access_claims
        .as_ref()
        .and_then(|claims| claims["exp"].as_i64())
        .or_else(|| tokens.expires_in.map(|seconds| now.saturating_add(seconds)))
        .ok_or_else(|| Error::Login("the access token has no expiry".into()))?;
    let refresh = match (tokens.refresh_token, previous) {
        (Some(refresh), _) => Secret(refresh),
        (None, Some(previous)) => previous.refresh.clone(),
        (None, None) => return Err(Error::Login("no refresh token was issued".into())),
    };
    let residency = match access_claims
        .as_ref()
        .map_or(Residency::Unstated, residency)
    {
        Residency::In(region) => Some(region),
        Residency::Unconstrained => None,
        Residency::Unstated => previous.and_then(|previous| previous.residency.clone()),
    };
    let installation_id = match previous {
        Some(previous) => previous.installation_id.clone(),
        None => crate::random::uuid_v4()?,
    };
    Ok(Tokens {
        installation_id,
        residency,
        access: Secret(tokens.access_token),
        refresh,
        expires_at,
        account_id,
    })
}

fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

fn account_id(claims: &Value) -> Option<String> {
    claims["chatgpt_account_id"]
        .as_str()
        .or_else(|| claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| claims["organizations"][0]["id"].as_str())
        .map(str::to_owned)
}

#[derive(Debug, PartialEq, Eq)]
enum Residency {
    Unstated,
    Unconstrained,
    In(String),
}

fn residency(claims: &Value) -> Residency {
    match claims["https://api.openai.com/auth"]["chatgpt_compute_residency"]
        .as_str()
        .or_else(|| claims["chatgpt_compute_residency"].as_str())
    {
        None => Residency::Unstated,
        Some("no_constraint") => Residency::Unconstrained,
        Some(region) => Residency::In(region.to_owned()),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs().cast_signed())
}

#[cfg(test)]
mod tests;
