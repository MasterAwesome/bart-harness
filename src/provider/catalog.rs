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
    ERROR_BODY_TIMEOUT, Error, Protocol, Provider, authorize, gemini, http_error, messages,
    responses,
};
use crate::auth::Credential;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Visibility {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Model {
    pub(crate) id: String,
    pub(crate) visibility: Visibility,
    pub(crate) name: Option<String>,
    pub(crate) efforts: Vec<String>,
    pub(crate) default_effort: Option<String>,
    pub(crate) summaries: Option<bool>,
    pub(crate) verbosity: Option<String>,
    pub(crate) images: Option<bool>,
    pub(crate) context_window: Option<u64>,
    pub(crate) max_context_window: Option<u64>,
    pub(crate) max_output_tokens: Option<u64>,
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
    fn of(credential: Option<&Credential>) -> Self {
        match credential {
            None => Self::None,
            Some(Credential::Chatgpt(tokens)) => Self::Account(tokens.account_id.clone()),
            Some(Credential::ApiKey { key }) => Self::Key(Sha256::digest(key.expose()).into()),
        }
    }
}

fn entry(provider: &Provider, credential: Option<&Credential>) -> Arc<Entry> {
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

pub(crate) async fn models(
    client: &Client,
    provider: &Provider,
    credential: Option<&Credential>,
) -> Result<Vec<Model>, Error> {
    let entry = entry(provider, credential);
    let mut models = entry.models.lock().await;
    Ok(loaded(&mut models, client, provider, credential, None)
        .await?
        .clone())
}

pub(crate) async fn model(
    client: &Client,
    provider: &Provider,
    credential: Option<&Credential>,
    id: &str,
) -> Result<Option<Model>, Error> {
    let entry = entry(provider, credential);
    let mut models = entry.models.lock().await;
    let Some(model) = loaded(&mut models, client, provider, credential, Some(id))
        .await?
        .iter_mut()
        .find(|model| model.id == id)
    else {
        return Ok(None);
    };
    if provider.protocol() == Protocol::Claude && model.context_window.is_none() {
        let window = super::claude_code::context_window(id).await?;
        model.context_window = Some(window);
        model.max_context_window = Some(window);
    }
    Ok(Some(model.clone()))
}

async fn loaded<'a>(
    models: &'a mut Option<Vec<Model>>,
    client: &Client,
    provider: &Provider,
    credential: Option<&Credential>,
    selected: Option<&str>,
) -> Result<&'a mut Vec<Model>, Error> {
    if models.is_none() {
        let listed = if provider.protocol() == Protocol::Claude {
            super::claude_code::models(selected).await?
        } else {
            catalog(client, provider, credential)
                .await
                .map_err(|error| error.sanitized(credential))?
        };
        *models = Some(listed);
    }
    Ok(models.as_mut().expect("the catalog was just loaded"))
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
    provider: &Provider,
    credential: Option<&Credential>,
) -> Result<Vec<Model>, Error> {
    let mut url = provider.url("/models")?;
    if provider.codex() {
        let version = codex_version().await;
        url.query_pairs_mut().append_pair("client_version", version);
    }
    let paging = match provider.protocol() {
        Protocol::Messages => Some(messages::PAGING),
        Protocol::Gemini => Some(gemini::PAGING),
        Protocol::Responses | Protocol::Chat => None,
        Protocol::Claude => unreachable!("Claude lists models through its CLI"),
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
        let list = model_page(client, provider, credential, page).await?;
        let Page {
            models: listed,
            next,
        } = match provider.protocol() {
            Protocol::Responses | Protocol::Chat if list.get("models").is_some() => Page {
                models: responses::codex_models(&list)?,
                next: None,
            },
            Protocol::Responses | Protocol::Chat => Page {
                models: data_models(&list)?,
                next: None,
            },
            Protocol::Messages => messages::models(&list)?,
            Protocol::Gemini => gemini::models(&list)?,
            Protocol::Claude => unreachable!("Claude lists models through its CLI"),
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
    max_input_tokens: Option<u64>,
    max_context_window: Option<u64>,
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
            images: None,
            context_window: model
                .context_length
                .or(model.context_window)
                .or(model.max_input_tokens),
            max_context_window: model.max_context_window,
            max_output_tokens: None,
        })
        .collect())
}

async fn model_page(
    client: &Client,
    provider: &Provider,
    credential: Option<&Credential>,
    url: reqwest::Url,
) -> Result<Value, Error> {
    let builder = authorize(client.get(url), provider, credential)?;
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
    use crate::provider::{Custom, Protocol};

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
            protocol: Protocol::Chat,
            url,
            key: true,
        };
        let provider = Provider::custom("catalog-owner", &custom).unwrap();
        let client = crate::http_client::new();
        let key = |key: &str| Credential::ApiKey {
            key: crate::auth::api_key(key).unwrap(),
        };
        let (a, b) = (key("key-a"), key("key-b"));
        let ids = |models: Result<Vec<Model>, Error>| {
            models
                .unwrap()
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>()
        };

        let cancelled = time::timeout(
            std::time::Duration::from_millis(20),
            models(&client, &provider, Some(&a)),
        )
        .await;
        assert!(cancelled.is_err());
        let loads = (0..8).map(|_| models(&client, &provider, Some(&a)));
        for listed in futures_util::future::join_all(loads).await {
            assert_eq!(ids(listed), ["a"]);
        }
        assert_eq!(gets.load(Ordering::SeqCst), 2);
        assert_eq!(ids(models(&client, &provider, Some(&b)).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 3);
        let alternate = Provider::custom(
            "catalog-owner",
            &Custom {
                url: format!("{}/alternate", custom.url),
                ..custom
            },
        )
        .unwrap();
        assert_eq!(ids(models(&client, &alternate, Some(&b)).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 4);
        assert_eq!(ids(models(&client, &provider, Some(&b)).await), ["b"]);
        assert_eq!(gets.load(Ordering::SeqCst), 5);
    }
}
