use std::{fmt::Write as _, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::OwnedMutexGuard, time};

use super::{
    Context, Definition, Output, Run,
    output::{Collector, Name},
    parse_arguments,
};
use capture::{Capture, Effects, denied_prior};
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
const MAX_KEEP_TERMS: usize = 8;
const MAX_KEEP_TERM_CHARS: usize = 100;
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
    keep_matching: Vec<String>,
    #[serde(default)]
    restart: bool,
}

impl Arguments {
    fn validated(arguments: &Value) -> Result<(Self, Duration), String> {
        let arguments: Self = parse_arguments(arguments)?;
        if arguments.code.trim().is_empty() && !arguments.restart {
            return Err(super::EMPTY_CELL_REFUSAL.into());
        }
        if arguments.keep_matching.len() > MAX_KEEP_TERMS
            || arguments
                .keep_matching
                .iter()
                .any(|term| term.is_empty() || term.chars().count() > MAX_KEEP_TERM_CHARS)
        {
            return Err(format!(
                "keep_matching takes at most {MAX_KEEP_TERMS} terms of 1 to {MAX_KEEP_TERM_CHARS} characters"
            ));
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
    pub(super) mcp_servers: &'a [String],
}

fn description(surface: &Surface<'_>) -> String {
    let mut text = header(surface);
    text.push_str(TYPES);
    text.push_str(&commands(surface.escape));
    text.push_str(&helpers());
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
        "{python}, initially in the working directory. Standard-library paths and user-installed \
         packages are available; user-site .pth startup files are not executed.\n\n\
         Write Python cells; variables/imports persist until state is lost or restarted. \
         Helpers below are built in. Signatures, imports and @auto_display are documentation, \
         not code to redeclare.\n\n\
         Choose the shortest direct call that preserves the evidence you need:\n\
         - Inspect files: read(path), or read(path, offset=59, limit=59) for lines 59–117.\n\
         - Search files or run programs: run([\"rg\", \"-n\", r\"pattern\", \"src\"]).\n\
         - Process data: assign the result, then parse or transform it in Python.\n\
         - Change files: edit for exact localized replacements; write for new files or full replacement.\n\
         - Long-running commands: bg_run, then act on job events while doing independent work.\n\n\
         Direct Text/Result expressions display labeled views, including in loops. Assignment \
         and composition are silent. Other values display via print() or the final expression's \
         repr; a final PIL image is shown instead. None is silent.\n\n\
         Beyond {output_kib} KiB, the cell shows a contiguous prefix with displayed/total bytes \
         and cell[N].output (up to {file_mib} MiB; incomplete saves are marked). Inspect omitted \
         evidence there. Reuse checked functions for repeated actions; wait on observable \
         conditions with a deadline or job events, rather than fixed sleeps.\n\
         Initially imported: collections, json, os, pathlib, re, sys, time.\n\n```python\n# builtins.pyi: \
         Bart additions\n",
        python = surface.python,
        output_kib = super::OUTPUT_BUDGET_BYTES / 1024,
        file_mib = super::output::MAX_FILE_BYTES / (1024 * 1024),
    )
}

const TYPES: &str = r#"from collections.abc import Mapping, Sequence
from os import PathLike
from pathlib import Path
from typing import Any, TypedDict, overload
from PIL.Image import Image

# @auto_display: call directly to inspect; assign to process the returned value.
# Examples in docstrings are independent usage patterns, not a script to run in order.

def whoami() -> dict:
    """Query current identity: whoami(). Keys: provider, model, reasoning_effort (None if unset).
    Call again after a model switch; earlier observations are historical."""

class Text(str):
    """A string with an automatic display view. Assign it for ordinary string processing."""

class SavedStream:
    path: Path  # exact bytes, no terminal formatting or host metadata; up to 64 MiB
    incomplete: str | None  # None means captured through EOF; otherwise a reason

class Result:
    """Direct expressions and print(result) show labeled output; assignment keeps it silent."""
    return_code: int | None  # None while a job is running; negative for signal/cancellation
    stdout: str  # complete UTF-8 capture, up to 64 MiB; raises BartError if incomplete/non-UTF-8
    stderr: str  # same completeness guarantee as stdout; use saved files for exact bytes
    stdout_file: SavedStream
    stderr_file: SavedStream
    outcome: dict | str | None  # None until the job scope settles; then exit, signal, killed or unknown
    timed_out: bool
    cleanup_error: str | None
    retained_job: int | None
    def __bool__(self) -> bool: ...  # exit zero; raises while a Job is running

class BartError(Exception):
    """A helper refused a call or failed; inspect the error and current state before recovery."""

class Job(Result):
    id: int
    command: str
    def kill(self) -> None:
        """Stop this job; harmless after it ended. Effects so far remain."""

class Cell:
    code: Path           # its code exactly as sent
    output: Path         # its terminal-rendered output
    status: Path         # JSONL process outcomes; host reports save completeness
    run: Mapping[int, Path]  # each run() call's terminal-rendered combined log, from 1
    stdout: Mapping[int, Path]  # raw stdout by run number
    stderr: Mapping[int, Path]  # raw stderr by run number

job: Mapping[int, Job]     # job[ID] has Result accessors plus kill(); use observed IDs
cell: Mapping[int, Cell]    # this session's cells by the N in "cell[N]", as files for read() or run()
image: Mapping[int, Path]   # images by the N in "image[N]"; display(image[N]) shows one
paste: Mapping[int, Path]   # text the user attached, by the N in "paste[N]"
history: Mapping[int, Path] # selected-context snapshots: json.loads(read(history[N]))

class _Edit(TypedDict):
    old: str
    new: str

class Edit(_Edit, total=False):
    replace_all: bool
    within: tuple[int, int]

"#;

fn commands(escape: bool) -> String {
    let default = super::execution::DEFAULT_SECONDS;
    let (run_escape, run_doc, bg_escape, bg_doc) = if escape {
        (
            ", sandbox: bool = True,\n       reason: str | None = None",
            "\n    When an authorized command needs to run outside the sandbox, pass sandbox=False\n    and reason=\"why this command needs access\". This requests approval; execution occurs\n    once approved. A refused command is not permission to bypass confinement.",
            ", sandbox: bool = True,\n          reason: str | None = None",
            "\n    `sandbox` and `reason` work as in run.",
        )
    } else {
        ("", "", "", "")
    };
    format!(
        r#"# Use Python for orchestration and complete manageable data; run(argv) for external programs.
class RunError(BartError):
    result: Result  # captured execution result; inspect completeness flags

@auto_display
def run(argv: Sequence[str], *, timeout: float = {default}, env: Mapping[str, str] | None = None{run_escape}) -> Result:
    """Run one program synchronously; argv contains literal arguments.

    Inspect consecutive commands with separate labeled views:
        run(["rg", "-n", r"pattern", "src"])
        run(["cargo", "check", "--locked", "--offline"])
    Assign for programmatic use; display the stored result when needed:
        r = run(["cargo", "check", "--locked", "--offline"])
        r
        if not r:
            raise RuntimeError("check failed; inspect diagnostics before proceeding")
    For a real shell pipeline, explicitly choose a shell:
        run(["bash", "-o", "pipefail", "-c", "printf '%s\n' hello | wc -l"])
    Shell operators inside ordinary argv are literal arguments, not pipelines or redirects.
    Ensure usage of raw python strings for regexs with `grep` or `rg`
    Prefer Python for orchestration and processing complete, manageable data.

    Ordinary exits, including nonzero, return Result. bool(r) means exit zero, not that the
    requested application outcome was achieved; verify that outcome before dependent effects.
    Signals/timeouts raise RunError with .result; inspect error.result as an expression for
    its labeled partial output after catching the exception. Cell/session interruption can raise
    KeyboardInterrupt without a Result; invalid arguments/launch errors can raise BartError
    before a child result exists. Inspect known state before retrying: effects are not rolled back.

    Commands use the session's original cwd and host environment. Python os.chdir/os.environ
    changes do not alter them. env overrides variables for this command; env=os.environ passes
    Python's environment. A shell observes only its own failure semantics, not every inner failure.
    Timeout: {min}–{max} seconds, default {default}.

    Views show [Bart run: exit E; cell[N].run[M]] followed by combined rendered stdout/stderr,
    up to 8 KiB. Only omitted output adds "truncated"; incomplete capture is flagged separately.
    Empty output shows just the header. print(result) keeps attribution; printing raw
    .stdout/.stderr loses it. Use direct calls for inspection, assignment for processing:
        data = json.loads(run(argv).stdout)
    stdout/stderr require complete UTF-8 capture or raise. For exact bytes:
        if r.stdout_file.incomplete is None:
            data = r.stdout_file.path.read_bytes()
    stderr_file works likewise. Completeness is a snapshot; descendants may still append.

    Host process statuses appear even when results are assigned. Up to 64 records / 16 KiB
    are inline; omitted records live in cell[N].status with reported save completeness.
    cell[N].run[M] is combined rendered output; cell[N].stdout[M]/stderr[M] hold raw bytes.
    Surviving descendants remain owned; r.retained_job identifies their job for cleanup.
    Stop temporary descendants when done; preserve intentional services.{run_doc}"""

def bg_run(argv: Sequence[str], *, notify: str | None = None, env: Mapping[str, str] | None = None{bg_escape}) -> Job:
    """Start one program; return its Job and show [Bart bg_run: job[ID] started].

    Example: build = bg_run(["cargo", "test", "--locked", "--offline"])
    Job has the same stdout, stderr, stdout_file, stderr_file, return_code and outcome
    accessors as Result. They refresh on access; outcome/return_code are None until the
    whole owned scope settles. Reading unfinished/incomplete stream text raises BartError.
    print(build) shows bounded merged output and current status; bare Job values stay
    silent to avoid an extra view at launch. bool(build) raises while it is running.
    job[ID] retrieves the same job after Python restarts. Inspect partial bytes explicitly
    through the saved stream paths if needed; merged output is for display only.

    Continue independent work after launch. When only the job remains, end the turn
    with a one-line status; its event resumes you. Use events rather than sleep loops
    or repeated status reads. notify is a Rust regex (no look-around/backreferences);
    the first matching rendered line wakes you, once per job.

    Launch errors raise immediately; later outcomes arrive as messages, not exceptions
    in unrelated cells. Root exit with descendants is a nonterminal event; final completion
    means the entire owned scope ended. Check the observed outcome before claiming success.
    No implicit shell or time limit; env and cwd follow run. Use an explicit shell when
    chaining programs is needed; Bart observes the shell's outcome, not every inner command.
    kill() stops work, not effects already made. Stop temporary descendants when done;
    preserve intentional services. In headless mode descendants alone do not keep a finished
    session alive; session exit stops them. Programs delegating required work must wait for it.{bg_doc}"""
"#,
        min = super::execution::MIN_SECONDS,
        max = super::execution::MAX_SECONDS,
    )
}

fn helpers() -> String {
    format!(
        r#"
@auto_display
def read(path: str | PathLike[str], offset: int = 1, limit: int | None = None) -> Text:
    """Inspect a UTF-8 file with its filename, line range and numbered lines.

    Whole file: read("src/main.rs")
    Lines 59–117 inclusive: read("src/provider/chatgpt/mod.rs", offset=59, limit=59)
    Multiple files:
        for path in ["src/main.rs", "src/provider/mod.rs"]:
            read(path, offset=1, limit=40)
    Data processing instead of inspection:
        config = json.loads(read("config.json"))

    offset is the 1-based starting line; limit is a count, not an ending line number.
    Omit limit to read through EOF. A direct call displays the labeled view; assignment
    or composition returns the text silently, preserving newlines. For file excerpts,
    prefer the range arguments over print/read wrappers or splitlines/slice/join chains.

    The returned text contains the complete requested range, even if its display is too
    large to show inline. An oversized view shows a contiguous prefix plus displayed/total
    bytes and cell[N].output; inspect omitted evidence using another bounded read.
    Requested data over {data_mib} MiB, incomplete ranges at the {scan_mib} MiB scan bound,
    NULs and invalid UTF-8 raise BartError instead of returning partial data.
    View basenames are bounded; explicit range labels distinguish excerpts from full files."""

def write(path: str | PathLike[str], content: str) -> None:
    """Create a file or replace its entire contents, creating missing directories.

    Example: write("notes.txt", "First observation\n")
    Use edit for a localized change to an existing file; write deliberately replaces all
    contents, so inspect existing work before replacing it. Success returns None silently.
    Over an existing all-CRLF file, new text gets CRLF line breaks. Non-empty text ends
    with a newline exactly when the existing file did; a new or empty file gets one."""

@overload
def edit(path: str | PathLike[str], old: str, new: str, replace_all: bool = False,
         within: tuple[int, int] | None = None) -> None: ...
@overload
def edit(path: str | PathLike[str], *,
         edits: list[tuple[str, str] | tuple[str, str, tuple[int, int]] | Edit]) -> None:
    """Make exact, localized replacements in an existing UTF-8 file.

    After inspecting the current text:
        edit("settings.py", "RETRIES = 2", "RETRIES = 3")
    Batch independent replacements against the same original file:
        edit("settings.py", edits=[("RETRIES = 2", "RETRIES = 3"),
                                   ("TIMEOUT = 10", "TIMEOUT = 20")])
    To disambiguate, supply within=(start, end), a 1-based inclusive line range.
    Use replace_all=True only when every occurrence is intended to change.

    old must be non-empty and match exactly once, unless replace_all is set. Matching
    includes whitespace. Every batch entry matches the original file; missing, ambiguous
    or overlapping matches leave it unchanged. Re-read after a conflict and correct the
    match rather than retrying unchanged or switching to whole-file overwrite.
    Newlines follow write's rules. Success returns None silently; inspect the diff and run
    relevant checks to establish that the intended change works."""

def display(*images: str | PathLike[str] | Image) -> None:
    """Display image files or PIL images after the cell's text result.

    Examples: display("plot.png"); display(image[1])
    Use an observed image ID. Passing a path to print displays text, not the image.
    Accepts PNG, JPEG, GIF, WebP or PIL images: at most {images} per cell, {image_mib} MiB each."""
"#,
        data_mib = FILE_BYTES / (1024 * 1024),
        scan_mib = READ_SCAN_BYTES / (1024 * 1024),
        images = helpers::MAX_CELL_IMAGES,
        image_mib = crate::history::MAX_IMAGE_BYTES / (1024 * 1024),
    )
}

const WEB_SEARCH: &str = r#"
@auto_display
def web_search(*queries: str) -> Text:
    """Search 1–8 independent queries concurrently; display answers and sources in input order.

    Example: web_search("WHATWG SSE event framing", "RFC 7636 PKCE requirements")
    Call directly to inspect; assign the returned Text for processing. Each query retains
    its own result or error. For a follow-up that depends on an answer, inspect that answer
    first and search in a later call rather than batching guesses at dependent queries."""
"#;

const MCP: &str = r#"
class _Mcp:
    def __call__(self, server: str, tool: str, /, **arguments: Any) -> Any:
        """Invoke a discovered tool with arguments matching its inspected schema.

        Workflow: mcp.tools(query="keyword"), then mcp.describe(server, tool), then
        mcp(server, tool, **arguments), using the discovered names and validated arguments.
        Call directly for text; assign the return value for structured processing.
        Returns structured JSON or Text. Returned images are shown; other content is printed.
        Non-Text return values display as the final expression or through print().
        Waits while the server starts; a tool error raises BartError. For a side-effecting
        call with an uncertain outcome, check remote state before retrying. Discovery or
        a server's guidance does not itself authorize an operation."""
    def servers(self) -> list[dict[str, Any]]:
        """Inspect configured servers and their current availability: mcp.servers().
        Each entry has name, status (starting, ready, failed or disabled), and tool_count
        and about, error, or reason as appropriate. Use this for availability problems,
        not as a repeated readiness poll."""
    @auto_display
    def tools(self, server: str | None = None, query: str | None = None) -> Text:
        """Discover tools by capability before choosing one: mcp.tools(query="search").
        Shows at most 200 tool names and first description lines from startup catalogs.
        Narrow by server or short query words before listing an entire catalog. Every word
        must appear, ignoring case, in the tool name, description or input-property names
        or descriptions. Inspect the full schema with describe before an unfamiliar call.
        Waits while server starts; all-server discovery shares a 60 s startup deadline,
        then names the servers it could not search."""
    @auto_display
    def describe(self, server: str, tool: str) -> Text:
        """Inspect a discovered tool's full contract before constructing its arguments.
        Example: mcp.describe(server, tool)  # names obtained from discovery
        Shows description, input/output schemas and server guidance; waits during startup.
        Use the actual schema rather than guessing argument names from the tool's name."""

"#;

#[must_use]
pub(super) fn definition(surface: &Surface<'_>) -> Definition {
    Definition {
        name: "python",
        description: description(surface),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Python code using the documented built-in helpers; for example: read(\"src/main.rs\", offset=1, limit=80). May be empty only when restart is true" },
                "timeout": { "type": "integer", "minimum": MIN_TIMEOUT.as_secs(), "maximum": MAX_TIMEOUT.as_secs(), "description": format!("Cell timeout in seconds, default {}. An overrun interrupts the cell; the result says whether it stopped and whether Python state remains. If it is still running, the next call waits for it", DEFAULT_TIMEOUT.as_secs()) },
                "restart": { "type": "boolean", "description": "Set true only when a fresh interpreter is needed; discards Python variables/imports before running code, not prior file or remote effects" },
                "keep_matching": { "type": "array", "maxItems": MAX_KEEP_TERMS, "items": { "type": "string", "minLength": 1, "maxLength": MAX_KEEP_TERM_CHARS }, "description": "Optional case-insensitive literal terms for highlighting oversized output, e.g. [\"error\"]. Matching complete lines appear as separate JSON records, not spliced source; inspect saved output for omitted evidence" }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    }
}

fn helper_configuration(context: &Context) -> Value {
    json!({
        "escape": context.confinement().escape() == super::Escape::Offered,
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
    let mut prior = Output::success(String::new());
    let mut slot = match prepared_slot(
        &arguments,
        id,
        context,
        live,
        review.gate.as_deref_mut(),
        timeout,
        &mut prior,
    )
    .await
    {
        Ok(slot) => slot,
        Err(reason) => return Ok(denied_prior(prior, reason)),
    };
    let mut collector = Collector::open_cell(
        context.output_budget,
        &context.artifacts,
        &arguments.code,
        &arguments.keep_matching,
    )
    .await
    .map_err(|error| format!("cannot create the output file: {error}"))?;
    let path = collector.path().to_path_buf();
    live.progress().file(&path);
    if !prior.text.is_empty() {
        collector.line(&format!(
            "[earlier cell result:]\n{}\n[this cell:]",
            &prior.text[..prior.prefix_len.unwrap_or(prior.text.len())]
        ));
    }
    let (running, note) = match start_cell(&mut slot, arguments.restart, context).await {
        Ok(started) => started,
        Err(error) => {
            collector.discard();
            return Err(error);
        }
    };
    let mut lease = Lease {
        stopped: Arc::clone(&context.interpreter.stopped),
        slot: Some(slot),
        running: Some(running),
        capture: Some(Capture::new(
            collector,
            Effects {
                process_summaries: Default::default(),
                processes: 0,
                jobs: prior.jobs,
                images: prior.images,
                diffs: prior.diffs,
                changed: Vec::new(),
            },
            live,
        )),
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
        .finish(end, timeout, true, note.map(str::to_owned), prior.notes)
        .await)
}

async fn prepared_slot(
    arguments: &Arguments,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    mut gate: Option<&mut crate::approval::Gate<'_>>,
    timeout: Duration,
    prior: &mut Output,
) -> Result<OwnedMutexGuard<Slot>, String> {
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
        *prior = capture
            .finish(end, timeout, false, None, String::new())
            .await;
    }
    if (reviewed || !arguments.restart)
        && (slot.cancelled.is_some() || slot.running.as_ref().is_some_and(|r| r.busy.is_some()))
    {
        let settled;
        (slot, settled) =
            collect_earlier(slot, id, context, live, gate.as_deref_mut(), timeout, prior).await?;
        if !settled && !arguments.restart {
            if let Some(gate) = gate.as_mut() {
                gate.stopped = true;
            }
            return Err("[not run: earlier cell has not settled]".into());
        }
    }
    if context
        .interpreter
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err("[not run: session stopped]".into());
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
        gate.check(id, json!({
            "code": arguments.code, "restart": arguments.restart,
            "timeout": timeout.as_secs(), "keep_matching": arguments.keep_matching,
            "cwd": context.cwd, "confinement": format!("{:?}", context.confinement()),
            "active_jobs": context.live.jobs().iter().filter(|job| job.state == super::LiveState::Running).map(|job| json!({"id": job.job, "command": job.title})).collect::<Vec<_>>(),
            "untrusted_earlier_cell_result": prior.text,
        }), &prompt).await?;
        slot = Arc::clone(&context.interpreter.slot).lock_owned().await;
    }
    if context
        .interpreter
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err("[not run: session stopped]".into());
    }
    Ok(slot)
}

