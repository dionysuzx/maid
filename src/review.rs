use crate::auto_review::authorizing_team;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::oneshot;
use tracing::{error, warn};

use crate::{
    domain::{PullRequest, TeamSlug},
    task_limit::TaskStartRecorder,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HeadSha(String);

impl HeadSha {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.len() != 40
            || !raw
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("head sha must be 40 lowercase hex digits");
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedPull {
    pub pr: PullRequest,
    pub head: HeadSha,
    pub source: ListSource,
    pub requested_teams: Vec<TeamSlug>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListSource {
    Repository,
    TeamActivity,
    PublicAuthor { login: String },
}

#[derive(Clone, Debug)]
pub struct AutoReviewPolicy {
    pub bot_login: String,
    pub repository_accounts: HashSet<String>,
    pub public_accounts: HashSet<String>,
    pub teams: Vec<TeamSlug>,
    pub team_window: ChronoDuration,
}

pub struct Fetched<T> {
    pub items: Vec<T>,
    pub received: usize,
}

pub async fn pages<T, F, Fut>(continuous: bool, page_size: usize, mut fetch: F) -> Result<Vec<T>>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<Fetched<T>>>,
{
    let mut page = 1u32;
    let mut all = Vec::new();
    loop {
        let fetched = match fetch(page).await {
            Ok(fetched) => fetched,
            Err(err) if all.is_empty() => return Err(err),
            Err(err) => {
                warn!(
                    page,
                    error = ?err,
                    "listing stopped; keeping pages already read"
                );
                return Ok(all);
            }
        };
        let full = page_size > 0 && fetched.received >= page_size;
        all.extend(fetched.items);
        if !continuous || !full {
            return Ok(all);
        }
        page = page.saturating_add(1);
    }
}

pub fn in_scope(row: &ListedPull, policy: &AutoReviewPolicy, now: DateTime<Utc>) -> bool {
    if row.pr.author.eq_ignore_ascii_case(&policy.bot_login) {
        return false;
    }
    let team = authorizing_team(
        &row.requested_teams,
        &policy.teams,
        row.updated_at,
        now,
        policy.team_window,
    )
    .is_some();
    match &row.source {
        ListSource::TeamActivity => team,
        ListSource::PublicAuthor { login } => {
            policy
                .public_accounts
                .iter()
                .any(|account| account.eq_ignore_ascii_case(login))
                && row.pr.author.eq_ignore_ascii_case(login)
        }
        ListSource::Repository => {
            team || policy
                .repository_accounts
                .iter()
                .any(|account| account.eq_ignore_ascii_case(&row.pr.author))
        }
    }
}

pub fn dedup_listed(rows: Vec<ListedPull>, policy: &AutoReviewPolicy) -> Vec<ListedPull> {
    let mut kept: Vec<ListedPull> = Vec::new();
    let mut index_by_url = std::collections::HashMap::<String, usize>::new();
    for row in rows {
        let key = row.pr.html_url.to_ascii_lowercase();
        if let Some(index) = index_by_url.get(&key).copied() {
            let existing = &kept[index];
            let existing_author = author_allowed(existing, policy);
            let candidate_author = author_allowed(&row, policy);
            let candidate_is_team = matches!(row.source, ListSource::TeamActivity);
            if !existing_author && candidate_author && !candidate_is_team {
                kept[index] = row;
            }
        } else {
            index_by_url.insert(key, kept.len());
            kept.push(row);
        }
    }
    kept
}

fn author_allowed(row: &ListedPull, policy: &AutoReviewPolicy) -> bool {
    match &row.source {
        ListSource::TeamActivity => false,
        ListSource::PublicAuthor { login } => {
            policy
                .public_accounts
                .iter()
                .any(|account| account.eq_ignore_ascii_case(login))
                && row.pr.author.eq_ignore_ascii_case(login)
        }
        ListSource::Repository => policy
            .repository_accounts
            .iter()
            .any(|account| account.eq_ignore_ascii_case(&row.pr.author)),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckoutRev {
    Commit(HeadSha),
    Named(String),
}

#[async_trait]
pub trait ReviewHead: Send + Sync {
    async fn review(&self, row: &ListedPull, rev: &CheckoutRev) -> Result<HeadSha>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOutcome {
    Started,
    Skipped,
    Failed,
}

struct Flight {
    sha: HeadSha,
    done: oneshot::Receiver<Result<()>>,
}

struct MentionFlight {
    keys: Vec<String>,
    done: oneshot::Receiver<()>,
}

pub struct MentionDone {
    tx: Option<oneshot::Sender<()>>,
}

impl Drop for MentionDone {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

pub struct Reviews<R> {
    path: PathBuf,
    max_concurrent_requests: usize,
    covered: BTreeSet<HeadSha>,
    heads: BTreeSet<HeadSha>,
    mentions: HashSet<String>,
    mention_slots: usize,
    mention_flights: Vec<MentionFlight>,
    flights: Vec<Flight>,
    reviewer: R,
    reports_queued: Arc<AtomicUsize>,
    task_starts: Arc<dyn TaskStartRecorder>,
}

impl<R> Reviews<R>
where
    R: ReviewHead + Clone + 'static,
{
    pub fn open(
        path: impl Into<PathBuf>,
        max_concurrent_requests: usize,
        task_starts: Arc<dyn TaskStartRecorder>,
        reviewer: R,
    ) -> Result<Self> {
        let path = path.into();
        let covered = load_covered(&path)?;
        Ok(Self {
            path,
            max_concurrent_requests,
            covered,
            heads: BTreeSet::new(),
            mentions: HashSet::new(),
            mention_slots: 0,
            mention_flights: Vec::new(),
            flights: Vec::new(),
            reviewer,
            reports_queued: Arc::new(AtomicUsize::new(0)),
            task_starts,
        })
    }

    pub fn reload(&mut self, path: PathBuf) -> Result<()> {
        self.covered = load_covered(&path)?;
        self.path = path;
        Ok(())
    }

    pub fn covered_len(&self) -> usize {
        self.covered.len()
    }

    pub fn set_task_starts(&mut self, task_starts: Arc<dyn TaskStartRecorder>) {
        self.task_starts = task_starts;
    }

    pub fn set_max(&mut self, max_concurrent_requests: usize) {
        self.max_concurrent_requests = max_concurrent_requests.max(1);
    }

    pub fn submit_head(&mut self, row: ListedPull) -> SubmitOutcome {
        self.apply();
        if self.covered.contains(&row.head) || self.heads.contains(&row.head) {
            return SubmitOutcome::Skipped;
        }
        if self.occupied() >= self.max_concurrent_requests {
            return SubmitOutcome::Skipped;
        }
        match self.task_starts.try_record_started() {
            Ok(crate::task_limit::TaskStartDecision::Recorded) => {}
            Ok(crate::task_limit::TaskStartDecision::AtLimit { .. }) => {
                return SubmitOutcome::Skipped;
            }
            Err(err) => {
                error!(error = ?err, "failed to record a task start");
                return SubmitOutcome::Failed;
            }
        }
        let sha = row.head.clone();
        self.heads.insert(sha);
        self.launch(row);
        SubmitOutcome::Started
    }

    pub fn begin_mention(&mut self, key: &str) -> Option<MentionDone> {
        self.apply();
        if self.mentions.contains(key) || self.occupied() >= self.max_concurrent_requests {
            return None;
        }
        self.mentions.insert(key.to_string());
        self.mention_slots += 1;
        let (tx, rx) = oneshot::channel();
        self.mention_flights.push(MentionFlight {
            keys: vec![key.to_string()],
            done: rx,
        });
        Some(MentionDone { tx: Some(tx) })
    }

    pub fn attach_mention_key(&mut self, primary: &str, key: &str) -> bool {
        if key == primary {
            return true;
        }
        if self.mentions.contains(key) {
            return false;
        }
        let Some(flight) = self
            .mention_flights
            .iter_mut()
            .find(|flight| flight.keys.iter().any(|existing| existing == primary))
        else {
            return false;
        };
        flight.keys.push(key.to_string());
        self.mentions.insert(key.to_string());
        true
    }

    pub async fn poll_listed(
        &mut self,
        rows: Vec<ListedPull>,
        policy: &AutoReviewPolicy,
        now: DateTime<Utc>,
    ) -> Result<()> {
        for row in rows {
            if in_scope(&row, policy, now) {
                let _ = self.submit_head(row);
            }
        }
        tokio::task::yield_now().await;
        Ok(())
    }

    pub async fn drain(&mut self) {
        loop {
            self.apply();
            if self.heads.is_empty() && self.mention_slots == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    pub fn in_flight(&self) -> usize {
        self.heads.len()
    }

    pub fn occupied(&self) -> usize {
        self.heads.len() + self.mention_slots
    }

    pub fn apply_finished(&mut self) {
        self.apply();
    }

    #[cfg(test)]
    fn reports_queued(&self) -> usize {
        self.reports_queued.load(Ordering::SeqCst)
    }

    fn apply(&mut self) {
        let mut pending = Vec::new();
        let mut wrote = false;
        for mut flight in self.flights.drain(..) {
            match flight.done.try_recv() {
                Ok(Ok(())) => {
                    self.heads.remove(&flight.sha);
                    self.covered.insert(flight.sha);
                    wrote = true;
                }
                Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => {
                    self.heads.remove(&flight.sha);
                }
                Err(oneshot::error::TryRecvError::Empty) => pending.push(flight),
            }
        }
        self.flights = pending;

        let mut mentions = Vec::new();
        for mut flight in self.mention_flights.drain(..) {
            match flight.done.try_recv() {
                Ok(()) | Err(oneshot::error::TryRecvError::Closed) => {
                    for key in flight.keys {
                        self.mentions.remove(&key);
                    }
                    self.mention_slots = self.mention_slots.saturating_sub(1);
                }
                Err(oneshot::error::TryRecvError::Empty) => mentions.push(flight),
            }
        }
        self.mention_flights = mentions;

        if wrote && let Err(err) = write_covered(&self.path, &self.covered) {
            error!(error = ?err, path = %self.path.display(), "failed to record posted head");
        }
    }

    fn launch(&mut self, row: ListedPull) {
        let (tx, rx) = oneshot::channel();
        self.flights.push(Flight {
            sha: row.head.clone(),
            done: rx,
        });
        let reviewer = self.reviewer.clone();
        let queued = Arc::clone(&self.reports_queued);
        tokio::spawn(async move {
            let rev = CheckoutRev::Commit(row.head.clone());
            let result = match reviewer.review(&row, &rev).await {
                Ok(detached) if detached == row.head => Ok(()),
                Ok(detached) => Err(anyhow::anyhow!(
                    "checkout detached {} instead of {}",
                    detached.as_str(),
                    row.head.as_str()
                )),
                Err(err) => Err(err),
            };
            let _ = tx.send(result);
            queued.fetch_add(1, Ordering::SeqCst);
        });
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PostedHeadsFile {
    heads: BTreeSet<String>,
}

fn load_covered(path: &Path) -> Result<BTreeSet<HeadSha>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let file: PostedHeadsFile = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    file.heads
        .into_iter()
        .map(|sha| HeadSha::parse(&sha))
        .collect()
}

fn write_covered(path: &Path, covered: &BTreeSet<HeadSha>) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let mut file = tempfile::Builder::new()
        .prefix(".posted-heads.")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create a temp file in {}", parent.display()))?;
    let heads = covered
        .iter()
        .map(|sha| sha.as_str().to_string())
        .collect::<BTreeSet<_>>();
    serde_json::to_writer_pretty(&mut file, &PostedHeadsFile { heads })
        .context("failed to encode posted heads")?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    file.into_temp_path()
        .persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
fn posted_heads(path: &Path) -> Result<BTreeSet<String>> {
    Ok(load_covered(path)?
        .into_iter()
        .map(|sha| sha.as_str().to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_limit::NoTaskLimit;
    use anyhow::{Result, anyhow};
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use std::{
        collections::HashSet,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct Edge {
        inner: Arc<EdgeInner>,
    }

    struct EdgeInner {
        pr_head: Mutex<HeadSha>,
        detached: Mutex<Vec<HeadSha>>,
        fail: Mutex<HashSet<HeadSha>>,
        entered: AtomicUsize,
        runs: AtomicUsize,
        prepare_gate: Option<Arc<Notify>>,
        codex_gate: Option<Arc<Notify>>,
    }

    impl Edge {
        fn new(pr_head: HeadSha) -> Self {
            Self {
                inner: Arc::new(EdgeInner {
                    pr_head: Mutex::new(pr_head),
                    detached: Mutex::new(Vec::new()),
                    fail: Mutex::new(HashSet::new()),
                    entered: AtomicUsize::new(0),
                    runs: AtomicUsize::new(0),
                    prepare_gate: None,
                    codex_gate: None,
                }),
            }
        }

        fn block_prepare(mut self) -> Self {
            let inner = Arc::get_mut(&mut self.inner).expect("edge is not shared yet");
            inner.prepare_gate = Some(Arc::new(Notify::new()));
            self
        }

        fn block_codex(mut self) -> Self {
            let inner = Arc::get_mut(&mut self.inner).expect("edge is not shared yet");
            inner.codex_gate = Some(Arc::new(Notify::new()));
            self
        }

        fn fail_sha(mut self, sha: HeadSha) -> Self {
            let inner = Arc::get_mut(&mut self.inner).expect("edge is not shared yet");
            inner.fail.lock().unwrap().insert(sha);
            self
        }

        fn set_pr_head(&self, sha: HeadSha) {
            *self.inner.pr_head.lock().unwrap() = sha;
        }

        fn detached(&self) -> Vec<HeadSha> {
            self.inner.detached.lock().unwrap().clone()
        }

        fn entered(&self) -> usize {
            self.inner.entered.load(Ordering::SeqCst)
        }

        fn runs(&self) -> usize {
            self.inner.runs.load(Ordering::SeqCst)
        }

        fn codex_gate(&self) -> Arc<Notify> {
            Arc::clone(
                self.inner
                    .codex_gate
                    .as_ref()
                    .expect("codex gate was not armed"),
            )
        }
    }

    #[async_trait]
    impl ReviewHead for Edge {
        async fn review(&self, row: &ListedPull, rev: &CheckoutRev) -> Result<HeadSha> {
            let _ = row;
            self.inner.entered.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.inner.prepare_gate {
                gate.notified().await;
            }
            let sha = match rev {
                CheckoutRev::Commit(sha) => sha.clone(),
                CheckoutRev::Named(_) => self.inner.pr_head.lock().unwrap().clone(),
            };
            if self.inner.fail.lock().unwrap().contains(&sha) {
                return Err(anyhow!("checkout failed"));
            }
            self.inner.detached.lock().unwrap().push(sha.clone());
            self.inner.runs.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.inner.codex_gate {
                gate.notified().await;
            }
            Ok(sha)
        }
    }

    #[tokio::test]
    async fn posted_head_is_not_reviewed_again_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_a = sha('a');
        std::fs::write(&path, format!(r#"{{"heads":["{}"]}}"#, sha_a.as_str())).unwrap();
        let edge = Edge::new(sha_a.clone());
        let mut reviews = open_reviews(&path, 1, &edge);

        let outcome = reviews.submit_head(listed("dionysuzx", sha_a, 1, ListSource::Repository));

        assert_eq!(
            (outcome, edge.runs()),
            (SubmitOutcome::Skipped, 0),
            "I1: a head with a posted Codex run was reviewed again after restart"
        );
    }

    #[tokio::test]
    async fn checkout_uses_the_sha_discovery_selected_after_the_ref_moves() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_a = sha('a');
        let sha_b = sha('b');
        let edge = Edge::new(sha_a.clone());
        edge.set_pr_head(sha_b);
        let mut reviews = open_reviews(&path, 1, &edge);

        assert_eq!(
            reviews.submit_head(listed(
                "dionysuzx",
                sha_a.clone(),
                1,
                ListSource::Repository
            )),
            SubmitOutcome::Started
        );
        wait_for_queue(&reviews).await;
        reviews.drain().await;

        assert_eq!(
            (
                edge.detached(),
                posted_heads(&path).unwrap().into_iter().collect::<Vec<_>>()
            ),
            (vec![sha_a.clone()], vec![sha_a.as_str().to_string()]),
            "I2: checkout or the coverage record followed the moved PR ref"
        );
    }

    #[tokio::test]
    async fn completing_worker_report_is_applied_before_a_second_admit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_a = sha('a');
        let edge = Edge::new(sha_a.clone());
        let mut reviews = open_reviews(&path, 2, &edge);
        let row = listed("dionysuzx", sha_a, 1, ListSource::Repository);

        assert_eq!(reviews.submit_head(row.clone()), SubmitOutcome::Started);
        wait_for_queue(&reviews).await;
        let second = reviews.submit_head(row);

        assert_eq!(
            (second, edge.runs()),
            (SubmitOutcome::Skipped, 1),
            "I3: a queued posted report did not stop the next admit"
        );
    }

    #[tokio::test]
    async fn covered_head_does_not_take_the_only_slot_from_a_mention() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_a = sha('a');
        std::fs::write(&path, format!(r#"{{"heads":["{}"]}}"#, sha_a.as_str())).unwrap();
        let edge = Edge::new(sha_a.clone());
        let mut reviews = open_reviews(&path, 1, &edge);

        let covered = reviews.submit_head(listed("dionysuzx", sha_a, 1, ListSource::Repository));
        let in_flight_after_covered = reviews.in_flight();
        let mention = reviews.begin_mention("mention-1");

        assert_eq!(
            (covered, in_flight_after_covered, mention.is_some()),
            (SubmitOutcome::Skipped, 0, true),
            "a covered head took the only slot a mention was waiting on"
        );
    }

    #[tokio::test]
    async fn stalled_checkout_does_not_block_the_poll_and_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let edge = Edge::new(sha('a')).block_prepare();
        let mut reviews = open_reviews(&path, 1, &edge);
        let rows = vec![
            listed("dionysuzx", sha('a'), 1, ListSource::Repository),
            listed("dionysuzx", sha('b'), 2, ListSource::Repository),
        ];

        let timed = tokio::time::timeout(
            Duration::from_millis(250),
            reviews.poll_listed(rows, &policy(), now()),
        )
        .await;

        assert!(
            timed.is_ok() && edge.entered() <= 1,
            "I5: stalled checkout blocked the poll or started more than max_concurrent_requests (returned={}, entered={})",
            timed.is_ok(),
            edge.entered(),
        );
    }

    #[tokio::test]
    async fn public_author_pages_follow_the_flag() {
        let first = vec![listed(
            "ada",
            sha('a'),
            1,
            ListSource::PublicAuthor {
                login: "ada".to_string(),
            },
        )];
        let second = vec![listed(
            "ada",
            sha('b'),
            2,
            ListSource::PublicAuthor {
                login: "ada".to_string(),
            },
        )];

        let off_pages = Arc::new(Mutex::new(Vec::new()));
        let off = collect(false, &off_pages, &first, &second).await;
        assert_eq!(
            *off_pages.lock().unwrap(),
            vec![1],
            "I6: Off fetched a later page"
        );
        assert!(
            !off.iter().any(|row| row.head == sha('b')),
            "I6: Off returned a head from a later page"
        );

        let on_pages = Arc::new(Mutex::new(Vec::new()));
        let on = collect(true, &on_pages, &first, &second).await;
        assert!(
            on.iter().any(|row| row.head == sha('b')),
            "I6: public-author search did not follow the page switch"
        );
        assert!(
            on_pages.lock().unwrap().contains(&2),
            "I6: public-author search did not request the later page"
        );
    }

    #[tokio::test]
    async fn dropped_row_does_not_end_a_full_page() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_page = Arc::clone(&seen);
        let rows = pages(true, 2, move |page| {
            let seen_page = Arc::clone(&seen_page);
            async move {
                seen_page.lock().unwrap().push(page);
                let items = match page {
                    1 => vec![listed(
                        "ada",
                        sha('a'),
                        1,
                        ListSource::PublicAuthor {
                            login: "ada".to_string(),
                        },
                    )],
                    2 => vec![listed(
                        "ada",
                        sha('b'),
                        2,
                        ListSource::PublicAuthor {
                            login: "ada".to_string(),
                        },
                    )],
                    _ => Vec::new(),
                };
                let received = if page == 1 { 2 } else { items.len() };
                Ok(Fetched { items, received })
            }
        })
        .await
        .unwrap();

        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
        assert!(rows.iter().any(|row| row.head == sha('b')));
    }

    #[tokio::test]
    async fn later_page_failure_keeps_pages_already_read() {
        let rows = pages(true, 1, |page| async move {
            if page == 1 {
                let items = vec![listed(
                    "ada",
                    sha('a'),
                    1,
                    ListSource::PublicAuthor {
                        login: "ada".to_string(),
                    },
                )];
                let received = items.len();
                Ok(Fetched { items, received })
            } else {
                Err(anyhow!("search page is not available"))
            }
        })
        .await
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].head, sha('a'));
    }

    #[tokio::test]
    async fn one_candidate_checkout_failure_does_not_stop_the_other() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let bad = sha('a');
        let edge = Edge::new(bad.clone()).fail_sha(bad.clone());
        let mut reviews = open_reviews(&path, 2, &edge);
        let rows = vec![
            listed("dionysuzx", bad, 1, ListSource::Repository),
            listed("dionysuzx", sha('b'), 2, ListSource::Repository),
        ];

        let result = reviews.poll_listed(rows, &policy(), now()).await;

        assert_eq!(
            (result.is_ok(), edge.runs(), edge.entered()),
            (true, 1, 2),
            "I7: one candidate's checkout failure stopped the other"
        );
    }

    #[tokio::test]
    async fn clean_shutdown_drains_the_posted_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_a = sha('a');
        let edge = Edge::new(sha_a.clone()).block_codex();
        let release = edge.codex_gate();
        let mut reviews = open_reviews(&path, 1, &edge);

        assert_eq!(
            reviews.submit_head(listed(
                "dionysuzx",
                sha_a.clone(),
                1,
                ListSource::Repository
            )),
            SubmitOutcome::Started
        );
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            release.notify_one();
        });
        let drained = tokio::time::timeout(Duration::from_secs(2), reviews.drain()).await;

        assert!(drained.is_ok(), "I8: drain hung");
        assert!(
            posted_heads(&path).unwrap().contains(sha_a.as_str()),
            "I8: clean shutdown returned before the posted head was recorded"
        );
    }

    #[tokio::test]
    async fn out_of_scope_author_is_not_reviewed_or_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("posted-heads.json");
        let sha_c = sha('c');
        let edge = Edge::new(sha_c.clone());
        let mut reviews = open_reviews(&path, 1, &edge);
        let row = listed("stranger", sha_c, 9, ListSource::Repository);

        reviews
            .poll_listed(vec![row], &policy(), now())
            .await
            .expect("I7 owns candidate errors; an out-of-scope row is not one");

        assert_eq!(
            (edge.runs(), posted_heads(&path).unwrap().is_empty()),
            (0, true),
            "I9: an out-of-scope author was reviewed or recorded"
        );
    }

    async fn collect(
        continuous: bool,
        seen: &Arc<Mutex<Vec<u32>>>,
        first: &[ListedPull],
        second: &[ListedPull],
    ) -> Vec<ListedPull> {
        let seen = Arc::clone(seen);
        let first = first.to_vec();
        let second = second.to_vec();
        pages(continuous, 1, move |page| {
            let seen = Arc::clone(&seen);
            let first = first.clone();
            let second = second.clone();
            async move {
                seen.lock().unwrap().push(page);
                let items = match page {
                    1 => first,
                    2 => second,
                    _ => Vec::new(),
                };
                let received = items.len();
                Ok(Fetched { items, received })
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_queue<R>(reviews: &Reviews<R>)
    where
        R: ReviewHead + Clone + 'static,
    {
        let wait = tokio::time::timeout(Duration::from_secs(2), async {
            while reviews.reports_queued() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(wait.is_ok(), "worker did not queue a report");
    }

    fn open_reviews(path: &Path, max: usize, edge: &Edge) -> Reviews<Edge> {
        Reviews::open(path, max, Arc::new(NoTaskLimit), edge.clone()).unwrap()
    }

    fn listed(author: &str, head: HeadSha, number: u64, source: ListSource) -> ListedPull {
        ListedPull {
            pr: PullRequest {
                owner: "o".to_string(),
                repo: "r".to_string(),
                number,
                author: author.to_string(),
                api_url: format!("https://api.github.com/repos/o/r/pulls/{number}"),
                html_url: format!("https://github.com/o/r/pull/{number}"),
                clone_url: "https://github.com/o/r.git".to_string(),
            },
            head,
            source,
            requested_teams: Vec::new(),
            updated_at: None,
        }
    }

    fn policy() -> AutoReviewPolicy {
        AutoReviewPolicy {
            bot_login: "maid-bot".to_string(),
            repository_accounts: HashSet::from(["dionysuzx".to_string()]),
            public_accounts: HashSet::new(),
            teams: Vec::new(),
            team_window: ChronoDuration::hours(24),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-27T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sha(label: char) -> HeadSha {
        HeadSha::parse(&label.to_string().repeat(40)).unwrap()
    }
}
