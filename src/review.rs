//! Red harness for continuous review. Not the feature.
//!
//! The types match `docs/continuous-review-design.md`. The bodies are the
//! naive stand-in the invariant tests reject: a posted head is still started,
//! checkout follows the moving PR ref, a queued result is not applied before
//! the next admit, covered heads take a slot, the poll awaits checkout and
//! aborts the batch on one error, paging ignores the flag, drain does not
//! wait, and every listed author is in scope. Replacing these bodies is the
//! implementation. Weakening the tests is not.

use anyhow::{Result, bail};
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
use tokio::sync::mpsc;

use crate::{
    domain::{PullRequest, TeamSlug},
    task_limit::TaskStartRecorder,
};

/// Full GitHub commit object name. Lowercase hex, length 40.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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

/// One open pull from a listing page. A wire row with no parseable SHA never
/// becomes a value of this type.
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

/// Scope inputs for `in_scope`. The naive poll does not read them.
#[derive(Clone, Debug)]
pub struct AutoReviewPolicy {
    pub bot_login: String,
    pub repository_accounts: HashSet<String>,
    pub public_accounts: HashSet<String>,
    pub teams: Vec<TeamSlug>,
    pub team_window: ChronoDuration,
}

/// Naive: `continuous` and `page_size` are ignored. Only page 1 is fetched.
///
/// The real function fetches page 1, and when `continuous` is set keeps
/// fetching until a page is shorter than `page_size`. `page_size` is the
/// `per_page` the caller already sent (50 for pulls, 100 for search).
pub async fn pages<T, F, Fut>(continuous: bool, page_size: usize, mut fetch: F) -> Result<Vec<T>>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<Vec<T>>>,
{
    let _ = (continuous, page_size);
    fetch(1).await
}

/// Commit is the selected object. Named is a moving ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckoutRev {
    Commit(HeadSha),
    Named(String),
}

/// Edge stand-in for `Worktrees::prepare`. Production checkout grows a
/// `CheckoutRev` argument; this trait exists so the red tests can see which
/// object was detached without rewriting every current `prepare` call.
#[async_trait]
pub trait ChecksOut: Send + Sync {
    async fn checkout(&self, rev: &CheckoutRev) -> Result<HeadSha>;
}

