use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::de::DeserializeOwned;
use serde_json::Value;

mod jobs;
mod live;
pub(crate) use live::{
    Activities, Live, Run, Snapshot, State as LiveState, Summary as LiveSummary,
};
mod output;
mod process;
pub(crate) use process::{Place, launch};

mod execution;
mod python;
mod web;

pub(crate) use jobs::{Cleanup, Jobs};
pub(crate) use output::thousands;
pub(crate) use web::Route as WebRoute;

pub(crate) const EMPTY_CELL_REFUSAL: &str = "No code was sent; nothing ran.";

pub(crate) const OUTPUT_BUDGET_BYTES: usize = 32 * 1024;

#[derive(Clone)]
pub(crate) struct Definition {
    pub(crate) name: &'static str,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

pub(crate) struct Context {
    pub(crate) cwd: PathBuf,
    pub(crate) executable: PathBuf,
    spill_dir: PathBuf,
    artifacts: Arc<output::Storage>,
    output_budget: usize,
    pub(crate) diffs: std::sync::atomic::AtomicBool,
    pub(crate) one_turn: std::sync::atomic::AtomicBool,
    environment: std::sync::Mutex<(Confinement, Arc<crate::mcp::Registry>)>,
    pub(crate) jobs: Arc<Jobs>,
    pub(crate) live: Arc<Live>,
    pub(crate) diagnostics: Option<Arc<crate::record::diagnostics::Log>>,
    interpreter: python::Interpreter,
    python: tokio::sync::Mutex<Option<(Confinement, Arc<str>)>>,
    web: std::sync::Mutex<Option<WebRoute>>,
    identity: std::sync::Mutex<Option<Identity>>,
}

#[derive(serde::Serialize)]
struct Identity {
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
}

impl Context {
    pub(crate) fn identify(&self, provider: &str, model: &str, reasoning_effort: Option<&str>) {
        *self.identity.lock().expect(crate::NOT_POISONED) = Some(Identity {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: reasoning_effort.map(str::to_owned),
        });
    }

    pub(crate) fn new(
        cwd: PathBuf,
        executable: PathBuf,
        spill_dir: PathBuf,
        output_budget: usize,
        confinement: Confinement,
        mcp: Arc<crate::mcp::Registry>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&spill_dir)?;
        std::fs::File::open(
            spill_dir
                .parent()
                .expect("the spill directory has a parent"),
        )?
        .sync_all()?;
        let artifacts = output::Storage::open(&spill_dir)?;
        let highest_job = artifacts.highest_job();
        Ok(Self {
            cwd,
            executable,
            artifacts,
            spill_dir,
            output_budget,
            diffs: std::sync::atomic::AtomicBool::new(true),
            one_turn: std::sync::atomic::AtomicBool::new(false),
            environment: std::sync::Mutex::new((confinement, mcp)),
            jobs: Arc::new(Jobs::after(highest_job)),
            live: Arc::default(),
            diagnostics: None,
            interpreter: python::Interpreter::default(),
            python: tokio::sync::Mutex::default(),
            web: std::sync::Mutex::default(),
            identity: std::sync::Mutex::default(),
        })
    }

    pub(crate) fn pycache_dir(&self) -> PathBuf {
        self.place().pycache_dir()
    }

    pub(crate) fn place(&self) -> process::Place {
        process::Place {
            cwd: self.cwd.clone(),
            executable: self.executable.clone(),
            spill_dir: self.spill_dir.clone(),
            confinement: self.confinement(),
        }
    }

    pub(crate) fn artifacts(&self) -> Artifacts {
        Artifacts(Arc::clone(&self.artifacts))
    }

    pub(crate) fn bind_artifacts(&self, writer: crate::record::WeakWriterHandle) {
        self.artifacts.bind(writer);
    }

    fn artifact_lease(&self) -> std::io::Result<Option<crate::record::WriterLease>> {
        self.artifacts.try_lease_writer()
    }

