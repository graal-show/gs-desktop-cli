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
            eprintln!("gs-desktop-cli: {error}");
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
        .map_err(|error| anyhow!(error.to_string()))?;

    let argv = env::args().collect::<Vec<_>>();
    let parsed = parser
        .parse_structured(&argv, Some(config_path_text))
        .map_err(|error| anyhow!(error.to_string()))?;
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
        .map_err(|error| anyhow!(error.to_string()))?;

    let command = config.FLAGS2ENV_COMMAND.as_deref().unwrap_or("");
    let daemon_url = validate_loopback_url(&config.GS_DESKTOP_DAEMON_URL)?;
    let timeout_ms = u64::try_from(config.GS_DESKTOP_TIMEOUT_MS)
        .ok()
        .filter(|value| *value > 0 && *value <= MAX_TIMEOUT_MS)
        .ok_or_else(|| anyhow!("--timeout must be between 1 and {MAX_TIMEOUT_MS} ms"))?;
    let token_path = token_path(config.GS_DESKTOP_TOKEN_FILE.as_deref())?;
    let token = read_token(&token_path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.saturating_add(2_000)))
        .build()?;

    match command {
        "status" => {
            get_and_print(&client, &daemon_url, "/v1/status", &token).await?;
        }
        "doctor" => {
            get_and_print(&client, &daemon_url, "/v1/doctor", &token).await?;
        }
        "cells" => {
            get_and_print(&client, &daemon_url, "/v1/cells", &token).await?;
        }
        "retire" => {
            let tenant_id = required(config.GS_DESKTOP_TENANT_ID, "--tenant")?;
            let deployment_id = required(config.GS_DESKTOP_DEPLOYMENT_ID, "--deployment")?;
            validate_identifier("tenant", &tenant_id)?;
            validate_identifier("deployment", &deployment_id)?;
            let path = format!("/v1/cells/{tenant_id}/{deployment_id}/retire");
            let response = client
                .post(endpoint(&daemon_url, &path)?)
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
            let payload_json = config.GS_DESKTOP_PAYLOAD.unwrap_or_else(|| json!({}));
            let body = json!({
                "invocation_id": Uuid::new_v4().to_string(),
                "tenant_id": tenant_id,
                "deployment_id": deployment_id,
                "payload_json": payload_json,
                "timeout_ms": timeout_ms,
            });
            let response = client
                .post(endpoint(&daemon_url, "/v1/invoke")?)
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

async fn get_and_print(
    client: &reqwest::Client,
    base: &Url,
    path: &str,
    token: &str,
) -> Result<()> {
    let response = client
        .get(endpoint(base, path)?)
        .bearer_auth(token)
        .send()
        .await?;
    return print_response(response).await;
}

async fn print_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        let summary = body.chars().take(2_048).collect::<String>();
        bail!("daemon returned {status}: {summary}");
    }
    let value: Value = serde_json::from_str(&body).context("daemon response was not JSON")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    return Ok(());
}

fn endpoint(base: &Url, path: &str) -> Result<Url> {
    return base.join(path).context("cannot build local daemon endpoint");
}

fn validate_loopback_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw).context("daemon URL is invalid")?;
    if url.scheme() != "http" {
        bail!("daemon URL must use http:// on loopback");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("daemon URL must not contain credentials");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("daemon URL has no host"))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if !loopback {
        bail!("daemon URL must target loopback");
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    return Ok(url);
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != "..";
    if !valid {
        bail!("invalid {name}");
    }
    return Ok(());
}

fn required(value: Option<String>, flag: &str) -> Result<String> {
    return value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{flag} is required"));
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
    let executable = env::current_exe()?;
    if let Some(parent) = executable.parent() {
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
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow!("HOME or USERPROFILE is required"))?;
    return Ok(PathBuf::from(home).join(".graal-show/daemon/token"));
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .ok_or_else(|| anyhow!("HOME or USERPROFILE is required"))?;
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(PathBuf::from(home).join(suffix));
    }
    return Ok(path.to_path_buf());
}

fn read_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        bail!("daemon token is invalid");
    }
    return Ok(token.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_url_must_be_loopback_http() {
        assert!(validate_loopback_url("http://127.0.0.1:8764").is_ok());
        assert!(validate_loopback_url("http://localhost:8764").is_ok());
        assert!(validate_loopback_url("https://127.0.0.1:8764").is_err());
        assert!(validate_loopback_url("http://example.com:8764").is_err());
        assert!(validate_loopback_url("http://user:pass@127.0.0.1:8764").is_err());
    }

    #[test]
    fn identifiers_reject_path_traversal() {
        assert!(validate_identifier("deployment", "deploy-1").is_ok());
        assert!(validate_identifier("deployment", "..").is_err());
        assert!(validate_identifier("deployment", "tenant/escape").is_err());
    }
}
