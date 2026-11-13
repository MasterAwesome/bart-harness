pub(crate) mod diagnostics;
mod format;

use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    future::Future,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Condvar, LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::history::Message;
pub(crate) use format::RecordedPrompt;
use format::{Entries, Entry, Header, Timed};

const RECORD: &str = "session.jsonl";
const PREVIEW_CHARS: usize = 80;
const MAX_REPLAY_BYTES: usize = 512 * 1024 * 1024;

pub(crate) type PendingWrite = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;

use crate::conversation::{Conversation, Repairs};

type ClosedNonempty = Arc<Mutex<Option<(PathBuf, Identity)>>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub(crate) cwd: PathBuf,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Sessions {
    root: PathBuf,
    dir: PathBuf,
    summaries: Arc<Mutex<HashMap<String, Cached>>>,
    closed_nonempty: ClosedNonempty,
}

pub(crate) struct Resumed {
    pub(crate) id: String,
    pub(crate) record: Recorder,
    pub(crate) previous: Meta,
    pub(crate) name: Option<String>,
    pub(crate) prompt: Option<RecordedPrompt>,
    pub(crate) conversation: Conversation,
}

#[derive(Clone)]
pub(crate) struct Summary {
    pub(crate) id: String,
    pub(crate) modified: SystemTime,
    pub(crate) description: Result<Description, String>,
    pub(crate) in_use: bool,
    pub(crate) here: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Description {
    pub(crate) meta: Meta,
    pub(crate) preview: String,
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    size: u64,
    modified: SystemTime,
}

#[derive(Clone)]
struct Cached {
    identity: Identity,
    description: Result<Description, String>,
}

impl Sessions {
    #[must_use]
    pub(crate) fn new(root: &Path, cwd: &Path) -> Self {
        let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
        let name: String = cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
        let hash = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
        Self {
            root: root.to_owned(),
            dir: root.join(format!("{name}-{hash:08x}")),
            summaries: Arc::default(),
            closed_nonempty: Arc::default(),
        }
    }

    #[must_use]
    pub(crate) fn dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub(crate) fn create(&self, meta: &Meta, project: &str) -> io::Result<(String, Recorder)> {
        let id = crate::random::uuid_v7()?;
        let path = self.dir.join(&id).join(RECORD);
        let mut recorder = Writer::create(&path, meta, project)?;
        recorder.closed_nonempty = Some((path, self.closed_nonempty.clone()));
        Ok((
            id,
            Recorder::from_writer(recorder, project.to_owned(), meta.cwd.clone()),
        ))
    }

    pub(crate) fn resume(&self, id: &str) -> io::Result<Resumed> {
        let path = self.record(id)?;
        let mut record = Writer::lock(crate::persist::open_append(&path)?)?;
        let Parsed {
            initial_cwd,
            meta,
            project,
            conversation,
            name,
            prompt,
            complete,
            retained,
        } = parse(Entries::new(&record.file))?;
        let (previous, project) = meta
            .zip(project)
            .ok_or_else(|| io::Error::other("the record does not describe its session"))?;
        let initial_cwd =
            initial_cwd.ok_or_else(|| io::Error::other("the record has no initial directory"))?;
        record.file.set_len(complete)?;
        record.retained = retained;
        record.closed_nonempty = Some((path, self.closed_nonempty.clone()));
        Ok(Resumed {
            id: id.to_owned(),
            record: Recorder::from_writer(record, project, initial_cwd),
            previous,
            name,
            prompt,
            conversation,
        })
    }

