use std::{fmt::Write as _, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::OwnedMutexGuard, time};

use super::{
    Context, Definition, Output, Run,
    output::{Collector, Name},
    parse_arguments,
};
use capture::{Capture, Earlier, EarlierNotes, Effects, Participation, denied_prior};
use host::{End, Lease, Slot, start_cell};
pub(in crate::tool) use host::{Interpreter, identity};

mod capture;
mod escape;
mod helpers;
mod host;
mod jsonl;
mod protocol;
mod transport;

struct CellReview<'a, 'b> {
    id: &'a str,
    context: &'a Arc<Context>,
    gate: Option<&'a mut crate::approval::Gate<'b>>,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const PROGRAM_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const FILE_BYTES: usize = 16 * 1024 * 1024;
const READ_SCAN_BYTES: usize = 64 * 1024 * 1024;
const CANCELLED: &str = "the call was cancelled; whether this helper took effect is unknown";
const INTERRUPTED: &str =
    "this helper call was cut off by a timeout; whether it took effect is unknown";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    code: String,
    timeout: Option<u64>,
    #[serde(default)]
    restart: bool,
}

impl Arguments {
    fn validated(arguments: &Value) -> Result<(Self, Duration), String> {
        let arguments: Self = parse_arguments(arguments)?;
        if arguments.code.trim().is_empty() && !arguments.restart {
            return Err(super::EMPTY_CELL_REFUSAL.into());
        }
        let timeout = arguments
            .timeout
            .map_or(DEFAULT_TIMEOUT, Duration::from_secs);
        if !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&timeout) {
            return Err(format!(
                "timeout must be from {} to {} seconds",
                MIN_TIMEOUT.as_secs(),
                MAX_TIMEOUT.as_secs()
            ));
        }
        Ok((arguments, timeout))
    }
}

pub(super) struct Surface<'a> {
    pub(super) python: &'a str,
    pub(super) escape: bool,
    pub(super) web_search: bool,
    pub(super) mode: super::TurnMode,
    pub(super) mcp_servers: &'a [String],
}

fn description(surface: &Surface<'_>) -> String {
    let mut text = header(surface);
    text.push_str(&files());
    text.push_str(&commands(surface.escape, surface.mode));
    text.push_str(&types(surface.mode.one_turn()));
    if surface.web_search {
        text.push_str(WEB_SEARCH);
    }
    if !surface.mcp_servers.is_empty() {
        text.push_str(MCP);
        let _ = writeln!(
            text,
            "mcp: _Mcp  # servers: {}",
            surface.mcp_servers.join(", ")
        );
    }
    text.push_str("```\n");
    text
}

fn header(surface: &Surface<'_>) -> String {
    format!(
        "{python}. Cells run in one persistent interpreter that starts in the working directory; \
         variables and imports last until it restarts. The standard library and user-installed \
         packages are importable. Already imported: collections, json, os, pathlib, re, sys, time.\n\n\
         The stub below documents the built-in helpers. Call them; do not redeclare them.\n\n\
         To look at something, call read or run bare, one call per line. Each prints a labeled \
         view: the path with numbered lines, or the exit code with the output.\n\n\
         \x20   read(\"SPEC.md\")\n\
         \x20   read(\"src/ledger.py\", offset=40, limit=30)\n\
         \x20   run([\"git\", \"status\", \"--short\"])\n\n\
         Bare calls print their view anywhere in a cell, including inside loops and functions. \
         Do not wrap them in print(): print(read(p)) loses the path and line numbers, and \
         print(r.stdout) loses the exit code. Assign the call (text = read(p)) only to process \
         its value; that prints nothing. Other values show through print() or as the cell's \
         final expression.\n\n\
         Size: a cell shows up to {output_kib} KiB. A longer one ends with how much was \
         shown and the call that shows the next part, cell[N].remaining().\n\n\
         cell[N].output and cell[N].run[M] are the saved files, usable without printing: \
         for line in open(cell[N].output) reads large JSONL; read(path, offset, limit) shows a slice.\n\n\
         ```python\n# builtins.pyi: Bart additions. @auto_display marks helpers that print a view when called bare.\n",
        python = surface.python,
        output_kib = super::OUTPUT_BUDGET_BYTES / 1024,
    )
}

