use clap::{Args, CommandFactory as _, Parser, Subcommand, builder::NonEmptyStringValueParser};

use crate::{
    approval::Mode,
    confinement::Network,
    provider::{Custom, HttpProtocol},
};

#[derive(Parser)]
#[command(
    name = "bart",
    about = "A terminal coding agent with a persistent Python interpreter",
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    chat: ChatArgs,
}

#[derive(Args)]
struct ChatArgs {
    #[arg(
        long,
        value_name = "NAME",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Provider to use instead of the remembered one",
    )]
    provider: Option<String>,
    #[arg(
        long,
        value_name = "ID",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Model to use instead of the remembered one",
    )]
    model: Option<String>,
    #[arg(
        long,
        value_name = "LEVEL",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Reasoning effort",
    )]
    effort: Option<String>,
    #[arg(
        long,
        short = 'p',
        value_name = "TEXT",
        allow_hyphen_values = true,
        value_parser = nonblank,
        help = "Run headless with this prompt, taken verbatim even when it starts with a dash",
    )]
    prompt: Option<String>,
    #[arg(long, requires = "prompt", conflicts_with_all = ["temporary", "jail", "json"], help = "Keep background jobs after successful -p completion; caller must clean them up")]
    keep_background_jobs: bool,
    #[arg(
        long,
        conflicts_with = "prompt",
        help = "Run a session over JSON lines: messages on stdin, events on stdout"
    )]
    json: bool,
    #[arg(long, value_parser = ["none", "manual", "auto"], help = "Cell approval mode (none, manual, or auto reviewer)")]
    approval: Option<String>,
    #[arg(
        long,
        help = "Run the interpreter under bubblewrap with this directory writable (Linux)"
    )]
    jail: bool,
    #[arg(long, requires = "jail", help = "Turn the jail's network off")]
    no_network: bool,
    #[arg(
        long,
        conflicts_with = "session",
        help = "Continue the latest session that ran in this directory"
    )]
    resume: bool,
    #[arg(long, conflicts_with_all = ["resume", "session"], help = "Discard session records and saved outputs on exit")]
    temporary: bool,
    #[arg(
        long,
        value_name = "ID",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Continue a session by ID (may require changing directory)",
    )]
    session: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    #[command(
        about = "Sign in to a provider: `chatgpt`, a configured name, or `<protocol>:<name>` to add one"
    )]
    Login(LoginArgs),
    #[command(about = "Forget a provider's credentials")]
    Logout {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
    },
    #[command(about = "List models across providers, or show details for one provider")]
    Models {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        provider: Option<String>,
    },
    #[command(subcommand, about = "Configure MCP servers")]
    Mcp(McpCommand),
    #[command(about = "Install the latest release in place of this binary")]
    Update,
}

#[derive(Subcommand)]
enum McpCommand {
    #[command(about = "Add a local server (`-- <command> [args...]`) or a remote one (`--url`)")]
    Add(McpAdd),
    #[command(about = "Add a server from its `mcp.json` entry")]
    AddJson {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
        json: String,
    },
    #[command(about = "List the configured servers")]
    List,
    #[command(about = "Connect to a server and list its tools")]
    Get {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
    },
    #[command(about = "Remove a server and its sign-in")]
    Remove {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
    },
    #[command(about = "Sign in to a remote server with OAuth")]
    Login {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
    },
    #[command(about = "Forget a remote server's sign-in")]
    Logout {
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,
    },
}

#[derive(Args)]
#[command(
    group = clap::ArgGroup::new("server").required(true).args(["url", "command"]),
    override_usage = "bart mcp add <NAME> [--env KEY=VALUE]... -- <COMMAND> [ARGS]...\n       \
        bart mcp add <NAME> --url <URL> [--header 'NAME: VALUE']... [--bearer-token-env VAR]",
    after_help = "Values may read the environment with ${VAR} or ${VAR:-default}, so secrets can stay out of mcp.json."
)]
struct McpAdd {
    #[arg(value_parser = NonEmptyStringValueParser::new(), help = "The server's name")]
    name: String,
    #[arg(
        short,
        long,
        value_name = "KEY=VALUE",
        conflicts_with = "url",
        help = "An environment variable for a local server"
    )]
    env: Vec<String>,
    #[arg(long, help = "The URL of a remote server")]
    url: Option<String>,
    #[arg(
        short = 'H',
        long,
        value_name = "NAME: VALUE",
        requires = "url",
        conflicts_with = "command",
        help = "An HTTP header for a remote server"
    )]
    header: Vec<String>,
    #[arg(
        long,
        value_name = "VAR",
        requires = "url",
        conflicts_with = "command",
        help = "Send `Authorization: Bearer ${VAR}`"
    )]
    bearer_token_env: Option<String>,
    #[arg(
        last = true,
        value_name = "COMMAND",
        help = "A local server's command and arguments"
    )]
    command: Vec<String>,
}

