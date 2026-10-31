use std::{fmt, mem};

const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    LineTooLong,
    EventTooLarge,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong => f.write_str("SSE line exceeds the 1 MiB safety limit"),
            Self::EventTooLarge => f.write_str("SSE event exceeds the 8 MiB safety limit"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: String,
    after_cr: bool,
    started: bool,
}

impl Decoder {
    pub(super) fn push(
        &mut self,
        mut bytes: &[u8],
        mut emit: impl FnMut(String),
    ) -> Result<(), DecodeError> {
        while !bytes.is_empty() {
            if mem::take(&mut self.after_cr)
                && let Some(rest) = bytes.strip_prefix(b"\n")
            {
                bytes = rest;
                continue;
            }
            let Some(end) = bytes.iter().position(|&b| b == b'\n' || b == b'\r') else {
                return self.extend_line(bytes);
            };
            self.after_cr = bytes[end] == b'\r';
            let line = &bytes[..end];
            bytes = &bytes[end + 1..];
            if self.line.is_empty() {
                if line.len() > MAX_LINE_BYTES {
                    return Err(DecodeError::LineTooLong);
                }
                self.process_line(line, &mut emit)?;
            } else {
                self.extend_line(line)?;
                let pending = mem::take(&mut self.line);
                let result = self.process_line(&pending, &mut emit);
                self.line = pending;
                self.line.clear();
                result?;
            }
        }
        Ok(())
    }

    fn extend_line(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        if self.line.len() + bytes.len() > MAX_LINE_BYTES {
            return Err(DecodeError::LineTooLong);
        }
        self.line.extend_from_slice(bytes);
        Ok(())
    }

    fn process_line(
        &mut self,
        line: &[u8],
        emit: &mut impl FnMut(String),
    ) -> Result<(), DecodeError> {
        let line = if mem::replace(&mut self.started, true) {
            line
        } else {
            line.strip_prefix(BOM).unwrap_or(line)
        };
        if line.is_empty() {
            self.dispatch(emit);
            return Ok(());
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (&*line, ""),
        };
        if field == "data" {
            if self.data.len() + value.len() + 1 > MAX_EVENT_BYTES {
                return Err(DecodeError::EventTooLarge);
            }
            self.data.push_str(value);
            self.data.push('\n');
        }
        Ok(())
    }

    fn dispatch(&mut self, emit: &mut impl FnMut(String)) {
        if !self.data.is_empty() {
            self.data.pop();
            emit(mem::take(&mut self.data));
        }
    }
}