    fn list_directories(
        &self,
        directories: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<Vec<Summary>> {
        let cached = self.summaries.lock().expect(crate::NOT_POISONED).clone();
        let mut refreshed = HashMap::new();
        let mut found = Vec::new();
        for dir in directories {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let path = entry.path().join(RECORD);
                let metadata = match fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        found.push(Summary {
                            id,
                            modified: entry.metadata()?.modified()?,
                            description: Err("session.jsonl is missing".into()),
                            in_use: false,
                            here: dir == self.dir,
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let modified = metadata.modified()?;
                let identity = Identity {
                    size: metadata.len(),
                    modified,
                };
                let description = cached
                    .get(&id)
                    .filter(|cached| cached.identity == identity)
                    .map_or_else(
                        || summarize(&path).map_err(|error| error.to_string()),
                        |cached| cached.description.clone(),
                    );
                refreshed.insert(
                    id.clone(),
                    Cached {
                        identity,
                        description: description.clone(),
                    },
                );
                let in_use = in_use(&path)?;
                found.push(Summary {
                    id,
                    modified,
                    description,
                    in_use,
                    here: dir == self.dir,
                });
            }
        }
        let descriptions: Vec<_> = found
            .iter()
            .map(|summary| (&summary.id, &summary.description))
            .collect();
        if (found.len() * size_of::<Summary>())
            .saturating_add(crate::history::encoded_len(&descriptions))
            > 128 * 1024 * 1024
        {
            return Err(io::Error::other(
                "session listing exceeds the 128 MiB safety limit",
            ));
        }
        *self.summaries.lock().expect(crate::NOT_POISONED) = refreshed;
        found.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
        Ok(found)
    }

    pub(crate) fn list_all(&self) -> io::Result<Vec<Summary>> {
        let projects = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut directories = Vec::new();
        for project in projects {
            let project = project?;
            if project.file_type()?.is_dir() {
                directories.push(project.path());
            }
        }
        self.list_directories(directories)
    }

    pub(crate) fn latest(&self) -> io::Result<Option<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut latest = None;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let modified = match fs::metadata(entry.path().join(RECORD)) {
                Ok(metadata) => metadata.modified()?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    entry.metadata()?.modified()?
                }
                Err(error) => return Err(error),
            };
            if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
                latest = Some((modified, entry.file_name().to_string_lossy().into_owned()));
            }
        }
        Ok(latest.map(|(_, id)| id))
    }

    pub(crate) fn locate(&self, id: &str) -> io::Result<Self> {
        if self.record(id)?.is_file() {
            return Ok(self.clone());
        }
        let mut incomplete = None;
        for project in fs::read_dir(&self.root)? {
            let dir = project?.path();
            if dir.join(id).is_dir() {
                let located = Self {
                    dir,
                    ..self.clone()
                };
                if located.record(id)?.is_file() {
                    return Ok(located);
                }
                incomplete.get_or_insert(located);
            }
        }
        incomplete.ok_or_else(|| io::Error::other(format!("no session named {id:?}")))
    }

    pub(crate) fn directory(&self, id: &str) -> io::Result<PathBuf> {
        Ok(summarize(&self.record(id)?)?.meta.cwd)
    }

    pub(crate) fn delete(&self, id: &str) -> io::Result<()> {
        let _record = match crate::persist::open(&self.record(id)?) {
            Ok(file) => Some(Writer::lock(file)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        fs::remove_dir_all(self.dir(id))
    }

    pub(crate) fn delete_if_empty(&self, id: &str) -> io::Result<bool> {
        let path = self.record(id)?;
        let record = Writer::lock(crate::persist::open(&path)?)?;
        let metadata = record.file.metadata()?;
        let identity = Identity {
            size: metadata.len(),
            modified: metadata.modified()?,
        };
        if self
            .closed_nonempty
            .lock()
            .expect(crate::NOT_POISONED)
            .as_ref()
            .is_some_and(|(closed, known)| *closed == path && *known == identity)
        {
            return Ok(true);
        }
        let outline = outline(&record.file)?;
        if outline.messages == 0 && outline.name.is_none() {
            fs::remove_dir_all(self.dir(id))?;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn record(&self, id: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(id).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Ok(self.dir.join(id).join(RECORD)),
            _ => Err(io::Error::other(format!("no session named {id:?}"))),
        }
    }
}

#[derive(Default)]
struct Retained {
    history: Vec<usize>,
    bytes: usize,
    window: Option<RetainedWindow>,
    checkpoints: Vec<RetainedCheckpoint>,
}

#[derive(Clone)]
struct RetainedWindow {
    bytes: usize,
    messages: Vec<usize>,
}

struct RetainedCheckpoint {
    through: usize,
    window: RetainedWindow,
}

impl Retained {
    fn apply(&mut self, entry: &Entry) -> io::Result<()> {
        let charge = |message: &Message| {
            crate::history::message_len(message).saturating_add(size_of::<Message>())
        };
        match entry {
            Entry::Messages(messages) => {
                for message in messages {
                    let bytes = charge(message);
                    self.history.push(bytes);
                    self.bytes = self.bytes.saturating_add(bytes);
                    if let Some(window) = &mut self.window {
                        window.bytes = window.bytes.saturating_add(bytes);
                        window.messages.push(bytes);
                    }
                }
            }
            Entry::Rewind(kept) => {
                if *kept > self.history.len() {
                    return Err(io::Error::other("rewind is outside history"));
                }
                self.history.truncate(*kept);
                self.bytes = self.history.iter().copied().fold(0, usize::saturating_add);
                self.checkpoints.retain(|saved| saved.through <= *kept);
                self.window = self.checkpoints.last().map(|saved| {
                    let mut window = saved.window.clone();
                    let suffix = &self.history[saved.through..];
                    window.messages.extend_from_slice(suffix);
                    window.bytes = window
                        .bytes
                        .saturating_add(suffix.iter().copied().fold(0, usize::saturating_add));
                    window
                });
            }
            Entry::Compact { window, estimate } => {
                if estimate.messages > window.len() {
                    return Err(io::Error::other("checkpoint is outside window"));
                }
                let window = RetainedWindow {
                    bytes: window.iter().map(charge).fold(0, usize::saturating_add),
                    messages: window.iter().map(charge).collect(),
                };
                self.checkpoints.push(RetainedCheckpoint {
                    through: self.history.len(),
                    window: window.clone(),
                });
                self.window = Some(window);
            }
            Entry::Repair { history, window } => {
                for (at, message) in history {
                    if *at > self.history.len() {
                        return Err(io::Error::other("repair is outside history"));
                    }
                    let bytes = charge(message);
                    for saved in &mut self.checkpoints {
                        if *at < saved.through {
                            saved.through += 1;
                        }
                    }
                    self.history.insert(*at, bytes);
                    self.bytes = self.bytes.saturating_add(bytes);
                }
                for (at, message) in window {
                    let Some(target) = &mut self.window else {
                        return Err(io::Error::other("repair has no compacted window"));
                    };
                    if *at > target.messages.len() {
                        return Err(io::Error::other("repair is outside window"));
                    }
                    let bytes = charge(message);
                    if let Some(saved) = self.checkpoints.last_mut()
                        && *at < saved.window.messages.len()
                    {
                        saved.window.messages.insert(*at, bytes);
                        saved.window.bytes = saved.window.bytes.saturating_add(bytes);
                    }
                    target.messages.insert(*at, bytes);
                    target.bytes = target.bytes.saturating_add(bytes);
                }
            }
            Entry::Session(_)
            | Entry::Metadata(_)
            | Entry::Name(_)
            | Entry::Prompt(_)
            | Entry::Review(_) => {}
        }
        if self
            .bytes
            .saturating_add(self.window.as_ref().map_or(0, |window| window.bytes))
            .saturating_add(
                self.checkpoints
                    .iter()
                    .map(|saved| saved.window.bytes)
                    .fold(0, usize::saturating_add),
            )
            > MAX_REPLAY_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "retained session replay exceeds the 512 MiB bound",
            ));
        }
        Ok(())
    }
}