impl From<McpCommand> for crate::mcp::Command {
    fn from(command: McpCommand) -> Self {
        match command {
            McpCommand::Add(add) => match (add.command.split_first(), add.url) {
                (Some((command, args)), None) => Self::Add(crate::mcp::Add::Stdio {
                    name: add.name,
                    command: command.clone(),
                    args: args.to_vec(),
                    env: add.env,
                }),
                (None, Some(url)) => Self::Add(crate::mcp::Add::Http {
                    name: add.name,
                    url,
                    header: add.header,
                    bearer_token_env: add.bearer_token_env,
                }),
                _ => unreachable!("clap requires exactly one of the command and --url"),
            },
            McpCommand::AddJson { name, json } => Self::AddJson { name, json },
            McpCommand::List => Self::List,
            McpCommand::Get { name } => Self::Get { name },
            McpCommand::Remove { name } => Self::Remove { name },
            McpCommand::Login { name } => Self::Login { name },
            McpCommand::Logout { name } => Self::Logout { name },
        }
    }
}

#[derive(Args)]
struct LoginArgs {
    #[arg(
        value_parser = NonEmptyStringValueParser::new(),
        help = "`chatgpt`, a configured provider, or `<responses|chat|messages|gemini>:<name>`; a key is read from stdin",
    )]
    target: String,
    #[arg(
        long,
        help = "Base URL of a new provider; the protocol's path is appended"
    )]
    url: Option<String>,
    #[arg(long, help = "The new provider needs no key")]
    no_key: bool,
    #[arg(
        long,
        help = "For chatgpt: sign in with a code entered on another device"
    )]
    device: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    Login(Login),
    Logout(String),
    Models(Option<String>),
    Mcp(crate::mcp::Command),
    Update,
    Chat(Chat),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Login {
    Chatgpt(LoginMethod),
    Key(String),
    Custom { name: String, config: Custom },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginMethod {
    Browser,
    Device,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Chat {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) prompt: Option<String>,
    pub(crate) keep_background_jobs: bool,
    pub(crate) json: bool,
    pub(crate) jail: Option<Network>,
    pub(crate) approval: Option<Mode>,
    pub(crate) resume: Resume,
    pub(crate) temporary: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resume {
    No,
    Latest,
    Session(String),
}

pub(crate) fn parse(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Invocation, clap::Error> {
    let cli = Cli::try_parse_from(args)?;
    let invalid =
        |message: &str| Cli::command().error(clap::error::ErrorKind::ArgumentConflict, message);
    Ok(match cli.command {
        Some(Command::Login(login)) => {
            Invocation::Login(login_target(login).map_err(|m| invalid(&m))?)
        }
        Some(Command::Logout { name }) => Invocation::Logout(name),
        Some(Command::Models { provider }) => Invocation::Models(provider),
        Some(Command::Mcp(command)) => Invocation::Mcp(command.into()),
        Some(Command::Update) => Invocation::Update,
        None => {
            let chat = cli.chat;
            if chat.jail && !cfg!(target_os = "linux") {
                return Err(invalid(
                    "--jail needs Linux, for bubblewrap and systemd-run",
                ));
            }
            let network = if chat.no_network {
                Network::Off
            } else {
                Network::On
            };
            Invocation::Chat(Chat {
                provider: chat.provider,
                model: chat.model,
                reasoning_effort: chat.effort,
                prompt: chat.prompt,
                keep_background_jobs: chat.keep_background_jobs,
                json: chat.json,
                jail: chat.jail.then_some(network),
                approval: chat
                    .approval
                    .map(|name| Mode::parse(&name).expect("clap checked the approval name")),
                temporary: chat.temporary,
                resume: match (chat.resume, chat.session) {
                    (_, Some(id)) => Resume::Session(id),
                    (true, None) => Resume::Latest,
                    (false, None) => Resume::No,
                },
            })
        }
    })
}

fn nonblank(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        Err("needs a nonblank value".into())
    } else {
        Ok(text.to_owned())
    }
}

fn login_target(login: LoginArgs) -> Result<Login, String> {
    let LoginArgs {
        target,
        url,
        no_key,
        device,
    } = login;
    if let Some((protocol, name)) = target.split_once(':') {
        if device {
            return Err("--device is for chatgpt".into());
        }
        let protocol = match protocol {
            "responses" => HttpProtocol::Responses,
            "chat" => HttpProtocol::Chat,
            "messages" => HttpProtocol::Messages,
            "gemini" => HttpProtocol::Gemini,
            _ => return Err(format!("unknown protocol: {protocol}")),
        };
        if name.is_empty() {
            return Err("login needs a provider name".into());
        }
        let url = url.ok_or("a new provider needs --url")?;
        return Ok(Login::Custom {
            name: name.into(),
            config: Custom {
                protocol,
                url,
                key: !no_key,
            },
        });
    }
    if url.is_some() || no_key {
        return Err("--url and --no-key are for a new `<protocol>:<name>` provider".into());
    }
    Ok(if target == "chatgpt" {
        Login::Chatgpt(if device {
            LoginMethod::Device
        } else {
            LoginMethod::Browser
        })
    } else if device {
        return Err("--device is for chatgpt".into());
    } else {
        Login::Key(target)
    })
}

#[cfg(test)]
mod tests;
