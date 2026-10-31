use std::time::Duration;

use super::{Error, Event, Reporter, routing::Routing};
use crate::{
    history::{Assistant, Content},
    provider::{self, Delta, Request, Retry},
    record::diagnostics::Log,
};

const MAX_RETRIES: u32 = 5;
pub(super) const NAME: &str = "Name this session from the user's first prompt. Do not continue the work. Reply with only JSON like {\"name\": \"fix-diff-padding\"}: two or three lowercase words joined by hyphens.";

pub(super) async fn respond(
    routing: &Routing,
    diagnostics: Option<&std::sync::Arc<Log>>,
    request: &Request<'_>,
    on_event: &mut impl Reporter,
) -> Result<Assistant, Error> {
    let mut retries = 0;
    let mut retry_of = None;
    loop {
        let credential = provider::credential(&routing.store, &routing.client, request.provider)
            .await
            .map_err(Error::Credential)?;
        let trace = diagnostics.map(|log| {
            log.start(
                request.provider.name(),
                request.model,
                matches!(request.tools, provider::Tools::WebSearch),
                retries + 1,
                retry_of,
            )
        });
        let request_id = trace.as_ref().map(crate::record::diagnostics::Trace::id);
        let mut streamed = false;
        let result = routing
            .transport
            .stream(
                &routing.client,
                credential.as_ref(),
                request,
                ResponseReporter {
                    reporter: on_event,
                    streamed: &mut streamed,
                },
                trace,
            )
            .await;
        let error = match result {
            Ok(assistant) => return Ok(assistant),
            Err(error @ provider::Error::ContextOverflow(_)) => {
                return Err(Error::Overflow {
                    error,
                    discarded: streamed,
                });
            }
            Err(error) => error,
        };
        if retries >= MAX_RETRIES && !routing.transport.fall_back(request).await {
            return Err(Error::Model(error));
        }
        let delay = match error.retry() {
            Retry::Reset(delay) if routing.preferences().effective.wait_for_reset => {
                on_event.event(Event::WaitingForReset {
                    error: &error,
                    delay,
                });
                delay
            }
            Retry::After(hint) => {
                let delay = hint.unwrap_or_else(|| backoff(retries));
                on_event.event(Event::Retrying {
                    error: &error,
                    delay,
                    discarded: streamed,
                });
                delay
            }
            _ => return Err(Error::Model(error)),
        };
        if let Some(log) = diagnostics
            && let Some(id) = request_id
        {
            log.retry(id, delay, matches!(error.retry(), Retry::Reset(_)));
        }
        retry_of = request_id;
        on_event.flush().await;
        if matches!(error.retry(), Retry::Reset(_)) {
            let until = std::time::SystemTime::now() + delay;
            while let Ok(remaining) = until.duration_since(std::time::SystemTime::now()) {
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(Duration::from_secs(30))).await;
            }
        } else {
            tokio::time::sleep(delay).await;
        }
        retries += 1;
    }
}

pub(super) async fn name(
    routing: &Routing,
    diagnostics: Option<&std::sync::Arc<Log>>,
    mut request: Request<'_>,
    on_event: &mut impl Reporter,
) -> Result<String, String> {
    request.reasoning_effort = routing
        .cached()
        .filter(|model| model.efforts.iter().any(|effort| effort == "low"))
        .map(|_| "low");
    request.summaries = false;
    request.verbosity = None;
    let credential =
        provider::credential(&routing.store, &routing.client, request.provider).await?;
    let trace =
        diagnostics.map(|log| log.start(request.provider.name(), request.model, false, 1, None));
    let assistant = routing
        .transport
        .isolated(
            &routing.client,
            credential.as_ref(),
            &request,
            StatusReporter(on_event),
            trace,
        )
        .await
        .map_err(|error| error.to_string())?;
    on_event.event(Event::Responded(assistant.usage));
    #[derive(serde::Deserialize)]
    struct Name {
        name: String,
    }
    let text = assistant_text(&assistant).collect::<String>();
    serde_json::from_str::<Name>(text.trim())
        .map(|reply| reply.name)
        .map_err(|_| "the model did not reply with {\"name\": ...}".into())
}

struct StatusReporter<'a, R>(&'a mut R);

impl<R: Reporter> provider::Observer for StatusReporter<'_, R> {
    fn delta(&mut self, _: Delta<'_>) {}

    fn observation(&mut self, observation: provider::Observation) {
        self.0.event(Event::ProviderObservation(observation));
    }

    async fn flush(&mut self) {
        self.0.flush().await;
    }
}

struct ResponseReporter<'a, R> {
    reporter: &'a mut R,
    streamed: &'a mut bool,
}

impl<R: Reporter> provider::Observer for ResponseReporter<'_, R> {
    fn delta(&mut self, delta: Delta<'_>) {
        *self.streamed |= matches!(delta, Delta::Text(_) | Delta::Reasoning(_));
        self.reporter.event(Event::Delta(delta));
    }

    fn observation(&mut self, observation: provider::Observation) {
        self.reporter.event(Event::ProviderObservation(observation));
    }

    async fn flush(&mut self) {
        self.reporter.flush().await;
    }
}

fn assistant_text(assistant: &Assistant) -> impl Iterator<Item = &str> {
    assistant
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            Content::Text(text) => Some(&**text),
            _ => None,
        })
}

fn backoff(retries: u32) -> Duration {
    let base = Duration::from_secs(1u64 << retries.min(6)).min(Duration::from_secs(60));
    let jitter = crate::random::bytes::<1>().map_or(0, |[byte]| byte);
    base + Duration::from_millis(u64::from(jitter) * 2)
}
