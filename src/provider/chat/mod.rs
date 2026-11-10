use serde::Deserialize;
use serde_json::{Value, json};

use super::{Delta, Error, Request};
use crate::history::{Assistant, Content, Message, Part, ToolCall, Usage};

const MAX_CALLS: usize = 4096;
const MAX_REASONING_DETAILS: usize = 4096;
const MAX_ANNOTATIONS: usize = 4096;

pub(super) fn body(request: &Request) -> std::io::Result<Value> {
    let mut messages = Vec::new();
    if !request.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": request.instructions }));
    }
    let google = super::at(
        request.provider.endpoint().expect("HTTP provider"),
        "generativelanguage.googleapis.com",
        "/v1beta/openai",
    );
    let turn = request
        .history
        .iter()
        .rposition(|message| !matches!(message, Message::Assistant(_) | Message::ToolResult(_)));
    for (at, message) in request.history.iter().enumerate() {
        match message {
            Message::Notification(note) => {
                messages.push(json!({"role":"user","content":&*note.text}))
            }

            Message::User(_)
            | Message::UserSource(_)
            | Message::Instruction(_)
            | Message::Skill(_) => {
                let text = message.model_text();
                messages.push(json!({ "role": "user", "content": text }));
            }
            Message::Image(image) => {
                let mut content = Vec::new();
                if !image.label.is_empty() {
                    content.push(json!({ "type": "text", "text": image.label }));
                }
                content.push(json!({ "type": "image_url", "image_url": { "url": image.url()? } }));
                messages.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let unsigned_call = google && turn.is_none_or(|turn| at > turn);
                messages.extend(assistant_message(
                    assistant,
                    request.same_model(assistant),
                    unsigned_call,
                ));
            }
            Message::ToolResult(result) => messages.push(json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": result.output,
            })),
        }
    }
    let mut body = json!({
        "model": request.model,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    body["messages"] = Value::Array(messages);
    if let super::Tools::Functions(tools) | super::Tools::Disabled(tools) = request.tools
        && !tools.is_empty()
    {
        body["tools"] = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body["tool_choice"] = "auto".into();
        body["parallel_tool_calls"] = true.into();
    }
    if matches!(request.tools, super::Tools::Disabled(_)) {
        body["tool_choice"] = "none".into();
    }
    if let Some(tokens) = request.max_output_tokens {
        body["max_completion_tokens"] = tokens.into();
    }
    if let Some(effort) = request.reasoning_effort {
        body["reasoning_effort"] = effort.into();
    }
    if let super::Tools::WebSearch = request.tools {
        body["plugins"] = json!([{ "id": "web" }]);
    }
    Ok(body)
}

fn assistant_message(
    assistant: &Assistant,
    same_model: bool,
    mut unsigned_call: bool,
) -> Option<Value> {
    let (mut text, mut metadata, mut calls) = (String::new(), serde_json::Map::new(), Vec::new());
    for part in &assistant.parts {
        match &part.content {
            Content::Text(piece) => text.push_str(piece),
            Content::Reasoning(_) | Content::Other => {
                if same_model && let Some(replay) = &part.replay {
                    for field in ["reasoning", "reasoning_content", "reasoning_details"] {
                        if let Some(value) = replay.get(field) {
                            metadata.insert(field.into(), value.clone());
                        }
                    }
                }
            }
            Content::ToolCall(call) => {
                let mut wire = json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments.wire() },
                });
                let signature = part
                    .replay
                    .as_ref()
                    .filter(|_| same_model)
                    .and_then(|replay| replay.pointer("/extra_content/google/thought_signature"))
                    .cloned();
                // Google's documented stand-in: a current-turn step's first call must carry a signature.
                let signature = match (signature, std::mem::take(&mut unsigned_call)) {
                    (Some(signature), _) => Some(signature),
                    (None, true) => Some("skip_thought_signature_validator".into()),
                    (None, false) => None,
                };
                if let Some(signature) = signature {
                    wire["extra_content"] = json!({"google":{"thought_signature":signature}});
                }
                calls.push(wire);
            }
        }
    }
    if text.is_empty() && calls.is_empty() && metadata.is_empty() {
        return None;
    }
    let mut message = json!({ "role": "assistant", "content": text });
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    message
        .as_object_mut()
        .expect("the message is built as an object")
        .extend(metadata);
    Some(message)
}

#[derive(Deserialize)]
struct Chunk {
    choices: Option<Vec<Choice>>,
    usage: Option<ChatUsage>,
    error: Option<ChatError>,
}

