use std::fmt::Write as _;

use reqwest::Client;

use crate::{
    auth::Store,
    history::Message,
    provider::{self, Provider, Request},
};

pub(super) const MAX_QUERIES: usize = 8;
const INSTRUCTIONS: &str = "Search the web and answer concisely from the results only. \
    Say so when the results do not answer the question.";

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) client: Client,
    pub(crate) store: Store,
    pub(crate) provider: Provider,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) diagnostics: Option<std::sync::Arc<crate::record::diagnostics::Log>>,
}

pub(super) async fn search(
    route: &Route,
    queries: &[String],
    live: &super::live::Progress,
) -> String {
    let results = futures_util::future::join_all(queries.iter().map(|query| {
        let activity = live.scoped_activity(&format!("web_search({query:?})"));
        async move {
            let result = search_one(route, query).await;
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
        let _ = writeln!(
            output,
            "[Bart web search {}: {}]",
            index + 1,
            query.escape_debug()
        );
        match result {
            Ok(text) => output.push_str(&text),
            Err(error) => {
                let _ = write!(output, "[search failed: {error}]");
            }
        }
    }
    output
}

async fn search_one(route: &Route, query: &str) -> Result<String, String> {
    let credential = provider::credential(&route.store, &route.client, &route.provider).await?;
    let catalog = provider::models(&route.client, &route.provider, credential.as_ref())
        .await
        .ok()
        .and_then(|models| models.into_iter().find(|model| model.id == route.model));
    let max_output_tokens = catalog.and_then(|model| model.max_output_tokens);
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
            reasoning_effort: route.effort.as_deref(),
            summaries: true,
            verbosity: None,
            max_output_tokens,
            cache: None,
        };
        let assistant = provider::stream(
            &route.client,
            credential.as_ref(),
            &request,
            |_: crate::provider::Delta<'_>| {},
            route
                .diagnostics
                .as_ref()
                .map(|log| log.start(route.provider.name(), &route.model, true, attempt, None)),
        )
        .await
        .map_err(|error| format!("web search failed: {error}"))?;
        let continues = assistant.continues;
        history.push(Message::Assistant(assistant));
        if !continues {
            return Ok(render(&provider::search::collect(
                route.provider.protocol(),
                &history,
            )));
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
            "\n\n[some searches failed: {}; the answer may be incomplete]",
            found.errors.join(", ")
        );
    }
    text
}

#[cfg(test)]
mod tests;