fn files() -> String {
    format!(
        r#"@auto_display
def read(path: str | PathLike[str], offset: int = 1, limit: int | None = None) -> Text:
    """View a UTF-8 text file as numbered lines; returns the text itself, without numbers.

        read("src/main.rs")                        # whole file
        read("src/main.rs", offset=59, limit=59)   # lines 59-117: limit is a count, not an end line
        config = json.loads(read("config.json"))   # silent, for processing

    Raises BartError for a missing file, NUL bytes, invalid UTF-8 or more than {data_mib} MiB
    requested; it never returns partial text."""

@overload
def edit(path: str | PathLike[str], old: str, new: str, replace_all: bool = False,
         within: tuple[int, int] | None = None) -> None: ...
@overload
def edit(path: str | PathLike[str], *,
         edits: list[tuple[str, str] | tuple[str, str, tuple[int, int]] | Edit]) -> None:
    """Replace exact text in an existing UTF-8 file. Silent on success.

        edit("settings.py", "RETRIES = 2", "RETRIES = 3")
        edit("settings.py", edits=[("RETRIES = 2", "RETRIES = 3"), ("TIMEOUT = 10", "TIMEOUT = 20")])

    old is matched literally, whitespace included and without read()'s line-number prefixes.
    It must occur exactly once: narrow it with within=(first_line, last_line), or pass
    replace_all=True to change every occurrence. Batch entries all match the original file.
    A missing, ambiguous or overlapping match raises BartError and leaves the file unchanged;
    re-read the text and correct the match."""

def write(path: str | PathLike[str], content: str) -> None:
    """Create a file, or replace all of an existing one; makes missing directories. Silent.
    Use edit to change part of an existing file. Line endings and the final newline follow
    the existing file; a new file always ends with a newline."""

"#,
        data_mib = FILE_BYTES / (1024 * 1024),
    )
}

