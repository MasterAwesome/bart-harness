use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read as _, Seek as _, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINE_BYTES: u64 = 4096;

pub(crate) struct Log {
    path: PathBuf,
    run: String,
    state: Mutex<State>,
}

struct State {
    file: File,
    bytes: u64,
    next: u64,
    stopped: bool,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Completed,
    Cancelled,
    TransportError,
    HttpError,
    Interrupted,
    InvalidResponse,
    LimitExceeded,
    ProviderFailed,
    Incomplete,
    ContextOverflow,
    CredentialError,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    AwaitingHeaders,
    Streaming,
    ErrorBody,
}

pub(crate) struct Trace {
    log: Arc<Log>,
    id: u64,
    started: Instant,
    last_report: Instant,
    status: Option<u16>,
    headers: Option<u64>,
    first_chunk: Option<u64>,
    last_chunk: Option<u64>,
    first_generation: Option<u64>,
    last_generation: Option<u64>,
    chunks: u64,
    wire_bytes: u64,
    text_bytes: u64,
    reasoning_bytes: u64,
    argument_bytes: u64,
    request_bytes: usize,
    outcome: Option<Outcome>,
}

impl Log {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join("diagnostics.jsonl");
        let mut file = open(&path)?;
        let mut bytes = file.metadata()?.len();
        if bytes > 0 {
            let tail_len = bytes.min(MAX_LINE_BYTES);
            file.seek(io::SeekFrom::End(
                -i64::try_from(tail_len).expect("bounded by MAX_LINE_BYTES"),
            ))?;
            let mut tail = vec![0; usize::try_from(tail_len).expect("bounded by MAX_LINE_BYTES")];
            file.read_exact(&mut tail)?;
            if tail.last() != Some(&b'\n') {
                bytes = tail
                    .iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |at| bytes - tail_len + at as u64 + 1);
                file.set_len(bytes)?;
            }
        }
        Ok(Self {
            path,
            run: format!("{}-{}", epoch_ms(), std::process::id()),
            state: Mutex::new(State {
                file,
                bytes,
                next: 0,
                stopped: false,
            }),
        })
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        provider: &str,
        model: &str,
        web: bool,
        attempt: u32,
        retry_of: Option<u64>,
    ) -> Trace {
        let id = {
            let mut state = self.state.lock().expect(crate::NOT_POISONED);
            state.next += 1;
            state.next
        };
        let trace = Trace {
            log: Arc::clone(self),
            id,
            started: Instant::now(),
            last_report: Instant::now(),
            status: None,
            headers: None,
            first_chunk: None,
            last_chunk: None,
            first_generation: None,
            last_generation: None,
            chunks: 0,
            wire_bytes: 0,
            text_bytes: 0,
            reasoning_bytes: 0,
            argument_bytes: 0,
            request_bytes: 0,
            outcome: None,
        };
        self.write(id, "start", &json!({"provider": label(provider), "model": label(model), "web_search": web, "attempt": attempt, "retry_of": retry_of}));
        trace
    }

    pub(crate) fn retry(&self, id: u64, delay: std::time::Duration, reset: bool) {
        self.write(
            id,
            "retry",
            &json!({"delay_ms": delay.as_millis(), "quota_reset": reset}),
        );
    }

    fn write(&self, id: u64, event: &str, data: &Value) {
        let mut state = self.state.lock().expect(crate::NOT_POISONED);
        if state.stopped {
            return;
        }
        let result = (|| -> io::Result<()> {
            let mut line = serde_json::to_vec(
                &json!({"time_ms": epoch_ms(), "run_id": self.run, "request_id": id, "event": event, "data": data}),
            )?;
            line.push(b'\n');
            if state.bytes + line.len() as u64 > MAX_BYTES {
                fs::rename(&self.path, self.path.with_extension("jsonl.1"))?;
                state.file = open(&self.path)?;
                state.bytes = 0;
            }
            state.file.write_all(&line)?;
            state.bytes += line.len() as u64;
            Ok(())
        })();
        if let Err(error) = result {
            state.stopped = true;
            let _ = state.file.set_len(state.bytes);
            crate::terminal_text::stderr_line(format_args!(
                "bart: diagnostic logging stopped: {error}"
            ));
        }
    }
}

impl Trace {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn elapsed(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn provider_error(
        &self,
        response_failed: bool,
        status: Option<u16>,
        usage_limit: bool,
        reset_timestamp: bool,
        retry_after: bool,
        retry: crate::provider::Retry,
    ) {
        let retry = match retry {
            crate::provider::Retry::Reset(_) => "reset",
            crate::provider::Retry::After(_) => "after",
            crate::provider::Retry::No => "no",
        };
        self.log.write(
            self.id,
            "provider_error",
            &json!({
                "event_type": if response_failed { "response.failed" } else { "error" },
                "status": status,
                "usage_limit": usage_limit,
                "reset_timestamp": reset_timestamp,
                "retry_after": retry_after,
                "retry": retry,
            }),
        );
    }

    pub(crate) fn body(&mut self, bytes: usize) {
        self.request_bytes = bytes;
    }

    pub(crate) fn headers(&mut self, status: u16) {
        self.status = Some(status);
        self.headers = Some(self.elapsed());
        self.snapshot("headers");
    }

    pub(crate) fn chunk(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let elapsed = self.elapsed();
        self.first_chunk.get_or_insert(elapsed);
        self.last_chunk = Some(elapsed);
        self.chunks = self.chunks.saturating_add(1);
        self.wire_bytes = self.wire_bytes.saturating_add(bytes as u64);
    }

    pub(crate) fn generation(&mut self, text: usize, reasoning: usize, arguments: usize) {
        if text == 0 && reasoning == 0 && arguments == 0 {
            return;
        }
        let elapsed = self.elapsed();
        self.first_generation.get_or_insert(elapsed);
        self.last_generation = Some(elapsed);
        self.text_bytes = self.text_bytes.saturating_add(text as u64);
        self.reasoning_bytes = self.reasoning_bytes.saturating_add(reasoning as u64);
        self.argument_bytes = self.argument_bytes.saturating_add(arguments as u64);
    }

    pub(crate) fn next_report(&self) -> Instant {
        self.last_report + std::time::Duration::from_secs(5)
    }

    pub(crate) fn snapshot(&mut self, event: &str) {
        self.last_report = Instant::now();
        let elapsed = self.elapsed();
        let phase = match self.status {
            None => Phase::AwaitingHeaders,
            Some(status) if status == 101 || (200..300).contains(&status) => Phase::Streaming,
            Some(_) => Phase::ErrorBody,
        };
        self.log.write(self.id, event, &json!({"phase": phase, "elapsed_ms": elapsed, "http_status": self.status, "headers_ms": self.headers, "request_bytes": self.request_bytes, "first_chunk_ms": self.first_chunk, "last_chunk_ms": self.last_chunk, "network_quiet_ms": elapsed.saturating_sub(self.last_chunk.or(self.headers).unwrap_or(0)), "first_generation_ms": self.first_generation, "last_generation_ms": self.last_generation, "chunks": self.chunks, "wire_bytes": self.wire_bytes, "text_bytes": self.text_bytes, "reasoning_bytes": self.reasoning_bytes, "argument_bytes": self.argument_bytes, "outcome": self.outcome}));
    }

    pub(crate) fn finish(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
        self.snapshot("finished");
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if self.outcome.is_none() {
            self.finish(Outcome::Cancelled);
        }
    }
}

fn label(s: &str) -> &str {
    &s[..s.floor_char_boundary(s.len().min(128))]
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
