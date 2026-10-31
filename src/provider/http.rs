use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use tokio::time;

use super::{
    Auth, Decoder, Error, Events, IDLE_TIMEOUT, Observer, Protocol, Provider, Request, Route,
    Trace, chat, chatgpt, deliver, gemini, http_error, messages, observe, observe_limits,
    prepare::{self, Snapshot},
    responses, validate,
};
use crate::{
    auth::{self, Credential},
    history::Assistant,
};

pub(super) fn authorize(
    builder: RequestBuilder,
    provider: &Provider,
    credential: Option<&Credential>,
) -> Result<RequestBuilder, Error> {
    let builder = match provider.protocol() {
        Protocol::Messages => messages::headers(builder),
        _ => builder,
    };
    Ok(match (provider.auth(), credential) {
        (Auth::ApiKey, Some(Credential::ApiKey { key })) => match provider.protocol() {
            Protocol::Responses | Protocol::Chat => builder.bearer_auth(key.expose()),
            Protocol::Messages => messages::authorize(builder, key.expose()),
            Protocol::Gemini => gemini::authorize(builder, key.expose()),
            Protocol::Claude => return Err(Error::WrongCredential),
        },
        (Auth::Chatgpt, Some(Credential::Chatgpt(tokens))) => {
            let builder = builder
                .bearer_auth(tokens.access.expose())
                .header("ChatGPT-Account-Id", &tokens.account_id)
                .header("originator", auth::chatgpt::ORIGINATOR)
                .header(header::USER_AGENT, auth::chatgpt::USER_AGENT);
            match &tokens.residency {
                Some(residency) => builder.header("x-openai-internal-codex-residency", residency),
                None => builder,
            }
        }
        (Auth::None, _) => builder,
        _ => return Err(Error::WrongCredential),
    })
}

fn serialize_body(request: &Request<'_>) -> Result<Value, Error> {
    let body = match request.provider.protocol() {
        Protocol::Responses => responses::body(request),
        Protocol::Chat => chat::body(request),
        Protocol::Messages => messages::body(
            request,
            request
                .max_output_tokens
                .unwrap_or(messages::DEFAULT_MAX_TOKENS),
        ),
        Protocol::Gemini => gemini::body(request),
        Protocol::Claude => return Err(Error::Invalid("Claude uses a local process".into())),
    };
    body.map_err(|error| Error::Failed {
        code: "image_file".into(),
        message: error.to_string(),
    })
}

pub(super) struct Http;

impl Route for Http {
    async fn send(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        on_delta: &mut impl Observer,
        trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        let metadata = match (request.cache, credential) {
            (Some(cache), Some(Credential::Chatgpt(tokens))) if request.provider.codex() => {
                Some(chatgpt::metadata(cache, tokens))
            }
            _ => None,
        };
        let slot = prepare::slot().await;
        let snapshot = Snapshot::new(request);
        let body = slot
            .run(move || {
                let mut body = serialize_body(&snapshot.request(&snapshot.history))?;
                if let Some(metadata) = metadata {
                    body["client_metadata"] = metadata;
                }
                serde_json::to_vec(&body)
                    .map_err(|error| Error::Invalid(format!("cannot encode the request: {error}")))
            })
            .await?;
        self.send_body(client, credential, request, body, on_delta, trace)
            .await
    }
}

impl Http {
    pub(super) async fn send_body(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        body: Vec<u8>,
        on_delta: &mut impl Observer,
        mut trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error> {
        let provider = request.provider;
        let events = match provider.protocol() {
            Protocol::Responses => Events::Responses(responses::Stream::new(
                provider.name(),
                request.model,
                provider.codex(),
            )),
            Protocol::Chat => Events::Chat(chat::Stream::new(provider.name(), request.model)),
            Protocol::Messages => {
                Events::Messages(messages::Stream::new(provider.name(), request.model))
            }
            Protocol::Gemini => Events::Gemini(gemini::Stream::new(provider.name(), request.model)),
            Protocol::Claude => unreachable!("HTTP route"),
        };
        let mut decoder = Decoder::new(events);
        let builder = client.post(provider.stream_url(request.model)?);
        let mut builder = authorize(builder, provider, credential)?;
        if provider.codex() {
            for (name, value) in chatgpt::identity(request) {
                builder = builder.header(name, value);
            }
        }
        if let Some(trace) = &mut trace {
            trace.body(body.len());
        }
        let sent = builder
            .header(header::ACCEPT, "text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        let mut response = observe(time::timeout(IDLE_TIMEOUT, sent), trace.as_deref_mut())
            .await
            .map_err(|_| {
                Error::Interrupted(format!("no response for {}s", IDLE_TIMEOUT.as_secs()))
            })?
            .map_err(Error::Transport)?;
        if let Some(trace) = &mut trace {
            trace.headers(response.status().as_u16());
        }
        if provider.codex() {
            observe_limits(response.headers(), on_delta).await;
        }
        if !response.status().is_success() {
            return Err(observe(http_error(response), trace.as_deref_mut()).await);
        }
        loop {
            let chunk = observe(
                time::timeout(IDLE_TIMEOUT, response.chunk()),
                trace.as_deref_mut(),
            )
            .await
            .map_err(|_| Error::Interrupted(format!("no data for {}s", IDLE_TIMEOUT.as_secs())))?
            .map_err(Error::Transport)?;
            let Some(chunk) = chunk else {
                return Err(Error::Interrupted(
                    "stream ended before the response completed".into(),
                ));
            };
            if let Some(trace) = &mut trace {
                trace.chunk(chunk.len());
            }
            for piece in chunk.chunks(16 * 1024) {
                let assistant = decoder.push(piece, &mut |delta| {
                    deliver(trace.as_deref_mut(), on_delta, delta)
                })?;
                on_delta.flush().await;
                if let Some(assistant) = assistant {
                    validate(request, &assistant)?;
                    return Ok(assistant);
                }
            }
        }
    }
}