struct Parsed {
    initial_cwd: Option<PathBuf>,
    retained: Retained,
    complete: u64,
    name: Option<String>,
    prompt: Option<RecordedPrompt>,
    meta: Option<Meta>,
    project: Option<String>,
    conversation: Conversation,
}

fn parse(mut entries: Entries<'_>) -> io::Result<Parsed> {
    let mut name = None;
    let mut prompt = None;
    let mut meta = None;
    let mut project = None;
    let mut initial_cwd = None;
    let mut conversation = Conversation::default();
    let mut retained = Retained::default();
    for entry in entries.by_ref() {
        let entry = entry?;
        retained.apply(&entry)?;
        match entry {
            Entry::Name(latest) => name = Some(latest),
            Entry::Prompt(latest) => prompt = Some(latest),
            Entry::Session(header) => {
                initial_cwd = Some(header.meta.cwd.clone());
                meta = Some(header.meta);
                project = Some(header.project);
            }
            Entry::Metadata(latest) => meta = Some(latest),
            Entry::Messages(messages) => messages
                .into_iter()
                .for_each(|message| conversation.push(message)),
            Entry::Rewind(kept) => conversation.rewind(kept),
            Entry::Compact { window, estimate } => conversation.compact(window, estimate),
            Entry::Repair { history, window } => conversation.repair(history, window),
            Entry::Review(review) => conversation.review(review),
        }
    }
    conversation.share_payloads();
    Ok(Parsed {
        initial_cwd,
        retained,
        name,
        prompt,
        complete: entries.complete,
        meta,
        project,
        conversation,
    })
}

