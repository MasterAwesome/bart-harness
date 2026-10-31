use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Delta, Error, Request};
use crate::history::{Arguments, Assistant, Content, Message, Part, ToolCall, Usage};

const VERSION: &str = "2023-06-01";
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub(super) const DEFAULT_MAX_TOKENS: u64 = 32_000;
pub(super) const PAGING: super::Paging = super::Paging {
    size: "limit",
    cursor: "after_id",
};

pub(super) fn headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder.header("anthropic-version", VERSION)
}

pub(super) fn authorize(builder: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    builder.header("x-api-key", key)
}

#[derive(Deserialize)]
struct Paged {
    has_more: bool,
    last_id: Option<String>,
}

pub(super) fn models(list: &Value) -> Result<super::Page, Error> {
    let paged = Paged::deserialize(list)
        .map_err(|error| Error::Invalid(format!("invalid model list: {error}")))?;
    let next = match paged {
        Paged {
            has_more: true,
            last_id: None,
        } => {
            return Err(Error::Invalid(
                "the model list has more pages but no cursor".into(),
            ));
        }
        Paged { has_more, last_id } => last_id.filter(|_| has_more),
    };
    let mut models = super::data_models(list)?;
    for model in &mut models {
        let item = list["data"].as_array().and_then(|items| {
            items
                .iter()
                .find(|item| item["id"].as_str() == Some(&model.id))
        });
        let capabilities = item.map(|item| &item["capabilities"]);
        model.max_output_tokens = item.and_then(|item| item["max_tokens"].as_u64());
        model.efforts = capabilities
            .filter(|caps| {
                caps["thinking"]["types"]["adaptive"]["supported"] == true
                    && caps["effort"]["supported"] == true
            })
            .map(|caps| {
                EFFORTS
                    .into_iter()
                    .filter(|effort| caps["effort"][*effort]["supported"] == true)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    }
    Ok(super::Page { models, next })
}
const MAX_BLOCKS: usize = 4096;

pub(super) fn body(request: &Request, max_tokens: u64) -> std::io::Result<Value> {
    let cache = json!({ "type": "ephemeral" });
    let mut messages = messages(request)?;
    if let Some(block) = messages
        .last_mut()
        .filter(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_array_mut())
        .and_then(|blocks| blocks.last_mut())
        .filter(|block| {
            !matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        })
    {
        block["cache_control"] = cache.clone();
    }
    let mut body = json!({
        "model": request.model,
        "max_tokens": max_tokens,
        "stream": true,
    });
    body["messages"] = Value::Array(messages);
    if !request.instructions.is_empty() {
        body["system"] =
            json!([{ "type": "text", "text": request.instructions, "cache_control": cache }]);
    }
    if let super::Tools::Functions(tools) | super::Tools::Disabled(tools) = request.tools
        && !tools.is_empty()
    {
        let mut tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache;
        }
        body["tools"] = tools.into();
    } else if let super::Tools::WebSearch = request.tools {
        body["tools"] = json!([{ "type": "web_search_20250305", "name": "web_search" }]);
    }
    if matches!(request.tools, super::Tools::Disabled(_)) {
        body["tool_choice"] = json!({ "type": "none" });
    }
    if let Some(effort) = request.reasoning_effort {
        body["thinking"] = json!({ "type": "adaptive" });
        if request
            .provider
            .endpoint()
            .is_some_and(|url| super::at(url, "api.anthropic.com", "/v1"))
        {
            body["thinking"]["display"] = if request.summaries {
                "summarized"
            } else {
                "omitted"
            }
            .into();
        }
        body["output_config"] = json!({ "effort": effort });
    }
    Ok(body)
}

fn messages(request: &Request) -> std::io::Result<Vec<Value>> {
    let mut messages: Vec<Value> = Vec::new();
    let mut push = |role: &str, block: Value| match messages.last_mut() {
        Some(last) if last["role"] == role => last["content"]
            .as_array_mut()
            .expect("content is built as an array")
            .push(block),
        _ => messages.push(json!({ "role": role, "content": [block] })),
    };
    for message in request.history {
        match message {
            Message::Notification(note) => {
                push("user", json!({"type":"text","text":note.model_text()}))
            }

            Message::User(_)
            | Message::UserSource(_)
            | Message::Instruction(_)
            | Message::Skill(_) => {
                let text = message.model_text();
                if !text.is_empty() {
                    push("user", json!({ "type": "text", "text": text }));
                }
            }
            Message::Image(image) => {
                if !image.label.is_empty() {
                    push("user", json!({ "type": "text", "text": image.label }));
                }
                push(
                    "user",
                    json!({ "type": "image", "source": {
                        "type": "base64", "media_type": image.media_type, "data": image.base64()?,
                    }}),
                );
            }
            Message::ToolResult(result) => push(
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": result.output,
                    "is_error": result.is_error,
                }),
            ),
            Message::Assistant(assistant) => {
                let same = request.same_model(assistant);
                for part in &assistant.parts {
                    match (&part.content, part.replay.as_deref().filter(|_| same)) {
                        (
                            Content::Text(_) | Content::Reasoning(_) | Content::Other,
                            Some(replay),
                        ) => {
                            push("assistant", Value::clone(replay));
                        }
                        (Content::Text(text), _) if !text.is_empty() => {
                            push("assistant", json!({ "type": "text", "text": text }));
                        }
                        (Content::ToolCall(call), _) => {
                            if let Some(text) = call.arguments.rejected_replay(&call.id) {
                                push("assistant", json!({"type":"text", "text":text}));
                            }
                            push(
                                "assistant",
                                json!({
                                    "type":"tool_use", "id":call.id, "name":call.name,
                                    "input":call.arguments.as_object().map_or_else(|| json!({}), |input| Value::Object(input.clone())),
                                }),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(messages)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageStart {
        message: Started,
    },
    ContentBlockStart {
        index: usize,
        content_block: Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: BlockDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: Stopped,
        #[serde(default)]
        usage: Tokens,
    },
    MessageStop,
    Error {
        error: Failure,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct Started {
    #[serde(rename = "id")]
    _id: String,
    model: String,
    #[serde(default)]
    usage: Tokens,
}

#[derive(Deserialize)]
struct Stopped {
    stop_reason: Option<StopReason>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StopReason {
    EndTurn,
    StopSequence,
    PauseTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    ModelContextWindowExceeded,
}

#[derive(Deserialize)]
struct Failure {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[allow(
    clippy::struct_field_names,
    reason = "fields mirror the provider's wire names"
)]
#[derive(Clone, Copy, Default, Deserialize)]
struct Tokens {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Clone, Copy, Deserialize)]
struct OutputDetails {
    thinking_tokens: Option<u64>,
}

impl Tokens {
    fn update(&mut self, later: Self) {
        self.input_tokens = later.input_tokens.or(self.input_tokens);
        self.output_tokens = later.output_tokens.or(self.output_tokens);
        self.output_tokens_details = later.output_tokens_details.or(self.output_tokens_details);
        self.cache_read_input_tokens = later
            .cache_read_input_tokens
            .or(self.cache_read_input_tokens);
        self.cache_creation_input_tokens = later
            .cache_creation_input_tokens
            .or(self.cache_creation_input_tokens);
    }

    fn usage(self) -> Option<Usage> {
        let (read, write) = (
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
        );
        Some(Usage {
            input_tokens: self
                .input_tokens?
                .saturating_add(read.unwrap_or(0))
                .saturating_add(write.unwrap_or(0)),
            cache_read_tokens: read,
            cache_write_tokens: write,
            output_tokens: self.output_tokens?,
            reasoning_tokens: self
                .output_tokens_details
                .and_then(|details| details.thinking_tokens),
            cost: None,
            cost_is_estimate: false,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum BlockDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "citations_delta")]
    Citations { citation: Value },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Kind {
    Text {
        #[serde(rename = "text")]
        _text: String,
        citations: Option<Vec<Citation>>,
    },
    Thinking {
        #[serde(rename = "thinking")]
        _thinking: String,
    },
    RedactedThinking {
        #[serde(rename = "data")]
        _data: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    ServerToolUse {
        #[serde(rename = "id")]
        _id: String,
        #[serde(rename = "name")]
        _name: String,
        #[serde(rename = "input")]
        _input: Map<String, Value>,
    },
    WebSearchToolResult {
        #[serde(rename = "tool_use_id")]
        _tool_use_id: String,
        content: SearchContent,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Citation {
    WebSearchResultLocation {
        url: String,
        title: Option<String>,
        #[serde(rename = "encrypted_index")]
        _encrypted_index: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SearchContent {
    Results(Vec<SearchResult>),
    Failed(SearchFailure),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename = "web_search_result")]
struct SearchResult {
    url: String,
    title: Option<String>,
    #[serde(rename = "encrypted_content")]
    _encrypted_content: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename = "web_search_tool_result_error")]
struct SearchFailure {
    error_code: String,
}

fn invalid(error: &serde_json::Error) -> Error {
    Error::Invalid(format!("invalid Messages event: {error}"))
}

pub(super) struct Stream {
    provider: String,
    model: String,
    served: Option<String>,
    tokens: Option<Tokens>,
    blocks: Vec<Block>,
    stop: Option<StopReason>,
}

struct Block {
    value: Value,
    kind: Kind,
    input: String,
    open: bool,
}

impl Stream {
    pub(super) fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            served: None,
            tokens: None,
            blocks: Vec::new(),
            stop: None,
        }
    }

    pub(super) fn event(
        &mut self,
        data: &str,
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Option<Assistant>, Error> {
        let event: Event = serde_json::from_str(data).map_err(|error| invalid(&error))?;
        let started = self.tokens.is_some();
        match event {
            Event::Ignored => {}
            Event::Error { error } => {
                return Err(Error::Failed {
                    code: error.kind,
                    message: error.message,
                });
            }
            Event::MessageStart { message } if !started => {
                self.served = (message.model != self.model).then_some(message.model);
                self.tokens = Some(message.usage);
            }
            Event::MessageStart { .. } => {
                return Err(Error::Invalid("message started twice".into()));
            }
            _ if !started => return Err(Error::Invalid("event before message_start".into())),
            Event::ContentBlockStart {
                index,
                content_block,
            } => {
                self.start(index, content_block)?;
                if matches!(self.blocks[index].kind, Kind::Text { .. }) {
                    on_delta(Delta::MessageBoundary);
                }
            }
            Event::ContentBlockDelta { index, delta } => {
                self.open(index)?.delta(delta, on_delta)?;
            }
            Event::ContentBlockStop { index } => {
                let block = self.open(index)?;
                block.open = false;
                if matches!(block.kind, Kind::Text { .. }) {
                    on_delta(Delta::MessageBoundary);
                }
            }
            Event::MessageDelta { delta, usage } => {
                self.stop = delta.stop_reason.or(self.stop);
                if let Some(tokens) = &mut self.tokens {
                    tokens.update(usage);
                }
            }
            Event::MessageStop => return self.finish().map(Some),
        }
        Ok(None)
    }

    fn start(&mut self, index: usize, value: Value) -> Result<(), Error> {
        if index != self.blocks.len() {
            return Err(Error::Invalid("content blocks must start in order".into()));
        }
        if self.blocks.len() == MAX_BLOCKS {
            return Err(Error::Limit(format!(
                "more than {MAX_BLOCKS} content blocks"
            )));
        }
        let kind = Kind::deserialize(&value).map_err(|error| invalid(&error))?;
        self.blocks.push(Block {
            value,
            kind,
            input: String::new(),
            open: true,
        });
        Ok(())
    }

    fn open(&mut self, index: usize) -> Result<&mut Block, Error> {
        self.blocks
            .get_mut(index)
            .filter(|block| block.open)
            .ok_or_else(|| Error::Invalid("content event for a block that is not open".into()))
    }

    fn finish(&mut self) -> Result<Assistant, Error> {
        if self.blocks.iter().any(|block| block.open) {
            return Err(Error::Invalid(
                "message stopped with an open content block".into(),
            ));
        }
        let calls = self
            .blocks
            .iter()
            .any(|block| matches!(block.kind, Kind::ToolUse { .. }));
        match self.stop {
            Some(
                StopReason::EndTurn
                | StopReason::StopSequence
                | StopReason::PauseTurn
                | StopReason::Refusal,
            ) if !calls => {}
            Some(StopReason::ToolUse) if calls => {}
            Some(StopReason::MaxTokens) => return Err(Error::Incomplete("max_tokens".into())),
            Some(StopReason::ModelContextWindowExceeded) => {
                return Err(Error::ContextOverflow(
                    "generation reached the model's context window".into(),
                ));
            }
            Some(_) => {
                return Err(Error::Invalid(
                    "stop reason is inconsistent with tool calls".into(),
                ));
            }
            None => {
                return Err(Error::Invalid(
                    "message stopped without a stop reason".into(),
                ));
            }
        }
        let parts = std::mem::take(&mut self.blocks)
            .into_iter()
            .map(Block::part)
            .collect::<Result<_, _>>()?;
        Ok(Assistant {
            protocol: crate::provider::Protocol::Messages,
            provider: self.provider.clone(),
            model: self.model.clone(),
            parts,
            usage: self.tokens.take().and_then(Tokens::usage),
            continues: self.stop == Some(StopReason::PauseTurn),
            refused: self.stop == Some(StopReason::Refusal),
            served_model: self.served.take(),
        })
    }
}

impl Block {
    fn delta(
        &mut self,
        delta: BlockDelta,
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<(), Error> {
        let (field, piece) = match (&self.kind, delta) {
            (Kind::Text { .. }, BlockDelta::Text { text }) => ("text", text),
            (Kind::Thinking { .. }, BlockDelta::Thinking { thinking }) => ("thinking", thinking),
            (Kind::Thinking { .. }, BlockDelta::Signature { signature }) => {
                ("signature", signature)
            }
            (
                Kind::ToolUse { .. } | Kind::ServerToolUse { .. },
                BlockDelta::InputJson { partial_json },
            ) => {
                self.input.push_str(&partial_json);
                on_delta(Delta::ToolBytes(partial_json.len()));
                return Ok(());
            }
            (Kind::Text { .. }, BlockDelta::Citations { citation }) => {
                Citation::deserialize(&citation).map_err(|error| invalid(&error))?;
                let citations = &mut self.value["citations"];
                if !citations.is_array() {
                    *citations = json!([]);
                }
                citations
                    .as_array_mut()
                    .expect("citations were just made an array")
                    .push(citation);
                return Ok(());
            }
            _ => {
                return Err(Error::Invalid(
                    "content delta does not match its block".into(),
                ));
            }
        };
        match &mut self.value[field] {
            Value::String(text) => text.push_str(&piece),
            value => *value = piece.clone().into(),
        }
        match field {
            "text" => on_delta(Delta::Text(&piece)),
            "thinking" if !piece.is_empty() => on_delta(Delta::Reasoning(&piece)),
            _ => {}
        }
        Ok(())
    }

    fn part(self) -> Result<Part, Error> {
        let mut value = self.value;
        let input = || -> Result<Map<String, Value>, Error> {
            let input = if self.input.is_empty() {
                "{}"
            } else {
                &self.input
            };
            serde_json::from_str(input)
                .map_err(|_| Error::Invalid("tool input is not a JSON object".into()))
        };
        let content = match self.kind {
            Kind::Text { .. } => match value["text"].as_str() {
                Some(text) if !text.is_empty() => Content::Text(text.to_owned().into()),
                _ => Content::Other,
            },
            Kind::Thinking { .. } => {
                if !value["signature"].is_string() {
                    return Err(Error::Invalid("thinking block without signature".into()));
                }
                match value["thinking"].as_str() {
                    Some(text) if !text.is_empty() => Content::Reasoning(text.to_owned().into()),
                    _ => Content::Other,
                }
            }
            Kind::RedactedThinking { .. } | Kind::WebSearchToolResult { .. } => Content::Other,
            Kind::ServerToolUse { .. } => {
                if !self.input.is_empty() {
                    value["input"] = Value::Object(input()?);
                }
                Content::Other
            }
            Kind::ToolUse { id, name } => {
                let arguments = Arguments::from_wire(if self.input.is_empty() {
                    "{}"
                } else {
                    &self.input
                });
                if arguments.as_object().is_none() {
                    return Err(Error::Invalid("tool input is not a JSON object".into()));
                }
                return Ok(Part {
                    content: Content::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                    }),
                    replay: None,
                });
            }
        };
        Ok(Part {
            content,
            replay: Some(value.into()),
        })
    }
}

pub(super) fn collect_search(parts: &[Part], search: &mut super::search::SearchResult) {
    let kind = |part: &Part| {
        part.replay
            .as_deref()
            .and_then(|value| Kind::deserialize(value).ok())
    };
    for part in parts {
        if let (Content::Text(text), Some(Kind::Text { citations, .. })) =
            (&part.content, kind(part))
        {
            for citation in citations.into_iter().flatten() {
                if let Citation::WebSearchResultLocation { url, title, .. } = citation {
                    search.cite(Some(&url), title.as_deref(), text);
                }
            }
        }
    }
    for part in parts {
        match kind(part) {
            Some(Kind::WebSearchToolResult {
                content: SearchContent::Results(results),
                ..
            }) => {
                for result in results {
                    search.source(Some(&result.url), result.title.as_deref());
                }
            }
            Some(Kind::WebSearchToolResult {
                content: SearchContent::Failed(failure),
                ..
            }) => {
                search.errors.push(failure.error_code);
            }
            _ => {}
        }
    }
}
