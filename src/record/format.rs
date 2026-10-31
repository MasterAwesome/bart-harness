use std::{
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _},
};

use serde::{Deserialize, Serialize};

use super::{Meta, Repairs};
use crate::{conversation::Estimate, history::Message};

const VERSION: u64 = 1;
pub(super) const MAX_LINE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(super) struct Header {
    version: u64,
    #[serde(flatten)]
    pub(super) meta: Meta,
    pub(super) project: String,
}

impl Header {
    pub(super) fn new(meta: &Meta, project: &str) -> Self {
        Self {
            version: VERSION,
            meta: meta.clone(),
            project: project.to_owned(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub(super) enum Entry {
    Session(Header),
    Metadata(Meta),
    Name(String),
    Messages(Vec<Message>),
    Rewind(usize),
    Compact {
        window: Vec<Message>,
        estimate: Estimate,
    },
    Review(crate::approval::Review),
    Repair {
        history: Repairs,
        window: Repairs,
    },
}

pub(super) struct Entries<'a> {
    reader: BufReader<&'a File>,
    consumed: u64,
    pub(super) complete: u64,
    index: usize,
    line: Vec<u8>,
    ended: bool,
}

impl<'a> Entries<'a> {
    pub(super) fn new(file: &'a File) -> Self {
        Self {
            reader: BufReader::new(file),
            consumed: 0,
            complete: 0,
            index: 0,
            line: Vec::new(),
            ended: false,
        }
    }

    fn read_entry(&mut self) -> io::Result<Option<Entry>> {
        self.line.clear();
        let length = (&mut self.reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut self.line)?;
        self.consumed += length as u64;
        if length as u64 > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "session record line exceeds the 256 MiB bound",
            ));
        }
        if self.line.last() != Some(&b'\n') {
            return Ok(None);
        }
        if self.index == 0 {
            check_header(&self.line)?;
        }
        let entry: Timed<Entry> =
            serde_json::from_slice(&self.line).map_err(|error| corrupt(self.index, &error))?;
        if self.index > 0 && matches!(entry.entry, Entry::Session(_)) {
            return Err(io::Error::other(format!(
                "the record has a second session header at line {}",
                self.index + 1
            )));
        }
        self.complete = self.consumed;
        self.index += 1;
        Ok(Some(entry.entry))
    }
}

fn check_header(line: &[u8]) -> io::Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|error| corrupt(0, &error))?;
    let version = (value.get("event").and_then(serde_json::Value::as_str) == Some("session"))
        .then(|| value.pointer("/data/version"))
        .flatten();
    match version.and_then(serde_json::Value::as_u64) {
        Some(VERSION) => Ok(()),
        Some(other) => Err(io::Error::other(format!(
            "the record uses session format {other}; this Bart reads format {VERSION}"
        ))),
        None => Err(io::Error::other(
            "the record does not start with a versioned session header",
        )),
    }
}

impl Iterator for Entries<'_> {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        let entry = self.read_entry().transpose();
        self.ended = !matches!(entry, Some(Ok(_)));
        entry
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Timed<T> {
    #[serde(flatten)]
    pub(super) entry: T,
    pub(super) time_ms: u64,
}

pub(super) fn corrupt(index: usize, error: &serde_json::Error) -> io::Error {
    io::Error::other(format!(
        "the record is malformed at line {}: {error}",
        index + 1
    ))
}