fn in_use(path: &Path) -> io::Result<bool> {
    let file = crate::persist::open(path)?;
    match file.try_lock_shared() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn summarize(path: &Path) -> io::Result<Description> {
    let file = crate::persist::open(path)?;
    let Outline {
        meta,
        first_user,
        name,
        ..
    } = outline(&file)?;
    let meta = meta.ok_or_else(|| io::Error::other("record has no session description"))?;
    let preview = name
        .or_else(|| {
            first_user.map(|(_, text)| text.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .unwrap_or_default();
    let preview = match preview.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &preview[..end]),
        None => preview,
    };
    Ok(Description { meta, preview })
}

#[derive(Default)]
struct Outline {
    name: Option<String>,
    meta: Option<Meta>,
    messages: usize,
    first_user: Option<(usize, Arc<str>)>,
}

fn outline(file: &File) -> io::Result<Outline> {
    let mut outline = Outline::default();
    for entry in Entries::new(file) {
        match entry? {
            Entry::Name(name) => outline.name = Some(name),
            Entry::Session(header) => outline.meta = Some(header.meta),
            Entry::Metadata(meta) => outline.meta = Some(meta),
            Entry::Messages(messages) => {
                for message in messages {
                    if outline.first_user.is_none()
                        && let Message::User(text) = message
                    {
                        outline.first_user = Some((outline.messages, text));
                    }
                    outline.messages += 1;
                }
            }
            Entry::Rewind(kept) => {
                if kept > outline.messages {
                    return Err(io::Error::other("rewind is outside history"));
                }
                outline.messages = kept;
                outline.first_user.take_if(|(at, _)| *at >= kept);
            }
            Entry::Repair { history, .. } => {
                for (at, _) in history {
                    if let Some((first, _)) = &mut outline.first_user
                        && at <= *first
                    {
                        *first += 1;
                    }
                    outline.messages += 1;
                }
            }
            Entry::Compact { .. } | Entry::Prompt(_) | Entry::Review(_) => {}
        }
    }
    Ok(outline)
}

type Release = Arc<(Mutex<bool>, Condvar)>;
static CLOSING: LazyLock<Mutex<HashMap<(u64, u64), Release>>> = LazyLock::new(Mutex::default);

pub(crate) struct WeakWriterHandle(std::sync::Weak<tokio::sync::Mutex<Writer>>);

#[derive(Clone)]
pub(crate) struct WriterLease {
    _writer: Arc<tokio::sync::Mutex<Writer>>,
}

impl WeakWriterHandle {
    pub(crate) fn try_lease_writer(&self) -> io::Result<WriterLease> {
        self.0
            .upgrade()
            .map(|writer| WriterLease { _writer: writer })
            .ok_or_else(|| io::Error::other("session is closed; artifact save refused"))
    }
}

pub(crate) struct Recorder {
    writer: Arc<tokio::sync::Mutex<Writer>>,
    identity: Option<(u64, u64)>,
    released: Release,
    project: String,
    initial_cwd: PathBuf,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.mark_closing();
    }
}

impl Recorder {
    pub(crate) fn weak_writer(&self) -> WeakWriterHandle {
        WeakWriterHandle(Arc::downgrade(&self.writer))
    }

    fn from_writer(writer: Writer, project: String, initial_cwd: PathBuf) -> Self {
        Self {
            identity: writer.file.metadata().ok().map(|m| (m.dev(), m.ino())),
            released: writer.released.clone(),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            project,
            initial_cwd,
        }
    }

    pub(crate) fn initial_cwd(&self) -> &Path {
        &self.initial_cwd
    }

    pub(crate) fn project(&self) -> &str {
        &self.project
    }

    fn mark_closing(&self) {
        if let Some(identity) = self.identity {
            CLOSING
                .lock()
                .expect(crate::NOT_POISONED)
                .insert(identity, self.released.clone());
        }
    }

    pub(crate) fn opening(
        &self,
        meta: Option<&Meta>,
        history: Repairs,
        window: Repairs,
        notices: &[crate::history::Notification],
    ) -> PendingWrite {
        let mut entries = Vec::new();
        if let Some(meta) = meta {
            entries.push(Entry::Metadata(meta.clone()));
        }
        if !history.is_empty() || !window.is_empty() {
            entries.push(Entry::Repair { history, window });
        }
        if !notices.is_empty() {
            entries.push(Entry::Messages(
                notices.iter().cloned().map(Message::Notification).collect(),
            ));
        }
        let writer = Arc::clone(&self.writer);
        Box::pin(async move {
            let mut writer = writer.lock_owned().await;
            tokio::task::spawn_blocking(move || {
                entries.into_iter().try_for_each(|entry| writer.line(entry))
            })
            .await
            .map_err(io::Error::other)?
        })
    }

    fn line(&self, entry: Entry) -> PendingWrite {
        let writer = Arc::clone(&self.writer);
        Box::pin(async move {
            let mut writer = writer.lock_owned().await;
            tokio::task::spawn_blocking(move || writer.line(entry))
                .await
                .map_err(io::Error::other)?
        })
    }

    pub(crate) fn append_batch(&self, messages: Vec<Message>) -> PendingWrite {
        self.line(Entry::Messages(messages))
    }

    pub(crate) async fn automatic_name(&self, base: &str) -> io::Result<String> {
        let path = self
            .writer
            .lock()
            .await
            .closed_nonempty
            .as_ref()
            .map(|(path, _)| path.clone())
            .ok_or_else(|| io::Error::other("session record has no path"))?;
        let base = base.to_owned();
        tokio::task::spawn_blocking(move || {
            let directory = path
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| io::Error::other("session record has no project directory"))?;
            let mut highest = 0_u64;
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let candidate = entry.path().join(RECORD);
                if candidate == path {
                    continue;
                }
                let file = match crate::persist::open(&candidate) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let Some(name) = outline(&file)?.name else {
                    continue;
                };
                if name == base {
                    highest = highest.max(1);
                } else if let Some(suffix) = name
                    .strip_prefix(&base)
                    .and_then(|rest| rest.strip_prefix('-'))
                    && !suffix.is_empty()
                    && suffix.bytes().all(|byte| byte.is_ascii_digit())
                {
                    highest = highest
                        .max(1)
                        .max(suffix.parse::<u64>().map_err(io::Error::other)?);
                }
            }
            if highest == 0 {
                Ok(base)
            } else {
                let next = highest
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("session name suffix overflow"))?;
                Ok(format!("{base}-{next}"))
            }
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) fn rename(&self, name: &str) -> PendingWrite {
        self.line(Entry::Name(name.to_owned()))
    }
    pub(crate) fn prompt(&self, prompt: &RecordedPrompt) -> PendingWrite {
        self.line(Entry::Prompt(prompt.clone()))
    }
    pub(crate) fn describe(&self, meta: &Meta) -> PendingWrite {
        self.line(Entry::Metadata(meta.clone()))
    }
    pub(crate) fn review(&self, review: &crate::approval::Review) -> PendingWrite {
        self.line(Entry::Review(review.clone()))
    }
    pub(crate) fn rewind(&self, kept: usize) -> PendingWrite {
        self.line(Entry::Rewind(kept))
    }
    pub(crate) fn repair(&self, history: Repairs, window: Repairs) -> PendingWrite {
        self.line(Entry::Repair { history, window })
    }

    pub(crate) fn compact(
        &self,
        window: &[Message],
        estimate: crate::conversation::Estimate,
    ) -> PendingWrite {
        self.line(Entry::Compact {
            window: window.to_vec(),
            estimate,
        })
    }

    pub(crate) fn close(
        self,
        notifications: Vec<crate::history::Notification>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.mark_closing();
        if notifications.is_empty() {
            return None;
        }
        Some(tokio::task::spawn_blocking(move || {
            let mut writer = self.writer.clone().blocking_lock_owned();
            let notifications = notifications
                .into_iter()
                .map(Message::Notification)
                .collect();
            if let Err(error) = writer.line(Entry::Messages(notifications)) {
                crate::terminal_text::stderr_line(format_args!(
                    "bart: cannot write the session record: {error}"
                ));
            }
        }))
    }
}

