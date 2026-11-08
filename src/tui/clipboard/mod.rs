use std::{
    io::{self, Write as _},
    process::Stdio,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{io::AsyncReadExt as _, process::Command, time};

use crate::history::{InlineImage, MAX_IMAGE_BYTES};

mod child;

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

pub(super) fn copy(text: &str, what: &str) -> io::Result<String> {
    let (sent, end) = sendable(text);
    let mut out = io::stdout();
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
    let mut child = child::Owner::spawn(&mut command)?;
    let (_, stdout, stderr) = child.pipes();
    let read = async {
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
        let status = child.wait().await?;
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
