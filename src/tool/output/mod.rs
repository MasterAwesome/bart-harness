use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::history::{ArtifactOmission, Completeness, SavedArtifact};

pub(in crate::tool) const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

const MAX_SESSION_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SESSION_FILES: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Name {
    Code(u64),
    Output(u64),
    Status(u64),
    Process {
        cell: u64,
        call: u32,
    },
    Job(u32),
    JobResult(u32),
    Image {
        number: u64,
        extension: &'static str,
    },
    Paste(u64),
    History(u64),
}

impl Name {
    fn file(self) -> String {
        match self {
            Self::Code(cell) => format!("code-{cell}.py"),
            Self::Output(cell) => format!("output-{cell}.log"),
            Self::Status(cell) => format!("status-{cell}.jsonl"),
            Self::Process { cell, call } => format!("run-{cell}-{call}.log"),
            Self::Job(id) => format!("job-{id}.log"),
            Self::JobResult(id) => format!("job-{id}.json"),
            Self::Image { number, extension } => format!("image-{number}.{extension}"),
            Self::Paste(id) => format!("paste-{id}.txt"),
            Self::History(id) => format!("history-{id}.json"),
        }
    }

    pub(crate) fn reference(self) -> String {
        match self {
            Self::Code(cell) => format!("cell[{cell}].code"),
            Self::Output(cell) => format!("cell[{cell}].output"),
            Self::Status(cell) => format!("cell[{cell}].status"),
            Self::Process { cell, call } => format!("cell[{cell}].run[{call}]"),
            Self::Job(id) | Self::JobResult(id) => format!("job[{id}]"),
            Self::Image { number, .. } => format!("image[{number}]"),
            Self::Paste(id) => format!("paste[{id}]"),
            Self::History(id) => format!("history[{id}]"),
        }
    }
}

/// Numbers never repeat within a session, across resumes and rewinds.
#[derive(Default)]
struct Highest {
    history: u64,
    cell: u64,
    image: u64,
    job: u32,
}

impl Highest {
    fn see(&mut self, file: &str) {
        let number = |prefix: &str| {
            file.strip_prefix(prefix)?
                .split(['.', '-'])
                .next()?
                .parse::<u64>()
                .ok()
        };
        if let Some(history) = number("history-") {
            self.history = self.history.max(history);
        } else if let Some(cell) = number("code-")
            .or_else(|| number("output-"))
            .or_else(|| number("status-"))
        {
            self.cell = self.cell.max(cell);
        } else if let Some(image) = number("image-") {
            self.image = self.image.max(image);
        } else if let Some(job) = number("job-").and_then(|job| u32::try_from(job).ok()) {
            self.job = self.job.max(job);
        }
    }
}

fn next(counter: &std::sync::atomic::AtomicU64, kind: &str) -> io::Result<u64> {
    let ordering = std::sync::atomic::Ordering::Relaxed;
    counter
        .try_update(ordering, ordering, |highest| highest.checked_add(1))
        .map(|highest| highest + 1)
        .map_err(|_| io::Error::other(format!("{kind} numbers are exhausted; start a new session")))
}

pub(in crate::tool) struct Storage {
    dir: PathBuf,
    usage: Mutex<(u64, usize)>,
    writer: std::sync::OnceLock<crate::record::WeakWriterHandle>,
    cells: std::sync::atomic::AtomicU64,
    images: std::sync::atomic::AtomicU64,
    highest_job: u32,
    histories: std::sync::atomic::AtomicU64,
}