/// Edge stand-in for one Codex run plus its comment post. `Ok` means the
/// comment was posted. The real runner still returns the response body.
#[async_trait]
pub trait CodexExec: Send + Sync {
    async fn run(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOutcome {
    Started,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionKey(String);

impl MentionKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// The owner already knows which SHA it started. The naive owner never reads these.
#[allow(dead_code)]
enum WorkerReport {
    Posted,
    Failed,
}

/// Not `Clone`. The poll task is the only holder of `&mut Reviews`.
/// Workers get a report sender and the cloneable edges, not this struct.
pub struct Reviews<C, X> {
    path: PathBuf,
    max_concurrent_requests: usize,
    in_flight: usize,
    preparer: C,
    coder: X,
    reports_tx: mpsc::UnboundedSender<WorkerReport>,
    /// Never read. Applying a report is the implementation's job.
    #[allow(dead_code)]
    reports: mpsc::UnboundedReceiver<WorkerReport>,
    reports_queued: Arc<AtomicUsize>,
    /// Never read. The task ledger is consulted only after a head is admitted.
    #[allow(dead_code)]
    task_starts: Arc<dyn TaskStartRecorder>,
}

impl<C, X> Reviews<C, X>
where
    C: ChecksOut + Clone + 'static,
    X: CodexExec + Clone + 'static,
{
    pub fn open(
        path: impl Into<PathBuf>,
        max_concurrent_requests: usize,
        task_starts: Arc<dyn TaskStartRecorder>,
        preparer: C,
        coder: X,
    ) -> Result<Self> {
        let (reports_tx, reports) = mpsc::unbounded_channel();
        Ok(Self {
            path: path.into(),
            max_concurrent_requests,
            in_flight: 0,
            preparer,
            coder,
            reports_tx,
            reports,
            reports_queued: Arc::new(AtomicUsize::new(0)),
            task_starts,
        })
    }

    /// Naive: ignore the posted-head file and any result already queued,
    /// take a slot, and check out `pull/N/head` rather than `row.head`.
    pub fn submit_head(&mut self, row: ListedPull) -> SubmitOutcome {
        let _queued = self.reports_queued.load(Ordering::SeqCst);
        let _pin = &row.head;
        self.in_flight += 1;
        let rev = CheckoutRev::Named(format!("pull/{}/head", row.pr.number));
        self.launch(rev);
        SubmitOutcome::Started
    }

    /// Naive: the slot count is a counter `submit_head` already bumped, so a
    /// covered head can fill the only slot before this runs.
    pub async fn with_mention<F, Fut>(&mut self, key: MentionKey, observe: F) -> SubmitOutcome
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<()>>>,
    {
        let _ = key;
        if self.in_flight >= self.max_concurrent_requests {
            return SubmitOutcome::Skipped;
        }
        self.in_flight += 1;
        match observe().await {
            Ok(Some(())) => SubmitOutcome::Started,
            _ => {
                self.in_flight -= 1;
                SubmitOutcome::Skipped
            }
        }
    }

    /// Naive: await every checkout on this task, and let one `Err` abort the
    /// rest. Scope is not consulted.
    pub async fn poll_listed(
        &mut self,
        rows: Vec<ListedPull>,
        policy: &AutoReviewPolicy,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let _ = (policy, now);
        for row in rows {
            let sha = self
                .preparer
                .checkout(&CheckoutRev::Commit(row.head.clone()))
                .await?;
            self.coder.run().await?;
            remember_checked_out(&self.path, &sha)?;
        }
        Ok(())
    }

    /// Naive: return while in-flight reviews have not recorded anything.
    /// Yielding here would hide that, so this function does not await.
    #[allow(clippy::unused_async)]
    pub async fn drain(&mut self) {
        let _ = self.path.as_path();
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn reports_queued(&self) -> usize {
        self.reports_queued.load(Ordering::SeqCst)
    }

    fn launch(&self, rev: CheckoutRev) {
        let preparer = self.preparer.clone();
        let coder = self.coder.clone();
        let tx = self.reports_tx.clone();
        let queued = Arc::clone(&self.reports_queued);
        let path = self.path.clone();
        tokio::spawn(async move {
            let prepared = match preparer.checkout(&rev).await {
                Ok(sha) => sha,
                Err(_) => {
                    let _ = tx.send(WorkerReport::Failed);
                    queued.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            if coder.run().await.is_err() {
                let _ = tx.send(WorkerReport::Failed);
                queued.fetch_add(1, Ordering::SeqCst);
                return;
            }
            let _ = remember_checked_out(&path, &prepared);
            let _ = tx.send(WorkerReport::Posted);
            queued.fetch_add(1, Ordering::SeqCst);
        });
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PostedHeadsFile {
    heads: BTreeSet<String>,
}

fn posted_heads(path: &Path) -> Result<BTreeSet<String>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let body = std::fs::read_to_string(path)?;
    let file: PostedHeadsFile = serde_json::from_str(&body)?;
    Ok(file.heads)
}

fn remember_checked_out(path: &Path, sha: &HeadSha) -> Result<()> {
    let mut heads = posted_heads(path)?;
    heads.insert(sha.as_str().to_string());
    let body = serde_json::to_vec_pretty(&PostedHeadsFile { heads })?;
    std::fs::write(path, body)?;
    Ok(())
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
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
    impl ChecksOut for Edge {
        async fn checkout(&self, rev: &CheckoutRev) -> Result<HeadSha> {
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
            Ok(sha)
        }
    }

    #[async_trait]
    impl CodexExec for Edge {
        async fn run(&self) -> Result<()> {
            self.inner.runs.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.inner.codex_gate {
                gate.notified().await;
            }
            Ok(())
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
        let observed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&observed);

        let covered = reviews.submit_head(listed("dionysuzx", sha_a, 1, ListSource::Repository));
        let in_flight_after_covered = reviews.in_flight();
        let mention = reviews
            .with_mention(MentionKey::new("mention-1"), || {
                flag.store(true, Ordering::SeqCst);
                async { Ok(Some(())) }
            })
            .await;

        assert_eq!(
            (
                covered,
                in_flight_after_covered,
                mention,
                observed.load(Ordering::SeqCst)
            ),
            (SubmitOutcome::Skipped, 0, SubmitOutcome::Started, true),
            "I4: a covered head took the only slot a mention was waiting on"
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
                Ok(match page {
                    1 => first,
                    2 => second,
                    _ => Vec::new(),
                })
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_queue<C, X>(reviews: &Reviews<C, X>)
    where
        C: ChecksOut + Clone + 'static,
        X: CodexExec + Clone + 'static,
    {
        let wait = tokio::time::timeout(Duration::from_secs(2), async {
            while reviews.reports_queued() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(wait.is_ok(), "worker did not queue a report");
    }

    fn open_reviews(path: &Path, max: usize, edge: &Edge) -> Reviews<Edge, Edge> {
        Reviews::open(path, max, Arc::new(NoTaskLimit), edge.clone(), edge.clone()).unwrap()
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
