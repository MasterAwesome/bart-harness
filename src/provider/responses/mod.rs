use serde::Deserialize;
use serde_json::{Value, json};

use super::{Delta, Error, Request};
use crate::{
    history::{Assistant, Content, Message, Part, ToolCall, Usage},
    tool::Definition,
};

const MAX_OUTPUT_ITEMS: usize = 4096;

#[derive(Deserialize)]
struct CodexList {
    models: Vec<Value>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    display_name: Option<String>,
    priority: Option<i64>,
    #[serde(default)]
    visibility: Value,
    context_window: Option<u64>,
    max_context_window: Option<u64>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Level>,
    default_reasoning_level: Option<String>,
    supports_reasoning_summary_parameter: Option<bool>,
    #[serde(default)]
    support_verbosity: bool,
    default_verbosity: Option<String>,
    input_modalities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Level {
    effort: String,
}

pub(super) fn codex_models(list: &Value) -> Result<Vec<super::Model>, Error> {
    let list = CodexList::deserialize(list)
        .map_err(|error| Error::Invalid(format!("invalid model list: {error}")))?;
    let mut listed: Vec<(i64, super::Model)> = list
        .models
        .iter()
        .filter_map(|model| CodexModel::deserialize(model).ok())
        .map(|model| {
            (
                model.priority.unwrap_or(i64::MAX),
                super::Model {
                    id: model.slug,
                    visibility: super::Visibility::parse(&model.visibility),
                    name: model.display_name,
                    efforts: model
                        .supported_reasoning_levels
                        .into_iter()
                        .map(|level| level.effort)
                        .collect(),
                    default_effort: model.default_reasoning_level,
                    summaries: model.supports_reasoning_summary_parameter,
                    verbosity: model.default_verbosity.filter(|_| model.support_verbosity),
                    images: model
                        .input_modalities
                        .map(|modalities| modalities.iter().any(|modality| modality == "image")),
                    context_window: model.context_window,
                    max_context_window: model.max_context_window,
                    max_output_tokens: None,
                },
            )
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    Ok(listed.into_iter().map(|(_, model)| model).collect())
}

pub(super) fn openai_api(url: &reqwest::Url) -> bool {
    super::at(url, "api.openai.com", "/v1")
}

pub(super) fn body(request: &Request) -> std::io::Result<Value> {
    let openai = request.provider.codex() || request.provider.endpoint().is_some_and(openai_api);
    let mut body = json!({
        "model": request.model,
        "stream": true,
    });
    body["input"] = Value::Array(input(request)?);
    if let Some(tokens) = request
        .max_output_tokens
        .filter(|_| request.provider.capabilities().output_limit)
    {
        body["max_output_tokens"] = tokens.into();
    }
    let mut include = Vec::new();
    if openai {
        body["store"] = false.into();
        include.push("reasoning.encrypted_content");
    }
    if !request.instructions.is_empty() {
        body["instructions"] = request.instructions.into();
    }
    match request.tools {
        super::Tools::WebSearch => {
            body["tools"] = json!([{ "type": "web_search" }]);
            body["tool_choice"] = "required".into();
            include.push("web_search_call.action.sources");
        }
        super::Tools::Functions(tools) | super::Tools::Disabled(tools) if !tools.is_empty() => {
            body["tools"] = tools.iter().map(tool).collect();
            body["tool_choice"] = if matches!(request.tools, super::Tools::Disabled(_)) {
                "none"
            } else {
                "auto"
            }
            .into();
            body["parallel_tool_calls"] = true.into();
        }
        super::Tools::Disabled(_) => {
            body["tool_choice"] = "none".into();
        }
        super::Tools::Functions(_) => {}
    }
    if !include.is_empty() {
        body["include"] = include.into();
    }
    if let Some(cache) = request.cache.filter(|_| openai) {
        body["prompt_cache_key"] = cache.key.into();
    }
    if let Some(effort) = request.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort });
        if request.summaries {
            body["reasoning"]["summary"] = "auto".into();
        }
    }
    if let Some(verbosity) = request.verbosity {
        body["text"] = json!({ "verbosity": verbosity });
    }
    Ok(body)
}

fn tool(definition: &Definition) -> Value {
    json!({
        "type": "function",
        "name": definition.name,
        "description": definition.description,
        "parameters": definition.parameters,
        "strict": false,
    })
}

