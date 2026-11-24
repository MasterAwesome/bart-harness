use std::{
    io::{self, Write},
    os::unix::process::CommandExt as _,
    process::Stdio,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::Command,
    time,
};

use crate::history::{InlineImage, MAX_IMAGE_BYTES};

mod child;

#[cfg(test)]
mod tests;

const TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "macos"))]
const TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// The most text one OSC 52 sequence carries, before encoding; terminals cap
/// the sequence, commonly near 100 KB encoded.
const MAX_COPY_BYTES: usize = 75_000;

#[derive(Debug, PartialEq, Eq)]
enum Sent {
    All,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Pbcopy,
    Wayland,
    Xclip,
}

impl Backend {
    fn command(self) -> std::process::Command {
        match self {
            Self::Pbcopy => {
                let mut command = std::process::Command::new("pbcopy");
                command.env_remove("LC_ALL").env("LC_CTYPE", "UTF-8");
                command
            }
            Self::Wayland => {
                let mut command = std::process::Command::new("wl-copy");
                command.args(["--type", "text/plain;charset=utf-8"]);
                command
            }
            Self::Xclip => {
                let mut command = std::process::Command::new("xclip");
                command.args(["-selection", "clipboard"]);
                command
            }
        }
    }

    fn accepts(self, text: &str) -> bool {
        self != Self::Pbcopy || !(text.starts_with(r"{\rtf") || text.starts_with("%!PS-Adobe"))
    }
}

fn backends(macos: bool, is_set: impl Fn(&str) -> bool) -> &'static [Backend] {
    use Backend::{Pbcopy, Wayland, Xclip};

    if ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .into_iter()
        .any(&is_set)
    {
        &[]
    } else if macos {
        &[Pbcopy]
    } else {
        match (is_set("WAYLAND_DISPLAY"), is_set("DISPLAY")) {
            (true, true) => &[Wayland, Xclip],
            (true, false) => &[Wayland],
            (false, true) => &[Xclip],
            (false, false) => &[],
        }
    }
}

pub(super) async fn copy_native(text: &str) -> bool {
    let tools = backends(cfg!(target_os = "macos"), |name| {
        std::env::var_os(name).is_some()
    });
    try_native(tools, Backend::command, text, TIMEOUT).await
}

async fn try_native(
    tools: &[Backend],
    command: impl Fn(Backend) -> std::process::Command,
    text: &str,
    timeout: Duration,
) -> bool {
    for &backend in tools {
        if backend.accepts(text)
            && time::timeout(timeout, write_native(command(backend), text))
                .await
                .is_ok_and(|result| result.is_ok_and(|status| status.success()))
        {
            return true;
        }
    }
    false
}

async fn write_native(
    mut command: std::process::Command,
    text: &str,
) -> io::Result<std::process::ExitStatus> {
    command
        .process_group(0)
        .stdin(Stdio::piped())
        // The clipboard holder can inherit these file descriptors.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = child::Owner::spawn(Command::from(command)).await?;
    let (stdin, _, _) = child.pipes();
    let mut stdin = stdin.expect("piped stdin");
    stdin.write_all(text.as_bytes()).await?;
    stdin.shutdown().await?;
    drop(stdin);
    child.wait(child::Descendants::KeepOnSuccess).await
}

pub(super) fn copy_terminal(mut out: impl Write, text: &str, what: &str) -> io::Result<String> {
    let (sent, end) = sendable(text);
    write!(out, "\x1b]52;c;{}\x07", STANDARD.encode(end))?;
    out.flush()?;
    Ok(match sent {
        Sent::All => format!("sent {what} to the terminal clipboard"),
        Sent::End => format!("sent the last 75 KB of {what} to the terminal clipboard"),
    })
}

