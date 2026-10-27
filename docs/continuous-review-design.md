# Continuous review

Design only. The bodies in `src/review.rs` are a naive stand-in so the invariant tests can fail. They are not the feature, and nothing in the daemon calls them (`review` is compiled for tests only).

## 1. Source of truth

Maid does not durably record "this commit has a posted Codex run."

What exists on `main`:

- A bot `+1` on the PR issue (`HANDLED_REACTION`). It is per pull request, not per SHA. `eyes` is written at start and never read. Auto-review never lists issue comments, and `post_json` throws away the created comment id. The comment body is the raw Codex response, so it is not an index.
- `pending-handled-markers.json` (`pull_request_html_urls`). It is a retry buffer for that `+1`, written only after a successful comment, keyed by HTML URL. A mention `+1` does not suppress auto-review, and the reverse is also true.
- The task-start ledger, which counts starts in a 24 hour window. It does not name a commit.
- No `head` field on `PullRequest` or `ApiPullRequest`. Checkout fetches `pull/{n}/head` and detaches `FETCH_HEAD`, so the reviewed object is whatever the ref points at when `git fetch` runs.

The `+1` cannot be the source of truth: it has no SHA, so importing it would either suppress a commit this record never saw or write a SHA this process did not review. Reading comment bodies or commit statuses to answer coverage would be a GitHub call per candidate, which is the cost covered heads are not allowed to pay, and it brings back the reaction-lag split (a posted review whose marker has not landed yet).

**The record is `~/.maid/posted-heads.json`.** One JSON object, `{ "heads": ["<40 lowercase hex>", ...] }`, a set of full SHAs. No PR URL, no comment id, no session id. Session ids stay in the log. The file is written by one atomic rename, the same temp-file, `sync`, rename the other ledgers use. Missing file loads as an empty set. Unreadable or unparseable file fails `Reviews::open`; the process does not start, and the file is left untouched. A corrupt file is not "nothing posted."

`InFlight` is not in the file. A crash drops it. That is the accepted gap: a comment that was posted and not yet renamed may be reviewed once by the next process. There is no claim row to expire, so there is no new clock and no new knob.

On first deploy, open tracked heads that already carry a bot `+1` are reviewed once. Closed pulls are not listed, so they are not reviewed and not recorded.

### Delete list

Removed in the same change that lands the owner. Not removed on this branch: the daemon still runs the old poll, and deleting it now would be the feature.

Auto-review only. Mention handling stays.

- `GithubClient::pr_state`, `mark_pr_started`, `mark_pr_handled`, `mark_pull_request_html_url_handled`, and the helpers `pr_has_reaction` / `add_pr_reaction` on the pull-request path.
- `PendingHandledMarker::PullRequest`, `pull_request_html_urls`, `PendingHandledMarker::for_pull_request`. On read, serde drops a leftover `pull_request_html_urls` field. The next write omits it. Those URLs are not imported as coverage.
- `plan_auto_review`, `AutoReviewObservation`, `AutoReviewAction`, and `ReviewState` on the auto-review path. `authorizing_team` moves into `review.rs`.
- `ConcurrentMaid`, `WorkQueue`, `WorkReservation`, and the inline auto-review finish inside `Maid::run_once`. One poll remains.
- `open_pull_requests` and `open_public_pull_requests_by_author` as list-all methods, and `MAX_SEARCH_RESULTS`.
- Auto-review checkout of `pull/N/head` and `FETCH_HEAD`.
- `poller.abort()` as the way a review ends.
- Tests that lock a reaction or a pending marker as auto-review suppression: `auto_review_does_not_restart_finished_pr_during_reaction_lag`, `retries_pending_auto_review_marker_without_rerunning_codex`, `retries_pending_auto_review_marker_without_open_pull_request`, `pending_marker_is_marked_without_starting_task`, and `skips_opened_pr_with_durable_handled_marker_after_restart`. Those five are already deleted on this branch. Their replacement is `posted_head_is_not_reviewed_again_after_restart`. The rest of the auto-review tests in `maid.rs` still describe the old poll, including once-per-URL and `+1` written after a successful review. They are replaced when the shell lands; updating them now, before the shell exists, would only make them fail to compile.

Kept: mention markers, mention eyes and `+1`, `mention_state`, mention reaction-lag tests, the observed-notification ledger, the task-start ledger, the per-repo flock, the daemon pid lock, team rules, the public-visibility filter, the bot-authored skip, account subset checks, and "empty `auto_review_repos` still runs public search." The updated-page listing still does not authorize authors by itself.