fn commands(escape: bool, mode: super::TurnMode) -> String {
    use super::{BackgroundJobs, TurnMode};
    let ending = match mode {
        TurnMode::Prompt(BackgroundJobs::Stop) => {
            "This session ends with your turn and stops its jobs: when you need a job's result, call\n    build.wait() instead of ending the turn."
        }
        TurnMode::Prompt(BackgroundJobs::Keep) => {
            "This session ends with your turn. Background jobs survive successful completion; the caller owns cleanup.\n    When you need a job's result, call build.wait() instead of ending the turn."
        }
        TurnMode::Session => {
            "When only the job remains, end the turn with a one-line status; the message resumes you."
        }
    };
    let default = super::execution::DEFAULT_SECONDS;
    let (run_escape, run_doc, bg_escape, bg_doc) = if escape {
        (
            ", sandbox: bool = True,\n       reason: str | None = None",
            "\n    Commands run in a sandbox. When an authorized command needs access outside it, pass\n    sandbox=False and reason=\"why\"; this asks for approval and runs once approved. A refusal\n    is not permission to get around the sandbox another way.",
            ", sandbox: bool = True,\n          reason: str | None = None",
            " sandbox and reason as in run.",
        )
    } else {
        ("", "", "", "")
    };
    format!(
        r#"@auto_display
def run(argv: Sequence[str], *, timeout: float = {default}, check: bool = True,
        cwd: str | PathLike[str] | None = None, env: Mapping[str, str] | None = None{run_escape}) -> Result:
    """Run one program and wait for it. argv is passed literally: no shell, no globbing.

        run(["rg", "-n", r"TODO|FIXME", "src"])                         # raw strings for regexes
        run(["bash", "-o", "pipefail", "-c", "make 2>&1 | tail -20"])   # pipes and redirects need a shell
        data = json.loads(run(["cargo", "metadata", "--format-version=1"]).stdout)   # assigned: silent

    The view is [Bart run: cell[N].run[M] exit E], then combined stdout and stderr.
    A nonzero exit or signal raises RunError and stops the cell unless caught; its .result
    holds the outcome and captured output; the error message is its view. Pass check=False
    to inspect an expected nonzero result. Like a shell, a program that cannot start exits
    127 (not found) or 126 (cannot execute). Timeouts and unknown outcomes always raise.
    bool(r) is true for exit 0.
    A failing run whose view is not shown is reported after the cell's output.
    The program starts in the session's working directory with the session's environment;
    os.chdir and os.environ do not change that. cwd= selects a directory (relative paths
    resolve from the session directory); env= overrides variables. Neither changes the
    defaults for later commands or expands sandbox access.
    timeout is {min}-{max} seconds. Processes the program leaves running become a
    background job, named by r.retained_job.{run_doc}"""

def bg_run(argv: Sequence[str], *, notify: str | None = None,
           cwd: str | PathLike[str] | None = None, env: Mapping[str, str] | None = None{bg_escape}) -> Job:
    """Start a long-running program without waiting; prints [Bart job: job[ID] started].
    print(build) shows [Bart job: job[ID] running] (or exit E), then its output so far.
    Completion shows [Bart job: job[ID] exit 0 after 0.3s; make test].
    Use it, not subprocess, for servers, watchers and slow builds or tests.

        build = bg_run(["cargo", "test", "--locked"])
        server = bg_run(["npm", "run", "dev"], notify=r"listening on")

    A message arrives when the job ends, and once when a line of its output first matches
    notify (a Rust regex: no look-around or backreferences). Meanwhile do independent work.
    {ending}
    Do not poll with sleep or repeated status reads.
    build.stdout raises until it ends.
    build.kill() stops it. job[ID] is the same Job, also after an interpreter restart.
    No shell and no time limit; cwd and env as in run.{bg_doc}"""

"#,
        min = super::execution::MIN_SECONDS,
        max = super::execution::MAX_SECONDS,
    )
}

fn types(one_turn: bool) -> String {
    let wait = if one_turn {
        "    def wait(self) -> Job: ...\n"
    } else {
        ""
    };
    format!(
        r#"class Result:
    return_code: int | None      # negative for a signal; None while a Job is running
    stdout: str                  # complete text; raises BartError if capture is incomplete or not UTF-8
    stderr: str
    stdout_file: SavedStream     # exact bytes at .path; .incomplete is None or the reason
    stderr_file: SavedStream
    outcome: dict | str | None   # {{"exit": n}}, {{"signal": n}}, "killed" or "unknown"; None while running
    timed_out: bool
    retained_job: int | None
    cleanup_error: str | None
    def __bool__(self) -> bool: ...  # exit 0; raises while a Job is running

class Job(Result):
    id: int
    command: str
{wait}    def kill(self) -> None: ...

class Cell:
    def remaining(self) -> Text:
        """Call with no arguments when this cell's notice names it and you need the rest.
        Each call shows the next part. To re-read, use read(cell[N].output); do not compute offsets."""

class BartError(Exception): ...  # a helper refused or failed; the message says why
class RunError(BartError):
    result: Result
class Text(str): ...             # a str with a display view
class Edit(TypedDict, total=False):
    old: str
    new: str
    replace_all: bool
    within: tuple[int, int]

# Saved artifacts by the number shown for them; the paths work with read() and run().
job: Mapping[int, Job]
cell: Mapping[int, Cell]     # .code, .output, .status (JSONL process outcomes) and, per run number M,
                             # .run[M] (combined log), .stdout[M], .stderr[M] (raw bytes)
image: Mapping[int, Path]    # images by the N in "image[N]"
paste: Mapping[int, Path]    # text the user attached, by the N in "paste[N]"
history: Mapping[int, Path]  # earlier context saved as JSON messages: json.loads(read(history[N]))

def display(*images: str | PathLike[str] | Image) -> None:
    """Show image files (PNG, JPEG, GIF, WebP) or PIL images: display("plot.png"); display(image[1])
    A PIL image as the final expression is shown too; print(path) shows only text.
    At most {images} per cell, {image_mib} MiB each."""

def whoami() -> dict: ...    # the current provider, model and reasoning_effort
"#,
        images = helpers::MAX_CELL_IMAGES,
        image_mib = crate::history::MAX_IMAGE_BYTES / (1024 * 1024),
    )
}