struct Writer {
    released: Release,
    retained: Retained,
    file: File,
    stopped: bool,
    closed_nonempty: Option<(PathBuf, ClosedNonempty)>,
}

impl Drop for Writer {
    // A forked child shares the file; closing ours alone would keep the lock until it execs.
    fn drop(&mut self) {
        if !self.retained.history.is_empty()
            && !self.stopped
            && let Some((path, cache)) = &self.closed_nonempty
            && let Ok(metadata) = self.file.metadata()
            && let Ok(modified) = metadata.modified()
        {
            *cache.lock().expect(crate::NOT_POISONED) = Some((
                path.clone(),
                Identity {
                    size: metadata.len(),
                    modified,
                },
            ));
        }
        let _ = self.file.unlock();
        if let Ok(metadata) = self.file.metadata() {
            let mut closing = CLOSING.lock().expect(crate::NOT_POISONED);
            let identity = (metadata.dev(), metadata.ino());
            if closing
                .get(&identity)
                .is_some_and(|released| Arc::ptr_eq(released, &self.released))
            {
                closing.remove(&identity);
            }
        }
        *self.released.0.lock().expect(crate::NOT_POISONED) = true;
        self.released.1.notify_all();
    }
}

impl Writer {
    fn lock(file: File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        let closing = CLOSING
            .lock()
            .expect(crate::NOT_POISONED)
            .get(&(metadata.dev(), metadata.ino()))
            .cloned();
        if let Some(released) = closing {
            drop(
                released
                    .1
                    .wait_while(released.0.lock().expect(crate::NOT_POISONED), |done| !*done)
                    .expect(crate::NOT_POISONED),
            );
        }
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                retained: Retained::default(),
                stopped: false,
                closed_nonempty: None,
                released: Arc::default(),
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::other(
                "the session is open in another Bart process",
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    fn create(path: &Path, meta: &Meta, project: &str) -> io::Result<Self> {
        let existing = path.ancestors().skip(1).find(|dir| dir.exists());
        if let Some(dir) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let mut recorder = Self::lock(file)?;
        recorder.line(Entry::Session(Header::new(meta, project)))?;
        for dir in path.ancestors().skip(1) {
            File::open(dir)?.sync_all()?;
            if Some(dir) == existing {
                break;
            }
        }
        Ok(recorder)
    }

    fn line(&mut self, entry: Entry) -> io::Result<()> {
        if self.stopped {
            return Err(io::Error::other(
                "an earlier write failed; the record stops at its last complete line",
            ));
        }
        let result = self
            .retained
            .apply(&entry)
            .and_then(|()| self.write_line(entry));
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn write_line(&mut self, entry: Entry) -> io::Result<()> {
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = Timed { entry, time_ms };
        let complete = self.file.metadata()?.len();
        let mut line = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        line.push(b'\n');
        if line.len() as u64 > format::MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "session record line exceeds the 256 MiB bound",
            ));
        }
        let written = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data());
        if written.is_err() {
            let _ = self.file.set_len(complete);
        }
        written
    }
}

#[cfg(test)]
mod tests;
