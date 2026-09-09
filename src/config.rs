use crate::{
    domain::{CodexPromptTemplates, GitHubUserId, RepoSlug, TrustedAccount},
    github::{
        DEFAULT_GITHUB_API_REQUESTS_PER_HOUR, DEFAULT_GITHUB_NOTIFICATION_WINDOW_HOURS,
        GitHubApiRequestRate, GitHubNotificationWindow,
    },
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Eq, PartialEq)]
pub struct Config {
    pub github_token: String,
    pub bot_login: String,
    pub master_accounts: Vec<TrustedAccount>,
    pub auto_review_accounts: Vec<TrustedAccount>,
    pub auto_review_public_accounts: Vec<TrustedAccount>,
    pub auto_review_repos: Vec<RepoSlug>,
    pub git_dir: PathBuf,
    pub daemon_pid_path: PathBuf,
    pub task_start_ledger_path: PathBuf,
    pub pending_handled_marker_ledger_path: PathBuf,
    pub observed_notification_ledger_path: PathBuf,
    pub task_limit_per_24h: Option<usize>,
    pub max_concurrent_requests: usize,
    pub github_api_requests_per_hour: GitHubApiRequestRate,
    pub github_notification_window: GitHubNotificationWindow,
    pub codex_home: PathBuf,
    pub codex_model: String,
    pub codex_reasoning_effort: String,
    pub codex_prompts: CodexPromptTemplates,
    pub github_api_ip: Option<IpAddr>,
    pub metrics_bind_address: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let maid_home = maid_home()?;
        let config_path = maid_home.join("config.toml");
        let file = ConfigFile::read(&config_path)?;
        let bot_login = non_empty(file.bot_login).with_context(|| {
            format!(
                "bot_login is required; run `just init` and fill out {}",
                config_path.display()
            )
        })?;
        let master_accounts = required_accounts(file.master_accounts, "master_accounts")
            .with_context(|| format!("master_accounts is required in {}", config_path.display()))?;
        let auto_review_accounts = select_accounts(
            optional_logins(file.auto_review_accounts, "auto_review_accounts")?,
            &master_accounts,
            "auto_review_accounts",
        )?
        .unwrap_or_else(|| master_accounts.clone());
        let auto_review_public_accounts = select_accounts(
            optional_logins(
                file.auto_review_public_accounts,
                "auto_review_public_accounts",
            )?,
            &master_accounts,
            "auto_review_public_accounts",
        )?
        .unwrap_or_default();
        let auto_review_repos = optional_repos(file.auto_review_repos, "auto_review_repos")?;
        let git_dir = non_empty(file.git_dir)
            .map(|path| expand_home(&path))
            .transpose()?
            .map(absolute_path)
            .transpose()?
            .unwrap_or_else(|| maid_home.join("git"));
        let task_limit_per_24h = file.task_limit_per_24h;
        let max_concurrent_requests = file.max_concurrent_requests.unwrap_or(1);
        if max_concurrent_requests == 0 {
            bail!("max_concurrent_requests must be at least 1");
        }
        let github_api_requests_per_hour = GitHubApiRequestRate::per_hour(
            file.github_api_requests_per_hour
                .unwrap_or(DEFAULT_GITHUB_API_REQUESTS_PER_HOUR),
        )?;
        let github_notification_window = GitHubNotificationWindow::hours(
            file.github_notification_window_hours
                .unwrap_or(DEFAULT_GITHUB_NOTIFICATION_WINDOW_HOURS),
        )?;
        let codex_home = non_empty(file.codex_home)
            .map(|path| expand_home(&path))
            .transpose()?
            .map(absolute_path)
            .transpose()?
            .unwrap_or_else(|| maid_home.join("codex"));
        let codex_model = required_string(file.codex_model, "codex_model")
            .with_context(|| format!("codex_model is required in {}", config_path.display()))?;
        let codex_reasoning_effort =
            required_string(file.codex_reasoning_effort, "codex_reasoning_effort").with_context(
                || {
                    format!(
                        "codex_reasoning_effort is required in {}",
                        config_path.display()
                    )
                },
            )?;
        let codex_prompts = required_codex_prompts(file.codex_prompts)
            .with_context(|| format!("codex_prompts is required in {}", config_path.display()))?;
        let github_api_ip = non_empty(file.github_api_ip)
            .map(|value| value.parse::<IpAddr>())
            .transpose()
            .context("github_api_ip must be an IPv4 or IPv6 address")?;
        let github_token = gh_token_for(&bot_login)?;
        let metrics_bind_address: SocketAddr = file
            .metrics_bind_address
            .as_deref()
            .unwrap_or("127.0.0.1:9464")
            .parse()
            .context("metrics_bind_address must be an IP address and port")?;
        if !metrics_bind_address.ip().is_loopback() {
            bail!("metrics_bind_address must use a loopback IP address");
        }

