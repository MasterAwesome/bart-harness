use std::sync::Arc;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::ChildStdout,
};

use super::{
    CANCELLED, CellReview, INTERRUPTED, MAX_FRAME_BYTES, PROGRAM_OUTPUT_BYTES,
    capture::Effects,
    escape,
    host::End,
    jsonl,
    protocol::{Helper, HostMessage, PythonMessage},
    transport::{Channel, Transition, transition},
};
use crate::{
    history::InlineImage,
    tool::{
        Context, Network,
        live::Progress,
        output::{Collector, Name},
        process::Launch,
        web,
    },
};

pub(super) const MAX_CELL_IMAGES: usize = 8;

pub(super) async fn answer(
    read: std::io::Result<usize>,
    channel: &mut Channel,
    effects: &mut Effects,
    cell: u64,
    review: &mut CellReview<'_, '_>,
    live: &Progress,
) -> std::io::Result<Option<End>> {
    let call = if channel.process.is_some() {
        None
    } else {
        match transition(read, channel, effects).await? {
            Transition::End(end) => return Ok(Some(end)),
            Transition::Continue => {
                channel
                    .requests
                    .write_all_buf(&mut channel.exchange.unsent)
                    .await?;
                return Ok(None);
            }
            Transition::Activity(activity) => {
                let value = match activity {
                    super::protocol::Activity::Start { title } => json!(live.activity(&title)),
                    super::protocol::Activity::Finish { id, outcome } => {
                        use super::protocol::ActivityOutcome;
                        let state = match outcome {
                            ActivityOutcome::Completed => crate::tool::live::State::Finished,
                            ActivityOutcome::Failed => crate::tool::live::State::Failed,
                            ActivityOutcome::Interrupted => crate::tool::live::State::Interrupted,
                        };
                        live.finish_activity(id, state);
                        Value::Null
                    }
                };
                channel.exchange.queue(&HostMessage::Result(value));
                channel
                    .requests
                    .write_all_buf(&mut channel.exchange.unsent)
                    .await?;
                return Ok(None);
            }
            Transition::Call(call) => Some(call),
        }
    };
    let Channel {
        exchange,
        requests,
        replies,
        process: process_task,
        ..
    } = channel;
    exchange.awaiting = true;
    if exchange.cancelled {
        process_task.take();
        exchange.queue(&HostMessage::Error(CANCELLED));
        requests.write_all_buf(&mut exchange.unsent).await?;
        return Ok(None);
    }
    let work = match call {
        None => Work::Resume,
        Some(Helper::Run(arguments)) => match crate::tool::execution::Request::parse(
            arguments,
            review.context.confinement().escape(),
        ) {
            Ok(request) if request.outside.is_some() => Work::Outside(request),
            Ok(request) => Work::Program(request),
            Err(error) => {
                exchange.queue(&HostMessage::Error(&error));
                requests.write_all_buf(&mut exchange.unsent).await?;
                return Ok(None);
            }
        },
        Some(Helper::Background(arguments)) => Work::Background(arguments),
        Some(call) => Work::Other(call),
    };
    let process_name = matches!(work, Work::Program(_) | Work::Outside(_)).then(|| {
        effects.processes += 1;
        Name::Process {
            cell,
            call: effects.processes,
        }
    });
    let helped = match work {
        Work::Resume => execute(None, review.context, replies, live, process_task).await?,
        Work::Program(request) => {
            let name = process_name.expect("a process call is numbered");
            execute(
                Some((request, name)),
                review.context,
                replies,
                live,
                process_task,
            )
            .await?
        }
        work => {
            let helping = async {
                match work {
                    Work::Outside(request) => Box::pin(outside_program(
                        request,
                        process_name.expect("a process call is numbered"),
                        review,
                        live,
                    ))
                    .await
                    .map(Helped::Program),
                    Work::Background(arguments) => job(arguments, review).await,
                    Work::Other(call) => helper(call, review.context, live).await,
                    Work::Resume | Work::Program(_) => unreachable!("handled above"),
                }
            };
            tokio::select! {
                result = helping => result,
                read = replies.read(MAX_FRAME_BYTES) => {
                    interrupt(read, replies)?;
                    Err(INTERRUPTED.into())
                }
            }
        }
    };
    let answer = match helped {
        Ok(Helped::Value(value)) => Ok(value),
        Ok(Helped::Program(result)) => {
            effects.jobs.extend(result.adopted);
            let mut summary = json!({"process": effects.processes, "return_code": result.code});
            if !matches!(result.outcome, crate::history::JobOutcome::Exit(_)) {
                summary["outcome"] = json!(result.outcome);
            }
            if result.timed_out {
                summary["timed_out"] = json!(true);
            }
            if result.cleanup_error.is_some() {
                summary["cleanup_error"] = json!(result.cleanup_error);
            }
            if let Some(reason) = &result.stdout_file.incomplete {
                summary["stdout_file_incomplete"] = json!(reason);
            }
            if let Some(reason) = &result.stderr_file.incomplete {
                summary["stderr_file_incomplete"] = json!(reason);
            }
            if result.adopted.is_some() {
                summary["retained_job"] = json!(result.adopted);
            }
            let mut fields = vec![format!(
                "cell[{cell}].run[{}] {}",
                effects.processes,
                result.outcome.label()
            )];
            if result.timed_out {
                fields.push("timed out".into());
            }
            if let Some(error) = &result.cleanup_error {
                fields.push(format!("cleanup failed: {error}"));
            }
            if let Some(reason) = &result.stdout_file.incomplete {
                fields.push(format!("stdout incomplete: {reason}"));
            }
            if let Some(reason) = &result.stderr_file.incomplete {
                fields.push(format!("stderr incomplete: {reason}"));
            }
            if let Some(job) = result.adopted {
                fields.push(format!("left processes running as job[{job}]"));
            }
            effects
                .process_summaries
                .push(summary, bart!(Status, "{}", fields.join("; ")));
            let mut value = json!(result);
            value["reference"] = json!(format!("cell[{cell}].run[{}]", effects.processes));
            Ok(value)
        }
        Ok(Helped::Job(job)) => {
            effects.jobs.push(job.id);
            Ok(json!(job))
        }
        Ok(Helped::Image(image)) => admit(&mut effects.images, image).map(|()| Value::Null),
        Ok(Helped::Mcp(mut outcome, label)) => {
            for (mime, data) in outcome.images {
                let shown = decode(&data, Some(&mime), label.clone())
                    .and_then(|image| admit(&mut effects.images, image));
                if let Err(error) = shown {
                    outcome
                        .notes
                        .push(bart!(Image, "{mime} not shown; {error}"));
                }
            }
            Ok(json!({"value": outcome.value, "error": outcome.error, "notes": outcome.notes}))
        }
        Err(error) => Err(error),
    };
    match answer {
        Ok(value) => exchange.queue(&HostMessage::Result(value)),
        Err(error) => exchange.queue(&HostMessage::Error(&error)),
    }
    requests.write_all_buf(&mut exchange.unsent).await?;
    Ok(None)
}

