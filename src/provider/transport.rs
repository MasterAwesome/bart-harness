use reqwest::Client;

use super::{Error, Http, Observer, Request, Tools, Trace, chatgpt, claude_code, finish};
use crate::{
    auth::Credential,
    history::{Assistant, ToolCall, ToolResult},
};

pub(super) trait Route {
    async fn send(
        &self,
        client: &Client,
        credential: Option<&Credential>,
        request: &Request<'_>,
        on_delta: &mut impl Observer,
        trace: Option<&mut Trace>,
    ) -> Result<Assistant, Error>;

    async fn prewarm(
        &self,
        _credential: Option<&Credential>,
        _request: &Request<'_>,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn begin_turn(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn fall_back(&self) -> bool {
        false
    }
}

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
        let result = if request.provider.protocol() == super::Protocol::Claude {
            self.claude
                .send(client, credential, request, &mut on_delta, trace.as_mut())
                .await
        } else if scope == Scope::Conversation && self.persistent(request) {
            self.claude.close().await;
            self.chatgpt
                .send(client, credential, request, &mut on_delta, trace.as_mut())
                .await
        } else {
            self.claude.close().await;
            Http.send(client, credential, request, &mut on_delta, trace.as_mut())
                .await
        };
        finish(result, trace, credential)
    }

    pub(crate) async fn prewarm(
        &self,
        credential: Option<&Credential>,
        request: &Request<'_>,
    ) -> Result<(), Error> {
        if request.provider.protocol() == super::Protocol::Claude {
            self.claude.prewarm(credential, request).await
        } else if self.persistent(request) {
            self.chatgpt.prewarm(credential, request).await
        } else {
            Http.prewarm(credential, request).await
        }
        .map_err(|error| error.sanitized(credential))
    }

    pub(crate) async fn admit(&self, call: &ToolCall) -> Result<(), Error> {
        self.claude.admit(call).await
    }

    pub(crate) async fn answer(&self, result: &ToolResult) -> Result<(), Error> {
        self.claude.answer(result).await
    }

    pub(crate) async fn begin_turn(&self) -> Result<(), Error> {
        self.chatgpt.begin_turn().await
    }

    pub(crate) async fn fall_back(&self, request: &Request<'_>) -> bool {
        if self.persistent(request) {
            self.chatgpt.fall_back().await
        } else {
            Http.fall_back().await
        }
    }

    /// Our WebSocket connection bypasses proxies, so use HTTP when a proxy is configured.
    fn persistent(&self, request: &Request<'_>) -> bool {
        request.provider.codex()
            && matches!(request.tools, Tools::Functions(_) | Tools::Disabled(_))
            && request
                .provider
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