#[derive(Deserialize)]
struct ChatError {
    #[serde(default)]
    code: Value,
    message: Option<String>,
}

#[derive(Deserialize)]
struct Choice {
    // Some compatible servers omit the index of their single choice.
    index: Option<u32>,
    delta: ChoiceDelta,
    finish_reason: Option<FinishReason>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    FunctionCall,
}

#[derive(Deserialize)]
struct ChoiceDelta {
    content: Option<String>,
    refusal: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Vec<Value>>,
    tool_calls: Option<Vec<CallFragment>>,
    function_call: Option<Value>,
    annotations: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct CallFragment {
    index: u64,
    id: Option<String>,
    #[serde(rename = "type")]
    _kind: Option<CallKind>,
    function: Option<FunctionDelta>,
    extra_content: Option<Extra>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallKind {
    Function,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct Extra {
    google: Option<Google>,
}

#[derive(Deserialize)]
struct Google {
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Annotation {
    #[serde(rename = "url_citation")]
    UrlCitation { url_citation: UrlCitation },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct UrlCitation {
    url: String,
    title: Option<String>,
    start_index: Option<usize>,
    end_index: Option<usize>,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    prompt_tokens_details: Option<CachedTokens>,
    completion_tokens_details: Option<ReasoningTokens>,
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct CachedTokens {
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ReasoningTokens {
    reasoning_tokens: Option<u64>,
}

impl ChatUsage {
    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            cache_read_tokens: self
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            cache_write_tokens: None,
            output_tokens: self.completion_tokens,
            reasoning_tokens: self
                .completion_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens),
            cost: self.cost,
            cost_is_estimate: false,
        }
    }
}

fn invalid(error: &serde_json::Error) -> Error {
    Error::Invalid(format!("invalid chat chunk: {error}"))
}

pub(super) struct Stream {
    provider: String,
    model: String,
    text: String,
    reasoning: String,
    metadata: serde_json::Map<String, Value>,
    details: Vec<Value>,
    calls: Vec<Call>,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    annotations: Vec<Value>,
    refused: bool,
}

struct Call {
    index: u64,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    signature: Option<String>,
}

impl Stream {
    pub(super) fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            text: String::new(),
            reasoning: String::new(),
            metadata: serde_json::Map::new(),
            details: Vec::new(),
            calls: Vec::new(),
            finish: None,
            usage: None,
            annotations: Vec::new(),
            refused: false,
        }
    }

    pub(super) fn event(
        &mut self,
        data: &str,
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Option<Assistant>, Error> {
        if data == "[DONE]" {
            return self.finish().map(Some);
        }
        let chunk: Chunk = serde_json::from_str(data).map_err(|error| invalid(&error))?;
        if let Some(error) = chunk.error {
            if error.code == super::CONTEXT_LENGTH_EXCEEDED {
                return Err(Error::ContextOverflow(
                    error.message.unwrap_or_else(|| "no message".into()),
                ));
            }
            return Err(Error::Failed {
                code: error
                    .code
                    .as_str()
                    .map_or_else(|| error.code.to_string(), str::to_owned),
                message: error.message.unwrap_or_else(|| "no message".into()),
            });
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.usage());
        }
        let mut choices = match chunk.choices {
            Some(choices) => choices,
            None if chunk.usage.is_some() => return Ok(None),
            None => return Err(Error::Invalid("chat chunk without choices array".into())),
        };
        let Some(choice) = choices.pop() else {
            return Ok(None);
        };
        if !choices.is_empty() || choice.index.is_some_and(|index| index != 0) {
            return Err(Error::Invalid(
                "expected exactly one chat choice with index 0".into(),
            ));
        }
        if self.finish.is_some() {
            return Err(Error::Invalid(
                "chat choice arrived after its finish reason".into(),
            ));
        }
        let delta = choice.delta;
        if delta.function_call.is_some_and(|call| !call.is_null()) {
            return Err(Error::Invalid(
                "deprecated function_call is unsupported; use function tools".into(),
            ));
        }
        self.refused |= delta
            .refusal
            .as_ref()
            .is_some_and(|refusal| !refusal.is_empty());
        for text in [delta.content, delta.refusal].into_iter().flatten() {
            on_delta(Delta::Text(&text));
            self.text.push_str(&text);
        }
        let direct_reasoning = delta.reasoning.is_some() || delta.reasoning_content.is_some();
        for (field, reasoning) in [
            ("reasoning_content", delta.reasoning_content),
            ("reasoning", delta.reasoning),
        ] {
            if let Some(reasoning) = reasoning {
                on_delta(Delta::Reasoning(&reasoning));
                self.reasoning.push_str(&reasoning);
                if let Value::String(text) = self
                    .metadata
                    .entry(field)
                    .or_insert_with(|| Value::String(String::new()))
                {
                    text.push_str(&reasoning);
                }
            }
        }
        for detail in delta.reasoning_details.into_iter().flatten() {
            self.detail(&detail)?;
            if !direct_reasoning {
                let text = match detail["type"].as_str() {
                    Some("reasoning.text") => detail["text"].as_str(),
                    Some("reasoning.summary") => detail["summary"].as_str(),
                    _ => None,
                };
                if let Some(text) = text {
                    on_delta(Delta::Reasoning(text));
                    self.reasoning.push_str(text);
                }
            }
        }
        for annotation in delta.annotations.into_iter().flatten() {
            Annotation::deserialize(&annotation).map_err(|error| invalid(&error))?;
            if self.annotations.len() == MAX_ANNOTATIONS {
                return Err(Error::Limit("too many annotations".into()));
            }
            self.annotations.push(annotation);
        }
        for fragment in delta.tool_calls.into_iter().flatten() {
            if let Some(arguments) = fragment
                .function
                .as_ref()
                .and_then(|function| function.arguments.as_ref())
            {
                on_delta(Delta::ToolBytes(arguments.len()));
            }
            self.fragment(fragment)?;
        }
        self.finish = choice.finish_reason;
        Ok(None)
    }

    fn detail(&mut self, detail: &Value) -> Result<(), Error> {
        let payload = match detail["type"].as_str() {
            Some("reasoning.text") => "text",
            Some("reasoning.summary") => "summary",
            Some("reasoning.encrypted") => "data",
            _ => return Err(Error::Invalid("unsupported reasoning_details type".into())),
        };
        if detail
            .get("index")
            .is_some_and(|index| !index.is_null() && index.as_u64().is_none())
        {
            return Err(Error::Invalid(
                "reasoning detail index must be a nonnegative integer".into(),
            ));
        }
        for field in [payload, "signature"] {
            if detail
                .get(field)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                return Err(Error::Invalid(
                    "reasoning detail payload must be a string".into(),
                ));
            }
        }
        let known = detail["index"]
            .as_u64()
            .filter(|_| payload != "data")
            .and_then(|index| {
                self.details.iter_mut().find(|known| {
                    known["index"].as_u64() == Some(index)
                        && known["type"] == detail["type"]
                        && (detail["id"].is_null()
                            || known["id"].is_null()
                            || known["id"] == detail["id"])
                })
            });
        if let (Some(known), Some(detail)) = (known, detail.as_object()) {
            for (field, value) in detail {
                if value.is_null() {
                    continue;
                }
                if field == payload {
                    let piece = value.as_str().ok_or_else(|| {
                        Error::Invalid("reasoning detail payload must be a string".into())
                    })?;
                    if let Some(Value::String(text)) = known.get_mut(field) {
                        text.push_str(piece);
                    } else {
                        known[field] = piece.into();
                    }
                } else if known.get(field).is_none_or(Value::is_null) {
                    known[field] = value.clone();
                } else if known[field] != *value {
                    return Err(Error::Invalid(
                        "reasoning detail changed its metadata".into(),
                    ));
                }
            }
        } else {
            if self.details.len() == MAX_REASONING_DETAILS {
                return Err(Error::Limit("too many reasoning details".into()));
            }
            self.details.push(detail.clone());
        }
        Ok(())
    }

    fn fragment(&mut self, fragment: CallFragment) -> Result<(), Error> {
        let CallFragment {
            index,
            id,
            _kind,
            function,
            extra_content,
        } = fragment;
        let position = match self.calls.iter().position(|call| call.index == index) {
            Some(position) => position,
            None if self.calls.len() == MAX_CALLS => {
                return Err(Error::Limit(format!("more than {MAX_CALLS} tool calls")));
            }
            None => {
                self.calls.push(Call {
                    index,
                    id: None,
                    name: None,
                    arguments: None,
                    signature: None,
                });
                self.calls.len() - 1
            }
        };
        let call = &mut self.calls[position];
        let (name, arguments) =
            function.map_or((None, None), |function| (function.name, function.arguments));
        if let Some(arguments) = arguments {
            call.arguments.get_or_insert_default().push_str(&arguments);
        }
        if let Some(signature) = extra_content.and_then(|extra| extra.google?.thought_signature) {
            if call
                .signature
                .as_ref()
                .is_some_and(|known| *known != signature)
            {
                return Err(Error::Invalid(
                    "tool call changed its thought_signature".into(),
                ));
            }
            call.signature = Some(signature);
        }
        for (field, value) in [(&mut call.id, id), (&mut call.name, name)] {
            match (field.as_deref(), value.filter(|value| !value.is_empty())) {
                (_, None) => {}
                (None, Some(value)) => *field = Some(value),
                (Some(known), Some(value)) if known == value => {}
                (Some(_), Some(_)) => {
                    return Err(Error::Invalid(format!(
                        "tool call {index} changed its id or name"
                    )));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Assistant, Error> {
        match self.finish {
            None => {
                return Err(Error::Invalid(
                    "stream ended without a finish reason".into(),
                ));
            }
            Some(FinishReason::Length) => return Err(Error::Incomplete("length".into())),
            Some(FinishReason::ContentFilter) => {
                return Err(Error::Failed {
                    code: "content_filter".into(),
                    message: "the provider filtered the response".into(),
                });
            }
            Some(FinishReason::FunctionCall) => {
                return Err(Error::Invalid(
                    "deprecated function_call finish reason is unsupported; use function tools"
                        .into(),
                ));
            }
            Some(FinishReason::Stop) if self.calls.is_empty() => {}
            Some(FinishReason::ToolCalls) if !self.calls.is_empty() => {}
            Some(FinishReason::Stop | FinishReason::ToolCalls) => {
                return Err(Error::Invalid(
                    "finish reason is inconsistent with tool calls".into(),
                ));
            }
        }
        let mut parts = Vec::new();
        if !self.annotations.is_empty() {
            parts.push(Part {
                content: Content::Other,
                replay: Some(
                    json!({ "annotations": std::mem::take(&mut self.annotations) }).into(),
                ),
            });
        }
        if !self.details.is_empty() {
            if self
                .details
                .iter()
                .all(|detail| detail["index"].as_u64().is_some())
            {
                self.details.sort_by_key(|detail| detail["index"].as_u64());
            }
            self.metadata.insert(
                "reasoning_details".into(),
                std::mem::take(&mut self.details).into(),
            );
        }
        if !self.reasoning.is_empty() || !self.metadata.is_empty() {
            parts.push(Part {
                content: Content::Reasoning(std::mem::take(&mut self.reasoning).into()),
                replay: Some(Value::Object(std::mem::take(&mut self.metadata)).into()),
            });
        }
        if !self.text.is_empty() {
            parts.push(Part {
                content: Content::Text(std::mem::take(&mut self.text).into()),
                replay: None,
            });
        }
        self.calls.sort_by_key(|call| call.index);
        for call in std::mem::take(&mut self.calls) {
            let (Some(id), Some(name)) = (call.id, call.name) else {
                return Err(Error::Invalid(format!(
                    "tool call {} without id or name",
                    call.index
                )));
            };
            let raw = call
                .arguments
                .ok_or_else(|| Error::Invalid("tool call without arguments".into()))?;
            parts.push(Part {
                content: Content::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: crate::history::Arguments::from_wire(&raw),
                }),
                replay: call.signature.map(|signature| {
                    json!({"extra_content":{"google":{"thought_signature":signature}}}).into()
                }),
            });
        }
        Ok(Assistant {
            protocol: crate::provider::Protocol::Chat,
            provider: self.provider.clone(),
            model: self.model.clone(),
            parts,
            usage: self.usage,
            continues: false,
            refused: self.refused,
            served_model: None,
        })
    }
}

pub(super) fn collect_search(parts: &[Part], search: &mut super::search::SearchResult) {
    let text: String = parts
        .iter()
        .filter_map(|part| match &part.content {
            Content::Text(text) => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    for part in parts {
        let annotations = part
            .replay
            .as_deref()
            .and_then(|replay| replay["annotations"].as_array());
        for annotation in annotations.into_iter().flatten() {
            if let Ok(Annotation::UrlCitation { url_citation }) =
                Annotation::deserialize(annotation)
            {
                let claim =
                    super::search::span(&text, url_citation.start_index, url_citation.end_index);
                search.cite(
                    Some(&url_citation.url),
                    url_citation.title.as_deref(),
                    &claim,
                );
            }
        }
    }
}