fn sendable(text: &str) -> (Sent, &str) {
    if text.len() <= MAX_COPY_BYTES {
        return (Sent::All, text);
    }
    let start = (text.len() - MAX_COPY_BYTES..text.len())
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(text.len());
    (Sent::End, &text[start..])
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn image() -> Result<Option<InlineImage>, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let tools: [(&str, &[&str], &[&str]); 2] = [
        ("wl-paste", &["--list-types"], &["--no-newline", "--type"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            &["-selection", "clipboard", "-o", "-t"],
        ),
    ];
    let mut failures = Vec::new();
    for (tool, list, get) in tools {
        if tool == "wl-paste" && !wayland {
            continue;
        }
        let listed = match run(tool, list, None, MAX_IMAGE_BYTES).await {
            Ok(listed) => listed,
            Err(error) => {
                failures.push(notice(tool, &error));
                continue;
            }
        };
        let listed = String::from_utf8_lossy(&listed);
        let Some(media_type) = TYPES
            .into_iter()
            .find(|kind| listed.lines().any(|line| line.trim() == *kind))
        else {
            return Ok(None);
        };
        match run(tool, get, Some(media_type), MAX_IMAGE_BYTES).await {
            Ok(bytes) => match image_of(media_type, bytes) {
                Ok(image) => return Ok(Some(image)),
                Err(error) => failures.push(format!("{tool}: {error}")),
            },
            Err(error) => failures.push(notice(tool, &error)),
        }
    }
    Err(failures.join("; "))
}

// AppleScript hands over the clipboard's PNG as «data PNGf<hex>».
#[cfg(target_os = "macos")]
pub(super) async fn image() -> Result<Option<InlineImage>, String> {
    let script = [
        "-e",
        "repeat with formatInfo in (clipboard info)
if item 1 of formatInfo is «class PNGf» then return the clipboard as «class PNGf»
end repeat
return \"no-png\"",
    ];
    let output = run("osascript", &script, None, 2 * MAX_IMAGE_BYTES + 64)
        .await
        .map_err(|error| notice("osascript", &error))?;
    let text = String::from_utf8_lossy(&output);
    if text.trim() == "no-png" {
        Ok(None)
    } else {
        decode_image(&text).map(Some)
    }
}

#[cfg(target_os = "macos")]
fn decode_image(text: &str) -> Result<InlineImage, String> {
    let error = "the clipboard's image could not be read";
    let hex = text
        .trim()
        .strip_prefix("«data PNGf")
        .and_then(|hex| hex.strip_suffix('»'))
        .filter(|hex| hex.len().is_multiple_of(2))
        .ok_or(error)?;
    let digit = |byte| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(digit(pair[0]).ok_or(error)? * 16 + digit(pair[1]).ok_or(error)?))
        .collect::<Result<Vec<u8>, &str>>()?;
    image_of("image/png", bytes)
}

fn image_of(media_type: &str, bytes: Vec<u8>) -> Result<InlineImage, String> {
    InlineImage::new(bytes, Some(media_type), String::new())
}

fn notice(tool: &str, error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        let package = match tool {
            "wl-paste" => "wl-clipboard",
            "xclip" => "xclip",
            _ => "the system clipboard utility",
        };
        format!("{tool} not found; install or restore {package}")
    } else {
        format!("{tool}: {error}; check the clipboard service and display connection")
    }
}

async fn run(
    tool: &str,
    arguments: &[&str],
    last: Option<&str>,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut command = Command::from(crate::supervise::detached(tool)?);
    command
        .args(arguments)
        .args(last)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let read = async {
        let mut child = child::Owner::spawn(command).await?;
        let (_, stdout, stderr) = child.pipes();
        let output = async {
            let mut bytes = Vec::new();
            let most = u64::try_from(limit).unwrap_or(u64::MAX) + 1;
            stdout
                .expect("piped stdout")
                .take(most)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("clipboard output exceeds the {limit}-byte limit"),
                ));
            }
            Ok(bytes)
        };
        let errors = async {
            let mut stderr = stderr.expect("piped stderr");
            let mut diagnostic = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stderr.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                let keep = count.min(1024 - diagnostic.len());
                diagnostic.extend_from_slice(&buffer[..keep]);
            }
            io::Result::Ok(diagnostic)
        };
        let (bytes, diagnostic) = tokio::try_join!(output, errors)?;
        let status = child.wait(child::Descendants::Kill).await?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "exited with {status}: {}",
                crate::terminal_text::safe_text(
                    &String::from_utf8_lossy(&diagnostic),
                    crate::terminal_text::TextMode::Diagnostic
                )
            )));
        }
        Ok(bytes)
    };
    time::timeout(TIMEOUT, read).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "clipboard helper timed out after 2 seconds",
        )
    })?
}
