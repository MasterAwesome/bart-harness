use tokio::sync::{Semaphore, SemaphorePermit};

use super::{Cache, Error, Provider, Request, Tools};
use crate::{history::Message, tool::Definition};

static SLOTS: Semaphore = Semaphore::const_new(1);

pub(super) struct Slot {
    _permit: SemaphorePermit<'static>,
}

pub(super) async fn slot() -> Slot {
    Slot {
        _permit: SLOTS
            .acquire()
            .await
            .expect("preparation semaphore is never closed"),
    }
}

impl Slot {
    /// Keep the permit in the worker so cancellation cannot release it early.
    pub(super) async fn run<T: Send + 'static>(
        self,
        job: impl FnOnce() -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        tokio::task::spawn_blocking(move || {
            let _slot = self;
            job()
        })
        .await
        .map_err(|error| Error::Failed {
            code: "request_body_worker".into(),
            message: format!("cannot prepare request body: {error}"),
        })?
    }
}

pub(super) struct Snapshot {
    provider: Provider,
    model: String,
    instructions: String,
    pub(super) history: Vec<Message>,
    tools: OwnedTools,
    reasoning_effort: Option<String>,
    summaries: bool,
    verbosity: Option<String>,
    max_output_tokens: Option<u64>,
    cache: Option<(String, u64)>,
}

enum OwnedTools {
    Functions(Vec<Definition>),
    Disabled(Vec<Definition>),
    WebSearch,
}

impl Snapshot {
    pub(super) fn new(request: &Request<'_>) -> Self {
        Self {
            provider: request.provider.clone(),
            model: request.model.to_owned(),
            instructions: request.instructions.to_owned(),
            history: request.history.to_vec(),
            tools: match request.tools {
                Tools::Functions(tools) => OwnedTools::Functions(tools.to_vec()),
                Tools::Disabled(tools) => OwnedTools::Disabled(tools.to_vec()),
                Tools::WebSearch => OwnedTools::WebSearch,
            },
            reasoning_effort: request.reasoning_effort.map(str::to_owned),
            summaries: request.summaries,
            verbosity: request.verbosity.map(str::to_owned),
            max_output_tokens: request.max_output_tokens,
            cache: request
                .cache
                .map(|cache| (cache.key.to_owned(), cache.window)),
        }
    }

    pub(super) fn request<'a>(&'a self, history: &'a [Message]) -> Request<'a> {
        Request {
            provider: &self.provider,
            model: &self.model,
            instructions: &self.instructions,
            history,
            tools: match &self.tools {
                OwnedTools::Functions(tools) => Tools::Functions(tools),
                OwnedTools::Disabled(tools) => Tools::Disabled(tools),
                OwnedTools::WebSearch => Tools::WebSearch,
            },
            reasoning_effort: self.reasoning_effort.as_deref(),
            summaries: self.summaries,
            verbosity: self.verbosity.as_deref(),
            max_output_tokens: self.max_output_tokens,
            cache: self.cache.as_ref().map(|(key, window)| Cache {
                key,
                window: *window,
            }),
        }
    }
}