impl Storage {
    pub(in crate::tool) fn open(dir: &Path) -> io::Result<Arc<Self>> {
        let mut bytes = 0_u64;
        let mut files = 0_usize;
        let mut highest = Highest::default();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            highest.see(&entry.file_name().to_string_lossy());
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
            if files >= MAX_SESSION_FILES {
                bytes = MAX_SESSION_BYTES;
                break;
            }
        }
        Ok(Arc::new(Self {
            dir: dir.to_owned(),
            usage: Mutex::new((bytes, files)),
            writer: std::sync::OnceLock::new(),
            cells: highest.cell.into(),
            images: highest.image.into(),
            highest_job: highest.job,
            histories: highest.history.into(),
        }))
    }

    pub(in crate::tool) fn next_history(&self) -> io::Result<u64> {
        next(&self.histories, "history")
    }

    pub(in crate::tool) fn next_cell(&self) -> io::Result<u64> {
        next(&self.cells, "cell")
    }

    pub(in crate::tool) fn next_image(&self) -> io::Result<u64> {
        next(&self.images, "image")
    }

    pub(in crate::tool) fn highest_job(&self) -> u32 {
        self.highest_job
    }

    pub(in crate::tool) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(in crate::tool) fn bind(&self, writer: crate::record::WeakWriterHandle) {
        assert!(
            self.writer.set(writer).is_ok(),
            "artifact storage already bound to a record writer"
        );
    }

    pub(in crate::tool) fn try_lease_writer(
        &self,
    ) -> io::Result<Option<crate::record::WriterLease>> {
        self.writer
            .get()
            .map(crate::record::WeakWriterHandle::try_lease_writer)
            .transpose()
    }

    fn reserve(&self, bytes: u64, files: usize) -> bool {
        let mut usage = self.usage.lock().expect(crate::NOT_POISONED);
        if usage.0.saturating_add(bytes) > MAX_SESSION_BYTES
            || usage.1.saturating_add(files) > MAX_SESSION_FILES
        {
            return false;
        }
        usage.0 += bytes;
        usage.1 += files;
        true
    }

    fn release(&self, bytes: u64, files: usize) {
        let mut usage = self.usage.lock().expect(crate::NOT_POISONED);
        usage.0 -= bytes;
        usage.1 -= files;
    }
}

struct Artifact {
    storage: Arc<Storage>,
    session_capped: bool,
    path: PathBuf,
    reference: String,
    earlier: Option<ArtifactOmission>,
    file: io::Result<BufWriter<File>>,
    written: usize,
    capped: bool,
    pending: Vec<u8>,
    lease: Option<crate::record::WriterLease>,
}

impl Artifact {
    fn named(storage: &Arc<Storage>, name: Name) -> io::Result<Self> {
        Self::create(storage.dir.join(name.file()), name.reference(), storage)
    }

    fn append(
        storage: &Arc<Storage>,
        name: Name,
        earlier: Option<ArtifactOmission>,
    ) -> io::Result<Self> {
        let path = storage.dir.join(name.file());
        let file = OpenOptions::new().append(true).open(&path)?;
        let written = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
        Ok(Self {
            storage: Arc::clone(storage),
            lease: storage.try_lease_writer()?,
            session_capped: false,
            path,
            reference: name.reference(),
            earlier,
            file: Ok(BufWriter::new(file)),
            written,
            capped: false,
            pending: Vec::new(),
        })
    }