const WEB_SEARCH: &str = r#"
@auto_display
def web_search(*queries: str) -> Text:
    """Answer 1-8 independent queries concurrently, each with sources, in input order.

        web_search("WHATWG SSE event framing", "RFC 7636 PKCE requirements")

    Each answer starts with [Bart web_search: query]. One failed query does not fail the others.
    Put a query that depends on an answer in a later cell, after reading that answer."""
"#;

const MCP: &str = r#"
class _Mcp:
    @auto_display
    def tools(self, server: str | None = None, query: str | None = None) -> Text:
        """Find tools: mcp.tools(query="issue search"). Lists up to 200 names with the first
        line of each description. Every query word must appear in the tool's name,
        description or parameters."""
    @auto_display
    def describe(self, server: str, tool: str) -> Text:
        """The tool's full description, input and output schemas and server guidance. Read
        it before a first call instead of guessing argument names."""
    def __call__(self, server: str, tool: str, /, **arguments: Any) -> Any:
        """Call a tool: mcp(server, tool, **arguments). Returns JSON or Text and shows any
        returned images. A tool error raises BartError; if a call that changes something
        fails midway, check the remote state before repeating it."""
    def servers(self) -> list[dict[str, Any]]:
        """Each server's name and status (starting, ready, failed or disabled), with
        tool_count, about, error or reason. Calls wait while a server starts."""

"#;

#[must_use]
pub(super) fn definition(surface: &Surface<'_>) -> Definition {
    Definition {
        name: "python",
        description: description(surface),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Python source for one cell. May be empty only with restart. Several cells in one response run in order, and a later one still runs if an earlier one failed" },
                "timeout": { "type": "integer", "minimum": MIN_TIMEOUT.as_secs(), "maximum": MAX_TIMEOUT.as_secs(), "description": format!("Seconds before the cell is interrupted, default {}. The result says whether it stopped; a cell that is still running makes the next call wait for it", DEFAULT_TIMEOUT.as_secs()) },
                "restart": { "type": "boolean", "description": "Start a fresh interpreter before running code, discarding variables and imports. For a cell that will not stop or a broken interpreter" }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    }
}

fn helper_configuration(context: &Context) -> Value {
    json!({
        "escape": context.confinement().escape() == super::Escape::Offered,
        "one_turn": context.turn_mode().one_turn(),
        "saved_dir": context.spill_dir,
        "request_bytes": MAX_FRAME_BYTES,
        "process_default_seconds": super::execution::DEFAULT_SECONDS,
        "file_bytes": FILE_BYTES,
        "diff_bytes": 4 * 1024 * 1024,
        "scan_bytes": READ_SCAN_BYTES,
        "edits": 1024,
        "replacements": 100_000,
        "match_lines": 20,
        "image_bytes": crate::history::MAX_IMAGE_BYTES,
        "chunk_bytes": 64 * 1024,
    })
}

pub(super) async fn call(
    arguments: &Value,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    gate: Option<&mut crate::approval::Gate<'_>>,
) -> Output {
    run(arguments, id, context, live, gate)
        .await
        .unwrap_or_else(Output::error)
}