`continuous_review` does not select a legacy path. With the flag off, coverage is still per head. The flag's only meaning is whether discovery walks past the first page.

## 2. Arena

The decision is who owns "is this head already covered, and is a review of it in flight?"

Four candidates:

| | Shape | Shared mutable coverage |
| --- | --- | --- |
| (a) | One owner on the poll task. Workers report a flight id back on a channel. They cannot see the map. | None. The map is `&mut` on one task. |
| (b) | Pure transitions `Absent → InFlight → Posted`, and the only shared write is one atomic record per head. | The worker and the next process both write that record. |
| (c) | GitHub is the record: comment body, commit status, or a `+1` keyed by SHA, read at admit time. | The poll and GitHub's reaction lag. |
| (d) | The ledger beside today's queue. Reserve, then re-check coverage, then run. Drafts #43, #44, and #45. | Ledger, in-memory reservations, semaphore permits, and the moving PR ref. |

(a) is the winner. (b), (c), and (d) lose.

(b) loses because a crash between claim and post leaves a row the next process must interpret. Treating the claim as posted suppresses a review that never happened. Treating it as absent means the claim was not worth writing. Expiring it needs a staleness rule, which is a new knob or a stuck head. The caller's surface grows claim, heartbeat, and release. The occupancy table below is private memory. The file never stores `InFlight`.

(c) loses because a listing payload is not that record. Every covered head pays a GitHub read, which breaks the "covered heads cost nothing" rule, and two stores drift the way the reaction and the pending marker already did. A `+1` still has no SHA.

(d) loses because another check beside `WorkQueue` does not remove the race. Coverage was a read on the poll, a reservation in the queue, and a write on the worker, against a ref that moves between them. Rechecking after reserve adds a read. It does not give the map and the slot one owner, and it does not make the checked-out object the recorded SHA. The blocking findings on those drafts (a second Codex run after a record-write failure, a checkout awaited on the poll, prepares with no permit, `drain` waiting while `just stop` exits at 5 seconds, covered heads occupying the slot a mention needs, public search ignoring the page switch, one head-lookup error aborting the batch) are one tangle with a patch each.

Both arena seats were Grok 4.7. Claude, GPT, and Gemini were not available in this environment, so there was no cross-family judge. The two seats still disagreed, and they agreed on the part that matters: both picked (a), unprompted, and both rejected (b), (c), and a recheck after reserve.

Inside (a) they diverged. The pick is the second seat's owner, with two pieces from the first.

Taken from the second seat:

- Occupancy is one map, `HeadSha → { InFlight, AwaitingRecord, Posted }`, and `next_occupancy` is a pure table. `Failed` does not clear `AwaitingRecord` or `Posted`. `CommentPosted` moves `InFlight` to `AwaitingRecord` before the rename. `RecordDurably` moves only `AwaitingRecord` to `Posted`.
- `submit_head` applies every queued report, then reads occupancy, then the slot, then the task ledger, then inserts `InFlight`, with no await between the read and the insert.
- The file is one JSON set, renamed atomically. A missing file is empty. A corrupt file fails `open`.
- `with_mention` does not read the thread when no slot is free, and it holds the slot across the read.
- Clean shutdown does not cancel Codex. `just stop` keeps the 5 second give-up unless the process has written a `{pid}.draining` sibling; then the same 0.25 second poll continues until the pid is gone. No new numeric cap, no `SIGKILL`.

Taken from the first seat:

- The worker report is `Posted(FlightId)` / `Failed(FlightId)`. The owner maps the id to the SHA it admitted. A report with a SHA field lets the worker name the commit, which reopens the moving-ref bug. `FlightId` on the wire, the occupancy table as the core.
- The slot count is derived: `in_use = in_flight heads + in_flight mentions`, compared with `max_concurrent_requests`. A semaphore beside the map is a second mutable resource. The drafts lost permits and reservations because those were two representations. `AwaitingRecord` and `Posted` do not count. A covered head never inserts, so it cannot change the count.

