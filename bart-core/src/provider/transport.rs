use reqwest::Client;

use super::{
    Error, HttpEndpoint, Observer, Request, Tools, Trace, chatgpt, claude_code, finish, http,
};
use crate::{
    auth::Credential,
    history::{Assistant, ToolCall, ToolResult},
};

#[derive(Default)]
pub(crate) struct Transport {
    chatgpt: chatgpt::Route,
    claude: claude_code::Route,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Conversation,
    Isolated,
}

impl Transport {
    pub(crate) fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            claude: claude_code::Route::new(cwd),
            ..Self::default()
        }
    }

    pub(crate) async fn close(&self) {
        self.claude.close().await;
    }

    pub(crate) async fn stream(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        on_delta: impl Observer,
        trace: Option<Trace>,
    ) -> Result<Assistant, Error> {
        self.send(
            client,
            credential,
            request,
            on_delta,
            trace,
            Scope::Conversation,
        )
        .await
    }

    pub(crate) async fn isolated(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        on_delta: impl Observer,
        trace: Option<Trace>,
    ) -> Result<Assistant, Error> {
        let transport = Self {
            claude: self.claude.isolated(),
            ..Self::default()
        };
        let result = transport
            .send(
                client,
                credential,
                request,
                on_delta,
                trace,
                Scope::Isolated,
            )
            .await;
        transport.close().await;
        result
    }

    async fn send(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        mut on_delta: impl Observer,
        mut trace: Option<Trace>,
        scope: Scope,
    ) -> Result<Assistant, Error> {
        let result = match request.provider.http() {
            None => {
                self.claude
                    .send(request, &mut on_delta, trace.as_mut())
                    .await
            }
            Some(endpoint)
                if scope == Scope::Conversation && self.persistent(endpoint, request) =>
            {
                self.claude.close().await;
                self.chatgpt
                    .send(
                        endpoint,
                        client,
                        credential,
                        request,
                        &mut on_delta,
                        trace.as_mut(),
                    )
                    .await
            }
            Some(endpoint) => {
                self.claude.close().await;
                http::send(
                    endpoint,
                    client,
                    credential,
                    request,
                    &mut on_delta,
                    trace.as_mut(),
                )
                .await
            }
        };
        finish(result, trace, credential)
    }

    pub(crate) async fn prewarm(
        &self,
        credential: Option<&Credential>,
        request: &Request<'_>,
    ) -> Result<(), Error> {
        match request.provider.http() {
            None => self.claude.prewarm(request).await,
            Some(endpoint) if self.persistent(endpoint, request) => {
                self.chatgpt.prewarm(endpoint, credential, request).await
            }
            Some(_) => Ok(()),
        }
        .map_err(|error| error.sanitized(credential))
    }

    pub(crate) async fn admit(
        &self,
        call: &ToolCall,
    ) -> Result<Option<tokio_util::sync::CancellationToken>, Error> {
        self.claude.admit(call).await
    }

    pub(crate) async fn answer(&self, result: &ToolResult) -> Result<(), Error> {
        self.claude.answer(result).await
    }

    pub(crate) async fn begin_turn(&self) -> Result<(), Error> {
        self.chatgpt.begin_turn().await
    }

    pub(crate) async fn fall_back(&self, request: &Request<'_>) -> bool {
        match request.provider.http() {
            Some(endpoint) if self.persistent(endpoint, request) => self.chatgpt.fall_back().await,
            _ => false,
        }
    }

    /// Our WebSocket connection bypasses proxies, so use HTTP when a proxy is configured.
    fn persistent(&self, endpoint: HttpEndpoint<'_>, request: &Request<'_>) -> bool {
        endpoint.codex()
            && matches!(request.tools, Tools::Functions(_) | Tools::Disabled(_))
            && endpoint
                .stream_url(request.model)
                .is_ok_and(|url| !crate::http_client::proxied(&url))
    }
}

pub(crate) async fn stream(
    client: &Client,
    credential: Option<&Credential>,
    request: &Request<'_>,
    on_delta: impl Observer,
    trace: Option<Trace>,
) -> Result<Assistant, Error> {
    Transport::default()
        .isolated(client, credential, request, on_delta, trace)
        .await
}