enum Refusal {
    Unsettled(Earlier),
    Denied {
        earlier: Option<Earlier>,
        reason: String,
    },
}

async fn run(
    arguments: &Value,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    gate: Option<&mut crate::approval::Gate<'_>>,
) -> Result<Output, String> {
    let (arguments, timeout) = Arguments::validated(arguments)?;
    let execution_deadline = gate
        .as_ref()
        .is_none_or(|g| g.approval.mode() == crate::approval::Mode::None)
        .then(|| time::Instant::now() + timeout);
    let mut review = CellReview { id, context, gate };
    let (mut slot, prior) = match prepared_slot(
        &arguments,
        id,
        context,
        live,
        review.gate.as_deref_mut(),
        timeout,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(refusal) => return Ok(denied_prior(refusal)),
    };
    let collector =
        Collector::open_cell(context.output_budget, &context.artifacts, &arguments.code)
            .await
            .map_err(|error| format!("cannot create the output file: {error}"))?;
    let path = collector.path().to_path_buf();
    live.progress().file(&path);
    let effects = Effects::new(&collector);
    let mut capture = Capture::new(collector, effects, live);
    if let Some(earlier) = &prior {
        capture
            .collector
            .line(earlier.prefix().trim_end_matches('\n'));
        capture
            .collector
            .line(&bart!(Cell, "cell[{}] result", capture.cell));
    }
    let prior = prior.map(|earlier| {
        let output = *earlier.output;
        capture.effects.jobs = output.jobs;
        capture.effects.images = output.images;
        capture.effects.diffs = output.diffs;
        EarlierNotes {
            cell: earlier.cell,
            notes: output.notes,
        }
    });
    let (running, note) = match start_cell(&mut slot, arguments.restart, context).await {
        Ok(started) => started,
        Err(error) => {
            capture.collector.discard();
            return Err(bart!(Cell, "cell[{}] not started; {error}", capture.cell));
        }
    };
    let mut lease = Lease {
        stopped: Arc::clone(&context.interpreter.stopped),
        slot: Some(slot),
        running: Some(running),
        capture: Some(capture),
    };
    let started = time::Instant::now();
    let deadline = execution_deadline
        .filter(|_| !arguments.restart)
        .unwrap_or(started + timeout);
    let end = lease
        .running
        .as_mut()
        .expect("leased until finished")
        .cell(
            Some(&arguments.code),
            deadline,
            lease.capture.as_mut().expect("leased capture"),
            &mut review,
        )
        .await;
    let capture = lease.finish(&end);
    Ok(capture
        .finish(
            end,
            timeout,
            Participation::Started,
            note.map(str::to_owned),
            prior,
        )
        .await)
}

