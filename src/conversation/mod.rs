mod payloads;
mod recovery;

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{
    approval::Review,
    history::{Message, Usage},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct UsageSummary {
    pub(crate) last: Option<Usage>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cost: Option<f64>,
    pub(crate) cost_is_estimate: bool,
}

impl UsageSummary {
    fn observe(&mut self, message: &Message) {
        let Message::Assistant(assistant) = message else {
            return;
        };
        self.last = assistant.usage;
        if let Some(usage) = self.last {
            self.output_tokens = Some(
                self.output_tokens
                    .unwrap_or(0)
                    .saturating_add(usage.output_tokens),
            );
            if let Some(cost) = usage.cost {
                *self.cost.get_or_insert(0.0) += cost;
                self.cost_is_estimate |= usage.cost_is_estimate;
            }
        }
    }

    fn of(history: &[Message]) -> Self {
        let mut summary = Self::default();
        for message in history {
            summary.observe(message);
        }
        summary
    }
}

const MAX_REVIEW_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Estimate {
    pub(crate) messages: usize,
    pub(crate) tokens: u64,
}

pub(crate) type Repairs = Vec<(usize, Message)>;

#[derive(Clone, Copy)]
pub(crate) enum Anchor {
    Unmeasured,
    Reported { after: usize, usage: Usage },
    Checkpoint(Estimate),
}

#[derive(Clone, Copy)]
pub(crate) struct ModelContext<'a> {
    messages: &'a [Message],
    anchor: Anchor,
    inserted: &'a [usize],
}

pub(crate) struct ContextSnapshot {
    messages: Vec<Message>,
    anchor: Anchor,
    inserted: Vec<usize>,
}

impl ContextSnapshot {
    pub(crate) fn view(&self) -> ModelContext<'_> {
        ModelContext {
            messages: &self.messages,
            anchor: self.anchor,
            inserted: &self.inserted,
        }
    }
}

impl<'a> ModelContext<'a> {
    pub(crate) fn messages(self) -> &'a [Message] {
        self.messages
    }

    pub(crate) fn snapshot(self) -> ContextSnapshot {
        ContextSnapshot {
            messages: self.messages.to_vec(),
            anchor: self.anchor,
            inserted: self.inserted.to_vec(),
        }
    }

    pub(crate) fn anchor(self) -> Anchor {
        self.anchor
    }

    pub(crate) fn adjustments(self) -> impl Iterator<Item = &'a Message> {
        self.inserted.iter().map(move |&at| &self.messages[at])
    }
}

#[derive(Clone)]
struct Checkpoint {
    estimate: Estimate,
    inserted: Vec<usize>,
}

#[derive(Clone)]
enum Selected {
    Original {
        reported: Option<(usize, Usage)>,
    },
    Compacted {
        messages: Vec<Message>,
        checkpoint: Checkpoint,
        reported: Option<(usize, Usage)>,
    },
}

impl Selected {
    fn repair_window(&mut self, window: Repairs) {
        if let Selected::Compacted {
            messages,
            checkpoint,
            reported,
        } = self
        {
            for (at, message) in window {
                if reported.is_some_and(|(after, _)| at < after) {
                    *reported = None;
                }
                if at < checkpoint.estimate.messages {
                    for previous in &mut checkpoint.inserted {
                        if *previous >= at {
                            *previous += 1;
                        }
                    }
                    checkpoint.inserted.push(at);
                    checkpoint.estimate.messages += 1;
                }
                messages.insert(at, message);
            }
        }
    }
}

impl Default for Selected {
    fn default() -> Self {
        Self::Original { reported: None }
    }
}

// Each saved selection covers only the history present when it was compacted.
// A later summary must never survive a rewind into the turns it summarizes.
struct SavedSelection {
    through: usize,
    selected: Selected,
}

#[derive(Default)]
pub(crate) struct Conversation {
    history: Vec<Message>,
    selected: Selected,
    checkpoints: Vec<SavedSelection>,
    // Valid provider measurements, indexed by the end of their original history.
    // Unlike assistant usage retained for billing, these are invalidated by repairs.
    reports: Vec<(usize, Usage)>,
    usage: UsageSummary,
    compactions: u64,
    reviews: VecDeque<Review>,
    review_bytes: usize,
}

impl Conversation {
    #[cfg(test)]
    pub(crate) fn of(history: Vec<Message>) -> Self {
        let mut conversation = Self::default();
        for message in history {
            conversation.push(message);
        }
        conversation
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = crate::history::Entry<'_>> {
        let mut checkpoints = self.checkpoints.iter().peekable();
        let mut at = 0;
        std::iter::from_fn(move || {
            loop {
                if checkpoints.peek().is_some_and(|saved| saved.through == at) {
                    let saved = checkpoints.next()?;
                    if let Selected::Compacted { messages, .. } = &saved.selected
                        && let Some(summary) = crate::agent::checkpoint_summary(messages)
                    {
                        return Some(crate::history::Entry::Compact(summary));
                    }
                    continue;
                }
                let message = self.history.get(at)?;
                at += 1;
                return Some(crate::history::Entry::Message(message));
            }
        })
    }

    pub(crate) fn history(&self) -> &[Message] {
        &self.history
    }

    pub(crate) fn usage(&self) -> UsageSummary {
        self.usage
    }

