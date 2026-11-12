use std::io::Cursor;

use serde_json::Value;
use tokio::{
    io::BufReader,
    process::{ChildStdin, ChildStdout},
};

use super::{
    capture::{Effects, diff},
    helpers::ProgramTask,
    host::End,
    jsonl,
    protocol::{Helper, HostMessage, PythonMessage},
};
use crate::tool::output::Collector;

pub(super) struct Channel {
    pub(super) requests: ChildStdin,
    pub(super) replies: jsonl::Reader<BufReader<ChildStdout>>,
    pub(super) exchange: Exchange,
    pub(super) process: Option<ProgramTask>,
    pub(super) inventory: Option<Value>,
}

pub(super) struct CellOutput {
    pub(super) marker: Vec<u8>,
    pub(super) barrier: Vec<u8>,
    pub(super) synchronized: u64,
    pub(super) pending: Vec<u8>,
    pub(super) output: Vec<u8>,
    pub(super) ended: bool,
}

#[derive(Default)]
pub(super) struct Exchange {
    pub(super) done: Option<bool>,
    pub(super) awaiting: bool,
    pub(super) cancelled: bool,
    pub(super) unsent: Cursor<Vec<u8>>,
}

impl Exchange {
    pub(super) fn queue(&mut self, message: &HostMessage<'_>) {
        self.awaiting = false;
        let mut line = serde_json::to_vec(message).expect("host messages serialize");
        line.push(b'\n');
        self.unsent = Cursor::new(line);
    }
}

impl CellOutput {
    pub(super) fn capture(
        &mut self,
        collector: &mut Collector,
        stream: &mut crate::tool::live::Stream,
    ) {
        self.take_output(|bytes| stream.render(bytes, |line| collector.line(line)));
    }
    pub(super) fn take_output(&mut self, mut sink: impl FnMut(&[u8])) {
        if self.ended {
            return;
        }
        let output = &mut self.output;
        while let Some(at) = output
            .windows(self.barrier.len())
            .position(|window| window == self.barrier)
        {
            sink(&output[..at]);
            output.drain(..at + self.barrier.len());
            self.synchronized += 1;
        }
        if let Some(at) = output
            .windows(self.marker.len())
            .position(|window| window == self.marker)
        {
            sink(&output[..at]);
            self.pending = output.split_off(at + self.marker.len());
            output.clear();
            self.ended = true;
        } else {
            let kept = [&self.marker, &self.barrier]
                .into_iter()
                .map(|marker| {
                    (1..marker.len().min(output.len() + 1))
                        .rev()
                        .find(|&n| output.ends_with(&marker[..n]))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            let rest = output.split_off(output.len() - kept);
            sink(output);
            *output = rest;
        }
    }
}

pub(super) enum Transition {
    Continue,
    End(End),
    Call(Helper),
    Activity(super::protocol::Activity),
}

pub(super) async fn transition(
    read: std::io::Result<usize>,
    channel: &mut Channel,
    effects: &mut Effects,
) -> std::io::Result<Transition> {
    if read? == 0 {
        return Ok(Transition::End(End::Exited));
    }
    let reply: PythonMessage =
        serde_json::from_slice(&channel.replies.frame).map_err(std::io::Error::other)?;
    match reply {
        PythonMessage::Done {
            failed,
            changed,
            inventory,
        } => {
            channel.inventory = Some(inventory);
            channel.exchange.done = Some(failed);
            effects.changed = changed;
        }
        PythonMessage::Interrupt {} => {}
        PythonMessage::DiffSummary { path, summary } => {
            effects.diffs.push(crate::tool::Diff {
                path,
                hunks: summary,
            });
            channel.exchange.queue(&HostMessage::Result(Value::Null));
        }
        PythonMessage::Diff {
            path,
            before,
            after,
        } => {
            let rendered = tokio::task::spawn_blocking(move || diff(path, &before, &after))
                .await
                .map_err(std::io::Error::other)?;
            effects.diffs.extend(rendered);
            channel.exchange.queue(&HostMessage::Result(Value::Null));
        }
        PythonMessage::Activity(activity) => {
            channel.exchange.awaiting = true;
            channel.replies.frame.clear();
            return Ok(Transition::Activity(activity));
        }
        PythonMessage::Call(call) => {
            channel.exchange.awaiting = true;
            channel.replies.frame.clear();
            return Ok(Transition::Call(call));
        }
    }
    channel.replies.frame.clear();
    Ok(Transition::Continue)
}
