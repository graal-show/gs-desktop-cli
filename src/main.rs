use anyhow::{Context as _, Result, anyhow, bail};
use flags2env::BundledFlags2Env;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const MAX_TIMEOUT_MS: u64 = 20 * 60 * 1_000;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_FILE_BYTES: u64 = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 4096;

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct CliConfig {
    GS_DESKTOP_DAEMON_URL: String,
    GS_DESKTOP_TIMEOUT_MS: i64,
    GS_DESKTOP_TENANT_ID: Option<String>,
    GS_DESKTOP_DEPLOYMENT_ID: Option<String>,
    GS_DESKTOP_PAYLOAD: Option<Value>,
    GS_DESKTOP_TOKEN_FILE: Option<String>,
    FLAGS2ENV_COMMAND: Option<String>,
}

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("gs-desktop: {error}");
            2
        }
    };
    std::process::exit(code);
}

async fn run() -> Result<()> {
    let config_path = resolve_config_path()?;
    let config_path_text = config_path
        .to_str()
        .ok_or_else(|| anyhow!(".cli-flags.toml path is not UTF-8"))?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(config_path_text))
        .map_err(|error| anyhow!("flags-2-env configuration audit failed: {error}"))?;

    let argv = env::args().collect::<Vec<_>>();
    let parsed = parser
        .parse_structured(&argv, Some(config_path_text))
        .map_err(|error| anyhow!("flags-2-env parse failed: {error}"))?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "unknown command-line options: {}",
            parsed.unknown_options.len()
        );
    }
    if !parsed.errors.is_empty() {
        bail!("invalid command-line values: {}", parsed.errors.join("; "));
    }
    if !parsed.extras.is_empty() {
        bail!("unexpected positional arguments: {}", parsed.extras.len());
    }

    let mut raw = env::vars().collect::<HashMap<_, _>>();
    raw.remove("FLAGS2ENV_COMMAND");
    raw.extend(parsed.provided_flags);
    let config = parser
        .coerce::<CliConfig, _>(&raw, Some(config_path_text))
        .map_err(|error| anyhow!("flags-2-env typed configuration failed: {error}"))?;

    let timeout_ms = u64::try_from(config.GS_DESKTOP_TIMEOUT_MS)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_TIMEOUT_MS)
        .ok_or_else(|| anyhow!("--timeout must be between 1 and {MAX_TIMEOUT_MS} ms"))?;
    let token_path = token_path(config.GS_DESKTOP_TOKEN_FILE.as_deref())?;
    let token = read_token(&token_path)?;
    let base = validate_daemon_origin(&config.GS_DESKTOP_DAEMON_URL)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(timeout_ms.saturating_add(2_000)))
        .build()?;

    match config.FLAGS2ENV_COMMAND.as_deref().unwrap_or("") {
        "status" => {
            send_get(&client, &base, "/v1/status", &token).await?;
        }
        "doctor" => {
            send_get(&client, &base, "/v1/doctor", &token).await?;
        }
        "cells" => {
            send_get(&client, &base, "/v1/cells", &token).await?;
        }
        "retire" => {
            let tenant_id = required(config.GS_DESKTOP_TENANT_ID, "--tenant")?;
            let deployment_id = required(config.GS_DESKTOP_DEPLOYMENT_ID, "--deployment")?;
            validate_identifier("tenant", &tenant_id)?;
            validate_identifier("deployment", &deployment_id)?;
            let path = format!("/v1/cells/{tenant_id}/{deployment_id}/retire");
            let response = client
                .post(endpoint(&base, &path)?)
                .bearer_auth(&token)
                .send()
                .await?;
            print_response(response).await?;
        }
        "invoke" => {
            let tenant_id = required(config.GS_DESKTOP_TENANT_ID, "--tenant")?;
            let deployment_id = required(config.GS_DESKTOP_DEPLOYMENT_ID, "--deployment")?;
            validate_identifier("tenant", &tenant_id)?;
            validate_identifier("deployment", &deployment_id)?;
            let body = json!({
                "invocation_id": Uuid::new_v4().to_string(),
                "tenant_id": tenant_id,
                "deployment_id": deployment_id,
                "payload_json": config.GS_DESKTOP_PAYLOAD.unwrap_or_else(|| json!({})),
                "timeout_ms": timeout_ms,
            });
            let response = client
                .post(endpoint(&base, "/v1/invoke")?)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?;
            print_response(response).await?;
        }
        _ => {
            bail!("command required: status, doctor, cells, retire, or invoke");
        }
    }
    return Ok(());
}

async fn send_get(client: &reqwest::Client, base: &Url, path: &str, token: &str) -> Result<()> {
    let response = client
        .get(endpoint(base, path)?)
        .bearer_auth(token)
        .send()
        .await?;
    return print_response(response).await;
}

