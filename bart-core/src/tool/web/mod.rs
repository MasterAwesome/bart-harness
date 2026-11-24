use std::fmt::Write as _;

use reqwest::Client;

use crate::{
    auth::Store,
    diagnostics::Attempts,
    history::Message,
    provider::{self, Provider, Request},
};

pub(super) const MAX_QUERIES: usize = 8;
const INSTRUCTIONS: &str = "Search the web and answer concisely from the results only. \
    Say so when the results do not answer the question.";

#[derive(Clone)]
pub struct Route {
    pub client: Client,
    pub store: Store,
    pub provider: Provider,
    pub model: String,
    pub effort: Option<String>,
    pub(crate) diagnostics: Option<std::sync::Arc<crate::diagnostics::Log>>,
}

const MAX_CONTINUATIONS: u32 = 8;

pub(super) async fn search(
    route: &Route,
    queries: &[String],
    live: &super::live::Progress,
) -> String {
    let resolved = resolve_search(route).await;
    let results = futures_util::future::join_all(queries.iter().map(|query| {
        let activity = live.scoped_activity(&format!("web_search({query:?})"));
        let resolved = &resolved;
        async move {
            let result = match resolved {
                Ok(max_output_tokens) => search_with(route, *max_output_tokens, query).await,
                Err(error) => Err(error.clone()),
            };
            activity.finish(result.is_ok());
            result
        }
    }))
    .await;
    let mut output = String::new();
    for (index, (query, result)) in queries.iter().zip(results).enumerate() {
        if index > 0 {
            output.push_str("\n\n");
        }
        let _ = writeln!(output, "{}", bart!(WebSearch, "{}", query.escape_debug()));
        match result {
            Ok(text) => output.push_str(&text),
            Err(error) => {
                let _ = write!(output, "{}", bart!(WebSearch, "failed; {error}"));
            }
        }
    }
    output
}

async fn resolve_search(route: &Route) -> Result<Option<u64>, String> {
    match provider::models(&route.client, &route.store, &route.provider).await {
        Ok(models) => Ok(models
            .into_iter()
            .find(|model| model.id == route.model)
            .and_then(|model| model.max_output_tokens)),
        Err(provider::Error::Credential(problem)) => Err(problem),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
async fn search_one(route: &Route, query: &str) -> Result<String, String> {
    let max_output_tokens = resolve_search(route).await?;
    search_with(route, max_output_tokens, query).await
}

async fn search_with(
    route: &Route,
    max_output_tokens: Option<u64>,
    query: &str,
) -> Result<String, String> {
    // Without the date, "latest" means the latest the model was trained on.
    let instructions = format!(
        "{INSTRUCTIONS} The current time is {}.",
        httpdate::fmt_http_date(std::time::SystemTime::now())
    );
    let mut history = vec![Message::User(query.into())];
    let mut attempt = 1u32;
    loop {
        let request = Request {
            provider: &route.provider,
            model: &route.model,
            instructions: &instructions,
            history: &history,
            tools: provider::Tools::WebSearch,
            service_tier: Default::default(),
            reasoning_effort: route.effort.as_deref(),
            summaries: true,
            verbosity: None,
            max_output_tokens,
            cache: None,
        };
        let mut attempts = route
            .diagnostics
            .as_ref()
            .map(|log| Attempts::new(log, attempt));
        let assistant = provider::stream(
            &route.client,
            &route.store,
            &request,
            |_: crate::provider::Delta<'_>| {},
            attempts.as_mut(),
        )
        .await
        .map_err(|error| match error {
            provider::Error::Credential(problem) => problem,
            _ => format!("web search failed: {error}"),
        })?;
        let continues = assistant.continues;
        history.push(Message::Assistant(assistant));
        if !continues {
            return Ok(render(&provider::search::collect(
                route.provider.protocol(),
                &history,
            )));
        }
        if attempt >= MAX_CONTINUATIONS {
            return Err(format!(
                "web search did not complete after {MAX_CONTINUATIONS} continuations"
            ));
        }
        attempt = attempt.saturating_add(1);
    }
}

fn render(found: &provider::search::SearchResult) -> String {
    let mut text = found.answer.trim().to_owned();
    if text.is_empty() {
        text.push_str("(no answer)");
    }
    if !found.sources.is_empty() {
        text.push_str("\n\nSources:");
        for (number, source) in found.sources.iter().enumerate() {
            let _ = write!(text, "\n{}. ", number + 1);
            if !source.title.is_empty() {
                let _ = write!(text, "{} — ", source.title);
            }
            text.push_str(&source.url);
            for claim in &source.claims {
                let _ = write!(text, "\n   supports: \"{claim}\"");
            }
        }
        if found.more {
            text.push_str("\n(more not shown)");
        }
    }
    if !found.errors.is_empty() {
        let _ = write!(
            text,
            "\n\n{}",
            bart!(
                WebSearch,
                "some searches failed ({}); the answer may be incomplete",
                found.errors.join(", ")
            )
        );
    }
    text
}

#[cfg(test)]
mod tests;