enum Work {
    Resume,
    Program(crate::tool::execution::Request),
    Outside(crate::tool::execution::Request),
    Background(Value),
    Other(Helper),
}

fn decode(data: &str, mime: Option<&str>, label: String) -> Result<InlineImage, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "invalid image base64".to_owned())?;
    InlineImage::new(bytes, mime, label)
}

fn admit(images: &mut Vec<InlineImage>, image: InlineImage) -> Result<(), String> {
    if images.len() == MAX_CELL_IMAGES {
        return Err(format!("at most {MAX_CELL_IMAGES} images per cell"));
    }
    images.push(image);
    Ok(())
}

pub(super) enum Helped {
    Value(Value),
    Program(crate::tool::process::ProgramResult),
    Job(crate::tool::jobs::Started),
    Image(InlineImage),
    Mcp(crate::mcp::Outcome, String),
}

pub(super) struct ProgramTask {
    pub(super) task: tokio::task::JoinHandle<crate::tool::process::ProgramResult>,
    cancellation: Option<crate::tool::jobs::CommandCancellation>,
}

impl Drop for ProgramTask {
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        self.task.abort();
    }
}

pub(super) async fn execute(
    request: Option<(crate::tool::execution::Request, Name)>,
    context: &Arc<Context>,
    replies: &mut jsonl::Reader<BufReader<ChildStdout>>,
    live: &Progress,
    pending: &mut Option<ProgramTask>,
) -> std::io::Result<Result<Helped, String>> {
    if let Some((shell, name)) = request {
        match start_program(&shell, name, context, live).await {
            Ok(task) => *pending = Some(task),
            Err(error) => return Ok(Err(error)),
        }
    }
    let result = tokio::select! {
        result = &mut pending.as_mut().expect("shell started").task => {
            if result.is_ok() { pending.as_mut().expect("shell completed").cancellation.take(); }
            result.map_err(|e| format!("shell collection failed: {e}"))
        }
        read = replies.read(MAX_FRAME_BYTES) => {
            interrupt(read, replies)?;
            Err(INTERRUPTED.into())
        }
    };
    pending.take();
    Ok(result.map(Helped::Program))
}

pub(super) fn interrupt(
    read: std::io::Result<usize>,
    replies: &mut jsonl::Reader<tokio::io::BufReader<tokio::process::ChildStdout>>,
) -> std::io::Result<()> {
    if read? == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    let parsed = serde_json::from_slice::<PythonMessage>(&replies.frame);
    replies.frame.clear();
    if matches!(parsed, Ok(PythonMessage::Interrupt {})) {
        Ok(())
    } else {
        Err(std::io::Error::other("invalid helper interrupt"))
    }
}

