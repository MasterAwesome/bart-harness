use crate::{
    conversation::{Anchor, Estimate, ModelContext},
    history::{Content, Message},
};

const RESERVE: u64 = 32_000;
const IMAGE_TOKENS: u64 = 2600;
const BYTES_PER_TOKEN: u64 = 4;

pub(super) fn threshold(window: u64) -> u64 {
    window.saturating_sub(RESERVE.min(window / 4))
}

fn bytes(len: usize) -> u64 {
    len as u64 / BYTES_PER_TOKEN + 1
}

pub(super) fn estimate(message: &Message) -> u64 {
    match message {
        Message::Image(_) => IMAGE_TOKENS,
        Message::UserSource(_)
        | Message::Instruction(_)
        | Message::Skill(_)
        | Message::Notification(_) => bytes(crate::history::encoded_len(&message.model_text())),
        Message::ToolResult(result) => bytes(crate::history::encoded_len(&result.output)) + 32,
        Message::Assistant(assistant) => {
            let reported = assistant.usage.and_then(|usage| usage.reasoning_tokens);
            let mut reasoning = None;
            let mut total = 0u64;
            for part in &assistant.parts {
                let len = crate::history::encoded_len(&part.content)
                    .max(part.replay.as_ref().map_or(0, crate::history::encoded_len));
                if matches!(part.content, Content::Reasoning(_))
                    && part.replay.is_some()
                    && let Some(tokens) = reported
                {
                    reasoning = Some(tokens);
                } else {
                    total = total.saturating_add(bytes(len));
                }
            }
            total.saturating_add(reasoning.unwrap_or(0))
        }
        _ => bytes(crate::history::encoded_len(message)),
    }
}

pub(super) fn base(instructions: &str) -> u64 {
    bytes(instructions.len()) + 2048
}

pub(super) fn appended(total: &mut u64, messages: &[Message]) {
    for message in messages {
        if let Message::Assistant(assistant) = message
            && let Some(usage) = assistant.usage
        {
            *total = usage.input_tokens.saturating_add(usage.output_tokens);
        } else {
            *total = total.saturating_add(estimate(message));
        }
    }
}

pub(super) fn tokens(context: ModelContext<'_>, instructions: &str) -> u64 {
    let (anchor, from) = match context.anchor() {
        Anchor::Reported { after, usage } => (
            usage.input_tokens.saturating_add(usage.output_tokens),
            after,
        ),
        Anchor::Checkpoint(estimate) => (estimate.tokens, estimate.messages),
        Anchor::Unmeasured => (base(instructions), 0),
    };
    context.messages()[from..]
        .iter()
        .chain(context.adjustments())
        .fold(anchor, |total, message| {
            total.saturating_add(estimate(message))
        })
}

pub(super) struct Reduced {
    pub(super) window: Vec<Message>,
    pub(super) estimate: Estimate,
}

#[cfg(test)]
mod tests;