Rejected from the first seat: cancelling Codex on `SIGTERM` so the existing 5 second loop usually succeeds. That does not let an in-flight review finish recording. A hung post can still outlast 5 seconds either way. The sibling is what "wait for the drain" means without a new knob. Also rejected: applying reports only after the whole candidate loop. A report that arrived before the next admit has to be applied at the start of that admit, or the admit is a stale read. Also rejected: a jsonl append as a new format. The rename matches the ledgers already on disk, and a corrupt file fails closed instead of loading a prefix.

`justfile` is edited when the shell lands. It is not edited on this branch.

## 3. Design

### What a reader has to hold

One question, asked in one place: given this SHA, this map, and this slot count, does a review start? The answer is `next_occupancy` plus the count. Time, git, GitHub, Codex, and the file sit in the shell. A worker cannot write the map. Two polls cannot run, because that would require two `&mut Reviews`.

Invalid combinations the types cannot express:

- A coverage row with no SHA, or a SHA that was not the one the owner admitted.
- An in-flight head that is also posted. One SHA, one occupancy.
- An auto-review checkout of `pull/N/head` or `FETCH_HEAD`. The commit arm has no such input.
- A later discovery page fetched by anything other than `walk_pages`. `Page`'s cursor has no getter.
- An out-of-scope row passed to `submit_head`. The argument is `AuthorizedHead`, and `authorize` is the only constructor.
- A second overlapping poll. `Reviews` is not `Clone`.

### Domain

```rust
pub struct HeadSha(String); // 40 lowercase hex, parsed at the listing and at the file

pub struct ListedPull {
    pub pr: PullRequest,
    pub head: HeadSha,
    pub source: ListSource, // Repository | TeamActivity | PublicAuthor { login }
    pub requested_teams: Vec<TeamSlug>,
    pub updated_at: Option<DateTime<Utc>>,
}

pub struct AutoReviewPolicy { /* bot, repository accounts, public accounts, teams, window */ }

pub struct AuthorizedHead { /* private: sha, pr, author */ }

pub fn authorize(row: ListedPull, policy: &AutoReviewPolicy, now: DateTime<Utc>) -> Option<AuthorizedHead>;

pub fn dedup_listed(rows: Vec<ListedPull>, policy: &AutoReviewPolicy) -> Vec<ListedPull>;
```

`HeadSha::parse` accepts 40 lowercase hex digits and nothing else. Short SHAs and uppercase are rejected, so one object has one key.

`authorize` is pure. No ledger, no GitHub, no clock read of its own (`now` is an argument).

- Bot author → `None`, even when a team is requested.
- `TeamActivity` → `authorizing_team` only. It is never an allowed author.
- `PublicAuthor` → the searched login is a public account and it matches the row's author.
- `Repository` → the author is a repository account, or `authorizing_team`.
- Anything else → `None`.

`None` is not submitted and not recorded.

`dedup_listed` keys on the lowercased HTML URL. The first row wins, except a later author-allowed row replaces the whole `ListedPull`, including its SHA, teams, and `updated_at`. `TeamActivity` is never that replacing row, so it cannot wipe a repository row's teams. The winner's SHA is the SHA. The two rows are not merged field by field, which is how a public row today can drop a listing SHA.

### Paging

```rust
pub enum ContinuousReview { Off, On } // the config bool, parsed once at process start

pub enum PageRequest { First, After(PageCursor) }

pub struct Page<T> { /* items, next. next is private. */ }

pub async fn walk_pages<T, F, Fut>(mode: ContinuousReview, stop: &impl Fn() -> bool, fetch: F) -> Result<Vec<T>>;
```

`Off` fetches `First` and drops `next`. `On` follows `next` until it is absent or `stop` returns. `stop` is shutdown. A stopped walk keeps the rows already fetched. There is no page-count variant and no `MAX_SEARCH_RESULTS`. One function for newest-created, recently-updated, and public-author pages.

The client fetches exactly one page:

- `pull_page`: REST `GET /repos/{owner}/{repo}/pulls?state=open&sort=created|updated&direction=desc&per_page=50&page=N`. `direction=desc` stays. `head.sha` is parsed with `HeadSha::parse`. A row that does not parse is dropped; the rest of the page is kept. A full page is `Page::more` with the next page number. A short page is `Page::done`.
- `public_author_page`: one `POST /graphql` through the existing request path, so the rate limiter and the 403/429 retry apply. REST issue search has no head SHA and no repository visibility. A follow-up GET per row would bill every covered public head on every poll. The page carries both:

