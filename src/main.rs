use anyhow::{Context as _, Result, anyhow, bail};
use flags2env::BundledFlags2Env;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, env, path::PathBuf, time::Duration};
use uuid::Uuid;

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
        bail!("unknown command-line options: {}", parsed.unknown_options.len());
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.saturating_add(2_000)))
        .build()?;
    let base = config.GS_DESKTOP_DAEMON_URL.trim_end_matches('/');

    match config.FLAGS2ENV_COMMAND.as_deref().unwrap_or("") {
        "status" => {
            let response = client
                .get(format!("{base}/v1/status"))
                .bearer_auth(&token)
                .send()
                .await?;
            print_response(response).await?;
        }
        "invoke" => {
            let tenant_id = required(config.GS_DESKTOP_TENANT_ID, "--tenant")?;
            let deployment_id = required(config.GS_DESKTOP_DEPLOYMENT_ID, "--deployment")?;
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
            bail!("command required: status or invoke");
        }
    }
    return Ok(());
}

async fn print_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!("daemon returned {status}: {body}");
    }
    let value: Value = serde_json::from_str(&body).context("daemon response was not JSON")?;
    println!("{}", serde_json::to_string_pretty(&value)?);
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
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
        PathBuf::from(home).join(".graal-show/daemon/token")
    };
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 {
        bail!("daemon token is invalid");
    }
    return Ok(token.to_owned());
}
