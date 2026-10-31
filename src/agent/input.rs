use std::sync::{Arc, Mutex, MutexGuard};

use crate::history::InlineImage;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Input {
    pub(crate) text: String,
    pub(crate) images: Vec<InlineImage>,
    pub(crate) pasted_bytes: usize,
    pub(crate) skills: Vec<crate::history::Skill>,
}

impl From<String> for Input {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
            pasted_bytes: 0,
            skills: Vec::new(),
        }
    }
}

impl Input {
    pub(crate) fn resolve_skill(&mut self) -> Result<(), String> {
        if self.skills.is_empty()
            && let Some(skill) = super::skills::resolve(&self.text)?
        {
            self.skills.push(skill);
        }
        Ok(())
    }
}

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 16;
const MAX_INPUTS: usize = 64;
pub(crate) const FULL: &str =
    "pending input budget exceeded (32 MiB, 16 attachments or 64 messages); input not accepted";

#[derive(Clone, Copy, Default)]
pub(crate) struct InputSize {
    bytes: usize,
    images: usize,
    inputs: usize,
}

impl InputSize {
    fn input(input: &Input) -> Self {
        Self::draft(&input.text, &input.images).with_pastes(
            input.pasted_bytes.saturating_add(
                input
                    .skills
                    .iter()
                    .map(|skill| skill.text.len())
                    .sum::<usize>(),
            ),
        )
    }

    pub(crate) fn draft<'a>(
        text: &str,
        images: impl IntoIterator<Item = &'a crate::history::InlineImage>,
    ) -> Self {
        let mut size = Self {
            bytes: text.len(),
            images: 0,
            inputs: 1,
        };
        for image in images {
            size.bytes = size.bytes.saturating_add(
                image.bytes().len().div_ceil(3) * 4 + image.label.len() + image.media_type().len(),
            );
            size.images += 1;
        }
        size
    }

    pub(crate) fn with_pastes(mut self, bytes: usize) -> Self {
        self.bytes = self.bytes.saturating_add(bytes);
        self
    }

    fn add(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.images = self.images.saturating_add(other.images);
        self.inputs = self.inputs.saturating_add(other.inputs);
    }

    fn remove(&mut self, other: Self) {
        self.bytes -= other.bytes;
        self.images -= other.images;
        self.inputs -= other.inputs;
    }

    fn fits(self) -> bool {
        self.bytes <= MAX_BYTES && self.images <= MAX_IMAGES && self.inputs <= MAX_INPUTS
    }
}

pub(crate) fn fits<'a>(inputs: impl IntoIterator<Item = &'a Input>, draft: InputSize) -> bool {
    let mut usage = InputSize::default();
    for input in inputs {
        usage.add(InputSize::input(input));
    }
    usage.add(draft);
    usage.fits()
}

#[derive(Default)]
struct Waiting {
    inputs: std::collections::VecDeque<Input>,
    usage: InputSize,
}

#[derive(Clone, Default)]
pub(crate) struct Inbox(Arc<Mutex<Waiting>>);

pub(super) struct HeldInput {
    inbox: Inbox,
    input: Option<Input>,
    usage: InputSize,
}

impl HeldInput {
    pub(super) fn input(&self) -> &Input {
        self.input.as_ref().expect("reserved input")
    }
    pub(super) fn accept(mut self) {
        self.input.take();
    }
}

impl Drop for HeldInput {
    fn drop(&mut self) {
        let mut waiting = self.inbox.lock();
        if let Some(input) = self.input.take() {
            waiting.inputs.push_front(input);
        } else {
            waiting.usage.remove(self.usage);
        }
    }
}

impl Inbox {
    pub(crate) fn push(&self, input: Input) -> Result<(), Input> {
        let mut waiting = self.lock();
        let mut usage = waiting.usage;
        usage.add(InputSize::input(&input));
        if !usage.fits() {
            return Err(input);
        }
        waiting.usage = usage;
        waiting.inputs.push_back(input);
        Ok(())
    }

    pub(super) fn reserve(&self, input: Input) -> Option<HeldInput> {
        let mut waiting = self.lock();
        let usage = InputSize::input(&input);
        let mut total = waiting.usage;
        total.add(usage);
        if !total.fits() {
            return None;
        }
        waiting.usage = total;
        Some(HeldInput {
            inbox: self.clone(),
            input: Some(input),
            usage,
        })
    }

    pub(super) fn next(&self) -> Option<HeldInput> {
        let input = self.lock().inputs.pop_front()?;
        let usage = InputSize::input(&input);
        Some(HeldInput {
            inbox: self.clone(),
            input: Some(input),
            usage,
        })
    }

    pub(crate) fn take(&self) -> Vec<Input> {
        let mut waiting = self.lock();
        let inputs: Vec<_> = waiting.inputs.drain(..).collect();
        for input in &inputs {
            waiting.usage.remove(InputSize::input(input));
        }
        inputs
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lock().inputs.is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.0.lock().expect(crate::NOT_POISONED)
    }
}
