use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use super::{Delta, Error, Request};
use crate::history::{Arguments, Assistant, Content, Message, Part, ToolCall, Usage};

const MAX_PARTS: usize = 4096;
// Google's documented stand-in: a current-turn step's first call must carry a signature.
const STAND_IN: &str = "skip_thought_signature_validator";
const EFFORTS: [&str; 2] = ["low", "high"];
pub(super) const PAGING: super::Paging = super::Paging {
    size: "pageSize",
    cursor: "pageToken",
};

pub(super) fn authorize(builder: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    builder.header("x-goog-api-key", key)
}

pub(super) fn path(model: &str) -> Result<String, Error> {
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return Err(Error::Failed {
            code: "model".into(),
            message: "a Gemini model ID uses only letters, digits, '-', '.' and '_'".into(),
        });
    }
    Ok(format!("/models/{model}:streamGenerateContent"))
}

// ProtoJSON parsers read a null field as unset, like an absent one.
fn unset<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    input: D,
) -> Result<T, D::Error> {
    Option::deserialize(input).map(Option::unwrap_or_default)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelList {
    #[serde(default, deserialize_with = "unset")]
    models: Vec<Value>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedModel {
    name: String,
    display_name: Option<String>,
    input_token_limit: Option<u64>,
    output_token_limit: Option<u64>,
    #[serde(default, deserialize_with = "unset")]
    supported_generation_methods: Vec<String>,
    #[serde(default, deserialize_with = "unset")]
    thinking: bool,
}

pub(super) fn models(list: &Value) -> Result<super::Page, Error> {
    let list = ModelList::deserialize(list)
        .map_err(|error| Error::Invalid(format!("invalid model list: {error}")))?;
    let models = list
        .models
        .iter()
        .filter_map(|model| ListedModel::deserialize(model).ok())
        .filter(|model| {
            model
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
        })
        .filter_map(|model| {
            Some(super::Model {
                id: model.name.strip_prefix("models/")?.to_owned(),
                visibility: super::Visibility::Listed,
                name: model.display_name,
                efforts: if model.thinking {
                    EFFORTS.map(String::from).to_vec()
                } else {
                    Vec::new()
                },
                default_effort: None,
                summaries: None,
                verbosity: None,
                images: None,
                context_window: model.input_token_limit,
                max_context_window: None,
                max_output_tokens: model.output_token_limit,
            })
        })
        .collect();
    Ok(super::Page {
        models,
        next: list.next_page_token,
    })
}
const GENERATED_ID: &str = "bart-gemini-";

pub(super) fn body(request: &Request) -> std::io::Result<Value> {
    let names: HashMap<&str, &str> = request
        .history
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(&assistant.parts),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match &part.content {
            Content::ToolCall(call) => Some((call.id.as_str(), call.name.as_str())),
            _ => None,
        })
        .collect();
    let mut contents: Vec<Value> = Vec::new();
    let mut push = |role: &str, part: Value| match contents.last_mut() {
        Some(last) if last["role"] == role => last["parts"]
            .as_array_mut()
            .expect("parts are built as an array")
            .push(part),
        _ => contents.push(json!({ "role": role, "parts": [part] })),
    };
    let turn = request
        .history
        .iter()
        .rposition(|message| !matches!(message, Message::Assistant(_) | Message::ToolResult(_)));
    for (at, message) in request.history.iter().enumerate() {
        match message {
            Message::Notification(note) => push("user", json!({"text":&*note.text})),

            Message::User(_)
            | Message::UserSource(_)
            | Message::Instruction(_)
            | Message::Skill(_) => {
                let text = message.model_text();
                if !text.is_empty() {
                    push("user", json!({ "text": text }));
                }
            }
            Message::Image(image) => {
                if !image.label.is_empty() {
                    push("user", json!({ "text": image.label }));
                }
                push(
                    "user",
                    json!({ "inlineData": { "mimeType": image.media_type, "data": image.base64()? } }),
                );
            }
            Message::ToolResult(result) => {
                let mut response = json!({
                    "name": names.get(result.call_id.as_str()).copied().unwrap_or_default(),
                    "response": { if result.is_error { "error" } else { "output" }: result.output },
                });
                if !result.call_id.starts_with(GENERATED_ID) {
                    response["id"] = result.call_id.as_str().into();
                }
                push("user", json!({ "functionResponse": response }));
            }
            Message::Assistant(assistant) => {
                let same = request.same_model(assistant);
                let mut unsigned_call = turn.is_none_or(|turn| at > turn);
                for part in &assistant.parts {
                    if matches!(part.content, Content::Other) {
                        continue;
                    }
                    if same && let Some(replay) = &part.replay {
                        let mut replay = Value::clone(replay);
                        if replay.get("functionCall").is_some()
                            && std::mem::take(&mut unsigned_call)
                            && replay.get("thoughtSignature").is_none()
                        {
                            replay["thoughtSignature"] = STAND_IN.into();
                        }
                        push("model", replay);
                        continue;
                    }
                    match &part.content {
                        Content::Text(text) if !text.is_empty() => {
                            push("model", json!({ "text": text }));
                        }
                        Content::ToolCall(call) => {
                            if let Some(text) = call.arguments.rejected_replay(&call.id) {
                                push("model", json!({"text": text}));
                            }
                            let mut wire = json!({
                                "name": call.name,
                                "args": call.arguments.as_object().map_or_else(|| json!({}), |args| Value::Object(args.clone())),
                            });
                            if !call.id.starts_with(GENERATED_ID) {
                                wire["id"] = call.id.as_str().into();
                            }
                            let mut part = json!({ "functionCall": wire });
                            if std::mem::take(&mut unsigned_call) {
                                part["thoughtSignature"] = STAND_IN.into();
                            }
                            push("model", part);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let mut body = json!({});
    body["contents"] = Value::Array(contents);
    if !request.instructions.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": request.instructions }] });
    }
    if let Some(tools) = tools(request.tools) {
        body["tools"] = tools;
    }
    if matches!(request.tools, super::Tools::Disabled(_)) {
        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "NONE" } });
    }
    if let Some(effort) = request.reasoning_effort {
        body["generationConfig"]["thinkingConfig"] = json!({
            "includeThoughts": request.summaries,
            "thinkingLevel": effort,
        });
    }
    if let Some(tokens) = request.max_output_tokens {
        body["generationConfig"]["maxOutputTokens"] = tokens.into();
    }
    Ok(body)
}

fn tools(tools: super::Tools) -> Option<Value> {
    match tools {
        super::Tools::WebSearch => Some(json!([{ "google_search": {} }])),
        super::Tools::Functions([]) | super::Tools::Disabled([]) => None,
        super::Tools::Functions(tools) | super::Tools::Disabled(tools) => Some(
            json!([{ "functionDeclarations": tools.iter().map(|tool| json!({
            "name": tool.name,
            "description": tool.description,
            "parametersJsonSchema": tool.parameters,
        })).collect::<Vec<_>>() }]),
        ),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Chunk {
    #[serde(default, deserialize_with = "unset")]
    candidates: Vec<Candidate>,
    usage_metadata: Option<UsageMetadata>,
    model_version: Option<String>,
    response_id: Option<String>,
    prompt_feedback: Option<Feedback>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    status: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Feedback {
    block_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    #[serde(default, deserialize_with = "unset")]
    index: u32,
    content: Option<Parts>,
    finish_reason: Option<String>,
    grounding_metadata: Option<Value>,
}

#[derive(Deserialize)]
struct Parts {
    #[serde(default, deserialize_with = "unset")]
    parts: Vec<Value>,
}

const PAYLOADS: [&str; 9] = [
    "text",
    "inlineData",
    "functionCall",
    "functionResponse",
    "fileData",
    "executableCode",
    "codeExecutionResult",
    "toolCall",
    "toolResponse",
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartView {
    text: Option<String>,
    #[serde(default, deserialize_with = "unset")]
    thought: bool,
    thought_signature: Option<String>,
    function_call: Option<FunctionCall>,
}

#[derive(Deserialize)]
struct FunctionCall {
    id: Option<String>,
    name: String,
    #[serde(rename = "args")]
    _args: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grounding {
    #[serde(default, deserialize_with = "unset")]
    grounding_chunks: Vec<GroundingChunk>,
    #[serde(default, deserialize_with = "unset")]
    grounding_supports: Vec<GroundingSupport>,
}

#[derive(Deserialize)]
struct GroundingChunk {
    web: Option<Web>,
}

#[derive(Deserialize)]
struct Web {
    uri: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroundingSupport {
    segment: Option<Segment>,
    #[serde(default, deserialize_with = "unset")]
    grounding_chunk_indices: Vec<u32>,
}

#[derive(Deserialize)]
struct Segment {
    #[serde(default, deserialize_with = "unset")]
    text: String,
}

#[allow(
    clippy::struct_field_names,
    reason = "fields mirror the provider's wire names"
)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: u64,
    cached_content_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    thoughts_token_count: Option<u64>,
}

impl UsageMetadata {
    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.prompt_token_count,
            cache_read_tokens: self.cached_content_token_count,
            cache_write_tokens: None,
            output_tokens: self
                .candidates_token_count
                .unwrap_or(0)
                .saturating_add(self.thoughts_token_count.unwrap_or(0)),
            reasoning_tokens: self.thoughts_token_count,
            cost: None,
            cost_is_estimate: false,
        }
    }
}

fn invalid(error: &serde_json::Error) -> Error {
    Error::Invalid(format!("invalid Gemini chunk: {error}"))
}

pub(super) struct Stream {
    provider: String,
    model: String,
    parts: Vec<Value>,
    usage: Option<Usage>,
    served: Option<String>,
    response: Option<String>,
    grounding: Vec<Value>,
}

impl Stream {
    pub(super) fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            parts: Vec::new(),
            usage: None,
            served: None,
            response: None,
            grounding: Vec::new(),
        }
    }

    pub(super) fn event(
        &mut self,
        data: &str,
        on_delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Option<Assistant>, Error> {
        let chunk: Chunk = serde_json::from_str(data).map_err(|error| invalid(&error))?;
        if let Some(error) = chunk.error {
            return Err(Error::Failed {
                code: error.status.unwrap_or_else(|| "unknown".into()),
                message: error.message.unwrap_or_else(|| "no message".into()),
            });
        }
        if let Some(reason) = chunk
            .prompt_feedback
            .and_then(|feedback| feedback.block_reason)
        {
            return Err(Error::Failed {
                code: reason,
                message: "the provider blocked the prompt".into(),
            });
        }
        if let Some(usage) = chunk.usage_metadata {
            self.usage = Some(usage.usage());
        }
        if let Some(version) = chunk.model_version {
            self.served = (version != self.model).then_some(version);
        }
        if chunk.response_id.is_some() {
            self.response = chunk.response_id;
        }
        let mut candidates = chunk.candidates;
        let Some(candidate) = candidates.pop() else {
            return Ok(None);
        };
        if !candidates.is_empty() || candidate.index != 0 {
            return Err(Error::Invalid("expected one candidate with index 0".into()));
        }
        if let Some(grounding) = candidate.grounding_metadata {
            Grounding::deserialize(&grounding).map_err(|error| invalid(&error))?;
            if self.grounding.len() == MAX_PARTS {
                return Err(Error::Limit("too much grounding metadata".into()));
            }
            self.grounding.push(grounding);
        }
        for part in candidate
            .content
            .map(|content| content.parts)
            .unwrap_or_default()
        {
            self.part(part, on_delta)?;
        }
        match candidate.finish_reason.as_deref() {
            None | Some("FINISH_REASON_UNSPECIFIED") => Ok(None),
            Some("STOP") => self.finish().map(Some),
            Some("MAX_TOKENS") => Err(Error::Incomplete("max_tokens".into())),
            Some("CONTINUATION") => Err(Error::Failed {
                code: "unsupported_continuation".into(),
                message: "Gemini opaque-token continuation is not supported; response retained only in diagnostics, no calls executed".into(),
            }),
            Some("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL") => Err(Error::Interrupted(
                "the model produced a malformed function call".into(),
            )),
            Some(reason) => Err(Error::Failed {
                code: reason.into(),
                message: "the provider stopped the response".into(),
            }),
        }
    }

    fn part(&mut self, mut part: Value, on_delta: &mut dyn FnMut(Delta<'_>)) -> Result<(), Error> {
        let view = PartView::deserialize(&part).map_err(|error| invalid(&error))?;
        if PAYLOADS
            .iter()
            .filter(|key| part.get(**key).is_some())
            .count()
            > 1
        {
            return Err(Error::Invalid(
                "a part carries more than one payload".into(),
            ));
        }
        if let Some(call) = &view.function_call {
            if call.name.is_empty() {
                return Err(Error::Invalid("function call without a name".into()));
            }
            on_delta(Delta::ToolBytes(
                part["functionCall"]["args"].to_string().len(),
            ));
            return self.add(part);
        }
        let Some(text) = view.text else {
            let signed_only = view.thought_signature.is_some()
                && part.as_object().is_some_and(|part| part.len() == 1);
            if !signed_only {
                return Err(Error::Invalid("unsupported content part".into()));
            }
            let last = self
                .parts
                .last_mut()
                .ok_or_else(|| Error::Invalid("thought signature without a part".into()))?;
            last["thoughtSignature"] = view.thought_signature.into();
            return Ok(());
        };
        on_delta(if view.thought {
            Delta::Reasoning(&text)
        } else {
            Delta::Text(&text)
        });
        if let Some(last) = self.parts.last_mut()
            && last.get("text").is_some()
            && (last["thought"] == true) == view.thought
            && last.get("thoughtSignature").is_none()
        {
            if let Value::String(merged) = &mut last["text"] {
                merged.push_str(&text);
            }
            if let Some(signature) = part.get_mut("thoughtSignature").map(Value::take) {
                last["thoughtSignature"] = signature;
            }
            return Ok(());
        }
        self.add(part)
    }

    fn add(&mut self, part: Value) -> Result<(), Error> {
        if self.parts.len() == MAX_PARTS {
            return Err(Error::Limit(format!("more than {MAX_PARTS} parts")));
        }
        self.parts.push(part);
        Ok(())
    }

    fn finish(&mut self) -> Result<Assistant, Error> {
        let mut parts = Vec::new();
        for (index, mut part) in std::mem::take(&mut self.parts).into_iter().enumerate() {
            let view = PartView::deserialize(&part).map_err(|error| invalid(&error))?;
            let content = if let Some(call) = view.function_call {
                let id = if let Some(id) = call.id.filter(|id| !id.is_empty()) {
                    id
                } else {
                    let response = self.response.as_deref().ok_or_else(|| {
                        Error::Invalid("function call without id in a response without id".into())
                    })?;
                    format!("{GENERATED_ID}{response}-{index}")
                };
                let args = &mut part["functionCall"]["args"];
                if args.is_null() {
                    *args = json!({});
                }
                Content::ToolCall(ToolCall {
                    id,
                    name: call.name,
                    arguments: Arguments::from(args.clone()),
                })
            } else if view.thought {
                Content::Reasoning(view.text.unwrap_or_default().into())
            } else if view.text.as_deref().is_some_and(|text| !text.is_empty())
                || view.thought_signature.is_some()
            {
                Content::Text(view.text.unwrap_or_default().into())
            } else {
                continue;
            };
            parts.push(Part {
                content,
                replay: Some(part.into()),
            });
        }
        for grounding in std::mem::take(&mut self.grounding) {
            parts.push(Part {
                content: Content::Other,
                replay: Some(json!({ "groundingMetadata": grounding }).into()),
            });
        }
        Ok(Assistant {
            protocol: crate::provider::Protocol::Gemini,
            provider: self.provider.clone(),
            model: self.model.clone(),
            parts,
            usage: self.usage.take(),
            continues: false,
            refused: false,
            served_model: self.served.take(),
        })
    }
}

pub(super) fn collect_search(parts: &[Part], search: &mut super::search::SearchResult) {
    let grounding: Vec<Grounding> = parts
        .iter()
        .filter_map(|part| part.replay.as_deref()?.get("groundingMetadata"))
        .filter_map(|metadata| Grounding::deserialize(metadata).ok())
        .collect();
    // Support indices refer to the chunks of their own metadata block.
    for metadata in &grounding {
        for support in &metadata.grounding_supports {
            let claim = support.segment.as_ref().map_or("", |segment| &segment.text);
            for &index in &support.grounding_chunk_indices {
                if let Some(web) = metadata
                    .grounding_chunks
                    .get(index as usize)
                    .and_then(|chunk| chunk.web.as_ref())
                {
                    search.cite(web.uri.as_deref(), web.title.as_deref(), claim);
                }
            }
        }
    }
    for web in grounding
        .iter()
        .flat_map(|metadata| &metadata.grounding_chunks)
        .filter_map(|chunk| chunk.web.as_ref())
    {
        search.source(web.uri.as_deref(), web.title.as_deref());
    }
}

#[cfg(test)]
mod tests;
