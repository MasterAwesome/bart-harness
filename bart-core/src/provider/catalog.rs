use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::time;

use super::{
    ERROR_BODY_TIMEOUT, Error, Protocol, Provider, Resolved, authorize, credential, gemini,
    http_error, messages, refresh_command, responses,
};
use crate::auth::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Listed,
    Unlisted,
}

impl Visibility {
    pub(super) fn parse(value: &Value) -> Self {
        if value.is_null() || value == "list" {
            Self::Listed
        } else {
            Self::Unlisted
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageInput {
    #[default]
    Supported,
    Unsupported,
}

impl ImageInput {
    #[must_use]
    pub const fn supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    pub(super) fn from_modalities(value: &Value) -> Option<Self> {
        let list = value.as_array()?;
        if list.is_empty() {
            return None;
        }
        let strings = list.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
        Some(Self::from_bool(strings.contains(&"image")))
    }

    pub(super) const fn from_bool(supported: bool) -> Self {
        if supported {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub visibility: Visibility,
    pub name: Option<String>,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub summaries: Option<bool>,
    pub verbosity: Option<String>,
    pub images: ImageInput,
    pub context_window: Option<u64>,
    pub service_tiers: Option<Vec<crate::preferences::ServiceTier>>,
    pub max_context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

const CODEX_RELEASES: &str = "https://github.com/openai/codex/releases/latest";
const CODEX_FALLBACK_VERSION: &str = "0.200.0";
static CODEX_VERSION: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
const MAX_MODELS_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODEL_PAGES: usize = 16;

static CATALOGS: LazyLock<Mutex<HashMap<String, Arc<Entry>>>> = LazyLock::new(Mutex::default);

// Models load once under this entry's lock. A failed or cancelled load leaves it empty.
struct Entry {
    provider: Provider,
    owner: Owner,
    models: tokio::sync::Mutex<Option<Vec<Model>>>,
}

#[derive(PartialEq, Eq)]
enum Owner {
    None,
    Account(String),
    Key([u8; 32]),
}

impl Owner {
    fn of(credential: Option<&Resolved>) -> Self {
        match credential {
            None => Self::None,
            Some(Resolved::Chatgpt(tokens)) => Self::Account(tokens.account_id.clone()),
            Some(key) => Self::Key(Sha256::digest(key.secret().expose()).into()),
        }
    }
}

fn entry(provider: &Provider, credential: Option<&Resolved>) -> Arc<Entry> {
    let owner = Owner::of(credential);
    let mut catalogs = CATALOGS.lock().expect(crate::NOT_POISONED);
    match catalogs.get(provider.name()) {
        Some(entry) if entry.provider == *provider && entry.owner == owner => Arc::clone(entry),
        _ => {
            let entry = Arc::new(Entry {
                provider: provider.clone(),
                owner,
                models: tokio::sync::Mutex::default(),
            });
            catalogs.insert(provider.name().to_owned(), Arc::clone(&entry));
            entry
        }
    }
}

/// # Errors
/// Returns an error if credentials or the provider catalog cannot be loaded.
pub async fn models(
    client: &Client,
    store: &Store,
    provider: &Provider,
) -> Result<Vec<Model>, Error> {
    lookup(client, store, provider, None, async |models| {
        Ok(models.to_vec())
    })
    .await
}

/// # Errors
/// Returns an error if credentials or the catalog cannot be loaded, or if Claude
/// Code cannot report the selected model’s context window.
pub async fn model(
    client: &Client,
    store: &Store,
    provider: &Provider,
    id: &str,
) -> Result<Option<Model>, Error> {
    lookup(client, store, provider, Some(id), async |models| {
        let Some(model) = models.iter_mut().find(|model| model.id == id) else {
            return Ok(None);
        };
        if provider.protocol() == Protocol::Claude && model.context_window.is_none() {
            let window = super::claude_code::context_window(id).await?;
            model.context_window = Some(window);
            model.max_context_window = Some(window);
        }
        Ok(Some(model.clone()))
    })
    .await
}

async fn lookup<T>(
    client: &Client,
    store: &Store,
    provider: &Provider,
    selected: Option<&str>,
    project: impl AsyncFnOnce(&mut [Model]) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut resolved = credential(client, store, provider).await?;
    let mut replay = true;
    loop {
        let credential = resolved.as_ref();
        let entry = entry(provider, credential);
        let mut models = entry.models.lock().await;
        if models.is_none() {
            let result = match provider.http() {
                None => super::claude_code::models(selected).await,
                Some(endpoint) => catalog(client, endpoint, credential).await,
            };
            match result {
                Ok(listed) => *models = Some(listed),
                Err(error) => {
                    if let Some(issued) = error.reject_command(credential)
                        && std::mem::take(&mut replay)
                    {
                        drop(models);
                        if let Some(fresh) = refresh_command(store, provider, issued).await? {
                            resolved = Some(fresh);
                            continue;
                        }
                    }
                    return Err(error.sanitized(credential));
                }
            }
        }
        return project(models.as_mut().expect("the catalog was just loaded")).await;
    }
}

async fn codex_version() -> &'static str {
    CODEX_VERSION
        .get_or_init(|| async {
            let latest = async {
                let response = Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .ok()?
                    .get(CODEX_RELEASES)
                    .send()
                    .await
                    .ok()?;
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)?
                    .to_str()
                    .ok()?;
                let version = location
                    .rsplit_once("/tag/")?
                    .1
                    .trim_start_matches(|c: char| !c.is_ascii_digit());
                (!version.is_empty() && version.split('.').all(|part| part.parse::<u64>().is_ok()))
                    .then(|| version.to_owned())
            };
            latest
                .await
                .unwrap_or_else(|| CODEX_FALLBACK_VERSION.to_owned())
        })
        .await
}

async fn catalog(
    client: &Client,
    endpoint: super::HttpEndpoint<'_>,
    credential: Option<&Resolved>,
) -> Result<Vec<Model>, Error> {
    let mut url = endpoint.url("/models");
    if endpoint.codex() {
        let version = codex_version().await;
        url.query_pairs_mut().append_pair("client_version", version);
    }
    let paging = match endpoint.protocol() {
        super::HttpProtocol::Messages => Some(messages::PAGING),
        super::HttpProtocol::Gemini => Some(gemini::PAGING),
        super::HttpProtocol::Responses | super::HttpProtocol::Chat => None,
    };
    if let Some(paging) = paging {
        url.query_pairs_mut().append_pair(paging.size, "1000");
    }
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_MODEL_PAGES {
        let mut page = url.clone();
        if let (Some(cursor), Some(paging)) = (&cursor, paging) {
            page.query_pairs_mut().append_pair(paging.cursor, cursor);
        }
        let list = model_page(client, endpoint, credential, page).await?;
        let Page {
            models: listed,
            next,
        } = match endpoint.protocol() {
            super::HttpProtocol::Responses | super::HttpProtocol::Chat
                if list.get("models").is_some() =>
            {
                Page {
                    models: responses::codex_models(&list)?,
                    next: None,
                }
            }
            super::HttpProtocol::Responses | super::HttpProtocol::Chat => Page {
                models: data_models(&list)?,
                next: None,
            },
            super::HttpProtocol::Messages => messages::models(&list)?,
            super::HttpProtocol::Gemini => gemini::models(&list)?,
        };
        models.extend(listed);
        match next.filter(|next| !next.is_empty()) {
            Some(next) if cursor.as_deref() == Some(&next) => {
                return Err(Error::Invalid("the model list repeated its page".into()));
            }
            Some(next) => cursor = Some(next),
            None => return Ok(models),
        }
    }
    Err(Error::Limit(format!(
        "the model list has more than {MAX_MODEL_PAGES} pages"
    )))
}

#[derive(Clone, Copy)]
pub(super) struct Paging {
    pub(super) size: &'static str,
    pub(super) cursor: &'static str,
}

pub(super) struct Page {
    pub(super) models: Vec<Model>,
    pub(super) next: Option<String>,
}

#[derive(Deserialize)]
struct Listed {
    data: Vec<Value>,
}

#[derive(Default, Deserialize)]
struct ListedModel {
    id: String,
    name: Option<String>,
    display_name: Option<String>,
    context_length: Option<u64>,
    context_window: Option<u64>,
    service_tiers: Option<Value>,
    max_input_tokens: Option<u64>,
    max_context_window: Option<u64>,
    input_modalities: Option<Value>,
    architecture: Option<Value>,
    #[serde(default)]
    visibility: Value,
}

pub(super) fn data_models(list: &Value) -> Result<Vec<Model>, Error> {
    let list = Listed::deserialize(list)
        .map_err(|error| Error::Invalid(format!("invalid model list: {error}")))?;
    Ok(list
        .data
        .iter()
        .filter_map(|model| {
            ListedModel::deserialize(model).ok().or_else(|| {
                Some(ListedModel {
                    id: model.get("id")?.as_str()?.to_owned(),
                    ..ListedModel::default()
                })
            })
        })
        .map(|model| Model {
            id: model.id,
            visibility: Visibility::parse(&model.visibility),
            name: model.name.or(model.display_name),
            efforts: Vec::new(),
            default_effort: None,
            summaries: None,
            verbosity: None,
            images: model
                .input_modalities
                .as_ref()
                .and_then(ImageInput::from_modalities)
                .or_else(|| {
                    model
                        .architecture
                        .as_ref()
                        .and_then(|arch| arch.get("input_modalities"))
                        .and_then(ImageInput::from_modalities)
                })
                .unwrap_or(ImageInput::Supported),
            context_window: model
                .context_length
                .or(model.context_window)
                .or(model.max_input_tokens),
            service_tiers: model.service_tiers.as_ref().and_then(responses::tier_ids),
            max_context_window: model.max_context_window,
            max_output_tokens: None,
        })
        .collect())
}

async fn model_page(
    client: &Client,
    endpoint: super::HttpEndpoint<'_>,
    credential: Option<&Resolved>,
    url: reqwest::Url,
) -> Result<Value, Error> {
    let builder = authorize(client.get(url), endpoint, credential)?;
    let deadline = time::Instant::now() + ERROR_BODY_TIMEOUT;
    let response = time::timeout_at(deadline, builder.send())
        .await
        .map_err(|_| Error::Interrupted("no response for the model list".into()))?
        .map_err(Error::Transport)?;
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    let body = crate::http_body::read(
        response,
        MAX_MODELS_BYTES,
        crate::http_body::Mode::Complete,
        deadline,
    )
    .await
    .complete()
    .map_err(|error| match error {
        crate::http_body::Error::Transport(error) => Error::Transport(error),
        crate::http_body::Error::Overflow => Error::Limit("the model list is too large".into()),
        crate::http_body::Error::Deadline => Error::Interrupted("the model list stalled".into()),
    })?;
    serde_json::from_slice(&body)
        .map_err(|error| Error::Invalid(format!("invalid model list: {error}")))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::auth::Credential;
    use crate::provider::{Custom, HttpProtocol};

    #[tokio::test]
    async fn catalogs_load_once_per_owner_and_survive_cancelled_loads() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let gets = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&gets);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let counted = Arc::clone(&counted);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut bytes = [0; 4096];
                        let read = socket.read(&mut bytes).await.unwrap();
                        head.extend_from_slice(&bytes[..read]);
                    }
                    counted.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let head = String::from_utf8_lossy(&head).to_lowercase();
                    let model = if head.contains("bearer key-a") {
                        "a"
                    } else {
                        "b"
                    };
                    let body = format!(r#"{{"data":[{{"id":"{model}"}}]}}"#);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        let custom = Custom {
            protocol: HttpProtocol::Chat,
            url,
            key: true,
        };
        let provider = Provider::custom("catalog-owner", &custom).unwrap();
        let client = crate::http_client::new();
        let key = |key: &str| Credential::ApiKey {
            key: crate::auth::api_key(key).unwrap(),
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("auth.json"));
        let save = async |value| {
            store
                .update(async |credentials| {
                    credentials
                        .providers
                        .insert(provider.name().to_owned(), key(value));
                    Ok(())
                })
                .await
                .unwrap();
        };
        save("key-a").await;
        let ids = |models: Result<Vec<Model>, Error>| {
            models
                .unwrap()
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
        };

        let cancelled = time::timeout(
            std::time::Duration::from_millis(20),
            models(&client, &store, &provider),
        )
        .await;
        assert!(cancelled.is_err());
        let loads = (0..8).map(|_| models(&client, &store, &provider));
        for listed in futures_util::future::join_all(loads).await {
            assert_eq!(ids(listed), ["a"]);
        }
        assert_eq!(gets.load(Ordering::SeqCst), 2);
        save("key-b").await;
        assert_eq!(ids(models(&client, &store, &provider).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 3);
        let alternate = Provider::custom(
            "catalog-owner",
            &Custom {
                url: format!("{}/alternate", custom.url),
                ..custom
            },
        )
        .unwrap();
        assert_eq!(ids(models(&client, &store, &alternate).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 4);
        assert_eq!(ids(models(&client, &store, &provider).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_401_on_a_later_page_restarts_the_catalog_under_the_new_key() {
        use serde_json::json;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (key, cursor, status, body) in [
                (
                    "key-1",
                    None,
                    200,
                    json!({"data":[{"id":"old"}], "has_more":true, "last_id":"old"}),
                ),
                (
                    "key-1",
                    Some("old"),
                    401,
                    json!({"error":{"message":"expired"}}),
                ),
                (
                    "key-2",
                    None,
                    200,
                    json!({"data":[{"id":"fresh"}], "has_more":true, "last_id":"fresh"}),
                ),
                (
                    "key-2",
                    Some("fresh"),
                    200,
                    json!({"data":[{"id":"selected"}], "has_more":false}),
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let read = socket.read(&mut bytes).await.unwrap();
                    assert_ne!(read, 0);
                    head.extend_from_slice(&bytes[..read]);
                }
                let head = String::from_utf8(head).unwrap();
                assert!(
                    head.lines()
                        .filter_map(|line| line.split_once(':'))
                        .any(|(name, value)| {
                            name.eq_ignore_ascii_case("x-api-key") && value.trim_ascii() == key
                        }),
                    "{head}"
                );
                let path = head
                    .lines()
                    .next()
                    .unwrap()
                    .split_ascii_whitespace()
                    .nth(1)
                    .unwrap();
                assert_eq!(
                    reqwest::Url::parse(&format!("http://localhost{path}"))
                        .unwrap()
                        .query_pairs()
                        .find_map(|(name, value)| (name == "after_id").then_some(value))
                        .as_deref(),
                    cursor
                );
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("auth.json"));
        let name = "catalog-rotation";
        super::super::CustomUpdate::prepare(
            dir.path(),
            name,
            Some(Custom {
                protocol: HttpProtocol::Messages,
                url,
                key: true,
            }),
        )
        .await
        .unwrap()
        .save()
        .unwrap();
        let provider = Provider::named(name, dir.path()).unwrap();
        let argv = [
            "sh",
            "-c",
            r#"if [ -e "$1/first" ]; then echo key-2; else touch "$1/first"; echo key-1; fi"#,
            "sh",
            dir.path().to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec();
        store
            .update(async |credentials| {
                credentials.providers.insert(
                    provider.name().into(),
                    Credential::Command {
                        endpoint: provider.endpoint().unwrap().to_string(),
                        argv,
                    },
                );
                Ok(())
            })
            .await
            .unwrap();

        let client = crate::http_client::new();
        let selected = model(&client, &store, &provider, "selected")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, "selected");
        server.await.unwrap();
        let listed = models(&client, &store, &provider).await.unwrap();
        assert_eq!(
            listed.into_iter().map(|model| model.id).collect::<Vec<_>>(),
            ["fresh", "selected"]
        );
    }

    #[test]
    fn catalog_parsers_extract_image_input_and_default_unknown_to_supported() {
        let data = serde_json::json!({
            "data": [
                { "id": "plain" },
                { "id": "or-text", "architecture": { "input_modalities": ["text"] } },
                { "id": "or-vision", "architecture": { "input_modalities": ["text", "image"] } },
                { "id": "gw-text", "input_modalities": ["text"] },
                { "id": "str-arch", "architecture": "transformer", "context_length": 16_000 }
            ]
        });
        let parsed = data_models(&data).unwrap();
        let images =
            |models: &[Model], id: &str| models.iter().find(|m| m.id == id).unwrap().images;
        assert_eq!(images(&parsed, "plain"), ImageInput::Supported);
        assert_eq!(images(&parsed, "or-text"), ImageInput::Unsupported);
        assert_eq!(images(&parsed, "or-vision"), ImageInput::Supported);
        assert_eq!(images(&parsed, "gw-text"), ImageInput::Unsupported);
        assert_eq!(images(&parsed, "str-arch"), ImageInput::Supported);

        let anthropic = messages::models(&serde_json::json!({
            "has_more": false,
            "data": [
                { "id": "a-vision", "capabilities": { "image_input": { "supported": true } } },
                { "id": "a-text", "capabilities": { "image_input": { "supported": false } } },
                { "id": "a-unknown", "capabilities": {} }
            ]
        }))
        .unwrap()
        .models;
        assert_eq!(images(&anthropic, "a-vision"), ImageInput::Supported);
        assert_eq!(images(&anthropic, "a-text"), ImageInput::Unsupported);
        assert_eq!(images(&anthropic, "a-unknown"), ImageInput::Supported);

        let codex = responses::codex_models(&serde_json::json!({
            "models": [
                { "slug": "c-text", "input_modalities": ["text"] },
                { "slug": "c-vision", "input_modalities": ["text", "image"] },
                { "slug": "c-unknown" }
            ]
        }))
        .unwrap();
        assert_eq!(images(&codex, "c-text"), ImageInput::Unsupported);
        assert_eq!(images(&codex, "c-vision"), ImageInput::Supported);
        assert_eq!(images(&codex, "c-unknown"), ImageInput::Supported);
    }
}