async fn prepared_slot(
    arguments: &Arguments,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    mut gate: Option<&mut crate::approval::Gate<'_>>,
    timeout: Duration,
) -> Result<(OwnedMutexGuard<Slot>, Option<Earlier>), Refusal> {
    let mut prior = None;
    let mut slot = Arc::clone(&context.interpreter.slot).lock_owned().await;
    context.interpreter.apply_reset(&mut slot);
    let reviewed = gate
        .as_ref()
        .is_some_and(|gate| gate.approval.mode() != crate::approval::Mode::None);
    if !reviewed
        && arguments.restart
        && let Some((capture, end)) = slot.cancelled.take()
    {
        let end = if matches!(end, End::TimedOut) {
            End::Discarded
        } else {
            end
        };
        prior = Some(Earlier {
            cell: capture.cell,
            continued: capture.continued,
            output: Box::new(
                capture
                    .finish(end, timeout, Participation::Waited, None, None)
                    .await,
            ),
        });
    }
    if (reviewed || !arguments.restart)
        && (slot.cancelled.is_some() || slot.running.as_ref().is_some_and(|r| r.busy.is_some()))
    {
        let (collected_slot, earlier, settled) =
            collect_earlier(slot, id, context, live, gate.as_deref_mut(), timeout)
                .await
                .map_err(|reason| Refusal::Denied {
                    earlier: None,
                    reason,
                })?;
        slot = collected_slot;
        if !settled && !arguments.restart {
            if let Some(gate) = gate.as_mut() {
                gate.stopped = true;
            }
            return Err(Refusal::Unsettled(earlier));
        }
        prior = Some(earlier);
    }
    if context
        .interpreter
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Refusal::Denied {
            earlier: prior,
            reason: bart!(const NotRun, "session stopped").into(),
        });
    }
    if let Some(gate) = gate.filter(|g| g.approval.mode() != crate::approval::Mode::None) {
        drop(slot);
        let prompt = format!(
            "Python cell{}:\n{}\n",
            if arguments.restart {
                ", after restarting the interpreter"
            } else {
                ""
            },
            arguments.code
        );
        if let Err(reason) = gate.check(id, json!({
            "code": arguments.code, "restart": arguments.restart,
            "timeout": timeout.as_secs(),
            "cwd": context.cwd, "confinement": format!("{:?}", context.confinement()),
            "active_jobs": context.live.jobs().iter().filter(|job| job.state == super::LiveState::Running).map(|job| json!({"id": job.job, "command": job.title})).collect::<Vec<_>>(),
            "untrusted_earlier_cell_result": prior.as_ref().map_or("", |earlier| earlier.output.text.as_str()),
        }), &prompt).await {
            return Err(Refusal::Denied { earlier: prior, reason });
        }
        slot = Arc::clone(&context.interpreter.slot).lock_owned().await;
    }
    if context
        .interpreter
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Refusal::Denied {
            earlier: prior,
            reason: bart!(const NotRun, "session stopped").into(),
        });
    }
    Ok((slot, prior))
}

async fn collect_earlier(
    mut slot: OwnedMutexGuard<Slot>,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    gate: Option<&mut crate::approval::Gate<'_>>,
    timeout: Duration,
) -> Result<(OwnedMutexGuard<Slot>, Earlier, bool), String> {
    let cancelled = slot.cancelled.take();
    let (capture, settled) = if let Some((mut capture, end)) = cancelled {
        capture.stream.redirect(live.progress());
        (capture, Some(end))
    } else {
        let busy = slot
            .running
            .as_ref()
            .and_then(|running| running.busy.clone())
            .ok_or_else(|| "cannot collect earlier cell: it is not running".to_owned())?;
        let collector = Collector::reopen(
            context.output_budget,
            &context.artifacts,
            Name::Output(busy.cell),
            busy.saved,
        )
        .await
        .map_err(|e| format!("cannot collect cell[{}]: {e}", busy.cell))?;
        let effects = Effects {
            processes: busy.processes,
            ..Effects::new(&collector)
        };
        (Capture::continuation(collector, effects, live), None)
    };
    let cell = capture.cell;
    let continued = capture.continued;
    let running = slot.running.take();
    let mut lease = Lease {
        stopped: Arc::clone(&context.interpreter.stopped),
        slot: Some(slot),
        running,
        capture: Some(capture),
    };
    let end = match settled {
        Some(end) if !matches!(end, End::TimedOut) => end,
        _ => {
            lease
                .running
                .as_mut()
                .expect("busy interpreter")
                .cell(
                    None,
                    time::Instant::now() + timeout,
                    lease.capture.as_mut().expect("leased capture"),
                    &mut CellReview { id, context, gate },
                )
                .await
        }
    };
    let settled = matches!(end, End::Done { .. } | End::Cancelled);
    let earlier = Earlier {
        cell,
        continued,
        output: Box::new(
            lease
                .finish(&end)
                .finish(end, timeout, Participation::Waited, None, None)
                .await,
        ),
    };
    Ok((
        Arc::clone(&context.interpreter.slot).lock_owned().await,
        earlier,
        settled,
    ))
}

#[cfg(test)]
mod tests;