```graphql
search(query: "is:pr is:open author:{login} sort:created-desc", type: ISSUE, first: 100, after: $after) {
  pageInfo { hasNextPage endCursor }
  nodes { ... on PullRequest { number url headRefOid author { login }
           repository { name url visibility owner { login } } } }
}
```

Keep a node only when `visibility` is `PUBLIC` and `headRefOid` parses. Other nodes are not candidates and not errors. `requested_teams` and `updated_at` stay empty on this source. `first: 100` is the existing search page size, private to the method, not a knob. `per_page=50` on the REST list is the existing page size, also not a knob. A transport failure, or a body with `errors` and no `data`, fails that source only. Parseable nodes in a successful `data` payload are kept.

Notifications keep their current windowed loop. They are not an auto-review discovery source.

### The decision

Private to the owner. One map, `HashMap<HeadSha, Occupancy>`.

```text
current          Admit             Failed          CommentPosted       RecordDurably
None             InFlight          None            None                None
InFlight         InFlight          None            AwaitingRecord      InFlight
AwaitingRecord   AwaitingRecord    AwaitingRecord  AwaitingRecord      Posted
Posted           Posted            Posted          Posted              Posted
```

`Admit` starts work only from `None`. `Failed` does not clear `AwaitingRecord` or `Posted`. `RecordDurably` changes a slot only from `AwaitingRecord`. `CommentPosted` is applied before the rename, so a failed rename leaves `AwaitingRecord` and this process will not start Codex again.

The slot count is not stored:

```text
in_use = (occupancy entries that are InFlight) + (mention keys)
start allowed when in_use < max_concurrent_requests
```

`AwaitingRecord` and `Posted` do not hold a slot. The review is finished; only the rename remains. A mention key is inserted before the thread read and removed if the read says there is no task, so "do not hydrate when the bucket is full" does not need a second resource.

### The shell

```rust
pub struct Reviews<C, X> { /* not Clone */ }

impl Reviews {
    pub fn open(path, max_concurrent_requests, task_starts, edges) -> Result<Self>;
    pub fn submit_head(&mut self, head: AuthorizedHead) -> SubmitOutcome;
    pub async fn with_mention(&mut self, key: MentionKey, observe) -> SubmitOutcome;
    pub async fn poll_listed(&mut self, rows: Vec<ListedPull>, policy: &AutoReviewPolicy, now: DateTime<Utc>) -> Result<()>;
    pub async fn drain(&mut self);
    pub fn in_flight(&self) -> usize;
}
```

`submit_head`, synchronous, no await between the occupancy read and the insert:

1. Apply every queued report. On `CommentPosted` (the report is `Posted(FlightId)`; the SHA is the one stored under that id), set `AwaitingRecord`, then try one rename of `Posted ∪ AwaitingRecord`. Success moves those SHAs to `Posted`. Failure leaves `AwaitingRecord`.
2. If shutdown is set, return `Skipped`.
3. If `Admit` does not produce a fresh `InFlight`, return `Skipped`. No slot, no task-ledger write, no checkout, no GitHub.
4. If `in_use >= max`, return `Skipped`. No task-ledger write.
5. `try_record_started`. `AtLimit` → `Skipped`. `Err` → `Failed`. Do not insert. The ledger's `AtLimit` arm prunes old timestamps and does not append a start, as it does today.
6. Insert `InFlight` under a new `FlightId`. That insert is the slot.
7. Spawn the one review. The spawn consumes the flight just created. There is no spawn that accepts a raw SHA.

`poll_listed` is the candidate loop: `dedup_listed`, then `authorize`, then `submit_head` for each `Some`. A `None` is counted skipped. A source `Err` from discovery is matched outside this function and does not `?` the poll. `submit_head` does not return `Err` for a candidate. The poll does not await checkout.

`with_mention` uses the same count. It applies queued reports first. If shutdown is set, the key is already in flight, or `in_use >= max`, it returns without calling `observe`. Otherwise it inserts the mention key, awaits `observe`, drops the key when the read returns `None` or errors, and on a real task calls `try_record_started` and the existing eyes reaction. Ledger refusal or an eyes failure drops the key and does not spawn. Success spawns the mention arm of the same review function.

`drain` stops further admits, waits until every in-flight flight has reported (a dropped task sends `Failed` from a guard, and the guard does not send `Failed` after `Posted`), applies those reports, tries one rename, and returns whether or not the rename succeeded. It does not loop on the rename. It does not cancel Codex.

