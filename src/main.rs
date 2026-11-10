#[macro_use]
mod marker;
mod agent;
mod approval;
mod auth;
mod child;
mod choice;
mod cli;
mod conversation;
mod detail_excerpt;
mod headless;
mod history;
mod http_body;
mod http_client;
mod mcp;
mod persist;
mod preferences;
mod provider;
mod random;
mod record;
mod relaunch;
mod shell_quote;
mod signals;
mod supervise;
mod terminal_text;
mod tool;
mod tui;
mod update;

use std::{
    env,
    io::{self, IsTerminal as _, Read as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::{
    agent::Settings,
    approval::Mode,
    auth::{Credential, Store, chatgpt},
    cli::{Chat, Invocation, Login, LoginMethod, Resume},
    preferences::Current,
    provider::{Auth, Provider},
    record::{Resumed, Sessions},
};
use reqwest::Client;

pub(crate) const NOT_POISONED: &str = "a panic aborts, so no lock is poisoned";

fn main() -> ExitCode {
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == supervise::DETACHED_MODE)
    {
        return supervise::detached_main();
    }
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == supervise::MODE)
    {
        return supervise::main();
    }
    // A core would hold credentials and conversation text.
    if let Err(error) = rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            ..rustix::process::getrlimit(rustix::process::Resource::Core)
        },
    ) {
        crate::terminal_text::stderr_line(format_args!("bart: cannot disable core dumps: {error}"));
        return ExitCode::FAILURE;
    }
    let invocation = match cli::parse(env::args_os()) {
        Ok(invocation) => invocation,
        Err(error) => error.exit(),
    };
    let temporary_root = match matches!(&invocation, Invocation::Chat(chat) if chat.temporary)
        .then(|| tempfile::Builder::new().prefix("bart-").tempdir())
        .transpose()
    {
        Ok(root) => root,
        Err(error) => {
            crate::terminal_text::stderr_line(format_args!(
                "bart: cannot create temporary session storage: {error}"
            ));
            return ExitCode::FAILURE;
        }
    };
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tui::restore_after_panic();
        report(info);
    }));
    let mut result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))
        .and_then(|runtime| {
            runtime.block_on(Box::pin(async {
                let result = run(invocation, temporary_root.as_ref().map(|root| root.path())).await;
                auth::refresh::drain().await;
                result
            }))
        });
    if let Some(root) = temporary_root {
        let path = root.path().to_owned();
        if let Err(error) = root.close() {
            let cleanup = format!(
                "cannot remove temporary session storage {}: {error}",
                path.display()
            );
            result = Err(match result {
                Ok(()) => cleanup,
                Err(error) => format!("{error}; {cleanup}"),
            });
        }
    }
    let status = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            crate::terminal_text::stderr_line(format_args!("bart: {error}"));
            ExitCode::FAILURE
        }
    };
    signals::requested().map_or(status, |termination| {
        ExitCode::from(termination.exit_status())
    })
}

async fn run(invocation: Invocation, temporary_root: Option<&Path>) -> Result<(), String> {
    let config = auth::config_dir().map_err(|error| error.to_string())?;
    let store = Store::new(auth::store_path(&config));
    match invocation {
        Invocation::Login(target) => login(&config, &store, target, &mut io::stdin()).await,
        Invocation::Logout(name) => logout(&config, &store, &name).await,
        Invocation::Models(name) => {
            let client = crate::http_client::new();
            let provider = Provider::named(&name, &config)?;
            let credential = provider::credential(&store, &client, &provider).await?;
            let models = provider::models(&client, &provider, credential.as_ref())
                .await
                .map_err(|error| error.to_string())?;
            for model in models
                .into_iter()
                .filter(|model| model.visibility == crate::provider::Visibility::Listed)
            {
                let efforts = if model.efforts.is_empty() {
                    String::new()
                } else {
                    format!("  (effort: {})", model.efforts.join(", "))
                };
                crate::terminal_text::stdout_line(format_args!(
                    "{}{}{efforts}",
                    model.id,
                    model
                        .name
                        .map_or_else(String::new, |name| format!("  {name}"))
                ));
            }
            Ok(())
        }
        Invocation::Mcp(command) => mcp::command(command, &config, &store).await,
        Invocation::Update => update::run().await,
        Invocation::Chat(options) => {
            Box::pin(chat(
                &config,
                store,
                crate::http_client::new(),
                options,
                temporary_root,
            ))
            .await
        }
    }
}