async fn collect_earlier(
    mut slot: OwnedMutexGuard<Slot>,
    id: &str,
    context: &Arc<Context>,
    live: &Run,
    gate: Option<&mut crate::approval::Gate<'_>>,
    timeout: Duration,
    prior: &mut Output,
) -> Result<(OwnedMutexGuard<Slot>, bool), String> {
    let cancelled = slot.cancelled.take();
    let (capture, settled) = if let Some((mut capture, end)) = cancelled {
        capture.stream.redirect(live.progress());
        (capture, Some(end))
    } else {
        let busy = slot
            .running
            .as_ref()
            .and_then(|running| running.busy.clone())
            .ok_or("cannot collect earlier cell: it is not running")?;
        let mut collector = Collector::reopen(
            context.output_budget,
            &context.artifacts,
            Name::Output(busy.cell),
            busy.saved,
        )
        .await
        .map_err(|e| format!("cannot collect earlier cell: {e}"))?;
        collector.line("[an earlier cell was still running; its output from here:]");
        let effects = Effects {
            processes: busy.processes,
            ..Effects::default()
        };
        (Capture::new(collector, effects, live), None)
    };
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
    *prior = lease
        .finish(&end)
        .finish(end, timeout, false, None, String::new())
        .await;
    Ok((
        Arc::clone(&context.interpreter.slot).lock_owned().await,
        settled,
    ))
}

#[cfg(test)]
mod tests;