The review function is private. Both arms check out, run Codex, post, and send a report. Neither arm reads or writes the map, the file, the task ledger, or the slot count.

```rust
pub enum CheckoutRev { Commit(HeadSha), Named(String) }
```

The head arm calls `checkout(CheckoutRev::Commit(pin))`: `git fetch --prune origin <sha>`, `git worktree add --detach <path> <sha>`, `git rev-parse HEAD` must equal the pin. The worktree path includes the SHA, so two heads of one pull do not share a directory. The repo flock is unchanged and still covers fetch and `worktree add` only. There is no fallback to `pull/N/head` and no detach of `FETCH_HEAD`. A mismatch is a checkout failure, not a record. The mention arm stays `CheckoutRev::Named(target.fetch_ref())`, which is today's ref.

`git fetch origin <full sha>` depends on GitHub serving a reachable object by SHA. The harness fakes that call. The first real fetch has to prove it. If it fails, the commit arm is wrong; the fix is not to detach `FETCH_HEAD`.

The worker's report carries `FlightId` only. The recorded SHA is the one the owner stored when it admitted that flight. Cleanup errors after a successful post are logged and do not change the report.

### Shutdown

On `SIGTERM` or ctrl-c the handler writes `{pid_path}.draining` by an atomic rename, without rewriting the pid file, then signals the poll to stop admitting and to `drain`. `main` joins that task. It does not call `poller.abort()`, and the 50ms sleep in front of the abort goes away with it. `DaemonLock`'s drop removes the sibling when it still owns the pid, the same way it removes the pid file.

`just stop` keeps one `SIGTERM` and the 0.25 second poll, and it still does not `SIGKILL`. If the pid is gone, stop exits 0. If the pid is alive and the sibling exists, the same poll continues until the pid is gone, then exits 0. If the pid is alive and the sibling does not exist, stop prints `maid did not stop after 5 seconds; pid still running: ...` and exits 1, which is today's give-up. `restart` and `update` already wait on `stop`, so they wait for the drain when the process has acknowledged, and they still refuse to start a replacement when stop exits 1.

A Codex that never returns holds the process, and therefore holds stop, after the sibling is written. That is the tradeoff for finishing the record. The poll itself is not blocked: the stalled checkout holds one slot.

### Modules

| Module | Owns | Does not own |
| --- | --- | --- |
| `src/review.rs` | occupancy, `next_occupancy`, `Reviews`, `walk_pages`, `authorize`, `dedup_listed`, the private review | wire JSON, git argv, the config file |
| `src/domain.rs` | `HeadSha`, `ListedPull`, `ListSource` once the shell lands | admission, paging policy |
| `src/github.rs` | one REST page, one GraphQL page, parse into `ListedPull` | loops over pages, coverage, slots |
| `src/worktree.rs` | `CheckoutRev`, flock, rev-parse, SHA-keyed path | which SHA to review |
| `src/main.rs` | open, signal, join `drain`, the draining sibling | the map |
| `justfile` | stop's sibling check | a new timeout |

On this branch the types live in `src/review.rs` under `#[cfg(test)]`, because the naive bodies must not be callable from the daemon. `ChecksOut` and `CodexExec` are the edge the tests drive. `FakeGithub`, `FakeWorktrees`, and `FakeCodex` are private to `maid.rs` and the production `prepare` takes no SHA, so those fakes cannot observe a pinned checkout without changing every current caller. When the shell lands, `ChecksOut` folds into `Worktrees::prepare` and `CodexExec` folds into `CodexRunner`. The tests stay at those edges. They do not mock `authorize` or `next_occupancy`.

### Invariants