        Ok(Self {
            github_token,
            bot_login,
            master_accounts,
            auto_review_accounts,
            auto_review_public_accounts,
            auto_review_repos,
            git_dir,
            daemon_pid_path: maid_home.join("maid.pid"),
            task_start_ledger_path: maid_home.join("task-starts.json"),
            pending_handled_marker_ledger_path: maid_home.join("pending-handled-markers.json"),
            observed_notification_ledger_path: maid_home.join("observed-notifications.json"),
            task_limit_per_24h,
            max_concurrent_requests,
            github_api_requests_per_hour,
            github_notification_window,
            codex_home,
            codex_model,
            codex_reasoning_effort,
            codex_prompts,
            github_api_ip,
            metrics_bind_address,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    bot_login: Option<String>,
    master_accounts: Option<Vec<TrustedAccountFile>>,
    auto_review_accounts: Option<Vec<String>>,
    auto_review_public_accounts: Option<Vec<String>>,
    auto_review_repos: Option<Vec<String>>,
    git_dir: Option<String>,
    task_limit_per_24h: Option<usize>,
    max_concurrent_requests: Option<usize>,
    github_api_requests_per_hour: Option<u32>,
    github_notification_window_hours: Option<u32>,
    codex_home: Option<String>,
    codex_model: Option<String>,
    codex_reasoning_effort: Option<String>,
    codex_prompts: Option<CodexPromptsFile>,
    github_api_ip: Option<String>,
    metrics_bind_address: Option<String>,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct TrustedAccountFile {
    login: String,
    id: u64,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct CodexPromptsFile {
    mention: Option<String>,
    pull_request_opened: Option<String>,
}

impl ConfigFile {
    fn read(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(config) => toml::from_str(&config)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}

fn maid_home() -> Result<PathBuf> {
    if let Ok(value) = env::var("MAID_HOME")
        && !value.trim().is_empty()
    {
        return absolute_path(expand_home(&value)?);
    }

    absolute_path(
        dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine the home directory"))?
            .join(".maid"),
    )
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_string(value: Option<String>, key: &str) -> Result<String> {
    non_empty(value).ok_or_else(|| anyhow!("{key} must not be empty"))
}

fn required_accounts(
    value: Option<Vec<TrustedAccountFile>>,
    key: &str,
) -> Result<Vec<TrustedAccount>> {
    let Some(raw_accounts) = value else {
        bail!("{key} must list at least one {{ login, id }} GitHub identity");
    };
    if raw_accounts.is_empty() {
        bail!("{key} must list at least one {{ login, id }} GitHub identity");
    }

    let mut accounts = Vec::new();
    for account in raw_accounts {
        let login = account.login.trim().to_ascii_lowercase();
        if login.is_empty() {
            bail!("{key} cannot contain an empty GitHub login");
        }
        let id = GitHubUserId::new(account.id).with_context(|| format!("invalid {key} ID"))?;
        if let Some(existing) = accounts
            .iter()
            .find(|existing: &&TrustedAccount| existing.login == login || existing.id == id)
        {
            bail!(
                "{key} contains a duplicate or conflicting GitHub identity: {} ({})",
                existing.login,
                existing.id.get()
            );
        }
        accounts.push(TrustedAccount { login, id });
    }
    Ok(accounts)
}

fn optional_logins(value: Option<Vec<String>>, key: &str) -> Result<Option<Vec<String>>> {
    value
        .map(|raw_logins| normalize_logins(raw_logins, key))
        .transpose()
}

fn optional_repos(value: Option<Vec<String>>, key: &str) -> Result<Vec<RepoSlug>> {
    let Some(raw_repos) = value else {
        return Ok(Vec::new());
    };

    let mut repos = Vec::new();
    for repo in raw_repos {
        let repo = RepoSlug::parse(&repo).with_context(|| format!("invalid {key} entry"))?;
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    Ok(repos)
}

fn select_accounts(
    logins: Option<Vec<String>>,
    trusted_accounts: &[TrustedAccount],
    key: &str,
) -> Result<Option<Vec<TrustedAccount>>> {
    let Some(logins) = logins else {
        return Ok(None);
    };
    let mut selected = Vec::new();
    for login in logins {
        let Some(account) = trusted_accounts
            .iter()
            .find(|account| account.login == login)
        else {
            bail!("{key} must be a subset of master_accounts: {login}");
        };
        selected.push(account.clone());
    }
    Ok(Some(selected))
}

fn required_codex_prompts(value: Option<CodexPromptsFile>) -> Result<CodexPromptTemplates> {
    let Some(prompts) = value else {
        bail!("codex_prompts must include mention and pull_request_opened templates");
    };

    let mention = non_empty(prompts.mention)
        .ok_or_else(|| anyhow!("codex_prompts.mention must not be empty"))?;
    let pull_request_opened = non_empty(prompts.pull_request_opened)
        .ok_or_else(|| anyhow!("codex_prompts.pull_request_opened must not be empty"))?;
    Ok(CodexPromptTemplates {
        mention,
        pull_request_opened,
    })
}

fn normalize_logins(raw_logins: Vec<String>, key: &str) -> Result<Vec<String>> {
    let mut logins = Vec::new();
    for login in raw_logins {
        let login = login.trim();
        if login.is_empty() {
            bail!("{key} cannot contain empty GitHub logins");
        }

        let normalized = login.to_ascii_lowercase();
        if !logins.contains(&normalized) {
            logins.push(normalized);
        }
    }

    Ok(logins)
}

fn expand_home(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("could not determine the home directory"));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("could not determine the home directory"))?
            .join(rest));
    }

    Ok(PathBuf::from(path))
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }

    Ok(env::current_dir()
        .context("failed to resolve the current directory")?
        .join(path))
}

fn gh_token_for(login: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com", "--user", login])
        .output()
        .with_context(|| {
            format!("failed to run `gh auth token --user {login}`; install and authenticate gh")
        })?;

    if !output.status.success() {
        bail!(
            "`gh auth token --user {login}` failed: {}; run `gh auth login` for {login} or check `gh auth status --hostname github.com`",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_config_file_values() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
bot_login = "maid-bot"
master_accounts = [{{ login = "dionysuzx", id = 1234 }}]
auto_review_accounts = ["dionysuzx"]
auto_review_public_accounts = ["dionysuzx"]
auto_review_repos = ["dionysuzx/maid"]
git_dir = "~/.maid/git"
task_limit_per_24h = 5
max_concurrent_requests = 3
github_api_requests_per_hour = 1200
github_notification_window_hours = 96
metrics_bind_address = "127.0.0.1:9999"
codex_home = "~/.maid/test-codex"
codex_model = "gpt-test"
codex_reasoning_effort = "high"
github_api_ip = "127.0.0.1"

[codex_prompts]
mention = "mention {{{{cleaned_text}}}}"
pull_request_opened = "review {{{{pr_url}}}}"
"#
        )
        .unwrap();

        let config = ConfigFile::read(file.path()).unwrap();

        assert_eq!(config.bot_login.as_deref(), Some("maid-bot"));
        assert_eq!(
            config.master_accounts,
            Some(vec![TrustedAccountFile {
                login: "dionysuzx".to_string(),
                id: 1234,
            }])
        );
        assert_eq!(
            config.auto_review_accounts,
            Some(vec!["dionysuzx".to_string()])
        );
        assert_eq!(
            config.auto_review_public_accounts,
            Some(vec!["dionysuzx".to_string()])
        );
        assert_eq!(
            config.auto_review_repos,
            Some(vec!["dionysuzx/maid".to_string()])
        );
        assert_eq!(config.git_dir.as_deref(), Some("~/.maid/git"));
        assert_eq!(config.task_limit_per_24h, Some(5));
        assert_eq!(config.max_concurrent_requests, Some(3));
        assert_eq!(config.github_api_requests_per_hour, Some(1200));
        assert_eq!(config.github_notification_window_hours, Some(96));
        assert_eq!(
            config.metrics_bind_address.as_deref(),
            Some("127.0.0.1:9999")
        );
        assert_eq!(config.codex_home.as_deref(), Some("~/.maid/test-codex"));
        assert_eq!(config.codex_model.as_deref(), Some("gpt-test"));
        assert_eq!(config.codex_reasoning_effort.as_deref(), Some("high"));
        let codex_prompts = config.codex_prompts.unwrap();
        assert_eq!(
            codex_prompts.mention.as_deref(),
            Some("mention {{cleaned_text}}")
        );
        assert_eq!(
            codex_prompts.pull_request_opened.as_deref(),
            Some("review {{pr_url}}")
        );
        assert_eq!(config.github_api_ip.as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn missing_config_file_is_empty_config() {
        let config = ConfigFile::read(Path::new("/tmp/maid-missing-config-for-test.toml")).unwrap();

        assert_eq!(config.bot_login, None);
        assert_eq!(config.master_accounts, None);
        assert_eq!(config.auto_review_accounts, None);
        assert_eq!(config.auto_review_public_accounts, None);
        assert_eq!(config.auto_review_repos, None);
        assert_eq!(config.git_dir, None);
        assert_eq!(config.task_limit_per_24h, None);
        assert_eq!(config.github_api_requests_per_hour, None);
        assert_eq!(config.github_notification_window_hours, None);
        assert_eq!(config.metrics_bind_address, None);
        assert_eq!(config.codex_prompts, None);
    }

    #[test]
    fn requires_codex_prompt_templates() {
        assert!(required_codex_prompts(None).is_err());
        assert!(
            required_codex_prompts(Some(CodexPromptsFile {
                mention: Some("mention".to_string()),
                pull_request_opened: None,
            }))
            .is_err()
        );
        assert!(
            required_codex_prompts(Some(CodexPromptsFile {
                mention: Some(" ".to_string()),
                pull_request_opened: Some("review".to_string()),
            }))
            .is_err()
        );

        assert_eq!(
            required_codex_prompts(Some(CodexPromptsFile {
                mention: Some("mention".to_string()),
                pull_request_opened: Some("review".to_string()),
            }))
            .unwrap(),
            CodexPromptTemplates {
                mention: "mention".to_string(),
                pull_request_opened: "review".to_string(),
            }
        );
    }

    #[test]
    fn trims_empty_strings_to_none() {
        assert_eq!(
            non_empty(Some("  maid-bot  ".to_string())).as_deref(),
            Some("maid-bot")
        );
        assert_eq!(non_empty(Some("  ".to_string())), None);
    }

    #[test]
    fn requires_non_empty_strings() {
        assert_eq!(
            required_string(Some("  gpt-test  ".to_string()), "codex_model").unwrap(),
            "gpt-test"
        );
        assert!(required_string(None, "codex_model").is_err());
        assert!(required_string(Some("  ".to_string()), "codex_model").is_err());
    }

    #[test]
    fn validates_required_trusted_accounts() {
        assert_eq!(
            required_accounts(
                Some(vec![
                    TrustedAccountFile {
                        login: "  Dionysuzx  ".to_string(),
                        id: 1234,
                    },
                    TrustedAccountFile {
                        login: "mayushii-admin".to_string(),
                        id: 5678,
                    },
                ]),
                "master_accounts"
            )
            .unwrap(),
            vec![
                TrustedAccount {
                    login: "dionysuzx".to_string(),
                    id: GitHubUserId::new(1234).unwrap(),
                },
                TrustedAccount {
                    login: "mayushii-admin".to_string(),
                    id: GitHubUserId::new(5678).unwrap(),
                },
            ]
        );

        assert!(required_accounts(None, "master_accounts").is_err());
        assert!(required_accounts(Some(Vec::new()), "master_accounts").is_err());
        assert!(
            required_accounts(
                Some(vec![TrustedAccountFile {
                    login: " ".to_string(),
                    id: 1,
                }]),
                "master_accounts"
            )
            .is_err()
        );
        assert!(
            required_accounts(
                Some(vec![TrustedAccountFile {
                    login: "trusted".to_string(),
                    id: 0,
                }]),
                "master_accounts"
            )
            .is_err()
        );
        assert!(
            required_accounts(
                Some(vec![
                    TrustedAccountFile {
                        login: "trusted".to_string(),
                        id: 1,
                    },
                    TrustedAccountFile {
                        login: "trusted".to_string(),
                        id: 2,
                    },
                ]),
                "master_accounts"
            )
            .is_err()
        );
        assert!(
            required_accounts(
                Some(vec![
                    TrustedAccountFile {
                        login: "trusted".to_string(),
                        id: 1,
                    },
                    TrustedAccountFile {
                        login: "trusted".to_string(),
                        id: 1,
                    },
                ]),
                "master_accounts"
            )
            .is_err()
        );
        assert!(
            required_accounts(
                Some(vec![
                    TrustedAccountFile {
                        login: "trusted".to_string(),
                        id: 1,
                    },
                    TrustedAccountFile {
                        login: "other".to_string(),
                        id: 1,
                    },
                ]),
                "master_accounts"
            )
            .is_err()
        );
    }

    #[test]
    fn trusted_identity_is_explicit_and_stable_across_config_reloads() {
        let source = r#"master_accounts = [{ login = "OldLogin", id = 1234 }]"#;

        let first: ConfigFile = toml::from_str(source).unwrap();
        let second: ConfigFile = toml::from_str(source).unwrap();
        let first = required_accounts(first.master_accounts, "master_accounts").unwrap();
        let second = required_accounts(second.master_accounts, "master_accounts").unwrap();

        assert_eq!(first, second);
        assert_eq!(first[0].id, GitHubUserId::new(1234).unwrap());
        assert!(toml::from_str::<ConfigFile>(r#"master_accounts = ["old-login"]"#).is_err());
    }

    #[test]
    fn optional_login_lists_allow_empty_lists() {
        assert_eq!(
            optional_logins(
                Some(vec!["  Dionysuzx  ".to_string(), "dionysuzx".to_string()]),
                "auto_review_accounts"
            )
            .unwrap(),
            Some(vec!["dionysuzx".to_string()])
        );
        assert_eq!(
            optional_logins(Some(Vec::new()), "auto_review_accounts").unwrap(),
            Some(Vec::new())
        );
        assert!(optional_logins(Some(vec![" ".to_string()]), "auto_review_accounts").is_err());
        assert_eq!(optional_logins(None, "auto_review_accounts").unwrap(), None);
    }

    #[test]
    fn optional_repositories_parse_to_slugs() {
        assert_eq!(
            optional_repos(
                Some(vec![
                    "  Dionysuzx/Maid  ".to_string(),
                    "dionysuzx/maid".to_string()
                ]),
                "auto_review_repos"
            )
            .unwrap(),
            vec![RepoSlug {
                owner: "dionysuzx".to_string(),
                repo: "maid".to_string(),
            }]
        );
        assert_eq!(
            optional_repos(Some(Vec::new()), "auto_review_repos").unwrap(),
            Vec::<RepoSlug>::new()
        );
        assert!(optional_repos(Some(vec!["dionysuzx".to_string()]), "auto_review_repos").is_err());
        assert_eq!(
            optional_repos(None, "auto_review_repos").unwrap(),
            Vec::<RepoSlug>::new()
        );
    }

    #[test]
    fn auto_review_accounts_must_be_trusted() {
        let masters = vec![TrustedAccount {
            login: "dionysuzx".to_string(),
            id: GitHubUserId::new(1234).unwrap(),
        }];

        assert!(
            select_accounts(
                Some(vec!["dionysuzx".to_string()]),
                &masters,
                "auto_review_public_accounts"
            )
            .is_ok()
        );
        assert_eq!(
            select_accounts(
                Some(vec!["untrusted".to_string()]),
                &masters,
                "auto_review_public_accounts"
            )
            .unwrap_err()
            .to_string(),
            "auto_review_public_accounts must be a subset of master_accounts: untrusted"
        );
    }

    #[test]
    fn expands_home_paths() {
        let home = dirs::home_dir().unwrap();

        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("~/git").unwrap(), home.join("git"));
        assert_eq!(
            expand_home("/tmp/maid").unwrap(),
            PathBuf::from("/tmp/maid")
        );
    }

    #[test]
    fn absolutizes_relative_paths() {
        let current_dir = env::current_dir().unwrap();

        assert_eq!(
            absolute_path(PathBuf::from("relative/git")).unwrap(),
            current_dir.join("relative/git")
        );
        assert_eq!(
            absolute_path(PathBuf::from("/tmp/maid")).unwrap(),
            PathBuf::from("/tmp/maid")
        );
    }
}