    fn create(path: PathBuf, reference: String, storage: &Arc<Storage>) -> io::Result<Self> {
        let lease = storage.try_lease_writer()?;
        if !storage.reserve(0, 1) {
            return Err(io::Error::other(
                "session artifact limit reached (2 GiB or 16384 files); no evidence deleted",
            ));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .inspect_err(|_| storage.release(0, 1))?;
        Ok(Self {
            storage: Arc::clone(storage),
            lease,
            session_capped: false,
            path,
            reference,
            earlier: None,
            file: Ok(BufWriter::new(file)),
            written: 0,
            capped: false,
            pending: Vec::new(),
        })
    }

    fn write(&mut self, text: &[u8], end: &[u8]) {
        let length = text.len() + end.len();
        if !self.admit(length) {
            return;
        }
        self.pending.extend_from_slice(text);
        self.pending.extend_from_slice(end);
    }

    fn admit(&mut self, length: usize) -> bool {
        if self.file.is_err() || self.capped || self.session_capped || self.earlier.is_some() {
            return false;
        }
        if self.written + length > MAX_FILE_BYTES {
            self.capped = true;
            return false;
        }
        if !self.storage.reserve(length as u64, 0) {
            self.session_capped = true;
            return false;
        }
        self.written += length;
        true
    }

    fn flush(&mut self) {
        if let Ok(file) = &mut self.file
            && let Err(error) = file
                .write_all(&std::mem::take(&mut self.pending))
                .and_then(|()| file.flush())
        {
            self.file = Err(error);
        }
    }

    async fn flush_live(&mut self) {
        if self.file.is_err() || self.pending.is_empty() {
            return;
        }
        let file = std::mem::replace(
            &mut self.file,
            Err(io::Error::other(
                "artifact flush interrupted; saved prefix may be incomplete",
            )),
        );
        let pending = std::mem::take(&mut self.pending);
        let lease = self.lease.clone();
        self.file = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let mut file = file?;
            file.write_all(&pending)?;
            file.flush()?;
            Ok(file)
        })
        .await
        .unwrap_or_else(|error| Err(io::Error::other(error)));
    }

    fn finalize(&mut self) {
        self.flush();
        if let Ok(file) = &self.file
            && let Err(error) = file.get_ref().sync_all().and_then(|()| {
                File::open(
                    self.path
                        .parent()
                        .expect("an artifact path has a directory"),
                )?
                .sync_all()
            })
        {
            self.file = Err(error);
        }
    }

    fn completeness(&self) -> Completeness {
        if let Some(omission) = &self.earlier {
            return Completeness::Incomplete(omission.clone());
        }
        if self.session_capped {
            return Completeness::Incomplete(ArtifactOmission::SessionLimit);
        }
        match (&self.file, self.capped) {
            (Err(error), _) => {
                Completeness::Incomplete(ArtifactOmission::WriteFailed(error.to_string()))
            }
            (Ok(_), true) => Completeness::Incomplete(ArtifactOmission::FileLimit),
            (Ok(_), false) => Completeness::Complete,
        }
    }

    fn complete(&self) -> bool {
        self.completeness() == Completeness::Complete
    }

    fn describe(&self, what: &str) -> String {
        let reason = if self.earlier.is_some() {
            Some("an earlier part was not saved".to_owned())
        } else if self.session_capped {
            Some("session artifact limit reached (2 GiB); no evidence deleted".to_owned())
        } else {
            match (&self.file, self.capped) {
                (Err(error), _) => Some(format!("save failed: {error}")),
                (Ok(_), true) => Some(format!(
                    "file limit reached ({} MiB); the saved prefix may be shorter",
                    MAX_FILE_BYTES / (1024 * 1024)
                )),
                (Ok(_), false) => None,
            }
        };
        let reference = &self.reference;
        reason.map_or_else(
            || format!("{what}: {reference}"),
            |reason| format!("{what}: {reference}, incomplete: {reason}"),
        )
    }
}

impl Artifact {
    fn remove(&self) {
        if fs::remove_file(&self.path).is_ok() {
            self.storage.release(self.written as u64, 1);
        }
    }
}

pub(in crate::tool) fn save(
    storage: &Arc<Storage>,
    name: Name,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let mut output = Artifact::named(storage, name)?;
    if output.admit(bytes.len())
        && let Ok(file) = &mut output.file
        && let Err(error) = file.write_all(bytes)
    {
        output.file = Err(error);
    }
    output.finalize();
    if !output.complete() {
        let error = io::Error::other(output.describe("saving"));
        output.remove();
        return Err(error);
    }
    Ok(output.path.clone())
}

pub(in crate::tool) struct StatusWriter {
    storage: Arc<Storage>,
    name: Name,
    resumed: bool,
    artifact: Option<io::Result<Artifact>>,
}

impl StatusWriter {
    pub(in crate::tool) fn write(&mut self, value: &serde_json::Value) {
        let artifact = self.artifact.get_or_insert_with(|| {
            if self.resumed && self.storage.dir.join(self.name.file()).try_exists()? {
                Artifact::append(
                    &self.storage,
                    self.name,
                    Some(ArtifactOmission::CaptureFailed(
                        "earlier collection's status completeness was not rechecked".into(),
                    )),
                )
            } else {
                Artifact::named(&self.storage, self.name)
            }
        });
        if let Ok(artifact) = artifact {
            artifact.write(format!("{value}\n").as_bytes(), b"");
            artifact.finalize();
        }
    }

