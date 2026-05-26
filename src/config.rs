use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::{
    env,
    io::{self, IsTerminal, Write},
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
            Err(_) => dirs::cache_dir()
                .ok_or_else(|| anyhow!("could not determine a cache directory"))?
                .join("maid"),
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

fn resolve_github_token(bot_login: &str) -> Result<String> {
    if let Ok(token) = env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(token);
    }

    let active_login = active_gh_login()?;
    if !io::stdin().is_terminal() {
        bail!(
            "GITHUB_TOKEN is not set and stdin is not interactive; set GITHUB_TOKEN or run Maid from a terminal to approve using gh account {active_login}"
        );
    }

    eprintln!("GITHUB_TOKEN is not set.");
    eprintln!("GitHub CLI active account: {active_login}");
    if !active_login.eq_ignore_ascii_case(bot_login) {
        eprintln!(
            "Note: MAID_BOT_LOGIN is {bot_login}, so Maid will still listen for @{bot_login}."
        );
    }
    eprint!("Use the active gh account token for Maid? [y/N] ");
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        bail!("GITHUB_TOKEN is required unless you approve using the active gh account");
    }

    gh_token_for(&active_login)
}

fn active_gh_login() -> Result<String> {
    let output = Command::new("gh")
        .args([
            "auth",
            "status",
            "--active",
            "--hostname",
            "github.com",
            "--json",
            "hosts",
        ])
        .output()
        .context("failed to run `gh auth status`; set GITHUB_TOKEN or install/authenticate gh")?;

    if !output.status.success() {
        bail!(
            "`gh auth status` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    active_gh_login_from_status_json(&String::from_utf8_lossy(&output.stdout))
}

fn active_gh_login_from_status_json(json: &str) -> Result<String> {
    let status: GhAuthStatus = serde_json::from_str(json).context("invalid gh auth status JSON")?;
    let login = status
        .hosts
        .github_com
        .into_iter()
        .find(|account| account.active && account.state == "success")
        .map(|account| account.login)
        .ok_or_else(|| anyhow!("gh has no active authenticated github.com account"))?;
    Ok(login)
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

#[derive(Debug, Deserialize)]
struct GhAuthStatus {
    hosts: GhAuthHosts,
}

#[derive(Debug, Deserialize)]
struct GhAuthHosts {
    #[serde(rename = "github.com")]
    github_com: Vec<GhAccount>,
}

#[derive(Debug, Deserialize)]
struct GhAccount {
    active: bool,
    login: String,
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_gh_login_from_status_json() {
        let login = active_gh_login_from_status_json(
            r#"{
                "hosts": {
                    "github.com": [
                        {"state": "success", "active": false, "login": "dionysuzx"},
                        {"state": "success", "active": true, "login": "mayushii-nyan"}
                    ]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(login, "mayushii-nyan");
    }

    #[test]
    fn rejects_status_json_without_active_successful_account() {
        let err = active_gh_login_from_status_json(
            r#"{
                "hosts": {
                    "github.com": [
                        {"state": "failed", "active": true, "login": "mayushii-nyan"}
                    ]
                }
            }"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("no active authenticated"));
    }
}