    pub(crate) async fn save_image(
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

    pub(crate) fn record_diagnostics(&mut self) -> std::io::Result<()> {
        let dir = self
            .spill_dir
            .parent()
            .ok_or_else(|| std::io::Error::other("saved output has no session directory"))?;
        self.diagnostics = Some(Arc::new(crate::record::diagnostics::Log::open(dir)?));
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.mcp().stop();
        self.jobs.stop_all();
        self.interpreter.stop();
    }

    pub(crate) fn confinement(&self) -> Confinement {
        self.environment.lock().expect(crate::NOT_POISONED).0
    }

    pub(crate) fn mcp(&self) -> Arc<crate::mcp::Registry> {
        Arc::clone(&self.environment.lock().expect(crate::NOT_POISONED).1)
    }

    pub(crate) async fn reconfine(
        &self,
        confinement: Confinement,
        mcp: Arc<crate::mcp::Registry>,
    ) -> (Cleanup, Result<(), String>) {
        let previous = {
            let mut environment = self.environment.lock().expect(crate::NOT_POISONED);
            std::mem::replace(&mut *environment, (confinement, mcp)).1
        };
        previous.stop();
        self.jobs.stop_all();
        let cleanup = self.jobs.settled().await;
        (cleanup, self.interpreter.restart(self).await)
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

    pub(crate) fn python_inventory(&self) -> String {
        self.interpreter.inventory()
    }

    pub(crate) fn reset_python(&self) {
        self.interpreter.reset();
    }

    pub(crate) fn route_web(&self, route: Option<WebRoute>) {
        let route = if self.confinement().network() == Network::Off {
            None
        } else {
            route
        };
        *self.web.lock().expect(crate::NOT_POISONED) = route;
    }

    pub(crate) fn searches(&self) -> bool {
        self.web_route().is_some()
    }

    fn web_route(&self) -> Option<WebRoute> {
        self.web.lock().expect(crate::NOT_POISONED).clone()
    }
}

pub(crate) fn check_confinement(
    cwd: &Path,
    config: &Path,
    confinement: Confinement,
) -> Result<(), String> {
    if confinement.jailed() {
        if config
            .canonicalize()
            .is_ok_and(|config| cwd.starts_with(config))
        {
            return Err("sandbox cannot run in Bart's credential directory".into());
        }
        process::check_jail(cwd)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confinement {
    Unrestricted,
    Jailed(Network, Escape),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Escape {
    Offered,
    Withheld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Network {
    On,
    Off,
}

impl Confinement {
    pub(crate) fn jailed(self) -> bool {
        matches!(self, Self::Jailed(..))
    }

    pub(crate) fn network(self) -> Network {
        match self {
            Self::Unrestricted => Network::On,
            Self::Jailed(network, _) => network,
        }
    }

    pub(crate) fn escape(self) -> Escape {
        match self {
            Self::Unrestricted => Escape::Withheld,
            Self::Jailed(_, escape) => escape,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unrestricted => "sandbox off",
            Self::Jailed(Network::On, _) => "sandbox on",
            Self::Jailed(Network::Off, _) => "sandbox on, network off",
        }
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
        one_turn: context.one_turn.load(std::sync::atomic::Ordering::Relaxed),
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
pub(crate) struct Output {
    pub(crate) notes: String,
    // Where captured output ends and Bart's notes begin. Never guess this from the text.
    prefix_len: Option<usize>,
    pub(crate) text: String,
    pub(crate) status: Status,
    pub(crate) jobs: Vec<u32>,
    pub(crate) images: Vec<crate::history::InlineImage>,
    pub(crate) saved: Option<crate::history::SavedArtifact>,
    pub(crate) cell: Option<u64>,
    pub(crate) diffs: Vec<Diff>,
    pub(crate) activities: Activities,
}

// Add all notes before applying the output size limit.
pub(crate) async fn deliver(
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
        output.notes.push_str(if sees_images {
            concat!(
                "\n",
                bart!(const Image, "limit of 8 per response reached; the rest were not shown")
            )
        } else {
            concat!(
                "\n",
                bart!(const Image, "not shown; this model cannot see images")
            )
        });
    }
    let mut images = Vec::new();
    for image in std::mem::take(&mut output.images) {
        match context.save_image(image).await {
            Ok(image) => images.push(image),
            Err(error) => {
                let _ = write!(output.notes, "\n{}", bart!(Image, "not shown; {error}"));
            }
        }
    }
    if output.is_error()
        && let Some(cell) = output.cell
    {
        let header = format!("{}\n", bart!(Cell, "{cell}"));
        if !output.notes.starts_with(&header) && !prefix.contains(&format!("cell[{cell}]")) {
            output.notes.insert_str(0, &header);
        }
    }
    let prefix = fit(
        prefix,
        context.output_budget,
        output.cell.filter(|_| output.saved.is_some()),
        output
            .saved
            .as_ref()
            .is_some_and(|saved| saved.is_complete()),
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
    complete: bool,
    notes: &mut String,
) -> String {
    if text.len() <= budget {
        return text;
    }
    let captured = text.len();
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    let reference = cell.map_or_else(
        || "no saved output".to_owned(),
        |cell| {
            let reference = output::Name::Output(cell).reference();
            format!(
                "{} output: {reference}{}",
                if complete { "full" } else { "incomplete" },
                output::resume(&text, &reference)
            )
        },
    );
    notes.insert_str(
        0,
        &format!(
            "{}\n",
            bart!(Output, "{end}/{captured} bytes shown; {reference}")
        ),
    );
    text
}

#[derive(Clone)]
pub(crate) struct Artifacts(Arc<output::Storage>);

impl Artifacts {
    pub(crate) fn save_history(&self, bytes: &[u8]) -> std::io::Result<u64> {
        let number = self.0.next_history()?;
        output::save(&self.0, output::Name::History(number), bytes)?;
        Ok(number)
    }

    pub(crate) fn save_paste(&self, id: u64, text: &str) -> std::io::Result<PathBuf> {
        output::save(&self.0, output::Name::Paste(id), text.as_bytes())
    }

    pub(crate) fn dir(&self) -> &Path {
        self.0.dir()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Diff {
    pub(crate) path: String,
    pub(crate) hunks: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Succeeded,
    Failed,
}

impl Output {
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
    pub(crate) fn is_error(&self) -> bool {
        self.status == Status::Failed
    }
}

fn parse_arguments<T: DeserializeOwned>(arguments: &Value) -> Result<T, String> {
    T::deserialize(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

impl Drop for Context {
    fn drop(&mut self) {
        self.stop();
    }
}