async fn login(
    config: &Path,
    store: &Store,
    target: Login,
    input: &mut dyn io::Read,
) -> Result<(), String> {
    let (provider, custom, method) = match target {
        Login::Chatgpt(method) => (Provider::named("chatgpt", config)?, None, method),
        Login::Key(name) => (Provider::named(&name, config)?, None, LoginMethod::Browser),
        Login::Custom {
            name,
            config: custom,
        } => (
            Provider::custom(&name, &custom)?,
            Some(custom),
            LoginMethod::Browser,
        ),
    };
    let update = match custom {
        Some(custom) => Some(
            provider::CustomUpdate::prepare(config, provider.name(), Some(custom))
                .await
                .map_err(|error| format!("cannot prepare the provider: {error}"))?,
        ),
        None => None,
    };
    let credential = match provider.auth() {
        Auth::None => None,
        Auth::External => {
            return Err(
                "Configure authentication in Claude Code; Bart does not manage Claude credentials."
                    .into(),
            );
        }
        Auth::ApiKey => {
            let mut text = String::new();
            input
                .take(64 * 1024)
                .read_to_string(&mut text)
                .map_err(|error| format!("cannot read the key from stdin: {error}"))?;
            Some(Credential::ApiKey {
                key: auth::api_key(&text).map_err(|error| error.to_string())?,
            })
        }
        Auth::Chatgpt => {
            let client = crate::http_client::new();
            let tokens = match method {
                LoginMethod::Device => chatgpt::login_device(&client, |url, code| {
                    crate::terminal_text::stderr_line(format_args!(
                        "On any device, open {url}\nsign in to ChatGPT and enter this code (it expires in 15 minutes):\n\n    {code}\n\nEnter it only if you started this login."
                    ));
                }).await,
                LoginMethod::Browser => chatgpt::login(&client, |url| auth::open_browser("ChatGPT", url)).await,
            }.map_err(|error| error.to_string())?;
            Some(Credential::Chatgpt(tokens))
        }
    };
    commit_identity(store, provider.name(), credential, update.as_ref()).await?;
    crate::terminal_text::stderr_line(format_args!(
        "Saved {} for {}.",
        if provider.auth() == Auth::None {
            "keyless provider"
        } else {
            "credentials"
        },
        provider.name()
    ));
    Ok(())
}

async fn commit_identity(
    store: &Store,
    name: &str,
    credential: Option<Credential>,
    update: Option<&provider::CustomUpdate>,
) -> Result<(bool, bool), String> {
    let locked = store.lock().await.map_err(|error| error.to_string())?;
    let mut after = locked.load().map_err(|error| error.to_string())?;
    let had_credential = after.providers.contains_key(name);
    let state = if let Some(credential) = credential {
        after.providers.insert(name.into(), credential);
        "credentials were saved"
    } else {
        after.providers.remove(name);
        if had_credential {
            "credentials were removed"
        } else {
            "no credentials were stored"
        }
    };
    locked
        .save(&after)
        .map_err(|error| credential_write_error(&error))?;
    let had_provider = update
        .map(provider::CustomUpdate::save)
        .transpose()
        .map_err(|error| format!("{error}; {state} for {name}"))?
        .unwrap_or(false);
    Ok((had_credential, had_provider))
}

fn credential_write_error(error: &auth::Error) -> String {
    format!("{error}; provider configuration was not changed")
}

async fn logout(config: &Path, store: &Store, name: &str) -> Result<(), String> {
    let update = provider::CustomUpdate::prepare(config, name, None)
        .await
        .map_err(|error| {
            format!("cannot prepare provider removal: {error}; credentials were not changed")
        })?;
    let (had_credential, had_provider) = commit_identity(store, name, None, Some(&update)).await?;
    let done = match (had_credential, had_provider) {
        (false, false) => return Err(format!("nothing is stored for {name}")),
        (true, true) => "provider and its key",
        (true, false) => "credentials",
        (false, true) => "provider",
    };
    crate::terminal_text::stderr_line(format_args!("Removed the {done} for {name}."));
    Ok(())
}

