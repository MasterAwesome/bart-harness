use std::{borrow::Cow, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SUMMARY: &str = "replaces earlier context";

pub fn is_summary(text: &str) -> bool {
    text.strip_prefix(bart!(const Summary, "").trim_end_matches(']'))
        .is_some_and(|fields| fields.starts_with(SUMMARY))
}

pub fn checkpoint_summary(messages: &[Message]) -> Option<&str> {
    messages.iter().rev().find_map(|message| match message {
        Message::Instruction(text) if is_summary(text) => {
            text.split_once('\n').map(|(_, summary)| summary)
        }
        _ => None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Responses,
    Chat,
    Messages,
    Gemini,
    Claude,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: Cow<'static, str>,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub path: String,
    pub hunks: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    Running,
    Finished,
    Failed,
    Interrupted,
}

impl ActivityState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activities {
    pub rows: Vec<Activity>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: usize,
}

impl Activities {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.omitted == 0
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub title: String,
    pub state: ActivityState,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

impl Risk {
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Authorization {
    Unknown,
    Low,
    Medium,
    High,
}

impl Authorization {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "lowercase")]
pub enum Review {
    Allow(Assessment),
    Deny(Assessment),
    Unavailable(Unavailable),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub call_id: String,
    pub constraint_conflict: bool,
    pub risk: Risk,
    pub authorization: Authorization,
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unavailable {
    pub call_id: String,
    pub reason: String,
}

impl Review {
    pub fn call_id(&self) -> &str {
        match self {
            Self::Allow(assessment) | Self::Deny(assessment) => &assessment.call_id,
            Self::Unavailable(unavailable) => &unavailable.call_id,
        }
    }

    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

pub enum Entry<'a> {
    Message(&'a Message),
    Compact(&'a str),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
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
pub struct UserSource {
    pub text: Arc<str>,
    pub archive: u64,
    pub index: usize,
}

impl Message {
    pub fn model_text(&self) -> std::borrow::Cow<'_, str> {
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
pub struct Skill {
    pub name: String,
    pub path: std::path::PathBuf,
    pub text: Arc<str>,
}

impl Skill {
    pub fn label(&self) -> String {
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

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    media_type: &'static str,
    bytes: Arc<[u8]>,
    pub label: String,
}

impl InlineImage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn media_type(&self) -> &'static str {
        self.media_type
    }

    pub fn new(bytes: Vec<u8>, advertised: Option<&str>, label: String) -> Result<Self, String> {
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
pub struct Image {
    pub number: u64,
    pub path: std::path::PathBuf,
    pub media_type: String,
    pub label: String,
    pub byte_len: usize,
}

impl Image {
    pub fn history_len(&self) -> usize {
        encoded_len(self).saturating_add(self.byte_len.div_ceil(3).saturating_mul(4))
    }

    pub fn url(&self) -> std::io::Result<String> {
        Ok(format!(
            "data:{};base64,{}",
            self.media_type,
            self.base64()?
        ))
    }

    pub fn base64(&self) -> std::io::Result<String> {
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
pub struct Notification {
    pub text: Arc<str>,
    pub kind: NotificationKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationKind {
    Recovery,
    JobLost {
        id: u32,
    },
    JobDetached {
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
pub enum JobOutcome {
    Exit(i32),
    Signal(i32),
    Killed,
    Unknown,
}

impl JobOutcome {
    pub fn label(self) -> String {
        match self {
            Self::Exit(code) => format!("exit {code}"),
            Self::Signal(signal) => format!("signal {signal}"),
            Self::Killed => "killed".into(),
            Self::Unknown => "outcome unknown".into(),
        }
    }
}

impl Notification {
    pub fn terminal_job(&self) -> Option<u32> {
        match self.kind {
            NotificationKind::Recovery
            | NotificationKind::JobMatched { .. }
            | NotificationKind::JobCommandExited { .. } => None,
            NotificationKind::JobLost { id }
            | NotificationKind::JobFinished { id, .. }
            | NotificationKind::JobDetached { id } => Some(id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    pub protocol: Protocol,
    pub provider: String,
    pub model: String,
    pub parts: Vec<Part>,
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continues: bool,
    pub refused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub content: Content,
    pub replay: Option<Arc<Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    Text(Arc<str>),
    Reasoning(Arc<str>),
    ToolCall(ToolCall),
    Other,
}

// Recorded as the exact text the provider sent; parsing it again restores the variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Arguments {
    Json { value: Arc<Value>, wire: Arc<str> },
    Invalid(Arc<str>),
}

impl Arguments {
    pub fn from_wire(raw: &str) -> Self {
        match serde_json::from_str(raw) {
            Ok(value) => Self::Json {
                value: Arc::new(value),
                wire: raw.to_owned().into(),
            },
            Err(_) => Self::Invalid(raw.to_owned().into()),
        }
    }

    pub fn wire(&self) -> &str {
        match self {
            Self::Json { wire, .. } | Self::Invalid(wire) => wire,
        }
    }

    pub fn json(&self) -> Option<&Value> {
        match self {
            Self::Json { value, .. } => Some(value),
            Self::Invalid(_) => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }

    pub fn rejected_replay(&self, call_id: &str) -> Option<String> {
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

    pub fn as_object(&self) -> Option<&serde_json::Map<String, Value>> {
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
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Arguments,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<Diff>,
    #[serde(default, skip_serializing_if = "Activities::is_empty")]
    pub activities: Activities,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    pub call_id: String,
    pub output: Arc<str>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<SavedArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedArtifact {
    pub path: std::path::PathBuf,
    pub completeness: Completeness,
    pub chars: usize,
    pub bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Incomplete(ArtifactOmission),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOmission {
    FileLimit,
    SessionLimit,
    WriteFailed(String),
    CaptureFailed(String),
    DrainDeadline,
    ResultNotes,
}

impl SavedArtifact {
    pub fn is_complete(&self) -> bool {
        self.completeness == Completeness::Complete
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cost_is_estimate: bool,
}

pub fn message_len(message: &Message) -> usize {
    match message {
        Message::Image(image) => image.history_len(),
        _ => encoded_len(message),
    }
}

pub fn call_ids(history: &[Message]) -> std::collections::HashSet<&str> {
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

pub fn encoded_len(value: &(impl Serialize + ?Sized)) -> usize {
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
