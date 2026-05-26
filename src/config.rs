use anyhow::{Context, Result, anyhow, bail};
use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub github_token: String,
    pub bot_login: String,
    pub cache_dir: PathBuf,
    pub poll_interval: Duration,
    pub codex_bin: String,
    pub github_api_ip: Option<IpAddr>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bot_login = env::var("MAID_BOT_LOGIN").unwrap_or_else(|_| "mayushii-nyan".to_string());
        let github_token = resolve_github_token(&bot_login)?;
        let bind_addr = env::var("MAID_BIND")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
            .parse()
            .context("MAID_BIND must be a socket address like 127.0.0.1:3000")?;
        let cache_dir = match env::var("MAID_CACHE_DIR") {
            Ok(value) => PathBuf::from(value),
            Err(_) => maid_home()?.join("cache"),
        };
        let poll_seconds = env::var("MAID_POLL_SECONDS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("MAID_POLL_SECONDS must be a positive integer")?
            .unwrap_or(20)
            .max(10);
        let codex_bin = env::var("MAID_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let github_api_ip = env::var("MAID_GITHUB_API_IP")
            .ok()
            .map(|value| value.parse::<IpAddr>())
            .transpose()
            .context("MAID_GITHUB_API_IP must be an IPv4 or IPv6 address")?;

        Ok(Self {
            bind_addr,
            github_token,
            bot_login,
            cache_dir,
            poll_interval: Duration::from_secs(poll_seconds),
            codex_bin,
            github_api_ip,
        })
    }
}

fn maid_home() -> Result<PathBuf> {
    if let Ok(value) = env::var("MAID_HOME")
        && !value.trim().is_empty()
    {
        return Ok(PathBuf::from(value));
    }

    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("could not determine the home directory"))?
        .join(".maid"))
}

fn resolve_github_token(bot_login: &str) -> Result<String> {
    if let Ok(token) = env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(token);
    }

    gh_token_for(bot_login).with_context(|| {
        format!("GITHUB_TOKEN is not set and gh has no usable token for {bot_login}")
    })
}

fn gh_token_for(login: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com", "--user", login])
        .output()
        .with_context(|| format!("failed to run `gh auth token --user {login}`"))?;

    if !output.status.success() {
        bail!(
            "`gh auth token --user {login}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let token = String::from_utf8(output.stdout)
        .context("gh returned a token that was not valid UTF-8")?
        .trim()
        .to_string();
    if token.is_empty() {
        bail!("gh returned an empty token for {login}");
    }

    Ok(token)
}
