use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) enum Entry<'a> {
    Message(&'a Message),
    Compact(&'a str),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub(crate) enum Message {
    User(Arc<str>),
    UserSource(UserSource),
    Instruction(Arc<str>),
    Skill(Skill),
    Assistant(Assistant),
    ToolResult(ToolResult),
    Notification(Notification),
    Image(Image),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct UserSource {
    pub(crate) text: Arc<str>,
    pub(crate) archive: u64,
    pub(crate) index: usize,
}

impl Message {
    pub(crate) fn model_text(&self) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match self {
            Self::User(text) | Self::Instruction(text) => Cow::Borrowed(text),
            Self::Notification(note) => Cow::Borrowed(&note.text),
            Self::UserSource(source) => Cow::Owned(format!(
                "{}\n{}",
                bart!(
                    UserMessage,
                    "kept verbatim; context: history[{}]",
                    source.archive
                ),
                source.text
            )),
            Self::Skill(skill) => Cow::Owned(format!(
                "{}\n{}",
                bart!(
                    Skill,
                    "{}; relative paths resolve from {}",
                    skill.name,
                    skill.path.parent().unwrap_or(&skill.path).display()
                ),
                skill.text
            )),
            _ => unreachable!("only text messages have model text"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) text: Arc<str>,
}

impl Skill {
    pub(crate) fn label(&self) -> String {
        let path = std::env::home_dir()
            .and_then(|home| {
                self.path
                    .strip_prefix(home)
                    .ok()
                    .map(|relative| format!("~/{}", relative.display()))
            })
            .unwrap_or_else(|| self.path.display().to_string());
        format!("Skill: {} · {path}", self.name)
    }
}

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineImage {
    media_type: &'static str,
    bytes: Arc<[u8]>,
    pub(crate) label: String,
}

impl InlineImage {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn media_type(&self) -> &'static str {
        self.media_type
    }

    pub(crate) fn new(
        bytes: Vec<u8>,
        advertised: Option<&str>,
        label: String,
    ) -> Result<Self, String> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("the image is larger than 10 MiB".into());
        }
        let media_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err("not a PNG, JPEG, GIF or WebP image".into());
        };
        if advertised.is_some_and(|mime| mime != media_type) {
            return Err("image MIME type does not match its bytes".into());
        }
        Ok(Self {
            media_type,
            bytes: bytes.into(),
            label,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Image {
    pub(crate) number: u64,
    pub(crate) path: std::path::PathBuf,
    pub(crate) media_type: String,
    pub(crate) label: String,
    pub(crate) byte_len: usize,
}

impl Image {
    pub(crate) fn history_len(&self) -> usize {
        encoded_len(self).saturating_add(self.byte_len.div_ceil(3).saturating_mul(4))
    }

    pub(crate) fn url(&self) -> std::io::Result<String> {
        Ok(format!(
            "data:{};base64,{}",
            self.media_type,
            self.base64()?
        ))
    }

    pub(crate) fn base64(&self) -> std::io::Result<String> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        crate::persist::open(&self.path)
            .and_then(|file| crate::persist::read_to(&file, MAX_IMAGE_BYTES as u64))
            .map(|bytes| STANDARD.encode(bytes))
            .map_err(|error| {
                std::io::Error::other(format!(
                    "cannot load image {}: {error}",
                    self.path.display()
                ))
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Notification {
    pub(crate) text: Arc<str>,
    pub(crate) kind: NotificationKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NotificationKind {
    Recovery,
    JobLost {
        id: u32,
    },
    JobFinished {
        id: u32,
        outcome: JobOutcome,
        command: String,
        capture: SavedArtifact,
    },
    JobMatched {
        id: u32,
    },
    JobCommandExited {
        id: u32,
        outcome: JobOutcome,
        command: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobOutcome {
    Exit(i32),
    Signal(i32),
    Killed,
    Unknown,
}

impl JobOutcome {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Exit(code) => format!("exit {code}"),
            Self::Signal(signal) => format!("signal {signal}"),
            Self::Killed => "killed".into(),
            Self::Unknown => "outcome unknown".into(),
        }
    }
}

impl Notification {
    pub(crate) fn terminal_job(&self) -> Option<u32> {
        match self.kind {
            NotificationKind::Recovery
            | NotificationKind::JobMatched { .. }
            | NotificationKind::JobCommandExited { .. } => None,
            NotificationKind::JobLost { id } | NotificationKind::JobFinished { id, .. } => Some(id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Assistant {
    pub(crate) protocol: crate::provider::Protocol,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) parts: Vec<Part>,
    pub(crate) usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) continues: bool,
    pub(crate) refused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) served_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Part {
    pub(crate) content: Content,
    pub(crate) replay: Option<Arc<Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Content {
    Text(Arc<str>),
    Reasoning(Arc<str>),
    ToolCall(ToolCall),
    Other,
}

// Recorded as the exact text the provider sent; parsing it again restores the variant.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Arguments {
    Json { value: Arc<Value>, wire: Arc<str> },
    Invalid(Arc<str>),
}

impl Arguments {
    pub(crate) fn from_wire(raw: &str) -> Self {
        match serde_json::from_str(raw) {
            Ok(value) => Self::Json {
                value: Arc::new(value),
                wire: raw.to_owned().into(),
            },
            Err(_) => Self::Invalid(raw.to_owned().into()),
        }
    }

    pub(crate) fn wire(&self) -> &str {
        match self {
            Self::Json { wire, .. } | Self::Invalid(wire) => wire,
        }
    }

    pub(crate) fn json(&self) -> Option<&Value> {
        match self {
            Self::Json { value, .. } => Some(value),
            Self::Invalid(_) => None,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    pub(crate) fn rejected_replay(&self, call_id: &str) -> Option<String> {
        self.as_object().is_none().then(|| {
            format!(
                "{}\n{}",
                bart!(
                    History,
                    "call {call_id} was rejected; its arguments were not a JSON object"
                ),
                self.wire()
            )
        })
    }

    pub(crate) fn as_object(&self) -> Option<&serde_json::Map<String, Value>> {
        self.json()?.as_object()
    }
}

impl Serialize for Arguments {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire())
    }
}

impl<'de> Deserialize<'de> for Arguments {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_wire(&String::deserialize(deserializer)?))
    }
}

impl From<Value> for Arguments {
    fn from(value: Value) -> Self {
        Self::Json {
            wire: value.to_string().into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Arguments,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) diffs: Vec<crate::tool::Diff>,
    #[serde(default, skip_serializing_if = "crate::tool::Activities::is_empty")]
    pub(crate) activities: crate::tool::Activities,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) notes: String,
    pub(crate) call_id: String,
    pub(crate) output: Arc<str>,
    pub(crate) is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) jobs: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) saved: Option<SavedArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cell: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedArtifact {
    pub(crate) path: std::path::PathBuf,
    pub(crate) completeness: Completeness,
    pub(crate) chars: usize,
    pub(crate) bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Completeness {
    Complete,
    Incomplete(ArtifactOmission),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactOmission {
    FileLimit,
    SessionLimit,
    WriteFailed(String),
    CaptureFailed(String),
    DrainDeadline,
    ResultNotes,
}

impl SavedArtifact {
    pub(crate) fn is_complete(&self) -> bool {
        self.completeness == Completeness::Complete
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Usage {
    pub(crate) input_tokens: u64,
    pub(crate) cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_write_tokens: Option<u64>,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: Option<u64>,
    pub(crate) cost: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) cost_is_estimate: bool,
}

pub(crate) fn message_len(message: &Message) -> usize {
    match message {
        Message::Image(image) => image.history_len(),
        _ => encoded_len(message),
    }
}

pub(crate) fn call_ids(history: &[Message]) -> std::collections::HashSet<&str> {
    history
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| &assistant.parts)
        .filter_map(|part| match &part.content {
            Content::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect()
}

pub(crate) fn encoded_len(value: &(impl Serialize + ?Sized)) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.0)
}
