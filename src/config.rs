use crate::domain::{CodexPromptTemplates, RepoSlug};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::{
    env,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub github_token: String,
    pub bot_login: String,
    pub master_accounts: Vec<String>,
    pub auto_review_accounts: Vec<String>,
    pub auto_review_repos: Vec<RepoSlug>,
    pub cache_dir: PathBuf,
    pub poll_interval: Duration,
    pub task_start_ledger_path: PathBuf,
    pub task_limit_per_24h: Option<usize>,
    pub codex_bin: String,
    pub codex_model: String,
    pub codex_reasoning_effort: String,
    pub codex_prompts: CodexPromptTemplates,
    pub github_api_ip: Option<IpAddr>,
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
        let master_accounts = required_logins(file.master_accounts, "master_accounts")
            .with_context(|| format!("master_accounts is required in {}", config_path.display()))?;
        let auto_review_accounts =
            optional_logins(file.auto_review_accounts, "auto_review_accounts")?
                .unwrap_or_else(|| master_accounts.clone());
        for login in &auto_review_accounts {
            if !master_accounts.contains(login) {
                bail!("auto_review_accounts must be a subset of master_accounts: {login}");
            }
        }
        let auto_review_repos = optional_repos(file.auto_review_repos, "auto_review_repos")?;
        let cache_dir = non_empty(file.cache_dir)
            .map(|path| expand_home(&path))
            .transpose()?
            .unwrap_or_else(|| maid_home.join("cache"));
        let poll_seconds = file.poll_seconds.unwrap_or(20).max(10);
        let task_limit_per_24h = file.task_limit_per_24h;
        let codex_bin = non_empty(file.codex_bin).unwrap_or_else(|| "codex".to_string());
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

        Ok(Self {
            github_token,
            bot_login,
            master_accounts,
            auto_review_accounts,
            auto_review_repos,
            cache_dir,
            poll_interval: Duration::from_secs(poll_seconds),
            task_start_ledger_path: maid_home.join("task-starts.json"),
            task_limit_per_24h,
            codex_bin,
            codex_model,
            codex_reasoning_effort,
            codex_prompts,
            github_api_ip,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    bot_login: Option<String>,
    master_accounts: Option<Vec<String>>,
    auto_review_accounts: Option<Vec<String>>,
    auto_review_repos: Option<Vec<String>>,
    cache_dir: Option<String>,
    poll_seconds: Option<u64>,
    task_limit_per_24h: Option<usize>,
    codex_bin: Option<String>,
    codex_model: Option<String>,
    codex_reasoning_effort: Option<String>,
    codex_prompts: Option<CodexPromptsFile>,
    github_api_ip: Option<String>,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
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
        return Ok(PathBuf::from(value));
    }

    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("could not determine the home directory"))?
        .join(".maid"))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required_string(value: Option<String>, key: &str) -> Result<String> {
    non_empty(value).ok_or_else(|| anyhow!("{key} must not be empty"))
}

fn required_logins(value: Option<Vec<String>>, key: &str) -> Result<Vec<String>> {
    let Some(raw_logins) = value else {
        bail!("{key} must list at least one GitHub login");
    };

    let logins = normalize_logins(raw_logins, key)?;
    if logins.is_empty() {
        bail!("{key} must list at least one GitHub login");
    }

    Ok(logins)
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
master_accounts = ["dionysuzx"]
auto_review_accounts = ["dionysuzx"]
auto_review_repos = ["dionysuzx/maid"]
cache_dir = "~/.maid/cache"
poll_seconds = 30
task_limit_per_24h = 5
codex_bin = "codex-test"
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
        assert_eq!(config.master_accounts, Some(vec!["dionysuzx".to_string()]));
        assert_eq!(
            config.auto_review_accounts,
            Some(vec!["dionysuzx".to_string()])
        );
        assert_eq!(
            config.auto_review_repos,
            Some(vec!["dionysuzx/maid".to_string()])
        );
        assert_eq!(config.cache_dir.as_deref(), Some("~/.maid/cache"));
        assert_eq!(config.poll_seconds, Some(30));
        assert_eq!(config.task_limit_per_24h, Some(5));
        assert_eq!(config.codex_bin.as_deref(), Some("codex-test"));
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
        assert_eq!(config.auto_review_repos, None);
        assert_eq!(config.poll_seconds, None);
        assert_eq!(config.task_limit_per_24h, None);
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
    fn validates_required_login_lists() {
        assert_eq!(
            required_logins(
                Some(vec![
                    "  Dionysuzx  ".to_string(),
                    "dionysuzx".to_string(),
                    "mayushii-admin".to_string()
                ]),
                "master_accounts"
            )
            .unwrap(),
            vec!["dionysuzx".to_string(), "mayushii-admin".to_string()]
        );

        assert!(required_logins(None, "master_accounts").is_err());
        assert!(required_logins(Some(Vec::new()), "master_accounts").is_err());
        assert!(required_logins(Some(vec![" ".to_string()]), "master_accounts").is_err());
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
    fn expands_home_paths() {
        let home = dirs::home_dir().unwrap();

        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("~/cache").unwrap(), home.join("cache"));
        assert_eq!(
            expand_home("/tmp/maid").unwrap(),
            PathBuf::from("/tmp/maid")
        );
    }
}