async fn chat(
    config: &Path,
    store: Store,
    client: Client,
    options: Chat,
    temporary_root: Option<&Path>,
) -> Result<(), String> {
    let Chat {
        provider,
        model,
        reasoning_effort,
        prompt,
        jail,
        approval,
        resume,
        temporary,
    } = options;
    let mut preferences =
        Current::load(config).map_err(|error| format!("cannot read settings: {error}"))?;
    let carried = if let Resume::Session(id) = &resume {
        relaunch::settings(id)?
    } else {
        None
    };
    if let Some(carried) = &carried {
        preferences.effective.clone_from(&carried.preferences);
    }
    preferences.override_permissions(approval, jail)?;
    let terminations = signals::Terminations::listen()
        .map_err(|error| format!("cannot listen for signals: {error}"))?;
    let confinement = preferences.effective.permissions.confinement()?;
    let interactive = prompt.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal();
    if !interactive && preferences.effective.permissions.approval.mode() == Mode::Manual {
        return Err(
            "manual approval needs the terminal interface to ask you; use --approval auto or none"
                .into(),
        );
    }
    let cwd = env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    tool::check_confinement(&cwd, config, confinement)?;
    let state = state_dir()?;
    let last = choice::load(&state).map_err(|error| format!("cannot load last choice: {error}"))?;
    let state = temporary_root.map_or(state, Path::to_owned);
    let sessions = Sessions::new(&state.join("sessions"), &cwd);
    let resume_sessions = sessions.clone();
    let resumed = tokio::task::spawn_blocking(move || resumed(&resume_sessions, resume))
        .await
        .map_err(|error| error.to_string())??;
    let previous = resumed.as_ref().map(|resumed| &resumed.previous);
    let explicit = provider.is_some() || carried.is_some();
    let provider = provider
        .or_else(|| carried.as_ref().map(|c| c.choice.provider.clone()))
        .or_else(|| previous.map(|meta| meta.provider.clone()))
        .or_else(|| last.as_ref().map(|last| last.provider.clone()));
    let model = model
        .or_else(|| carried.as_ref().map(|c| c.choice.model.clone()))
        .or_else(|| previous.map(|meta| meta.model.clone()))
        .or_else(|| last.as_ref().map(|last| last.model.clone()));
    let reasoning_effort = if let Some(carried) = carried {
        reasoning_effort.or(carried.choice.effort)
    } else {
        reasoning_effort.or_else(|| match previous {
            Some(meta) => meta.effort.clone(),
            None => last.and_then(|last| last.effort),
        })
    };
    let settings = chosen(provider, explicit, model, reasoning_effort, config)?;
    let opener = agent::Opener {
        executable: env::current_exe().map_err(|error| error.to_string())?,
        sessions: sessions.clone(),
        cwd: cwd.clone(),
        client: client.clone(),
        store: store.clone(),
    };
    if interactive {
        return tui::run(
            tui::Launch {
                settings,
                preferences,
                resumed,
                opener,
                sessions,
                client,
                store,
                config: config.to_path_buf(),
                state,
                temporary,
            },
            terminations,
        )
        .await;
    }
    let settings = settings.ok_or_else(|| {
        "a headless run needs --provider and --model the first time; see bart --help".to_owned()
    })?;
    crate::terminal_text::stderr_line(format_args!("{}", settings.describe(confinement)));
    let (session, _) = opener.open(settings, preferences, resumed).await?;
    Box::pin(headless::run(session, prompt, terminations)).await
}

fn resumed(sessions: &Sessions, resume: Resume) -> Result<Option<Resumed>, String> {
    let id = match resume {
        Resume::No => return Ok(None),
        Resume::Latest => sessions
            .latest()
            .map_err(|error| format!("cannot list sessions: {error}"))?
            .ok_or("no earlier session ran in this directory")?,
        Resume::Session(id) => {
            let located = sessions.locate(&id).map_err(|error| error.to_string())?;
            let current = std::env::current_dir().map_err(|error| error.to_string())?;
            let check_directory = |cwd: &Path| {
                if cwd == current {
                    Ok(())
                } else {
                    Err(format!(
                        "session is in another directory; run: {}",
                        relaunch::command(cwd, &id)
                    ))
                }
            };
            if located.dir(&id) != sessions.dir(&id) {
                let cwd = located.directory(&id).map_err(|error| error.to_string())?;
                check_directory(&cwd)?;
            }
            let resumed = located.resume(&id).map_err(|error| error.to_string())?;
            check_directory(&resumed.previous.cwd)?;
            return Ok(Some(resumed));
        }
    };
    sessions
        .resume(&id)
        .map(Some)
        .map_err(|error| format!("cannot resume session {id}: {error}"))
}

fn state_dir() -> Result<PathBuf, String> {
    auth::xdg_base("XDG_STATE_HOME", ".local/state")
        .map(|base| base.join("bart"))
        .map_err(|error| error.to_string())
}

// A remembered provider may have been removed from configuration; only an explicit one is an error.
fn chosen(
    provider: Option<String>,
    explicit: bool,
    model: Option<String>,
    reasoning_effort: Option<String>,
    config: &Path,
) -> Result<Option<Settings>, String> {
    let (Some(provider), Some(model)) = (provider, model) else {
        return Ok(None);
    };
    match Provider::named(&provider, config) {
        Ok(provider) => Ok(Some(Settings {
            provider,
            model,
            reasoning_effort,
        })),
        Err(error) if explicit => Err(error),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests;
