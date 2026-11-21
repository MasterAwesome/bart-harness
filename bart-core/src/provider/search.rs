use crate::history::Message;

use super::{Protocol, chat, gemini, messages, responses};

const MAX_SOURCES: usize = 20;
const MAX_CLAIMS: usize = 3;
const MAX_SHOWN_CHARS: usize = 200;

pub(crate) struct Source {
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) claims: Vec<String>,
}

#[derive(Default)]
pub(crate) struct SearchResult {
    pub(crate) answer: String,
    pub(crate) sources: Vec<Source>,
    pub(crate) more: bool,
    pub(crate) errors: Vec<String>,
}

fn shown(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_SHOWN_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

impl SearchResult {
    pub(super) fn source(&mut self, url: Option<&str>, title: Option<&str>) -> Option<&mut Source> {
        let url = url.filter(|url| !url.is_empty())?;
        if let Some(at) = self.sources.iter().position(|source| source.url == url) {
            return Some(&mut self.sources[at]);
        }
        if self.sources.len() == MAX_SOURCES {
            self.more = true;
            return None;
        }
        self.sources.push(Source {
            url: url.to_owned(),
            title: shown(title.unwrap_or_default()),
            claims: Vec::new(),
        });
        self.sources.last_mut()
    }

    pub(super) fn cite(&mut self, url: Option<&str>, title: Option<&str>, claim: &str) {
        let claim = shown(claim);
        if let Some(source) = self.source(url, title)
            && !claim.is_empty()
            && source.claims.len() < MAX_CLAIMS
            && !source.claims.contains(&claim)
        {
            source.claims.push(claim);
        }
    }
}

pub(super) fn span(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    match (start, end) {
        (Some(start), Some(end)) if start <= end => {
            text.chars().skip(start).take(end - start).collect()
        }
        _ => String::new(),
    }
}

pub(crate) fn collect(protocol: Protocol, history: &[Message]) -> SearchResult {
    let mut result = SearchResult::default();
    for assistant in history.iter().filter_map(|message| match message {
        Message::Assistant(assistant) => Some(assistant),
        _ => None,
    }) {
        let parts = &assistant.parts;
        for part in parts {
            if let crate::history::Content::Text(text) = &part.content {
                result.answer.push_str(text);
            }
        }
        match protocol {
            Protocol::Responses => responses::collect_search(parts, &mut result),
            Protocol::Chat => chat::collect_search(parts, &mut result),
            Protocol::Messages => messages::collect_search(parts, &mut result),
            Protocol::Gemini => gemini::collect_search(parts, &mut result),
            Protocol::Claude => {}
        }
    }
    result
}
