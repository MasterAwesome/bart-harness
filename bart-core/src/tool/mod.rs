use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::de::DeserializeOwned;
use serde_json::Value;

pub(crate) use crate::confinement::Place;
pub use crate::confinement::{Confinement, Escape, Network, check as check_confinement};
pub(crate) use crate::history::ToolDefinition as Definition;
pub use crate::history::{Activities, ActivityState as LiveState, Diff};

mod jobs;
mod live;
pub(crate) use live::Run;
pub use live::{Live, Snapshot, Summary as LiveSummary};
mod output;
mod process;

mod execution;
mod python;
mod web;

pub(crate) use jobs::Cleanup;
pub use jobs::Jobs;
pub use output::thousands;
pub(crate) use web::Route as WebRoute;

pub(crate) const EMPTY_CELL_REFUSAL: &str = "No code was sent; nothing ran.";

pub(crate) const OUTPUT_BUDGET_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundJobs {
    Stop,
    Keep,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnMode {
    #[default]
    Session,
    Prompt(BackgroundJobs),
}

impl TurnMode {
    pub fn one_turn(self) -> bool {
        matches!(self, Self::Prompt(_))
    }
}

pub struct Context {
    pub cwd: PathBuf,
    pub executable: PathBuf,
    spill_dir: PathBuf,
    artifacts: Arc<output::Storage>,
    output_budget: usize,
    cell_timeout: std::time::Duration,
    sees_images: std::sync::atomic::AtomicBool,
    pub diffs: std::sync::atomic::AtomicBool,
    turn_mode: std::sync::Mutex<TurnMode>,
    environment: std::sync::Mutex<Environment>,
    pub jobs: Arc<Jobs>,
    pub live: Arc<Live>,
    pub(crate) diagnostics: Option<Arc<crate::diagnostics::Log>>,
    interpreter: python::Interpreter,
    python: tokio::sync::Mutex<Option<(Confinement, Arc<str>)>>,
    web: std::sync::Mutex<Option<WebRoute>>,
    identity: std::sync::Mutex<Option<Identity>>,
}

struct Environment {
    confinement: Confinement,
    mcp: Arc<crate::mcp::Registry>,
    mcp_pending: bool,
}

#[derive(serde::Serialize)]
struct Identity {
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
}

impl Context {
    pub fn turn_mode(&self) -> TurnMode {
        *self.turn_mode.lock().expect(crate::NOT_POISONED)
    }

    pub fn set_turn_mode(&self, mode: TurnMode) {
        *self.turn_mode.lock().expect(crate::NOT_POISONED) = mode;
    }

    pub fn identify(&self, provider: &str, model: &str, reasoning_effort: Option<&str>) {
        *self.identity.lock().expect(crate::NOT_POISONED) = Some(Identity {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: reasoning_effort.map(str::to_owned),
        });
    }

    pub fn new(
        cwd: PathBuf,
        executable: PathBuf,
        spill_dir: PathBuf,
        output_budget: usize,
        confinement: Confinement,
        mcp: Arc<crate::mcp::Registry>,
    ) -> std::io::Result<Self> {
        Self::open_at(
            Place {
                cwd,
                executable,
                spill_dir,
                confinement,
            },
            output_budget,
            |_| mcp,
        )
    }

    pub(crate) fn open_at(
        place: Place,
        output_budget: usize,
        start: impl FnOnce(&Place) -> Arc<crate::mcp::Registry>,
    ) -> std::io::Result<Self> {
        let spill_dir = &place.spill_dir;
        std::fs::create_dir_all(spill_dir)?;
        std::fs::File::open(
            spill_dir
                .parent()
                .expect("the spill directory has a parent"),
        )?
        .sync_all()?;
        let artifacts = output::Storage::open(spill_dir)?;
        let highest_job = artifacts.highest_job();
        let mcp = start(&place);
        let Place {
            cwd,
            executable,
            spill_dir,
            confinement,
        } = place;
        Ok(Self {
            cwd,
            executable,
            artifacts,
            spill_dir,
            output_budget,
            cell_timeout: python::CELL_TIMEOUT,
            sees_images: std::sync::atomic::AtomicBool::new(true),
            diffs: std::sync::atomic::AtomicBool::new(true),
            turn_mode: std::sync::Mutex::default(),
            environment: std::sync::Mutex::new(Environment {
                confinement,
                mcp,
                mcp_pending: false,
            }),
            jobs: Arc::new(Jobs::after(highest_job)),
            live: Arc::default(),
            diagnostics: None,
            interpreter: python::Interpreter::default(),
            python: tokio::sync::Mutex::default(),
            web: std::sync::Mutex::default(),
            identity: std::sync::Mutex::default(),
        })
    }

    pub fn pycache_dir(&self) -> PathBuf {
        self.place().pycache_dir()
    }

    pub fn place(&self) -> Place {
        Place {
            cwd: self.cwd.clone(),
            executable: self.executable.clone(),
            spill_dir: self.spill_dir.clone(),
            confinement: self.confinement(),
        }
    }

    pub fn artifacts(&self) -> Artifacts {
        Artifacts(Arc::clone(&self.artifacts))
    }

    pub(crate) fn bind_artifacts(&self, writer: crate::record::WeakWriterHandle) {
        self.artifacts.bind(writer);
    }

    fn artifact_lease(&self) -> std::io::Result<Option<crate::record::WriterLease>> {
        self.artifacts.try_lease_writer()
    }

    pub async fn save_image(
        &self,
        image: crate::history::InlineImage,
    ) -> std::io::Result<crate::history::Image> {
        let artifacts = Arc::clone(&self.artifacts);
        let lease = self.artifact_lease()?;
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let extension = image
                .media_type()
                .strip_prefix("image/")
                .expect("admitted images have image media types");
            let number = artifacts.next_image()?;
            let path = output::save(
                &artifacts,
                output::Name::Image { number, extension },
                image.bytes(),
            )?;
            Ok(crate::history::Image {
                number,
                path,
                media_type: image.media_type().into(),
                byte_len: image.bytes().len(),
                label: image.label,
            })
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub fn record_diagnostics(&mut self) -> std::io::Result<()> {
        let dir = self
            .spill_dir
            .parent()
            .ok_or_else(|| std::io::Error::other("saved output has no session directory"))?;
        self.diagnostics = Some(Arc::new(crate::diagnostics::Log::open(dir)?));
        Ok(())
    }

    pub fn stop(&self) {
        self.mcp().stop();
        self.jobs.stop_all();
        self.interpreter.stop();
    }

    pub fn confinement(&self) -> Confinement {
        self.environment
            .lock()
            .expect(crate::NOT_POISONED)
            .confinement
    }

    pub fn mcp_pending(&self) -> bool {
        self.environment
            .lock()
            .expect(crate::NOT_POISONED)
            .mcp_pending
    }

    pub fn mcp(&self) -> Arc<crate::mcp::Registry> {
        Arc::clone(&self.environment.lock().expect(crate::NOT_POISONED).mcp)
    }

    pub async fn reconfine(
        &self,
        confinement: Confinement,
        replacement: impl FnOnce() -> Arc<crate::mcp::Registry>,
    ) -> (Cleanup, Result<(), String>) {
        let (mcp_only, previous) = {
            let mut environment = self.environment.lock().expect(crate::NOT_POISONED);
            let mcp_only = environment.confinement == confinement && environment.mcp_pending;
            environment.confinement = confinement;
            environment.mcp_pending = true;
            (mcp_only, Arc::clone(&environment.mcp))
        };
        previous.stop();
        if !mcp_only {
            self.jobs.stop_all();
            self.interpreter.reset();
        }
        let closed = previous.close().await;
        if closed.is_ok() {
            let mcp = replacement();
            let mut environment = self.environment.lock().expect(crate::NOT_POISONED);
            environment.mcp = mcp;
            environment.mcp_pending = false;
        }
        if mcp_only {
            return (Cleanup::Settled, closed);
        }
        let cleanup = self.jobs.settled().await;
        let restart = self.interpreter.restart(self).await;
        (cleanup, closed.and(restart))
    }

    // Cache the Python version, even if detection fails, so the tool description stays stable.
    // Check again when sandbox settings change: the sandbox may use a different Python.
    async fn python(&self) -> Arc<str> {
        let confinement = self.confinement();
        let mut known = self.python.lock().await;
        if let Some((asked, name)) = &*known
            && *asked == confinement
        {
            return Arc::clone(name);
        }
        let name: Arc<str> = python::identity(self)
            .await
            .map_or_else(|| "python3".into(), Into::into);
        *known = Some((confinement, Arc::clone(&name)));
        name
    }

    pub fn python_inventory(&self) -> String {
        self.interpreter.inventory()
    }

    pub fn reset_python(&self) {
        self.interpreter.reset();
    }

    pub fn set_image_input(&self, support: crate::provider::ImageInput) {
        self.sees_images
            .store(support.supported(), std::sync::atomic::Ordering::Relaxed);
    }

    #[must_use]
    pub fn sees_images(&self) -> bool {
        self.sees_images.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn route_web(&self, route: Option<WebRoute>) {
        let route = if self.confinement().network() == Network::Off {
            None
        } else {
            route
        };
        *self.web.lock().expect(crate::NOT_POISONED) = route;
    }

    pub fn searches(&self) -> bool {
        self.web_route().is_some()
    }

    fn web_route(&self) -> Option<WebRoute> {
        self.web.lock().expect(crate::NOT_POISONED).clone()
    }
}

#[must_use]
pub(crate) async fn definitions(context: &Context) -> Vec<Definition> {
    let python = context.python().await;
    let mcp_servers = context.mcp().names();
    vec![python::definition(&python::Surface {
        python: &python,
        escape: context.confinement().escape() == Escape::Offered,
        web_search: context.searches(),
        display: context.sees_images(),
        mode: context.turn_mode(),
        mcp_servers: &mcp_servers,
    })]
}

pub(crate) async fn run(
    call: &crate::history::ToolCall,
    context: &Arc<Context>,
    live: &Run,
    gate: Option<&mut crate::approval::Gate<'_>>,
) -> Output {
    match call.name.as_str() {
        "python" => match call.arguments.json() {
            Some(arguments) => python::call(arguments, &call.id, context, live, gate).await,
            None => Output::error("invalid arguments: not valid JSON".into()),
        },
        other => Output::error(format!("no tool named {other}")),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Output {
    pub notes: String,
    // Where captured output ends and Bart's notes begin. Never guess this from the text.
    prefix_len: Option<usize>,
    pub text: String,
    pub status: Status,
    pub jobs: Vec<u32>,
    pub images: Vec<crate::history::InlineImage>,
    pub saved: Option<crate::history::SavedArtifact>,
    pub cell: Option<u64>,
    pub diffs: Vec<Diff>,
    pub activities: Activities,
}

// Add all notes before applying the output size limit.
pub async fn deliver(
    output: &mut Output,
    context: &Context,
    image_room: usize,
    sees_images: bool,
) -> Vec<crate::history::Image> {
    use std::fmt::Write as _;
    let prefix = match output.prefix_len {
        Some(end) => output.text[..end].to_owned(),
        None => std::mem::take(&mut output.text),
    };
    if output.images.len() > image_room {
        output.images.truncate(image_room);
        if !output.notes.is_empty() && !output.notes.ends_with('\n') {
            output.notes.push('\n');
        }
        output.notes.push_str(if sees_images {
            bart!(const Image, "limit of 8 per response reached; the rest were not shown")
        } else {
            bart!(const Image, "not shown; this model cannot see images")
        });
    }
    let mut images = Vec::new();
    for image in std::mem::take(&mut output.images) {
        match context.save_image(image).await {
            Ok(image) => images.push(image),
            Err(error) => {
                if !output.notes.is_empty() && !output.notes.ends_with('\n') {
                    output.notes.push('\n');
                }
                let _ = write!(output.notes, "{}", bart!(Image, "not shown; {error}"));
            }
        }
    }
    let prefix = fit(
        prefix,
        context.output_budget,
        output.cell,
        output.saved.as_ref(),
        &context.artifacts,
        &mut output.notes,
    );
    output.prefix_len = Some(prefix.len());
    output.text = output::render(&prefix, &output.notes);
    images
}

fn fit(
    mut text: String,
    budget: usize,
    cell: Option<u64>,
    saved: Option<&crate::history::SavedArtifact>,
    storage: &output::Storage,
    notes: &mut String,
) -> String {
    if text.len() <= budget {
        return text;
    }
    let captured = text.len();
    output::truncate_prefix(&mut text, budget);
    let (shown, recovery) = saved.zip(cell).map_or(
        (text.len(), output::Recovery::Unsaved(cell)),
        |(saved, cell)| storage.remember(cell, saved, Some(text.len())),
    );
    notes.insert_str(
        0,
        &format!("{}\n", output::truncation(shown, captured, recovery)),
    );
    text
}

#[derive(Clone)]
pub struct Artifacts(Arc<output::Storage>);

impl Artifacts {
    pub fn save_history(&self, bytes: &[u8]) -> std::io::Result<u64> {
        let number = self.0.next_history()?;
        output::save(&self.0, output::Name::History(number), bytes)?;
        Ok(number)
    }

    pub fn save_paste(&self, id: u64, text: &str) -> std::io::Result<PathBuf> {
        output::save(&self.0, output::Name::Paste(id), text.as_bytes())
    }

    pub fn dir(&self) -> &Path {
        self.0.dir()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Succeeded,
    Failed,
}

impl Output {
    #[cfg(test)]
    #[must_use]
    fn success(text: String) -> Self {
        Self {
            notes: String::new(),
            prefix_len: None,
            text,
            status: Status::Succeeded,
            jobs: Vec::new(),
            images: Vec::new(),
            saved: None,
            cell: None,
            diffs: Vec::new(),
            activities: Activities::default(),
        }
    }

    #[must_use]
    pub(crate) fn error(text: String) -> Self {
        Self {
            notes: String::new(),
            prefix_len: None,
            text,
            status: Status::Failed,
            jobs: Vec::new(),
            images: Vec::new(),
            saved: None,
            cell: None,
            diffs: Vec::new(),
            activities: Activities::default(),
        }
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        self.status == Status::Failed
    }
}

fn parse_arguments<T: DeserializeOwned>(arguments: &Value) -> Result<T, String> {
    T::deserialize(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

impl Drop for Context {
    fn drop(&mut self) {
        self.stop();
    }
}