async fn print_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = read_response_body(response).await?;
    if !status.is_success() {
        let summary = String::from_utf8_lossy(&body)
            .chars()
            .take(2_048)
            .collect::<String>();
        bail!("daemon returned {status}: {summary}");
    }
    let value: Value = serde_json::from_slice(&body).context("daemon response was not JSON")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    return Ok(());
}

async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("daemon response exceeds {MAX_RESPONSE_BYTES} bytes");
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("daemon response exceeds {MAX_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    return Ok(body);
}

fn endpoint(base: &Url, path: &str) -> Result<Url> {
    return base
        .join(path)
        .context("cannot build local daemon endpoint");
}

fn required(value: Option<String>, flag: &str) -> Result<String> {
    return value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{flag} is required"));
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("invalid {name} identifier");
    }
    return Ok(());
}

fn validate_daemon_origin(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("GS_DESKTOP_DAEMON_URL is not a valid URL")?;
    if url.scheme() != "http" {
        bail!("GS_DESKTOP_DAEMON_URL must use http on loopback");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("GS_DESKTOP_DAEMON_URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("GS_DESKTOP_DAEMON_URL must not contain query or fragment data");
    }
    if url.path() != "/" && !url.path().is_empty() {
        bail!("GS_DESKTOP_DAEMON_URL must be an origin without a path");
    }
    if url.port().is_none() {
        bail!("GS_DESKTOP_DAEMON_URL must include an explicit port");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("GS_DESKTOP_DAEMON_URL requires a host"))?;
    let normalized = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let ip = normalized
        .parse::<IpAddr>()
        .context("GS_DESKTOP_DAEMON_URL host must be a literal IP address")?;
    if !ip.is_loopback() {
        bail!("GS_DESKTOP_DAEMON_URL must target a literal loopback address");
    }
    return Ok(url);
}

fn resolve_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GS_DESKTOP_FLAGS_CONFIG") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!("GS_DESKTOP_FLAGS_CONFIG is not a readable file");
    }
    let current = env::current_dir()?.join(".cli-flags.toml");
    if current.is_file() {
        return Ok(current);
    }
    if let Some(parent) = env::current_exe()?.parent() {
        let adjacent = parent.join(".cli-flags.toml");
        if adjacent.is_file() {
            return Ok(adjacent);
        }
    }
    bail!("cannot locate .cli-flags.toml");
}

fn token_path(configured: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = configured.filter(|value| !value.trim().is_empty()) {
        return expand_home(Path::new(path));
    }
    return Ok(home_dir()?.join(".graal-show/daemon/token"));
}

fn read_token(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect daemon token at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("daemon token must be a regular non-symlink file");
    }
    if metadata.len() == 0 || metadata.len() > MAX_TOKEN_FILE_BYTES {
        bail!("daemon token file has an invalid size");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("daemon token file must not be accessible by group or other users");
        }
    }

    let token = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_whitespace) {
        bail!("daemon token is invalid");
    }
    return Ok(token.to_owned());
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(home_dir()?.join(suffix));
    }
    return Ok(path.to_path_buf());
}

fn home_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(profile) = env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(profile));
    }
    let drive = env::var_os("HOMEDRIVE").filter(|value| !value.is_empty());
    let path = env::var_os("HOMEPATH").filter(|value| !value.is_empty());
    if let (Some(drive), Some(path)) = (drive, path) {
        let mut value = PathBuf::from(drive);
        value.push(path);
        return Ok(value);
    }
    return Err(anyhow!("cannot determine user home directory"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_literal_loopback_http_origins() {
        assert!(validate_daemon_origin("http://127.0.0.1:8764").is_ok());
        assert!(validate_daemon_origin("http://[::1]:8764").is_ok());
        assert!(validate_daemon_origin("http://localhost:8764").is_err());
        assert!(validate_daemon_origin("https://127.0.0.1:8764").is_err());
        assert!(validate_daemon_origin("http://example.com:8764").is_err());
        assert!(validate_daemon_origin("http://user:pass@127.0.0.1:8764").is_err());
        assert!(validate_daemon_origin("http://127.0.0.1:8764/v1").is_err());
        assert!(validate_daemon_origin("http://127.0.0.1").is_err());
    }

    #[test]
    fn identifiers_reject_path_traversal() {
        assert!(validate_identifier("tenant", "tenant-1").is_ok());
        assert!(validate_identifier("deployment", "generation.v1").is_ok());
        assert!(validate_identifier("tenant", "..").is_err());
        assert!(validate_identifier("tenant", "tenant/child").is_err());
    }
}