    pub(in crate::tool) fn finish(self, omitted: u64) -> String {
        match self.artifact {
            Some(Ok(mut artifact)) => {
                artifact.finalize();
                if omitted == 0 && artifact.complete() {
                    String::new()
                } else {
                    let records = artifact.describe("all records");
                    if omitted > 0 {
                        bart!(Run, "{omitted} more not shown; {records}")
                    } else {
                        bart!(Run, "{records}")
                    }
                }
            }
            Some(Err(error)) => {
                let failure = bart!(Run, "records not saved; {error}");
                if omitted > 0 {
                    format!("{}\n{failure}", bart!(Run, "{omitted} more not shown"))
                } else {
                    failure
                }
            }
            None => String::new(),
        }
    }
}

pub(super) enum CaptureFailure {
    Read(io::Error),
    DrainDeadline,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(in crate::tool) struct SavedStream {
    pub(in crate::tool) path: PathBuf,
    pub(in crate::tool) incomplete: Option<String>,
}

pub(in crate::tool) struct Collector {
    streams: Option<[io::Result<Artifact>; 2]>,
    capture_failure: Option<CaptureFailure>,
    budget: usize,
    head: String,
    lines: usize,
    bytes: usize,
    chars: usize,
    overflowed: bool,
    resumed: bool,
    name: Name,
    output: Artifact,
    last_flush: std::time::Instant,
}

impl Collector {
    pub(in crate::tool) fn new(
        budget: usize,
        storage: &Arc<Storage>,
        name: Name,
    ) -> io::Result<Self> {
        Ok(Self::with(budget, name, Artifact::named(storage, name)?))
    }

    fn with(budget: usize, name: Name, output: Artifact) -> Self {
        Self {
            streams: None,
            capture_failure: None,
            budget,
            head: String::new(),
            lines: 0,
            bytes: 0,
            chars: 0,
            overflowed: false,
            resumed: false,
            name,
            output,
            last_flush: std::time::Instant::now(),
        }
    }

