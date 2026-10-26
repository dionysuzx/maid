use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const LEDGER_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewAgent {
    Codex,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HeadSha(String);

impl HeadSha {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim().to_ascii_lowercase();
        let hex = value.len() == 40 || value.len() == 64;
        if hex && value.chars().all(|ch| ch.is_ascii_hexdigit()) {
            Ok(Self(value))
        } else {
            bail!("head sha must be 40 or 64 hex characters: {value:?}")
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PrUrl(String);

impl PrUrl {
    pub fn parse(html_url: &str) -> Result<Self> {
        let html_url = html_url.trim().to_ascii_lowercase();
        if html_url.is_empty() {
            bail!("pull request URL is empty");
        }
        Ok(Self(html_url))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PromptHash([u8; 32]);

impl PromptHash {
    pub fn of(rendered: &str) -> Self {
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(rendered.as_bytes()).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn parse_hex(value: &str) -> Result<Self> {
        if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
            bail!("prompt hash must be 64 hex characters");
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .context("prompt hash is not hex")?;
        }
        Ok(Self(bytes))
    }

    fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ModelName(String);

impl ModelName {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            bail!("review model is empty");
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EffortName(String);

impl EffortName {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            bail!("review effort is empty");
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentUrl(String);

impl CommentUrl {
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty() {
            bail!("comment URL is empty");
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Posted { comment_url: CommentUrl },
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRun {
    agent: ReviewAgent,
    model: ModelName,
    prompt_hash: PromptHash,
    effort: EffortName,
    started_unix: u64,
    finished_unix: u64,
    outcome: Outcome,
}

impl ReviewRun {
    pub fn new(
        agent: ReviewAgent,
        model: ModelName,
        prompt_hash: PromptHash,
        effort: EffortName,
        started_unix: u64,
        finished_unix: u64,
        outcome: Outcome,
    ) -> Result<Self> {
        if finished_unix < started_unix {
            bail!("review run finished before it started");
        }
        Ok(Self {
            agent,
            model,
            prompt_hash,
            effort,
            started_unix,
            finished_unix,
            outcome,
        })
    }

    pub fn agent(&self) -> ReviewAgent {
        self.agent
    }

    pub fn model(&self) -> &str {
        self.model.as_str()
    }

    pub fn effort(&self) -> &str {
        self.effort.as_str()
    }

    pub fn prompt_hash(&self) -> &PromptHash {
        &self.prompt_hash
    }

    pub fn started_unix(&self) -> u64 {
        self.started_unix
    }

    pub fn finished_unix(&self) -> u64 {
        self.finished_unix
    }

    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Idle,
    Review,
}

pub fn covered_by(runs: &[ReviewRun], agent: ReviewAgent) -> bool {
    runs.iter()
        .any(|run| run.agent == agent && matches!(run.outcome, Outcome::Posted { .. }))
}

pub fn plan(runs_at_head: &[ReviewRun], agent: ReviewAgent) -> Action {
    if covered_by(runs_at_head, agent) {
        Action::Idle
    } else {
        Action::Review
    }
}

pub trait ReviewLedger: Send + Sync {
    fn ensure_readable(&self) -> Result<()>;
    fn record(&self, pr: &PrUrl, sha: &HeadSha, run: ReviewRun) -> Result<()>;
    fn runs(&self, pr: &PrUrl, sha: &HeadSha) -> Result<Vec<ReviewRun>>;
    fn is_empty(&self) -> Result<bool>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryReviewLedger {
    state: Arc<Mutex<ReviewState>>,
}

impl ReviewLedger for MemoryReviewLedger {
    fn ensure_readable(&self) -> Result<()> {
        Ok(())
    }

    fn record(&self, pr: &PrUrl, sha: &HeadSha, run: ReviewRun) -> Result<()> {
        self.lock()?.record(pr, sha, run);
        Ok(())
    }

    fn runs(&self, pr: &PrUrl, sha: &HeadSha) -> Result<Vec<ReviewRun>> {
        Ok(self.lock()?.runs(pr, sha))
    }

    fn is_empty(&self) -> Result<bool> {
        Ok(self.lock()?.is_empty())
    }
}

impl MemoryReviewLedger {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ReviewState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("review ledger lock is poisoned"))
    }
}

#[derive(Clone, Debug)]
pub struct FileReviewLedger {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileReviewLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ReviewLedger for FileReviewLedger {
    fn ensure_readable(&self) -> Result<()> {
        let _guard = self.file_lock()?;
        ReviewFile::read(&self.path).map(|_| ())
    }

    fn record(&self, pr: &PrUrl, sha: &HeadSha, run: ReviewRun) -> Result<()> {
        let _guard = self.file_lock()?;
        let mut file = ReviewFile::read(&self.path)?;
        let mut state = file.into_state()?;
        state.record(pr, sha, run);
        file = ReviewFile::from_state(&state)?;
        file.write(&self.path)
    }

    fn runs(&self, pr: &PrUrl, sha: &HeadSha) -> Result<Vec<ReviewRun>> {
        let _guard = self.file_lock()?;
        Ok(ReviewFile::read(&self.path)?.into_state()?.runs(pr, sha))
    }

    fn is_empty(&self) -> Result<bool> {
        let _guard = self.file_lock()?;
        Ok(ReviewFile::read(&self.path)?.into_state()?.is_empty())
    }
}

impl FileReviewLedger {
    fn file_lock(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        self.lock
            .lock()
            .map_err(|_| anyhow!("review ledger lock is poisoned"))
    }
}

#[derive(Debug, Default)]
struct ReviewState {
    pulls: BTreeMap<String, BTreeMap<String, Vec<ReviewRun>>>,
}

impl ReviewState {
    fn runs(&self, pr: &PrUrl, sha: &HeadSha) -> Vec<ReviewRun> {
        self.pulls
            .get(pr.as_str())
            .and_then(|heads| heads.get(sha.as_str()))
            .cloned()
            .unwrap_or_default()
    }

    fn record(&mut self, pr: &PrUrl, sha: &HeadSha, run: ReviewRun) {
        self.pulls
            .entry(pr.as_str().to_string())
            .or_default()
            .entry(sha.as_str().to_string())
            .or_default()
            .push(run);
    }

    fn is_empty(&self) -> bool {
        self.pulls
            .values()
            .all(|heads| heads.values().all(|runs| runs.is_empty()))
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ReviewFile {
    version: u32,
    pulls: BTreeMap<String, BTreeMap<String, Vec<RunRecord>>>,
}

impl ReviewFile {
    fn read(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let file: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("failed to parse review ledger {}", path.display()))?;
                if file.version != LEDGER_VERSION {
                    bail!(
                        "review ledger {} has version {}, expected {LEDGER_VERSION}",
                        path.display(),
                        file.version
                    );
                }
                Ok(file)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: LEDGER_VERSION,
                pulls: BTreeMap::new(),
            }),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn into_state(self) -> Result<ReviewState> {
        let mut state = ReviewState::default();
        for (pr, heads) in self.pulls {
            let pr = PrUrl::parse(&pr)?;
            for (sha, runs) in heads {
                let sha = HeadSha::parse(&sha)?;
                for run in runs {
                    state.record(&pr, &sha, run.into_run()?);
                }
            }
        }
        Ok(state)
    }

    fn from_state(state: &ReviewState) -> Result<Self> {
        let mut pulls = BTreeMap::new();
        for (pr, heads) in &state.pulls {
            let mut stored_heads = BTreeMap::new();
            for (sha, runs) in heads {
                let records = runs.iter().cloned().map(RunRecord::from_run).collect();
                stored_heads.insert(sha.clone(), records);
            }
            pulls.insert(pr.clone(), stored_heads);
        }
        Ok(Self {
            version: LEDGER_VERSION,
            pulls,
        })
    }

    fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let mut file = tempfile::Builder::new()
            .prefix(".maid-pr-reviews.")
            .tempfile_in(parent)
            .with_context(|| {
                format!(
                    "failed to create temp review ledger in {}",
                    parent.display()
                )
            })?;
        serde_json::to_writer_pretty(&mut file, self).context("failed to encode review ledger")?;
        file.as_file()
            .sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        file.into_temp_path()
            .persist(path)
            .map_err(|err| err.error)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunRecord {
    agent: ReviewAgent,
    model: String,
    prompt_hash: String,
    effort: String,
    started_unix: u64,
    finished_unix: u64,
    outcome: OutcomeWire,
}

impl RunRecord {
    fn from_run(run: ReviewRun) -> Self {
        Self {
            agent: run.agent,
            model: run.model.as_str().to_string(),
            prompt_hash: run.prompt_hash.to_hex(),
            effort: run.effort.as_str().to_string(),
            started_unix: run.started_unix,
            finished_unix: run.finished_unix,
            outcome: OutcomeWire::from_outcome(run.outcome),
        }
    }

    fn into_run(self) -> Result<ReviewRun> {
        ReviewRun::new(
            self.agent,
            ModelName::parse(&self.model)?,
            PromptHash::parse_hex(&self.prompt_hash)?,
            EffortName::parse(&self.effort)?,
            self.started_unix,
            self.finished_unix,
            self.outcome.into_outcome()?,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum OutcomeWire {
    #[serde(rename = "posted")]
    Posted { comment_url: String },
    #[serde(rename = "failed")]
    Failed,
}

impl OutcomeWire {
    fn from_outcome(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Posted { comment_url } => Self::Posted {
                comment_url: comment_url.as_str().to_string(),
            },
            Outcome::Failed => Self::Failed,
        }
    }

    fn into_outcome(self) -> Result<Outcome> {
        match self {
            Self::Posted { comment_url } => Ok(Outcome::Posted {
                comment_url: CommentUrl::parse(&comment_url)?,
            }),
            Self::Failed => Ok(Outcome::Failed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

    #[test]
    fn a_posted_run_covers_only_that_head() {
        let posted = posted_run(ReviewAgent::Codex, "gpt-5.5", "high", "prompt");
        assert_eq!(plan(&[], ReviewAgent::Codex), Action::Review);
        assert_eq!(plan(&[failed_run()], ReviewAgent::Codex), Action::Review);
        assert_eq!(
            plan(std::slice::from_ref(&posted), ReviewAgent::Codex),
            Action::Idle
        );

        let ledger = MemoryReviewLedger::default();
        let pr = PrUrl::parse("https://github.com/o/r/pull/1").unwrap();
        let sha_a = HeadSha::parse(SHA_A).unwrap();
        ledger.record(&pr, &sha_a, posted).unwrap();
        assert_eq!(
            plan(&ledger.runs(&pr, &sha_a).unwrap(), ReviewAgent::Codex),
            Action::Idle
        );
        assert_eq!(
            plan(
                &ledger.runs(&pr, &HeadSha::parse(SHA_B).unwrap()).unwrap(),
                ReviewAgent::Codex
            ),
            Action::Review
        );
    }

    #[test]
    fn rejects_impossible_runs_and_shas() {
        assert!(HeadSha::parse("abc").is_err());
        assert!(HeadSha::parse(&"aa".repeat(32)).is_ok());
        let err = ReviewRun::new(
            ReviewAgent::Codex,
            ModelName::parse("gpt-5.5").unwrap(),
            PromptHash::of("prompt"),
            EffortName::parse("high").unwrap(),
            10,
            9,
            Outcome::Failed,
        );
        assert!(err.is_err());
        assert!(ModelName::parse(" ").is_err());
        assert!(CommentUrl::parse("").is_err());
    }

    #[test]
    fn missing_file_is_empty_and_a_posted_run_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pr-reviews.json");
        let ledger = FileReviewLedger::new(&path);
        let pr = PrUrl::parse("https://github.com/o/r/pull/1").unwrap();
        let sha = HeadSha::parse(SHA_A).unwrap();

        ledger.ensure_readable().unwrap();
        assert!(ledger.is_empty().unwrap());
        assert!(!path.exists());

        let run = posted_run(ReviewAgent::Codex, "gpt-5.5", "high", "please review");
        ledger.record(&pr, &sha, run.clone()).unwrap();

        let reloaded = FileReviewLedger::new(&path);
        let runs = reloaded.runs(&pr, &sha).unwrap();
        assert_eq!(runs, vec![run]);
        assert_eq!(
            plan(&reloaded.runs(&pr, &sha).unwrap(), ReviewAgent::Codex),
            Action::Idle
        );
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 1"));
        assert!(raw.contains("\"agent\": \"codex\""));
        assert!(raw.contains("#issuecomment-1"));
    }

    #[test]
    fn corrupt_file_is_left_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pr-reviews.json");
        fs::write(&path, "{").unwrap();
        let ledger = FileReviewLedger::new(&path);

        assert!(ledger.ensure_readable().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{");
    }

    #[test]
    fn unknown_version_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pr-reviews.json");
        fs::write(&path, "{\"version\": 2, \"pulls\": {}}").unwrap();
        assert!(FileReviewLedger::new(&path).ensure_readable().is_err());
        assert!(path.exists());
    }

    fn posted_run(agent: ReviewAgent, model: &str, effort: &str, prompt: &str) -> ReviewRun {
        ReviewRun::new(
            agent,
            ModelName::parse(model).unwrap(),
            PromptHash::of(prompt),
            EffortName::parse(effort).unwrap(),
            1_700_000_000,
            1_700_000_100,
            Outcome::Posted {
                comment_url: CommentUrl::parse("https://github.com/o/r/pull/1#issuecomment-1")
                    .unwrap(),
            },
        )
        .unwrap()
    }

    fn failed_run() -> ReviewRun {
        ReviewRun::new(
            ReviewAgent::Codex,
            ModelName::parse("gpt-5.5").unwrap(),
            PromptHash::of("prompt"),
            EffortName::parse("high").unwrap(),
            1_700_000_000,
            1_700_000_050,
            Outcome::Failed,
        )
        .unwrap()
    }
}