pub(super) async fn start_program(
    request: &crate::tool::execution::Request,
    name: Name,
    context: &Arc<Context>,
    live: &Progress,
) -> Result<ProgramTask, String> {
    let command = request.command.label();
    let timeout = request.timeout;
    let collector = Collector::open(PROGRAM_OUTPUT_BYTES, &context.artifacts, name)
        .await
        .map_err(|e| format!("cannot collect program output: {e}"))?;
    live.push(format!("[run: {}]\n", command.lines().next().unwrap_or_default()).as_bytes());
    let process = match crate::tool::process::Process::spawn(
        &request.command,
        context,
        Launch::Session,
    )
    .await
    {
        Ok(process) => process,
        Err(error) => {
            collector.discard();
            return Err(error);
        }
    };
    let progress = live.clone();
    let cancellation = Some(process.cancellation());
    Ok(ProgramTask {
        cancellation,
        task: tokio::spawn(
            async move { process.finish_result(collector, &progress, timeout).await },
        ),
    })
}

async fn outside_program(
    request: crate::tool::execution::Request,
    name: Name,
    review: &mut CellReview<'_, '_>,
    live: &Progress,
) -> Result<crate::tool::process::ProgramResult, String> {
    let reason = request.outside.as_deref().unwrap_or_default();
    let outside = escape::approve(review, "run", &request.command.review_label(), reason).await?;
    let context = review.context;
    let collector = Collector::open(PROGRAM_OUTPUT_BYTES, &context.artifacts, name)
        .await
        .map_err(|e| format!("cannot collect program output: {e}"))?;
    live.push(format!("[run outside the sandbox: {}]\n", request.command.label()).as_bytes());
    let process =
        crate::tool::process::Process::spawn(&request.command, context, Launch::Outside(outside))
            .await?;
    Ok(process
        .finish_result(collector, live, request.timeout)
        .await)
}

async fn job(arguments: Value, review: &mut CellReview<'_, '_>) -> Result<Helped, String> {
    let request =
        crate::tool::jobs::Request::parse(arguments, review.context.confinement().escape())?;
    let launch = match &request.outside {
        Some(reason) => Launch::Outside(
            escape::approve(review, "bg_run", &request.review_label(), reason).await?,
        ),
        None => Launch::Session,
    };
    let context = review.context;
    context
        .jobs
        .start_command(request, context, launch)
        .await
        .map(Helped::Job)
}

pub(super) async fn helper(
    call: Helper,
    context: &Arc<Context>,
    live: &Progress,
) -> Result<Helped, String> {
    match call {
        Helper::Whoami {} => {
            let identity = context.identity.lock().expect(crate::NOT_POISONED);
            let current = identity
                .as_ref()
                .expect("a session sets its identity before tools run");
            Ok(Helped::Value(
                serde_json::to_value(current).expect("identity serializes"),
            ))
        }
        Helper::Run(_) | Helper::Background(_) => {
            Err("commands require the interpreter exchange".into())
        }
        Helper::Job { id } => context.jobs.snapshot(id, context).await.map(Helped::Value),
        Helper::Kill { job } => context
            .jobs
            .kill(job)
            .await
            .map(|text| Helped::Value(text.map_or(Value::Null, Value::String))),
        Helper::Image { label, data } => decode(&data, None, label).map(Helped::Image),
        Helper::WebSearch { queries } => {
            if !(1..=web::MAX_QUERIES).contains(&queries.len()) {
                return Err(format!(
                    "web_search requires 1–{} queries",
                    web::MAX_QUERIES
                ));
            }
            if context.confinement().network() == Network::Off {
                return Err("web search is off: this session runs with --no-network".into());
            }
            let route = context
                .web_route()
                .ok_or("web search needs a provider that offers it")?;
            Ok(Helped::Value(Value::String(
                web::search(&route, &queries, live).await,
            )))
        }
        Helper::McpServers {} => Ok(Helped::Value(context.mcp().servers())),
        Helper::McpTools { server, query } => context
            .mcp()
            .tools(server.as_deref(), query.as_deref())
            .await
            .map(|text| Helped::Value(Value::String(text))),
        Helper::McpDescribe { server, tool } => context
            .mcp()
            .describe(&server, &tool)
            .await
            .map(|text| Helped::Value(Value::String(text))),
        Helper::McpTool {
            server,
            tool,
            arguments,
        } => {
            let label = bart!(Image, "from {server}/{tool}");
            context
                .mcp()
                .call(&server, &tool, arguments)
                .await
                .map(|outcome| Helped::Mcp(outcome, label))
        }
    }
}