pub(super) fn input(request: &Request) -> std::io::Result<Vec<Value>> {
    let mut items = Vec::new();
    for message in request.history {
        match message {
            Message::Notification(note) => items.push(json!({
                "type":"message", "role":"user", "content":[{"type":"input_text","text":&*note.text}]
            })),

            Message::User(_)
            | Message::UserSource(_)
            | Message::Instruction(_)
            | Message::Skill(_) => {
                let text = message.model_text();
                items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            Message::Image(image) => {
                let mut content = Vec::new();
                if !image.label.is_empty() {
                    content.push(json!({ "type": "input_text", "text": image.label }));
                }
                content.push(json!({ "type": "input_image", "image_url": image.url()? }));
                items.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let same_model = request.same_model(assistant);
                for part in &assistant.parts {
                    match &part.replay {
                        Some(item) if same_model => items.push((**item).clone()),
                        _ => items.extend(rebuild(&part.content)),
                    }
                }
            }
            Message::ToolResult(result) => items.push(json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": result.output,
            })),
        }
    }
    Ok(items)
}

fn rebuild(content: &Content) -> Option<Value> {
    match content {
        Content::Text(text) => {
            Some(json!({ "type": "message", "role": "assistant", "content": text }))
        }
        Content::ToolCall(ToolCall {
            id,
            name,
            arguments,
        }) => Some(json!({
            "type": "function_call",
            "call_id": id,
            "name": name,
            "arguments": arguments.wire(),
        })),
        Content::Reasoning(_) | Content::Other => None,
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_item.added")]
    Added {
        #[serde(rename = "sequence_number")]
        _sequence_number: u64,
        output_index: u64,
        item: ItemHead,
    },
    #[serde(rename = "response.output_text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_part.added")]
    SummaryPart { summary_index: u64 },
    #[serde(
        rename = "response.reasoning_summary_text.delta",
        alias = "response.reasoning_text.delta"
    )]
    ReasoningDelta { delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    ArgumentsDelta { delta: String },
    #[serde(rename = "response.output_item.done")]
    Done {
        #[serde(rename = "sequence_number")]
        _sequence_number: u64,
        output_index: u64,
        item: Value,
    },
    #[serde(rename = "response.completed")]
    Completed {
        #[serde(rename = "sequence_number")]
        _sequence_number: u64,
        response: Box<Terminal>,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Value },
    #[serde(rename = "response.failed")]
    Failed { response: FailedResponse },
    #[serde(rename = "error")]
    Error {
        error: Option<ApiError>,
        code: Option<String>,
        message: Option<String>,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct ItemHead {
    id: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct FailedResponse {
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: Option<String>,
    message: Option<String>,
}

impl ApiError {
    fn failure(self) -> Error {
        if self.code.as_deref() == Some(super::CONTEXT_LENGTH_EXCEEDED) {
            return Error::ContextOverflow(self.message.unwrap_or_else(|| "no message".into()));
        }
        Error::Failed {
            code: self.code.unwrap_or_else(|| "unknown".into()),
            message: self.message.unwrap_or_else(|| "no message".into()),
        }
    }
}

#[derive(Deserialize)]
struct Terminal {
    id: String,
    object: String,
    #[serde(rename = "created_at")]
    _created_at: u64,
    status: String,
    model: String,
    error: Value,
    incomplete_details: Value,
    output: Vec<Value>,
    usage: Option<ResponsesUsage>,
    end_turn: Option<bool>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    input_tokens_details: Option<Details>,
    output_tokens_details: Option<Details>,
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct Details {
    cached_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

impl ResponsesUsage {
    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.input_tokens,
            cache_read_tokens: self
                .input_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            cache_write_tokens: None,
            output_tokens: self.output_tokens,
            reasoning_tokens: self
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.reasoning_tokens),
            cost: self.cost,
            cost_is_estimate: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item {
    Message {
        role: String,
        status: String,
        content: Vec<MessageContent>,
    },
    Reasoning {
        summary: Vec<ReasoningText>,
        content: Option<Vec<ReasoningText>>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    WebSearchCall {},
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MessageContent {
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Value>,
    },
    Refusal {
        refusal: String,
    },
}

#[derive(Deserialize)]
struct ReasoningText {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Annotation {
    #[serde(rename = "url_citation")]
    UrlCitation {
        url: String,
        title: Option<String>,
        start_index: Option<usize>,
        end_index: Option<usize>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Sources {
    action: Option<Action>,
}

#[derive(Deserialize)]
struct Action {
    #[serde(default)]
    sources: Vec<SearchSource>,
}

#[derive(Deserialize)]
struct SearchSource {
    url: Option<String>,
    title: Option<String>,
}

fn invalid(error: &serde_json::Error) -> Error {
    Error::Invalid(format!("invalid Responses event: {error}"))
}

pub(super) struct Stream {
    provider: String,
    model: String,
    codex: bool,
    items: Vec<(u64, Value)>,
    open: Vec<(u64, String)>,
}

impl Stream {
    pub(super) fn new(provider: &str, model: &str, codex: bool) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            codex,
            items: Vec::new(),
            open: Vec::new(),
        }
    }

    pub(super) fn event(
        &mut self,
        data: &str,
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Option<Assistant>, Error> {
        if data == "[DONE]" {
            return Ok(None);
        }
        match serde_json::from_str(data).map_err(|error| invalid(&error))? {
            Event::Added {
                output_index, item, ..
            } => {
                if item.kind == "message" {
                    on_delta(Delta::MessageBoundary);
                }
                self.added(output_index, item.id)?;
            }
            Event::TextDelta { delta } => on_delta(Delta::Text(&delta)),
            Event::SummaryPart { summary_index } if summary_index > 0 => {
                on_delta(Delta::Reasoning("\n\n"));
            }
            Event::ReasoningDelta { delta } => on_delta(Delta::Reasoning(&delta)),
            Event::ArgumentsDelta { delta } => on_delta(Delta::ToolBytes(delta.len())),
            Event::Done {
                output_index, item, ..
            } => {
                let message = item["type"] == "message";
                self.done(output_index, item)?;
                if message {
                    on_delta(Delta::MessageBoundary);
                }
            }
            Event::Completed { response, .. } => {
                response.validate("completed")?;
                let continues = self.codex && response.end_turn == Some(false);
                return self.finish(*response, continues).map(Some);
            }
            Event::Incomplete { response } => {
                let reason = response["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                if reason == "interrupted" && self.codex {
                    let response =
                        Terminal::deserialize(&response).map_err(|error| invalid(&error))?;
                    response.validate("incomplete")?;
                    return self.finish(response, true).map(Some);
                }
                return Err(Error::Incomplete(reason.into()));
            }
            Event::Failed { response } => {
                return Err(response.error.map_or_else(
                    || Error::Failed {
                        code: "unknown".into(),
                        message: "no message".into(),
                    },
                    ApiError::failure,
                ));
            }
            Event::Error {
                error,
                code,
                message,
            } => {
                return Err(error.unwrap_or(ApiError { code, message }).failure());
            }
            Event::SummaryPart { .. } | Event::Ignored => {}
        }
        Ok(None)
    }

    fn added(&mut self, index: u64, id: String) -> Result<(), Error> {
        if self.open.iter().any(|(seen, _)| *seen == index)
            || self.items.iter().any(|&(seen, _)| seen == index)
        {
            return Err(Error::Invalid("output item started twice".into()));
        }
        if self.open.len() == MAX_OUTPUT_ITEMS {
            return Err(Error::Limit(format!(
                "more than {MAX_OUTPUT_ITEMS} output items"
            )));
        }
        if id.is_empty() {
            return Err(Error::Invalid("added output item without id".into()));
        }
        self.open.push((index, id));
        Ok(())
    }

    fn done(&mut self, index: u64, item: Value) -> Result<(), Error> {
        if self.items.iter().any(|&(seen, _)| seen == index) {
            return Err(Error::Invalid(format!("output item {index} arrived twice")));
        }
        if self.items.len() == MAX_OUTPUT_ITEMS {
            return Err(Error::Limit(format!(
                "more than {MAX_OUTPUT_ITEMS} output items"
            )));
        }
        if !item["type"].is_string() {
            return Err(Error::Invalid(format!("output item {index} has no type")));
        }
        if self
            .open
            .iter()
            .any(|(open, id)| *open == index && item["id"].as_str() != Some(id))
        {
            return Err(Error::Invalid("output item changed its id".into()));
        }
        self.open.retain(|(open, _)| *open != index);
        self.items.push((index, item));
        Ok(())
    }

    fn finish(&mut self, response: Terminal, continues: bool) -> Result<Assistant, Error> {
        if let Some((index, _)) = self.open.first() {
            return Err(Error::Invalid(format!(
                "the response completed with output item {index} unfinished"
            )));
        }
        self.items.sort_by_key(|&(index, _)| index);
        let output = &response.output;
        // The ChatGPT backend sends an empty terminal output after a streamed function call;
        // an empty array repeats nothing, a non-empty one must match what was streamed.
        if !output.is_empty()
            && (output.len() != self.items.len()
                || self.items.iter().enumerate().any(|(at, (index, item))| {
                    usize::try_from(*index) != Ok(at) || output[at] != *item
                }))
        {
            return Err(Error::Invalid(
                "terminal response output does not match completed output items".into(),
            ));
        }
        let parts: Vec<Part> = std::mem::take(&mut self.items)
            .into_iter()
            .map(|(_, item)| part(item))
            .collect::<Result<_, _>>()?;
        let mut item_ids = std::collections::HashSet::new();
        if !parts
            .iter()
            .filter_map(|part| part.replay.as_ref()?["id"].as_str())
            .all(|id| item_ids.insert(id))
        {
            return Err(Error::Invalid("duplicate output item id".into()));
        }
        let refused = parts.iter().any(|part| {
            part.replay.as_ref().is_some_and(|item| {
                item["content"]
                    .as_array()
                    .is_some_and(|content| content.iter().any(|piece| piece["type"] == "refusal"))
            })
        });
        Ok(Assistant {
            protocol: crate::provider::Protocol::Responses,
            provider: self.provider.clone(),
            model: self.model.clone(),
            parts,
            usage: response.usage.as_ref().map(ResponsesUsage::usage),
            continues,
            refused,
            served_model: Some(response.model).filter(|served| *served != self.model),
        })
    }
}

impl Terminal {
    fn validate(&self, status: &str) -> Result<(), Error> {
        if self.id.is_empty() || self.model.is_empty() {
            return Err(Error::Invalid(
                "terminal response without valid id or model".into(),
            ));
        }
        if self.object != "response" {
            return Err(Error::Invalid(
                "terminal response requires object=response".into(),
            ));
        }
        if self.status != status {
            return Err(Error::Invalid(
                "terminal response status conflicts with event type".into(),
            ));
        }
        if !self.error.is_null() || status == "completed" && !self.incomplete_details.is_null() {
            return Err(Error::Invalid(
                "terminal response has conflicting error or incomplete_details".into(),
            ));
        }
        Ok(())
    }
}

fn part(item: Value) -> Result<Part, Error> {
    if item["id"].as_str().is_none_or(str::is_empty) {
        return Err(Error::Invalid("output item without id".into()));
    }
    if item
        .get("status")
        .is_some_and(|status| status != "completed")
    {
        return Err(Error::Invalid(
            "completed output item has non-completed status".into(),
        ));
    }
    let content = match Item::deserialize(&item).map_err(|error| invalid(&error))? {
        Item::Message {
            role,
            status,
            content,
        } => {
            if role != "assistant" || status != "completed" {
                return Err(Error::Invalid(
                    "completed assistant message requires role assistant and status completed"
                        .into(),
                ));
            }
            Content::Text(
                content
                    .iter()
                    .map(|piece| match piece {
                        MessageContent::OutputText { text, .. } => text.as_str(),
                        MessageContent::Refusal { refusal } => refusal.as_str(),
                    })
                    .collect::<String>()
                    .into(),
            )
        }
        Item::Reasoning { summary, content } => {
            let joined = |pieces: &[ReasoningText], kind: &str| {
                pieces
                    .iter()
                    .filter(|piece| piece.kind == kind)
                    .map(|piece| piece.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            Content::Reasoning(
                match joined(&summary, "summary_text") {
                    summary if summary.is_empty() => {
                        joined(content.as_deref().unwrap_or_default(), "reasoning_text")
                    }
                    summary => summary,
                }
                .into(),
            )
        }
        Item::FunctionCall {
            call_id,
            name,
            arguments,
        } => Content::ToolCall(ToolCall {
            id: call_id,
            name,
            arguments: crate::history::Arguments::from_wire(&arguments),
        }),
        Item::WebSearchCall {} => Content::Other,
    };
    Ok(Part {
        content,
        replay: Some(item.into()),
    })
}

pub(super) fn collect_search(parts: &[Part], search: &mut super::search::SearchResult) {
    let item = |part: &Part| {
        part.replay
            .as_deref()
            .and_then(|item| Item::deserialize(item).ok())
    };
    for part in parts {
        let Some(Item::Message { content, .. }) = item(part) else {
            continue;
        };
        for piece in content {
            let MessageContent::OutputText { text, annotations } = piece else {
                continue;
            };
            for annotation in annotations {
                if let Ok(Annotation::UrlCitation {
                    url,
                    title,
                    start_index,
                    end_index,
                }) = Annotation::deserialize(&annotation)
                {
                    let claim = super::search::span(&text, start_index, end_index);
                    search.cite(Some(&url), title.as_deref(), &claim);
                }
            }
        }
    }
    for part in parts {
        let Some(replay) = part
            .replay
            .as_deref()
            .filter(|item| item["type"] == "web_search_call")
        else {
            continue;
        };
        let sources = Sources::deserialize(replay)
            .ok()
            .and_then(|sources| sources.action);
        for source in sources.into_iter().flat_map(|action| action.sources) {
            search.source(source.url.as_deref(), source.title.as_deref());
        }
    }
}
