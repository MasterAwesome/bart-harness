mod payloads;
mod recovery;

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::history::{Message, Review, Usage, checkpoint_summary};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageSummary {
    pub last: Option<Usage>,
    pub output_tokens: Option<u64>,
    pub cost: Option<f64>,
    pub cost_is_estimate: bool,
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
    reports: &'a [(usize, Usage)],
    route: Option<(&'a crate::provider::Provider, &'a str)>,
    inserted: &'a [usize],
}

impl<'a> ModelContext<'a> {
    pub(crate) fn messages(self) -> &'a [Message] {
        self.messages
    }

    fn matches_route(self, after: usize) -> bool {
        self.route.is_none_or(|(provider, model)| matches!(&self.messages[after - 1], Message::Assistant(a) if a.provider == provider.name() && a.protocol == provider.protocol() && a.model == model))
    }

    pub(crate) fn measurements(self) -> impl Iterator<Item = Estimate> + 'a {
        self.reports
            .iter()
            .copied()
            .filter(move |&(after, _)| self.matches_route(after))
            .map(|(after, usage)| Estimate {
                messages: after,
                tokens: usage.input_tokens.saturating_add(usage.output_tokens),
            })
    }

    pub(crate) fn for_route(
        mut self,
        provider: &'a crate::provider::Provider,
        model: &'a str,
    ) -> Self {
        self.route = Some((provider, model));
        if let Anchor::Reported { after, .. } = self.anchor
            && !self.matches_route(after)
        {
            self.anchor = Anchor::Unmeasured;
        }
        self
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

#[derive(Clone, Default)]
struct Reports {
    valid: Vec<(usize, Usage)>,
    // Whole-context accounting falls back to content (or its checkpoint) if
    // a repair invalidated its latest anchor. Earlier prefixes still have evidence.
    latest_invalidated: bool,
}

impl Reports {
    fn push(&mut self, after: usize, usage: Usage) {
        self.valid.push((after, usage));
        self.latest_invalidated = false;
    }

    fn repair(&mut self, at: usize) {
        let keep = self.valid.partition_point(|&(after, _)| after <= at);
        self.latest_invalidated |= keep < self.valid.len();
        self.valid.truncate(keep);
    }

    fn latest(&self) -> Option<(usize, Usage)> {
        if self.latest_invalidated {
            None
        } else {
            self.valid.last().copied()
        }
    }
}

#[derive(Clone, Default)]
enum Selected {
    #[default]
    Original,
    Compacted {
        messages: Vec<Message>,
        checkpoint: Checkpoint,
        reports: Reports,
    },
}

impl Selected {
    fn repair_window(&mut self, window: Repairs) {
        if let Selected::Compacted {
            messages,
            checkpoint,
            reports,
        } = self
        {
            for (at, message) in window {
                reports.repair(at);
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
    reports: Reports,
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
                        && let Some(summary) = checkpoint_summary(messages)
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
        let (messages, reports, checkpoint) = match &self.selected {
            Selected::Original => (&self.history, &self.reports, None),
            Selected::Compacted {
                messages,
                checkpoint,
                reports,
            } => (messages, reports, Some(checkpoint)),
        };
        let (anchor, inserted) = match (reports.latest(), checkpoint) {
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
            reports: &reports.valid,
            route: None,
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
        if let Selected::Compacted {
            messages, reports, ..
        } = &mut self.selected
        {
            messages.push(message.clone());
            if let Some(usage) = usage {
                reports.push(messages.len(), usage);
            }
        }
        if let Some(usage) = usage {
            self.reports.push(self.history.len() + 1, usage);
        }
        self.history.push(message);
    }

    pub(crate) fn rewind(&mut self, at: usize) {
        self.history.truncate(at);
        self.usage = UsageSummary::of(&self.history);
        self.checkpoints.retain(|saved| saved.through <= at);
        self.reports.valid.retain(|(after, _)| *after <= at);
        self.reports.latest_invalidated = false;
        self.selected = if let Some(saved) = self.checkpoints.last() {
            let mut selected = saved.selected.clone();
            if let Selected::Compacted {
                messages, reports, ..
            } = &mut selected
            {
                let prefix = messages.len();
                messages.extend_from_slice(&self.history[saved.through..]);
                let start = self
                    .reports
                    .valid
                    .partition_point(|&(after, _)| after <= saved.through);
                for &(after, usage) in &self.reports.valid[start..] {
                    reports.push(prefix + after - saved.through, usage);
                }
            }
            selected
        } else {
            Selected::Original
        };
        let kept = crate::history::call_ids(&self.history);
        self.reviews
            .retain(|review| kept.contains(review.call_id()));
        self.review_bytes = self.reviews.iter().map(crate::history::encoded_len).sum();
    }

    pub(crate) fn compact(&mut self, messages: Vec<Message>, checkpoint: Estimate) {
        let valid = messages[checkpoint.messages..]
            .iter()
            .enumerate()
            .filter_map(|(at, message)| match message {
                Message::Assistant(assistant) => assistant
                    .usage
                    .map(|usage| (checkpoint.messages + at + 1, usage)),
                _ => None,
            })
            .collect();
        self.selected = Selected::Compacted {
            messages,
            checkpoint: Checkpoint {
                estimate: checkpoint,
                inserted: Vec::new(),
            },
            reports: Reports {
                valid,
                latest_invalidated: false,
            },
        };
        self.checkpoints.push(SavedSelection {
            through: self.history.len(),
            selected: self.selected.clone(),
        });
        self.compactions += 1;
    }

    pub(crate) fn repair(&mut self, history: Repairs, window: Repairs) {
        let changes_usage = history
            .iter()
            .any(|(_, message)| matches!(message, Message::Assistant(_)));
        for (at, message) in history {
            // Reports after an insertion measured a different prompt. Do not
            // restore them from saved assistant usage on rewind.
            self.reports.repair(at);
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
            Selected::Original => Vec::new(),
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
