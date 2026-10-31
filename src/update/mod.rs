use std::{
    fs,
    io::Read as _,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use sha2::{Digest as _, Sha256};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SUMS_BYTES: usize = 64 * 1024;
const MAX_BINARY_BYTES: usize = 128 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn run() -> Result<(), String> {
    let latest = latest().await?;
    if !newer(&latest, VERSION)? {
        println!("bart {VERSION} is up to date");
        return Ok(());
    }
    let asset = asset()?;
    let binary = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot find the running binary: {error}"))?;
    let release = format!("{REPOSITORY}/releases/download/v{latest}");
    let client = client(reqwest::redirect::Policy::limited(10))?;
    let sums = download(&client, &format!("{release}/SHA256SUMS"), MAX_SUMS_BYTES).await?;
    let expected = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim_start_matches([' ', '*']) == asset).then(|| hash.to_owned())
        })
        .ok_or_else(|| format!("release v{latest} has no checksum for {asset}"))?;
    let bytes = download(&client, &format!("{release}/{asset}"), MAX_BINARY_BYTES).await?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err(format!(
            "{asset} from release v{latest} failed its checksum"
        ));
    }
    install(&binary, &bytes, &latest)?;
    println!("updated bart {VERSION} to {latest}");
    Ok(())
}

async fn latest() -> Result<String, String> {
    let response = client(reqwest::redirect::Policy::none())?
        .get(format!("{REPOSITORY}/releases/latest"))
        .send()
        .await
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|location| location.to_str().ok())
        .and_then(|location| location.rsplit_once("/tag/v"))
        .map(|(_, version)| version.to_owned())
        .ok_or_else(|| "no published release found".to_owned())
}

fn newer(latest: &str, current: &str) -> Result<bool, String> {
    let parse = |version: &str| {
        version
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| format!("unexpected version {version}"))
    };
    Ok(parse(latest)? > parse(current)?)
}

fn client(redirects: reqwest::redirect::Policy) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(redirects)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

fn asset() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("bart-linux-x86_64"),
        ("linux", "aarch64") => Ok("bart-linux-aarch64"),
        ("macos", "aarch64") => Ok("bart-darwin-arm64"),
        ("macos", "x86_64") => Ok("bart-darwin-x86_64"),
        (os, arch) => Err(format!("no release build for {os} {arch}")),
    }
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let failed = |error: reqwest::Error| format!("cannot download {url}: {error}");
    let too_large = || format!("{url} is larger than {} KiB", limit / 1024);
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failed)?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if bytes.len() + chunk.len() > limit {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

// Keep the temporary file on the same filesystem for rename.
fn install(binary: &Path, bytes: &[u8], version: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("cannot replace {}: {error}", binary.display());
    let directory = binary.parent().expect("an executable lives in a directory");
    let mut file = tempfile::Builder::new()
        .prefix(".bart-update-")
        .tempfile_in(directory)
        .map_err(failed)?;
    std::io::Write::write_all(&mut file, bytes).map_err(failed)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))
        .map_err(failed)?;
    // Linux cannot execute a file that is open for writing.
    let path = file.into_temp_path();
    probe(&path, version)?;
    path.persist(binary).map_err(|error| failed(error.error))
}

fn probe(path: &Path, version: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("the downloaded build does not run: {error}");
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(failed)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(failed)? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the downloaded build did not answer --version".to_owned());
            }
        }
    };
    let mut output = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(1024).read_to_string(&mut output);
    }
    if !status.success() || output.trim() != format!("bart {version}") {
        return Err(format!(
            "the downloaded build does not report version {version}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
