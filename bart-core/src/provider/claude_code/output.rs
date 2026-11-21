use serde_json::Value;

use super::super::messages::{Stream, Tokens};
use super::{Error, Observer, Protocol, Request, Trace, deliver};
use crate::history::{Assistant, Content, Part, Usage};

#[derive(Default)]
pub(super) struct Output {
    parts: Vec<Part>,
    message: Option<String>,
    tokens: Option<Tokens>,
    stream: Option<Stream>,
    streamed: bool,
    usage: Option<Usage>,
    served_model: Option<String>,
    refused: bool,
}

impl Output {
    pub(super) fn event(
        &mut self,
        event: &Value,
        request: &Request<'_>,
        observer: &mut impl Observer,
        mut trace: Option<&mut Trace>,
    ) -> Result<(), Error> {
        if event["type"] == "message_start" {
            self.finish_usage();
            self.message = event["message"]["id"].as_str().map(str::to_owned);
            self.stream = None;
            self.streamed = true;
            self.refused = false;
        }
        let stream = self
            .stream
            .get_or_insert_with(|| Stream::observing(request.provider.name(), request.model));
        if let Some(observed) = stream.observe(event, &mut |delta| {
            deliver(trace.as_deref_mut(), observer, delta);
        })? {
            self.add_usage(observed.usage);
            self.refused = observed.refused;
            self.stream = None;
        }
        Ok(())
    }

    pub(super) fn assistant(&mut self, frame: &Value) -> Result<(), Error> {
        let message = &frame["message"];
        let id = message["id"]
            .as_str()
            .ok_or_else(|| super::failure("Claude Code assistant message has no ID"))?;
        if self.message.as_deref() != Some(id) {
            self.finish_usage();
            self.message = Some(id.to_owned());
            self.streamed = false;
            self.refused = false;
        }
        if !self.streamed {
            self.refused = message["stop_reason"] == "refusal";
            self.tokens = Some(serde_json::from_value(message["usage"].clone()).map_err(
                |error| super::failure(format!("invalid Claude Code assistant usage: {error}")),
            )?);
        }
        self.served_model = message["model"].as_str().map(str::to_owned);
        let blocks = message["content"]
            .as_array()
            .ok_or_else(|| super::failure("Claude Code assistant content is not an array"))?;
        for block in blocks {
            let content = match block["type"].as_str() {
                Some("text") => block["text"]
                    .as_str()
                    .map(|text| Content::Text(text.to_owned().into())),
                Some("thinking") => block["thinking"]
                    .as_str()
                    .map(|text| Content::Reasoning(text.to_owned().into())),
                Some("redacted_thinking") => {
                    self.parts.push(Part {
                        content: Content::Other,
                        replay: Some(block.clone().into()),
                    });
                    None
                }
                _ => None,
            };
            if let Some(content) = content {
                self.parts.push(Part {
                    content,
                    replay: None,
                });
            }
        }
        Ok(())
    }

    fn finish_usage(&mut self) {
        let usage = self.tokens.take().and_then(Tokens::usage);
        self.add_usage(usage);
    }

    fn add_usage(&mut self, usage: Option<Usage>) {
        let Some(mut usage) = usage else { return };
        if let Some(previous) = self.usage.take() {
            usage.output_tokens = usage.output_tokens.saturating_add(previous.output_tokens);
            usage.reasoning_tokens = match (usage.reasoning_tokens, previous.reasoning_tokens) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            };
        }
        self.usage = Some(usage);
    }

    pub(super) fn take(&mut self, request: &Request<'_>, finished: bool) -> Assistant {
        if finished {
            self.finish_usage();
        }
        let refused = std::mem::take(&mut self.refused);
        Assistant {
            protocol: Protocol::Claude,
            provider: request.provider.name().to_owned(),
            model: request.model.to_owned(),
            parts: std::mem::take(&mut self.parts),
            usage: self.usage.take(),
            continues: false,
            refused: finished && refused,
            served_model: self
                .served_model
                .as_ref()
                .filter(|model| model.as_str() != request.model)
                .cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::provider::{Delta, Kind, Provider, Tools};

    #[test]
    fn second_message_start_without_stop_resets_stream() {
        let provider = Provider { kind: Kind::Claude };
        let request = Request {
            provider: &provider,
            model: "claude-sonnet-4-6",
            instructions: "",
            history: &[],
            tools: Tools::Functions(&[]),
            reasoning_effort: None,
            summaries: false,
            verbosity: None,
            max_output_tokens: None,
            cache: None,
        };
        let mut output = Output::default();
        let mut observer = |_: Delta<'_>| {};
        let first = json!({
            "type": "message_start",
            "message": { "id": "msg_1", "model": "claude-sonnet-4-6", "usage": { "input_tokens": 10, "output_tokens": 1 } }
        });
        let block = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        });
        let second = json!({
            "type": "message_start",
            "message": { "id": "msg_2", "model": "claude-sonnet-4-6", "usage": { "input_tokens": 20, "output_tokens": 1 } }
        });
        output.event(&first, &request, &mut observer, None).unwrap();
        output.event(&block, &request, &mut observer, None).unwrap();
        output
            .event(&second, &request, &mut observer, None)
            .expect("second message_start should reset stream cleanly");
        output
            .event(&block, &request, &mut observer, None)
            .expect("content_block_start at index 0 after reset should succeed");
    }
}
