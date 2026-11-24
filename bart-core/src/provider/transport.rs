use reqwest::Client;

use super::{
    Error, HttpEndpoint, Observer, Request, Tools, chatgpt, claude_code, credential, finish, http,
    refresh_command,
};
use crate::{
    auth::Store,
    diagnostics::Attempts,
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
        store: &Store,
        request: &Request<'_>,
        on_delta: impl Observer,
        attempts: Option<&mut Attempts<'_>>,
    ) -> Result<Assistant, Error> {
        self.send(
            client,
            store,
            request,
            on_delta,
            attempts,
            Scope::Conversation,
        )
        .await
    }

    pub(crate) async fn isolated(
        &self,
        client: &Client,
        store: &Store,
        request: &Request<'_>,
        on_delta: impl Observer,
        attempts: Option<&mut Attempts<'_>>,
    ) -> Result<Assistant, Error> {
        let transport = Self {
            claude: self.claude.isolated(),
            ..Self::default()
        };
        let result = transport
            .send(client, store, request, on_delta, attempts, Scope::Isolated)
            .await;
        transport.close().await;
        result
    }

    async fn send(
        &self,
        client: &Client,
        store: &Store,
        request: &Request<'_>,
        mut on_delta: impl Observer,
        mut attempts: Option<&mut Attempts<'_>>,
        scope: Scope,
    ) -> Result<Assistant, Error> {
        let mut resolved = credential(client, store, request.provider).await?;
        let mut replay = true;
        loop {
            let credential = resolved.as_ref();
            let mut trace = attempts.as_deref_mut().map(|attempts| {
                attempts.start(
                    request.provider.name(),
                    request.model,
                    matches!(request.tools, Tools::WebSearch),
                )
            });
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
            let result = finish(result, trace, credential);
            if let Some(issued) = result
                .as_ref()
                .err()
                .and_then(|error| error.reject_command(credential))
                && std::mem::take(&mut replay)
                && let Some(fresh) = refresh_command(store, request.provider, issued).await?
            {
                resolved = Some(fresh);
                continue;
            }
            return result;
        }
    }

    pub(crate) async fn prewarm(
        &self,
        client: &Client,
        store: &Store,
        request: &Request<'_>,
    ) -> Result<(), Error> {
        match request.provider.http() {
            None => self
                .claude
                .prewarm(request)
                .await
                .map_err(|error| error.sanitized(None)),
            Some(endpoint) if self.persistent(endpoint, request) => {
                let credential = credential(client, store, request.provider).await?;
                self.chatgpt
                    .prewarm(endpoint, credential.as_ref(), request)
                    .await
                    .map_err(|error| error.sanitized(credential.as_ref()))
            }
            Some(_) => Ok(()),
        }
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

#[cfg(test)]
mod tests;

pub(crate) async fn stream(
    client: &Client,
    store: &Store,
    request: &Request<'_>,
    on_delta: impl Observer,
    attempts: Option<&mut Attempts<'_>>,
) -> Result<Assistant, Error> {
    Transport::default()
        .isolated(client, store, request, on_delta, attempts)
        .await
}