    pub(in crate::tool) async fn open(
        budget: usize,
        storage: &Arc<Storage>,
        name: Name,
    ) -> io::Result<Self> {
        let storage = Arc::clone(storage);
        let lease = storage.try_lease_writer()?;
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            Self::new(budget, &storage, name)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(in crate::tool) async fn reopen(
        budget: usize,
        storage: &Arc<Storage>,
        name: Name,
        earlier: SavedArtifact,
    ) -> io::Result<Self> {
        let storage = Arc::clone(storage);
        let lease = storage.try_lease_writer()?;
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let omission = match earlier.completeness {
                Completeness::Complete => None,
                Completeness::Incomplete(omission) => Some(omission),
            };
            let output = Artifact::append(&storage, name, omission)?;
            let mut collector = Self::with(budget, name, output);
            collector.chars = earlier.chars;
            collector.resumed = true;
            Ok(collector)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(in crate::tool) async fn open_cell(
        budget: usize,
        storage: &Arc<Storage>,
        code: &str,
    ) -> io::Result<Self> {
        let storage = Arc::clone(storage);
        let code = code.to_owned();
        let lease = storage.try_lease_writer()?;
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let cell = storage.next_cell()?;
            save(&storage, Name::Code(cell), code.as_bytes())?;
            Self::new(budget, &storage, Name::Output(cell))
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) async fn start_streams(&mut self) {
        let storage = Arc::clone(&self.output.storage);
        let path = self.output.path.clone();
        let references = ["stdout", "stderr"].map(|stream| match self.name {
            Name::Process { cell, call } => format!("cell[{cell}].{stream}[{call}]"),
            Name::Job(id) => format!("job[{id}].{stream}_file.path"),
            _ => unreachable!("only command collectors capture streams"),
        });
        self.streams = Some(
            tokio::task::spawn_blocking(move || {
                [0, 1].map(|i| {
                    let path = path.with_extension(["stdout", "stderr"][i]);
                    Artifact::create(path, references[i].clone(), &storage)
                })
            })
            .await
            .expect("stream artifact worker"),
        );
    }

    pub(super) fn stream_bytes(&mut self, source: usize, bytes: &[u8]) {
        if let Some(streams) = &mut self.streams
            && let Ok(artifact) = &mut streams[source]
        {
            artifact.write(bytes, b"");
        }
    }

    pub(super) fn saved_streams(&self, ended: [bool; 2]) -> [SavedStream; 2] {
        std::array::from_fn(|i| {
            let incomplete = match &self.streams.as_ref().expect("streams started")[i] {
                Err(error) => Some(format!("cannot save stream: {error}")),
                Ok(artifact) if !artifact.complete() => Some(artifact.describe("saved prefix")),
                Ok(_) if self.capture_failure.is_some() => {
                    Some("output capture failed before completion".to_owned())
                }
                Ok(_) if !ended[i] => Some("capture has not reached EOF".to_owned()),
                Ok(_) => None,
            };
            SavedStream {
                path: self.output.path.with_extension(["stdout", "stderr"][i]),
                incomplete,
            }
        })
    }

    pub(super) fn finish_streams(&mut self, ended: [bool; 2]) -> Option<[SavedStream; 2]> {
        self.streams.as_ref()?;
        for artifact in self.streams.iter_mut().flatten().flatten() {
            artifact.finalize();
        }
        Some(self.saved_streams(ended))
    }

    pub(in crate::tool) fn discard(self) {
        for artifact in self.streams.into_iter().flatten().flatten() {
            artifact.remove();
        }
        self.output.remove();
    }

    pub(in crate::tool) fn link(&self, name: Name) -> io::Result<()> {
        let storage = &self.output.storage;
        if !storage.reserve(0, 1) {
            return Err(io::Error::other("session artifact limit reached"));
        }
        fs::hard_link(&self.output.path, storage.dir.join(name.file()))
            .inspect_err(|_| storage.release(0, 1))
    }

    pub(in crate::tool) fn saved(&self) -> SavedArtifact {
        let completeness = match (&self.capture_failure, self.output.completeness()) {
            (Some(CaptureFailure::Read(error)), Completeness::Complete) => {
                Completeness::Incomplete(ArtifactOmission::CaptureFailed(error.to_string()))
            }
            (Some(CaptureFailure::DrainDeadline), Completeness::Complete) => {
                Completeness::Incomplete(ArtifactOmission::DrainDeadline)
            }
            (_, completeness) => completeness,
        };
        SavedArtifact {
            path: self.output.path.clone(),
            completeness,
            chars: self.chars,
        }
    }

    pub(in crate::tool) fn status_writer(&self) -> StatusWriter {
        StatusWriter {
            storage: Arc::clone(&self.output.storage),
            name: Name::Status(self.cell().expect("process status belongs to a cell")),
            resumed: self.resumed,
            artifact: None,
        }
    }

    pub(in crate::tool) fn cell(&self) -> Option<u64> {
        match self.name {
            Name::Output(cell) => Some(cell),
            _ => None,
        }
    }

    pub(super) fn storage(&self) -> &Arc<Storage> {
        &self.output.storage
    }

    pub(in crate::tool) fn path(&self) -> &Path {
        &self.output.path
    }

    pub(in crate::tool) fn line(&mut self, text: &str) {
        for line in text.split('\n') {
            self.physical_line(line);
        }
    }

    fn physical_line(&mut self, line: &str) {
        self.lines = self.lines.saturating_add(1);
        self.bytes = self.bytes.saturating_add(line.len()).saturating_add(1);
        self.chars = self
            .chars
            .saturating_add(line.chars().count())
            .saturating_add(1);
        self.output.write(line.as_bytes(), b"\n");
        if !self.overflowed {
            let room = self.budget.saturating_sub(self.head.len());
            let mut end = room.min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            self.head.push_str(&line[..end]);
            if end == line.len() && self.head.len() < self.budget {
                self.head.push('\n');
            } else {
                // Once a byte is omitted, later lines can never join this prefix.
                self.overflowed = true;
            }
        }
    }

    pub(super) fn capture_failed(&mut self, failure: CaptureFailure) {
        self.capture_failure = Some(failure);
    }

    pub(in crate::tool) async fn flush_until(&mut self, deadline: tokio::time::Instant) -> bool {
        tokio::time::timeout_at(deadline, self.flush())
            .await
            .is_ok()
    }

    pub(in crate::tool) async fn persist_until(&mut self, deadline: tokio::time::Instant) -> bool {
        tokio::time::timeout_at(deadline, async {
            for artifact in self.streams.iter_mut().flatten().flatten() {
                artifact.flush_live().await;
            }
            self.output.flush_live().await;
            self.last_flush = std::time::Instant::now();
            self.flush().await;
        })
        .await
        .is_ok()
    }

    pub(in crate::tool) async fn flush(&mut self) {
        for artifact in self.streams.iter_mut().flatten().flatten() {
            if artifact.pending.len() >= 64 * 1024
                || self.last_flush.elapsed() >= std::time::Duration::from_millis(100)
            {
                artifact.flush_live().await;
            }
        }
        if self.output.pending.len() >= 64 * 1024
            || self.last_flush.elapsed() >= std::time::Duration::from_millis(100)
        {
            self.output.flush_live().await;
            self.last_flush = std::time::Instant::now();
        }
    }

    /// Keep output bytes and host-authored delivery metadata separate. The saved
    /// artifact contains captured output only, never an inserted omission notice.
    pub(in crate::tool) fn finish(mut self) -> (String, String, SavedArtifact) {
        self.output.finalize();
        let saved = self.saved();
        let mut notes = String::new();
        if self.overflowed {
            let _ = writeln!(
                notes,
                "{}",
                truncation(
                    &self.head,
                    self.bytes,
                    Some(&self.output.reference),
                    Truncation {
                        complete: saved.is_complete(),
                        resumed: self.resumed
                    },
                )
            );
        }
        if !self.output.complete() {
            let _ = writeln!(
                notes,
                "{}",
                bart!(Output, "{}", self.output.describe("saved"))
            );
        }
        match &self.capture_failure {
            Some(CaptureFailure::Read(error)) => {
                let _ = writeln!(
                    notes,
                    "{}",
                    bart!(
                        Output,
                        "{error}; {} incomplete; total output size unknown",
                        self.output.reference
                    )
                );
            }
            Some(CaptureFailure::DrainDeadline) => {
                let _ = writeln!(
                    notes,
                    "{}",
                    bart!(
                        Output,
                        "drain deadline before EOF; {} incomplete; total output size unknown",
                        self.output.reference
                    )
                );
            }
            None => {}
        }
        (self.head, notes, saved)
    }
}

pub(in crate::tool) const HOST_METADATA_BYTES: usize = 24 * 1024;
pub(in crate::tool) const HOST_METADATA_BOUNDARY: &str = "\n";

pub(in crate::tool) fn render(prefix: &str, notes: &str) -> String {
    if notes.is_empty() {
        return prefix.to_owned();
    }
    let omitted = bart!(const Output, "further notes omitted");
    let mut result = prefix.to_owned();
    if !prefix.is_empty() && !prefix.ends_with('\n') {
        result.push_str(HOST_METADATA_BOUNDARY);
    }
    let start = result.len();
    for line in notes.lines() {
        if result.len() - start + line.len() + 1 > HOST_METADATA_BYTES - omitted.len() - 1 {
            result.push_str(omitted);
            return result;
        }
        result.push_str(line);
        result.push('\n');
    }
    result.truncate(result.trim_end_matches('\n').len());
    result
}

pub(crate) fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub(super) struct Truncation {
    pub(super) complete: bool,
    pub(super) resumed: bool,
}

pub(super) fn truncation(
    head: &str,
    total: usize,
    reference: Option<&str>,
    Truncation { complete, resumed }: Truncation,
) -> String {
    let recovery = match reference {
        Some(reference) if head.contains('\n') => {
            let next = head.lines().count() + usize::from(head.ends_with('\n'));
            let incomplete = if complete {
                ""
            } else {
                "; saved copy incomplete"
            };
            format!("read({reference}, offset={next}){incomplete}")
        }
        Some(reference) => {
            let label = if complete {
                "all of it"
            } else {
                "saved copy (incomplete)"
            };
            format!("{label}: {reference}")
        }
        None => "the rest was not saved".into(),
    };
    let scope = if resumed { "new " } else { "" };
    bart!(
        Output,
        "first {} of {total} {scope}bytes shown; {recovery}",
        head.len()
    )
}

#[cfg(test)]
mod tests;