    pub(crate) fn context(&self) -> ModelContext<'_> {
        let (messages, reported, checkpoint) = match &self.selected {
            Selected::Original { reported } => (&self.history, *reported, None),
            Selected::Compacted {
                messages,
                checkpoint,
                reported,
            } => (messages, *reported, Some(checkpoint)),
        };
        let (anchor, inserted) = match (reported, checkpoint) {
            (Some((after, usage)), _) => (Anchor::Reported { after, usage }, &[][..]),
            (None, Some(checkpoint)) => (
                Anchor::Checkpoint(checkpoint.estimate),
                checkpoint.inserted.as_slice(),
            ),
            (None, None) => (Anchor::Unmeasured, &[][..]),
        };
        ModelContext {
            messages,
            anchor,
            inserted,
        }
    }

    pub(crate) fn compactions(&self) -> u64 {
        self.compactions
    }

    pub(crate) fn reviews(&self) -> &VecDeque<Review> {
        &self.reviews
    }

    pub(crate) fn share_payloads(&mut self) {
        if let Selected::Compacted { messages, .. } = &mut self.selected {
            payloads::share_payloads(&mut self.history, messages);
        }
    }

    pub(crate) fn push(&mut self, message: Message) {
        self.usage.observe(&message);
        let usage = match &message {
            Message::Assistant(assistant) => assistant.usage,
            _ => None,
        };
        let (reported, after) = match &mut self.selected {
            Selected::Original { reported } => (reported, self.history.len() + 1),
            Selected::Compacted {
                messages, reported, ..
            } => {
                messages.push(message.clone());
                (reported, messages.len())
            }
        };
        if let Some(usage) = usage {
            *reported = Some((after, usage));
            self.reports.push((self.history.len() + 1, usage));
        }
        self.history.push(message);
    }

    pub(crate) fn rewind(&mut self, at: usize) {
        self.history.truncate(at);
        self.usage = UsageSummary::of(&self.history);
        self.checkpoints.retain(|saved| saved.through <= at);
        self.reports.retain(|(after, _)| *after <= at);
        self.selected = if let Some(saved) = self.checkpoints.last() {
            let mut selected = saved.selected.clone();
            if let Selected::Compacted {
                messages, reported, ..
            } = &mut selected
            {
                let prefix = messages.len();
                messages.extend_from_slice(&self.history[saved.through..]);
                if let Some(&(after, usage)) = self.reports.last()
                    && after > saved.through
                {
                    *reported = Some((prefix + after - saved.through, usage));
                }
            }
            selected
        } else {
            Selected::Original {
                reported: self.reports.last().copied(),
            }
        };
        let kept = crate::history::call_ids(&self.history);
        self.reviews
            .retain(|review| kept.contains(review.call_id()));
        self.review_bytes = self.reviews.iter().map(crate::history::encoded_len).sum();
    }

    pub(crate) fn compact(&mut self, messages: Vec<Message>, checkpoint: Estimate) {
        let reported = messages[checkpoint.messages..]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, message)| match message {
                Message::Assistant(assistant) => assistant
                    .usage
                    .map(|usage| (checkpoint.messages + at + 1, usage)),
                _ => None,
            });
        self.selected = Selected::Compacted {
            messages,
            checkpoint: Checkpoint {
                estimate: checkpoint,
                inserted: Vec::new(),
            },
            reported,
        };
        self.checkpoints.push(SavedSelection {
            through: self.history.len(),
            selected: self.selected.clone(),
        });
        self.compactions += 1;
    }

    pub(crate) fn repair(&mut self, history: Repairs, window: Repairs) {
        if let Selected::Original { reported } = &mut self.selected
            && reported.is_some_and(|(after, _)| history.iter().any(|(at, _)| *at < after))
        {
            *reported = None;
        }
        let changes_usage = history
            .iter()
            .any(|(_, message)| matches!(message, Message::Assistant(_)));
        for (at, message) in history {
            // Reports after an insertion measured a different prompt. Do not
            // restore them from saved assistant usage on rewind.
            self.reports.retain(|(after, _)| *after <= at);
            for saved in &mut self.checkpoints {
                if at < saved.through {
                    saved.through += 1;
                }
            }
            self.history.insert(at, message);
        }
        if changes_usage {
            self.usage = UsageSummary::of(&self.history);
        }
        // Only the current checkpoint has this window's coordinates. Earlier
        // checkpoints remain valid summaries; history insertions shift their cutoffs.
        if let Some(saved) = self.checkpoints.last_mut()
            && let Selected::Compacted { messages, .. } = &saved.selected
        {
            let mut len = messages.len();
            let prefix = window
                .iter()
                .filter(|(at, _)| {
                    if *at < len {
                        len += 1;
                        true
                    } else {
                        false
                    }
                })
                .cloned()
                .collect();
            saved.selected.repair_window(prefix);
        }
        self.selected.repair_window(window);
    }

    pub(crate) fn recover(&mut self, output: &str) -> (Repairs, Repairs) {
        let history = recovery::orphaned_calls(&self.history, output);
        let window = match &self.selected {
            Selected::Original { .. } => Vec::new(),
            Selected::Compacted { messages, .. } => recovery::orphaned_calls(messages, output),
        };
        self.repair(history.clone(), window.clone());
        (history, window)
    }

    pub(crate) fn resumed(&mut self) -> recovery::Recovered {
        let (repairs, window_repairs) = self.recover(recovery::INTERRUPTED_BY_EXIT);
        recovery::Recovered {
            repairs,
            window_repairs,
            notices: recovery::notices(&self.history),
        }
    }

    pub(crate) fn review(&mut self, review: Review) {
        self.review_bytes += crate::history::encoded_len(&review);
        self.reviews.push_back(review);
        while self.review_bytes > MAX_REVIEW_BYTES {
            let oldest = self
                .reviews
                .pop_front()
                .expect("over budget means non-empty");
            self.review_bytes -= crate::history::encoded_len(&oldest);
        }
    }
}
