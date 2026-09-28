use anyhow::{Context as _, Result, anyhow, bail};
use flags2env::BundledFlags2Env;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOKEN_FILE_BYTES: u64 = 4096;

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct CliConfig {
    GS_DESKTOP_DAEMON_URL: String,
    GS_DESKTOP_TIMEOUT_MS: i64,
    GS_DESKTOP_TENANT_ID: Option<String>,
    GS_DESKTOP_DEPLOYMENT_ID: Option<String>,
    GS_DESKTOP_PAYLOAD: Option<Value>,
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
        .filter(|value| *value > 0 && *value <= 1_200_000)
        .ok_or_else(|| anyhow!("--timeout must be between 1 and 1200000 ms"))?;
    let token = read_token()?;
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
            let response = client
                .post(format!(
                    "{base}/v1/cells/{tenant_id}/{deployment_id}/retire"
                ))
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
                .post(format!("{base}/v1/invoke"))
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

async fn send_get(client: &reqwest::Client, base: &str, path: &str, token: &str) -> Result<()> {
    let response = client
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await?;
    return print_response(response).await;
}

async fn print_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = read_bounded_body(response, MAX_RESPONSE_BYTES).await?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        bail!("daemon returned {status}: {text}");
    }
    let value: Value = serde_json::from_slice(&body).context("daemon response was not JSON")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    return Ok(());
}

async fn read_bounded_body(mut response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("daemon response exceeds {max_bytes} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("daemon response exceeds {max_bytes} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    return Ok(body);
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

fn validate_daemon_origin(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw).context("GS_DESKTOP_DAEMON_URL is not a valid URL")?;
    if url.scheme() != "http" {
        bail!("GS_DESKTOP_DAEMON_URL must use http on loopback");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("GS_DESKTOP_DAEMON_URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("GS_DESKTOP_DAEMON_URL must not contain query or fragment data");
    }
    if !matches!(url.path(), "" | "/") {
        bail!("GS_DESKTOP_DAEMON_URL must be an origin without a path");
    }
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "::1" | "[::1]") {
        bail!("GS_DESKTOP_DAEMON_URL must use a literal loopback address");
    }
    if url.port().is_none() {
        bail!("GS_DESKTOP_DAEMON_URL must include an explicit port");
    }
    return Ok(url.as_str().trim_end_matches('/').to_owned());
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

fn read_token() -> Result<String> {
    let path = if let Some(path) = env::var_os("GS_DESKTOP_TOKEN_FILE") {
        PathBuf::from(path)
    } else {
        home_dir()?.join(".graal-show/daemon/token")
    };
    return read_token_file(&path);
}

fn home_dir() -> Result<PathBuf> {
    return env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME/USERPROFILE is required"));
}

fn read_token_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect daemon token at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("daemon token path must be a regular non-symlink file");
    }
    if metadata.len() == 0 || metadata.len() > MAX_TOKEN_FILE_BYTES {
        bail!("daemon token file size is invalid");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("daemon token file must not be accessible by group/other users");
        }
    }
    let token = fs::read_to_string(path)
        .with_context(|| format!("cannot read daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_whitespace) {
        bail!("daemon token is invalid");
    }
    return Ok(token.to_owned());
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

    #[cfg(unix)]
    #[test]
    fn token_reader_rejects_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = env::temp_dir().join(format!("gs-cli-token-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create test dir");
        let target = root.join("target");
        fs::write(&target, "abcdefghijklmnopqrstuvwxyz0123456789\n").expect("write target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod target");
        let link = root.join("token");
        symlink(&target, &link).expect("create symlink");
        assert!(read_token_file(&link).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
