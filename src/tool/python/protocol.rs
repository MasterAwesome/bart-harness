use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum HostMessage<'a> {
    Run {
        cell: u64,
        code: &'a str,
        web_search: bool,
        diffs: bool,
    },
    Result(Value),
    Error(&'a str),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum PythonMessage {
    Done {
        failed: bool,
        changed: Vec<String>,
        inventory: Value,
    },
    Call(Helper),
    Activity(Activity),
    Interrupt {},
    DiffSummary {
        path: String,
        summary: String,
    },
    Diff {
        path: String,
        before: String,
        after: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Activity {
    Start { title: String },
    Finish { id: u64, outcome: ActivityOutcome },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ActivityOutcome {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", tag = "name", content = "arguments")]
pub(super) enum Helper {
    Whoami {},
    Background(Value),
    Run(Value),
    Kill {
        job: u32,
    },
    Job {
        id: u32,
    },
    Image {
        label: String,
        data: String,
    },
    #[serde(rename = "web_search")]
    WebSearch {
        queries: Vec<String>,
    },
    #[serde(rename = "mcp_servers")]
    McpServers {},
    #[serde(rename = "mcp_tools")]
    McpTools {
        server: Option<String>,
        query: Option<String>,
    },
    #[serde(rename = "mcp_describe")]
    McpDescribe {
        server: String,
        tool: String,
    },
    #[serde(rename = "mcp_call")]
    McpTool {
        server: String,
        tool: String,
        arguments: serde_json::Map<String, Value>,
    },
}
