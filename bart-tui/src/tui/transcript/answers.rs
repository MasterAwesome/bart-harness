use std::collections::VecDeque;

const MAX_ANSWERS: usize = 256;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(in crate::tui) enum LookupError {
    OutOfRange(usize),
    TooLarge,
}

enum Answer {
    Markdown(String),
    TooLarge,
}

impl Answer {
    fn len(&self) -> usize {
        match self {
            Self::Markdown(text) => text.len(),
            Self::TooLarge => 0,
        }
    }

    fn source(&self) -> Result<&str, LookupError> {
        match self {
            Self::Markdown(text) => Ok(text),
            Self::TooLarge => Err(LookupError::TooLarge),
        }
    }
}

enum Current {
    Markdown(String),
    Overflow { nonblank: bool },
}

impl Default for Current {
    fn default() -> Self {
        Self::Markdown(String::new())
    }
}

impl Current {
    fn append(&mut self, delta: &str) {
        match self {
            Self::Markdown(text) if delta.len() <= MAX_BYTES - text.len() => text.push_str(delta),
            Self::Markdown(text) => {
                *self = Self::Overflow {
                    nonblank: !text.trim().is_empty() || !delta.trim().is_empty(),
                };
            }
            Self::Overflow { nonblank } => *nonblank |= !delta.trim().is_empty(),
        }
    }

    fn source(&self) -> Option<Result<&str, LookupError>> {
        match self {
            Self::Markdown(text) if !text.trim().is_empty() => Some(Ok(text)),
            Self::Overflow { nonblank: true } => Some(Err(LookupError::TooLarge)),
            _ => None,
        }
    }

    fn finish(self) -> Option<Answer> {
        match self {
            Self::Markdown(text) if !text.trim().is_empty() => Some(Answer::Markdown(text)),
            Self::Overflow { nonblank: true } => Some(Answer::TooLarge),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(super) struct Answers {
    history: VecDeque<Answer>,
    bytes: usize,
    current: Current,
}

impl Answers {
    pub(super) fn append(&mut self, delta: &str) {
        self.current.append(delta);
    }

    pub(super) fn finish(&mut self) {
        if let Some(answer) = std::mem::take(&mut self.current).finish() {
            self.bytes += answer.len();
            self.history.push_back(answer);
            while self.history.len() > MAX_ANSWERS || self.bytes > MAX_BYTES {
                self.bytes -= self.history.pop_front().expect("answer over budget").len();
            }
        }
    }

    pub(super) fn discard(&mut self) {
        self.current = Current::default();
    }

    pub(super) fn get(&self, index: usize) -> Result<&str, LookupError> {
        let current = self.current.source();
        let count = (self.history.len() + usize::from(current.is_some())).min(MAX_ANSWERS);
        if index >= count {
            return Err(LookupError::OutOfRange(count));
        }
        current
            .into_iter()
            .chain(self.history.iter().rev().map(Answer::source))
            .nth(index)
            .expect("answer index in range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_only_the_latest_answers() {
        let mut answers = Answers::default();
        for n in 0..=MAX_ANSWERS {
            answers.append(&format!("**answer {n}**"));
            answers.finish();
        }
        assert_eq!(answers.get(MAX_ANSWERS - 1), Ok("**answer 1**"));
        assert_eq!(
            answers.get(MAX_ANSWERS),
            Err(LookupError::OutOfRange(MAX_ANSWERS))
        );
        answers.append("**draft**");
        assert_eq!(answers.get(0), Ok("**draft**"));
        assert_eq!(answers.get(MAX_ANSWERS - 1), Ok("**answer 2**"));
    }

    #[test]
    fn byte_limit_keeps_a_contiguous_suffix_after_multiple_large_answers() {
        let mut answers = Answers::default();
        let padding = " ".repeat(MAX_BYTES / 3);
        let large: Vec<_> = (0..4).map(|n| format!("answer {n}{padding}")).collect();
        answers.append("original");
        answers.finish();
        for text in &large[..2] {
            answers.append(text);
            answers.finish();
        }
        answers.append(&large[2]);
        assert_eq!(answers.get(0), Ok(large[2].as_str()));
        assert_eq!(answers.get(1), Ok(large[1].as_str()));
        assert_eq!(answers.get(2), Ok(large[0].as_str()));
        assert_eq!(answers.get(3), Ok("original"));
        answers.discard();
        assert_eq!(answers.get(2), Ok("original"));
        for n in 2..large.len() {
            answers.append(&large[n]);
            answers.finish();
            assert_eq!(answers.get(0), Ok(large[n].as_str()));
            assert_eq!(answers.get(1), Ok(large[n - 1].as_str()));
            assert_eq!(answers.get(2), Err(LookupError::OutOfRange(2)));
        }
    }

    #[test]
    fn byte_limit_evicts_old_answers_but_never_copies_partial_markdown() {
        let mut answers = Answers::default();
        for text in ["old", "new"] {
            answers.append(text);
            answers.append(&" ".repeat(MAX_BYTES / 2));
            answers.finish();
        }
        assert_eq!(answers.get(1), Err(LookupError::OutOfRange(1)));
        assert!(answers.get(0).unwrap().starts_with("new"));
        answers.append(&" ".repeat(MAX_BYTES));
        answers.append("  ");
        assert!(answers.get(0).unwrap().starts_with("new"));
        answers.append("nonblank");
        assert_eq!(answers.get(0), Err(LookupError::TooLarge));
        answers.finish();
        assert_eq!(answers.get(0), Err(LookupError::TooLarge));
        assert!(answers.get(1).unwrap().starts_with("new"));
    }
}