| | | |
| --- | --- | --- |
| I1 | Structure | `Admit` runs against the one map loaded from the file at `open`. `Posted` stays `Posted`. A worker cannot delete it. A second poll would need a second `&mut Reviews`. The SHA absent from the file is the accepted restart gap; the map does not invent it. |
| I2 | Structure | `ListedPull` carries the discovery `HeadSha`. The head arm checks out `Commit` of that SHA and refuses to report when rev-parse differs. The report has no SHA field. The rename stores the SHA stored under the flight. `pull/N/head` and `FETCH_HEAD` are not inputs of that arm. |
| I3 | Structure | Apply reports, read occupancy, take the slot, write the task ledger, insert `InFlight`, in one synchronous stretch. A stale "uncovered" value is not stored and acted on later. The `Skipped` returns do not call `try_record_started`. |
| I4 | Structure | An occupied SHA returns before the slot count changes and before `try_record_started`. `submit_head` does not call GitHub. The SHA arrived on the listing page. Mentions use the same count, so a covered head cannot fill it. |
| I5 | Structure | A spawn exists only while an `InFlight` entry or a mention key holds a slot, and the count is compared with `max_concurrent_requests`. `poll_listed` does not await checkout. A stalled fetch holds one slot and, for that bare repo, the flock. Other repos proceed. Flock waiters already hold their own slots. |
| I6 | Structure | `walk_pages` is the only reader of `Page`'s cursor. `Off` fetches `First`. `On` follows until the source stops. Newest-created, recently-updated, and public-author listings call that function. The client methods fetch one page. The enum cannot express a separate public-search cap. |
| I7 | Structure for the missing retry layer; code for isolation | `Failed(FlightId)` clears that slot and nothing else. Nothing queues a delay. The next poll is the retry. Discovery matches each source `Result` and continues. The candidate loop does not use `?`. GitHub 403/429 retries stay inside the client. There is no head-lookup GET: a row with no parseable oid is dropped, and the rest of the page is kept. |
| I8 | Structure for the in-process half and for clean shutdown; code for the crash and for the 5 second give-up | `AwaitingRecord` is set before the rename, and `Failed` does not clear it, so a rename error does not start a second Codex run in this process. `drain` waits for every in-flight report and then flushes. `main` does not abort the poller. The file changes only by the rename, so a kill before the rename leaves the SHA out and the next process may review once. `just stop` exits 1 only when the sibling was never written; if the sibling exists, it waits until the pid is gone. The sibling is not a timeout. |
| I9 | Structure | `authorize` returns `None` for a bot, an unauthorized author, a public row whose author is not the searched account, and a team row outside the window or without a configured team. `None` has no `submit_head`. The record path accepts a posted flight only, and that flight is produced only by a spawned review. |
| I10 | This branch deletes the five suppression tests. The remaining auto-review tests still lock the old poll and are replaced with the shell. | |

### Diff against `main`, including tests

The production core is a few hundred lines: the owner and the table, the commit arm of checkout, one GraphQL page, the REST page adapter, `main` joining `drain`, the config bool, and the `just stop` sibling. `continuous_review` is `#[serde(default)]`, default false, so existing config files still parse.

The large diff is tests. `src/maid.rs` is about four thousand lines, mostly tests that lock one review per URL, `+1` suppression, permit-before-`pr_state`, and public search that always walks up to ten pages. Those tests move onto `poll_listed`, `submit_head`, and `drain`, or they are deleted with the path they describe. Expect on the order of 1500 to 3000 lines of diff, most of it that replacement. The size is the deletion of the second poll. Leaving `Maid::run_once`'s inline auto-review and `ConcurrentMaid` in place would keep two reviewers, which is how the braid comes back.

This branch is the design, the deletion of the five suppression tests, and `src/review.rs`. The naive bodies are smaller than the real owner on purpose. They exist so the tests fail.

### Harness

`src/review.rs` compiles only in tests. The nine tests call the signatures above and assert the invariant. The bodies do the naive thing:

- `open` ignores the file.
- `authorize` returns `Some` for every row.
- `submit_head` does not apply the queued report, bumps a counter, and checks out `pull/N/head`.
- `with_mention` treats that counter as the slot bucket.
- `poll_listed` awaits checkout on the poll task and uses `?`.
- `walk_pages` fetches one page for both `Off` and `On`.
- `drain` returns without waiting and without writing. It does not yield: a yield would let the blocked worker record and hide the missing wait.

The checkout and Codex edges record the SHA they were asked to detach, can fail one SHA, and can block. That is the git and Codex boundary, not a mock of the decision.

## 4. Failure matrix

"Die" means the process is killed immediately after the call returns.

