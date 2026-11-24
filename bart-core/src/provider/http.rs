use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use tokio::time;

use super::{
    Decoder, Error, Events, HttpAuth, HttpEndpoint, HttpProtocol, IDLE_TIMEOUT, Observer, Request,
    Resolved, Trace, chat, chatgpt, deliver, gemini, http_error, messages, observe, observe_limits,
    prepare, responses, validate,
};
use crate::{auth, history::Assistant};

pub(super) fn authorize(
    builder: RequestBuilder,
    endpoint: HttpEndpoint<'_>,
    credential: Option<&Resolved>,
) -> Result<RequestBuilder, Error> {
    let builder = match endpoint.protocol() {
        HttpProtocol::Messages => messages::headers(builder),
        _ => builder,
    };
    Ok(match (endpoint.auth(), credential) {
        (HttpAuth::ApiKey, Some(credential @ (Resolved::ApiKey(_) | Resolved::Command(_)))) => {
            let key = credential.secret().expose();
            match endpoint.protocol() {
                HttpProtocol::Responses | HttpProtocol::Chat => builder.bearer_auth(key),
                HttpProtocol::Messages => messages::authorize(builder, key),
                HttpProtocol::Gemini => gemini::authorize(builder, key),
            }
        }
        (HttpAuth::Chatgpt, Some(Resolved::Chatgpt(tokens))) => {
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
        (HttpAuth::None, _) => builder,
        _ => return Err(Error::wrong_credential()),
    })
}

fn serialize_body(
    protocol: HttpProtocol,
    endpoint: &reqwest::Url,
    codex: bool,
    request: &Request<'_>,
) -> Result<Value, Error> {
    let body = match protocol {
        HttpProtocol::Responses => responses::body(request, endpoint, codex),
        HttpProtocol::Chat => chat::body(request, endpoint),
        HttpProtocol::Messages => messages::body(
            request,
            endpoint,
            request
                .max_output_tokens
                .unwrap_or(messages::DEFAULT_MAX_TOKENS),
        ),
        HttpProtocol::Gemini => gemini::body(request),
    };
    body.map_err(|error| Error::Failed {
        code: "image_file".into(),
        message: error.to_string(),
    })
}

pub(super) async fn send(
    endpoint: HttpEndpoint<'_>,
    client: &Client,
    credential: Option<&Resolved>,
    request: &Request<'_>,
    on_delta: &mut impl Observer,
    trace: Option<&mut Trace>,
) -> Result<Assistant, Error> {
    let protocol = endpoint.protocol();
    let url = endpoint.base_url().clone();
    let codex = endpoint.codex();
    let metadata = match (request.cache, credential) {
        (Some(cache), Some(Resolved::Chatgpt(tokens))) if codex => {
            Some(chatgpt::metadata(cache, tokens))
        }
        _ => None,
    };
    let body = prepare::encode(request, move |snapshot| {
        let mut body = serialize_body(protocol, &url, codex, &snapshot.request(&snapshot.history))?;
        if let Some(metadata) = metadata {
            body["client_metadata"] = metadata;
        }
        serde_json::to_vec(&body)
            .map_err(|error| Error::Invalid(format!("cannot encode the request: {error}")))
    })
    .await?;
    send_body(endpoint, client, credential, request, body, on_delta, trace).await
}

async fn send_body(
    endpoint: HttpEndpoint<'_>,
    client: &Client,
    credential: Option<&Resolved>,
    request: &Request<'_>,
    body: Vec<u8>,
    on_delta: &mut impl Observer,
    mut trace: Option<&mut Trace>,
) -> Result<Assistant, Error> {
    let events = match endpoint.protocol() {
        HttpProtocol::Responses => Events::Responses(responses::Stream::new(
            endpoint.name(),
            request.model,
            endpoint.codex(),
        )),
        HttpProtocol::Chat => Events::Chat(chat::Stream::new(endpoint.name(), request.model)),
        HttpProtocol::Messages => {
            Events::Messages(messages::Stream::new(endpoint.name(), request.model))
        }
        HttpProtocol::Gemini => Events::Gemini(gemini::Stream::new(endpoint.name(), request.model)),
    };
    let mut decoder = Decoder::new(events);
    let builder = client.post(endpoint.stream_url(request.model)?);
    let mut builder = authorize(builder, endpoint, credential)?;
    if endpoint.codex() {
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
        .map_err(|_| Error::Interrupted(format!("no response for {}s", IDLE_TIMEOUT.as_secs())))?
        .map_err(Error::Transport)?;
    if let Some(trace) = &mut trace {
        trace.headers(response.status().as_u16());
    }
    if endpoint.codex() {
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