- Posted-heads load, missing file: empty set, process starts. Die before any poll: nothing was reviewed.
- Posted-heads load, corrupt or unreadable: `open` returns `Err`, process does not start, file untouched.
- `notifications()` error: the poll returns `Err` before listing, no coverage write. Die: nothing new admitted.
- One repo's `pull_page` or one author's `public_author_page` error: that source adds no rows, the failure is counted, other sources and the mention-marker retry still run. Next poll tries the source. Die: no partial admission from the failed source.
- A listing row whose `head.sha` or `headRefOid` does not parse: the row is dropped, the rest of the page is kept, nothing is recorded. There is no separate head-lookup GET to 404.
- `authorize` returns `None`: no submit, no record, no slot.
- Occupied SHA, or `in_use >= max`: `Skipped`. No task-ledger start, no GitHub, no checkout.
- `try_record_started` returns `AtLimit`: `Skipped`, SHA not inserted. The ledger prunes old timestamps and does not append a start.
- `try_record_started` returns `Err`: `Failed`, SHA not inserted. If a timestamp landed anyway, one task slot is burned and the head is still uncovered, so a later poll can start it. Same ledger behavior as today.
- `git fetch`, `worktree add`, or rev-parse unequal to the pin: `Failed` for that flight, slot cleared, no comment, no record. Next poll retries. The task slot stays consumed. Die during fetch: no record, next process retries.
- Codex error: `Failed`, slot cleared, no comment, no record. Next poll retries. The task slot stays consumed. Die during Codex: no record, next process retries. The child is not `kill_on_drop`. `drain` waits for it on a clean shutdown.
- `post_pr_comment` error: `Failed`, slot cleared, no record. Next poll retries. Die during the POST, before a confirmed response: the comment may exist and the file does not; the next process may review once. Accepted.
- `post_pr_comment` succeeded and the report was applied: occupancy is `AwaitingRecord` before the rename. This process does not start Codex again.
- Rename succeeded: occupancy `Posted`. A later admit does not start. Die after the rename: the next process loads the SHA.
- Rename failed: stay `AwaitingRecord`. Later admits and `drain` retry one rename. No second Codex run. Die with the rename unfinished: the next process may review once.
- Worktree cleanup error after the report was sent: logged, report unchanged, the record still proceeds. Die during cleanup after the send: `drain` or the next admit still records.
- Worker panic or drop before a report: the guard sends `Failed`. The slot clears. Next poll retries. The guard does not send `Failed` after `Posted`.
- `drain`'s rename fails: the process still exits. The SHA is absent. The next process may review once. `drain` does not loop on the rename.
- Mention thread read error: the mention key is dropped, that notification counts failed, the poll continues.
- Mention eyes error after the task slot was taken: the key is dropped, the task slot stays consumed, next poll can retry, no Codex.
- Mention comment succeeded: the owner, on the poll task, writes the mention marker and then attempts the mention `+1`. A `+1` failure leaves the marker for the existing retry, which takes no slot and no task slot.
- Observed-notification ledger error, and the initial mention-marker load: still fail the poll, as they do today. They are not candidate failures.

### Accepted tradeoffs

- One extra Codex run per visible in-scope head after deploy. A historical `+1` has no SHA.
- Two open pulls with the same head SHA share one Codex run and one comment, on the pull admitted first. The key is the head.
- Public-author listing goes through GraphQL, so the SHA and the visibility arrive on the page. A token that cannot `POST /graphql` fails that author source only; the next poll retries it.
- `continuous_review = false` lists one page of public-author search. Today that search walks up to ten pages with the flag absent.
- `just stop` waits without a deadline after the sibling appears. A Codex that never returns keeps stop waiting. The 5 second exit remains the response to a process that never acknowledges.
- A corrupt posted-heads file refuses process start.
- `AwaitingRecord` lives only in memory. A crash does not leave a claim the next process must expire. The price is the one extra review already accepted.
- The task slot stays consumed when checkout, Codex, or the comment fails. The current ledger has no refund, and this design does not add one.
- A long `walk_pages` runs on the poll task and can delay the next mention admit. It takes no slot. Reviews already spawned keep running. A second discovery actor, or a page cap, would be a new knob and a second owner.
- Drafts are not filtered. They are not filtered today, and a draft becoming ready is why the team page sorts by `updated`.
- `github_notification_window_hours` remains both the team window and the notification window. Config is still read at process start only.

## Tests on this branch

| Test | Invariant | Naive failure |
| --- | --- | --- |
| `posted_head_is_not_reviewed_again_after_restart` | I1 | `open` ignores the file, `submit_head` returns `Started` |
| `checkout_uses_the_sha_discovery_selected_after_the_ref_moves` | I2 | checkout detaches the moved `pull/N/head` and records that SHA |
| `completing_worker_report_is_applied_before_a_second_admit` | I3 | the second admit does not apply the queued `Posted` flight |
| `covered_head_does_not_take_the_only_slot_from_a_mention` | I4 | the covered head bumps the only slot, so the mention is skipped |
| `stalled_checkout_does_not_block_the_poll_and_is_bounded` | I5 | `poll_listed` awaits the stalled checkout |
| `public_author_pages_follow_the_flag` | I6 | `On` still fetches one page; `Off` already stops at the first |
| `one_candidate_checkout_failure_does_not_stop_the_other` | I7 | `?` returns before the second candidate |
| `clean_shutdown_drains_the_posted_record` | I8 | `drain` returns before the blocked review records the SHA |
| `out_of_scope_author_is_not_reviewed_or_recorded` | I9 | `authorize` returns `Some` and the poll runs Codex and writes the file |

I10 is the deletion of the five suppression tests named above. Mention suppression tests stay, including `mention_thread::pending_marker_is_marked_without_starting_task`.

## What these invariants do not pin down

A later review can still flag these. They are not reasons to add a knob or a second store.

- **Restart clock.** Coverage does not use the clock. The task-start window does, and a step of the system clock can admit or refuse a start the same way it can today. A kill after the comment and before the rename reviews the head once more, on purpose. A kill before the comment reviews it once, with no claim left behind.
- **`just stop` before the sibling exists.** Stop still exits 1 at 5 seconds if the process never writes the sibling. `restart` and `update` then leave Maid stopped. That is the current give-up, kept for a handler that did not acknowledge.
- **Hung Codex after the sibling.** Stop waits until the pid is gone. A review that never returns holds stop. Cancelling it would drop the record.
- **Rate-limit retry inside a post.** The client's 403/429 retry can run for as long as it runs today. `drain` is waiting on that worker. Same shape as a hung Codex, not a new queue.
- **Empty lists.** Nothing is admitted. Not a failure.
- **Huge lists.** `walk_pages` on the poll task delays the next mention admit and does not take a slot. Already-spawned work keeps running. No page cap.
- **Drafts.** Still not filtered. No new rule.
- **Closed pulls.** Not listed, so not reviewed and not recorded. Reopening with a SHA already in the file does not start a review. Reopening with a new SHA does.
- **Force-push, and a head returning to an earlier SHA.** The key is the object name. A new SHA is uncovered. A return to a SHA that was posted is skipped, even if the pull's branch was rewritten and then reset. Two different trees that somehow shared a SHA are the same object. "Review every push event" would re-review a posted SHA, which I1 forbids.
- **Forks.** Public search is by author, and a node is kept only when the repository is `PUBLIC`. `clone_url` is the base repository, as it is today.
- **Deleted branches.** Fetch of that SHA fails the candidate. The next poll retries. The task slot from the failed start stays consumed. There is no fallback ref.
- **Pagination.** A short page stops. `Link` headers are not a second cursor; the REST cursor is the next page number, and the GraphQL cursor is `endCursor`. `incomplete_results` still uses the items that arrived. A search whose `total_count` is over 1000 is no longer cut at `MAX_SEARCH_RESULTS`; the connection ending is the stop. `Off` does not follow a cursor that the first page returned.
- **Duplicate candidates.** `dedup_listed` keeps one row per HTML URL, and a replacing row brings its own SHA. Two pulls that share a SHA share one run, on the pull admitted first. The other is skipped once that SHA is occupied.
- **A `+1` that was not written by this record.** It is not consulted. A human `+1`, or a historical bot `+1`, does not count as a posted Codex run. The first poll after deploy reviews that head once.
- **GraphQL auth failure, and `git fetch` of a raw SHA.** Each fails that source or that candidate. Neither falls back to REST-search-plus-GET or to `FETCH_HEAD`.
- **Mention and auto-review in one tick.** They share the derived count. The mention thread is still not read when the count is already at the max.
- **Config.** `continuous_review` is read at process start, like the rest of the file. The team window and the notification window remain one field.
- **Orphaned Codex after `SIGKILL`.** Children are still not `kill_on_drop`, and `just start` still uses `setsid`. A hard kill can leave a Codex process. Clean shutdown waits instead. Adding `kill_on_drop` would change mention runs too, and it is not part of this design.
- **Two in-flight heads of one repository.** The flock serializes fetch and `worktree add`. Codex runs side by side, as it does today, and each holds its own slot.
