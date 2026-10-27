# Continuous review

## Which design

Five designs, judged by concept count, shared mutable state, whether each of I1–I10 is structural, local reasoning, legacy paths, magic numbers, and GraphQL.

| Design | Concepts a reader holds | Why it loses |
| --- | --- | --- |
| PR #43 `cursor/pr-review-ledger-0c95` (+2343/−496) | Ledger row (agent, model, prompt hash, effort, outcome), per-PR reservation held until the write, write-until-success, post-checkout replan, head-lookup GET, `just stop` wait change | Coverage was bolted onto the old poll. I2 is a second plan after checkout. I3 is a reservation beside the ledger. The retry loop is `AwaitingRecord` under another name. |
| PR #44 `cursor/continuous-review-0c95` | Everything in #43, plus `continuous_review` paging and `max_failures_per_head` | Paging is a flag on the old lister. The new failure cap is a knob. Merges #43 rather than removing it. |
| PR #45 `7fe1a23` (+416/−207) | `reviewed-heads.json` keyed by URL and SHA, `plan_auto_review`, an in-flight key held until the record | Fewest lines, and the three blocking findings are the structure: checkout can move off the discovered SHA (I2), a coverage read can race a worker that has already posted (I3), public-author search ignores `continuous_review` (I6). |
| `8e99f3e` | 14: the eight below, plus `AuthorizedHead`, page cursors, `walk_pages`, GraphQL, the occupancy table, `AwaitingRecord`, `FlightId`, the draining sibling, and the flush retry | I1–I10 are structural, and the reader pays for protocols no invariant test requires. GraphQL is one of them. |
| `ba6f634` | 8 | Winner. |

Nothing in #43, #44, or #45 removes a concept from `ba6f634` or closes an invariant that design still leaves open. The public-author pull GET and the unchanged 5-second `just stop` are the accepted weakenings of I4 and I8. No graft.

The rest of this document is that design.

## Concepts

Eight. The previous draft asked a reader to hold fourteen named pieces: `HeadSha`, `ListedPull`, `AuthorizedHead`, the page-cursor types, `walk_pages`, GraphQL, the four-state occupancy table, `AwaitingRecord`, `FlightId`, the posted-heads file, the slot count, the `{pid}.draining` sibling, the flush retry, and the pinned checkout. A reader can stop here.

1. **HeadSha.** 40 lowercase hex digits, parsed once where a GitHub payload or the posted-heads file enters. This is the only coverage key.
2. **ListedPull.** One discovered pull: the pull request, its `HeadSha`, which source found it, requested teams, `updated_at`. The SHA is part of the row, so a later source cannot keep the row and drop the commit.
3. **Covered set.** The SHAs whose Codex comment was posted. Loaded from `~/.maid/posted-heads.json` at start. One owner holds it.
4. **In-flight set.** The SHAs whose review has been started and has not finished. Same owner. Not written to disk.
5. **Slot count.** In-flight heads plus admitted mentions, compared with `max_concurrent_requests`. One count, on the owner. A mention hold drops when the mention task finishes, and the next admit applies that result.
6. **`pages`.** Fetch REST page 1. If `continuous_review` is on, fetch page 2, 3, … until the response GitHub returned is shorter than the `per_page` that request already used (50 for pulls, 100 for search). Dropped rows do not shorten that count. A later page that cannot be fetched ends the walk and keeps the pages already read. The bool is read here and nowhere else.
7. **Pinned checkout.** A head review fetches and detaches that `HeadSha`. `rev-parse` must equal it. Mentions still check out a named ref.
8. **One owner.** The poll task holds the covered set, the in-flight set, and the slot count. Workers get a checkout, Codex, and GitHub. They do not get the sets. `Maid` is cloned into a mention task, so the owner sits behind a mutex; mention tasks do not lock it.

Scope is a function, `in_scope`, not a type. Bot, unauthorized author, a public row whose author is not the searched account, and a team row outside the window return false. The poll does not submit those rows and does not record them. The rules are today's auto-review rules minus reactions. `TeamActivity` is authorized only by `authorizing_team`. `PublicAuthor` only when the searched login is a public account and matches the author. `Repository` by account or team. Dedup is the existing URL merge: the first row wins, except a later in-scope author row replaces the whole `ListedPull`, including its SHA. A team-activity row never replaces a repository row.

## What was cut

Each of these had a premise. None of them is required by a test of I1–I10. The structural pieces above are what the tests actually reject.

| Cut | Premise | Why it goes |
| --- | --- | --- |
| GraphQL | REST search has no SHA and no visibility, so public discovery needs another protocol. | David rejected it. Public discovery stays `GET /search/issues`, the repo visibility GET it already does, and one GET of the pull URL the search hit already carries, to read `head.sha`. Repo and team listings stay `GET /pulls` and gain `head.sha` plus `&page=`. |
| `Page`, `PageCursor`, `PageRequest`, `ContinuousReview`, `walk_pages` | A cursor type is required so only one walker can turn the page, and so GraphQL's `endCursor` and REST page numbers share a shape. | Both sources already take an integer page. `pages(continuous, page_size, fetch)` is that shape. I6 fails when the flag is ignored. Nothing else has to be a type. |
| Four-state occupancy table and `AwaitingRecord` | A failed rename after a successful post needs its own state so a later admit retries the write and does not start Codex. | After the comment is posted, the owner inserts the admitted SHA into the covered set, then writes the file. A failed write is logged. The SHA is already covered for this process, so the next admit does not start Codex (I8, I1). No test distinguishes a third state from that set. The I8 test only requires `drain` to wait until a successful post is in the file. |
| Recording the SHA before the comment is posted | The file write cannot fail after the post if it happens first. | A crash between the write and the post would mark a commit covered that was never reviewed. That breaks I2. The write stays after the post. |
| `FlightId` | A worker that sends a SHA might send the moved ref. | The worker is not a boundary. The owner records the SHA it passed to checkout. The boundary check is `rev-parse`. I2 fails today because checkout uses `pull/N/head` and the file receives that commit. An id does not show up in the test. |
| `{pid}.draining` | `just stop` gives up after 5 seconds, so a long drain needs a side channel before stop will wait. | No test reads the file. `just stop` stays as it is: one SIGTERM, then 5 seconds, then exit 1. The process drains and then exits. See the weakened sentence under I8. |
| Flush-retry loop | `drain` and later admits must keep retrying a failed rename. | One write after the post. `drain` waits for in-flight reviews to finish that write. It does not loop. A failed write is the log above. |
| Flag-off as its own coverage rule | With the flag off, a posted run might cover every later head of that pull request, or per-head coverage might be a second mode. | One coverage rule. The bool does not select a legacy once-per-URL path, and it does not have a second sentence about coverage. I1's test restarts against the file. It does not mention the flag. |

## Source of truth

`main` cannot answer "this commit has a posted Codex run." The bot `+1` has no SHA. `pending-handled-markers.json` retries that `+1` and is keyed by HTML URL. Checkout detaches whatever `pull/N/head` points at.

The record is `~/.maid/posted-heads.json`: `{ "heads": ["<40 lowercase hex>", ...] }`. One set. Atomic rename, same as the other ledgers. Missing file is empty. A file that does not parse fails `open` and the process does not start. That check is the file boundary, not a check inside admit. `InFlight` is not in the file. A kill after the comment and before the write may review that head once. There is no claim to expire.

Historical `+1`s are not imported. Open tracked heads are reviewed once on the first deploy. Closed pulls are not listed.

Removed with the shell:

- PR `pr_state`, `mark_pr_started`, `mark_pr_handled`, `mark_pull_request_html_url_handled`, and the PR reaction helpers.
- `PendingHandledMarker::PullRequest` and `pull_request_html_urls`. A leftover field is dropped on read and not imported.
- `plan_auto_review` and its reaction actions. `authorizing_team` stays, and `in_scope` calls it.
- `WorkQueue` and its semaphore. Mention slots and head slots are the one count on the owner.
- The list-all client methods and `MAX_SEARCH_RESULTS`. Page limits live in `pages`.
- Auto-review checkout of `pull/N/head` and `FETCH_HEAD`.
- `poller.abort()` and the 50ms shutdown sleep. Shutdown is a oneshot between polls, then `drain`.

`auto_review_does_not_restart_finished_pr_during_reaction_lag`, `retries_pending_auto_review_marker_without_rerunning_codex`, `retries_pending_auto_review_marker_without_open_pull_request`, `pending_marker_is_marked_without_starting_task`, and `skips_opened_pr_with_durable_handled_marker_after_restart` are gone. Mention suppression stays.

Mention markers, mention reactions, the task-start ledger, the observed-notification ledger, the repo flock, and the daemon lock stay.

## Owner

```text
submit(row):
    apply finished results
    if row.head is covered or in flight: return Skip
    if in_flight.len() + mentions >= max: return Skip
    task ledger: AtLimit -> Skip, error -> Fail, else continue
    in_flight.insert(row.head)
    spawn review(row.head)

review:
    checkout Commit(sha)
    codex, post comment
    tell the owner success or failure
    the owner, not the worker, inserts sha into covered and writes the file
```

`apply` runs at the start of `submit` and at the start of mention admission, before the slot is considered. A result that arrived while the owner was busy cannot be ignored by the next decision. That is I3, and the test queues the result before the second `submit`.

A covered SHA returns before the slot count changes, before the task ledger, before checkout, and before any GitHub call that is not the listing that produced the row. Mentions use the same count, and they still do not read the thread when the count is already at the max.

A failure removes the SHA from in-flight and does not insert it into covered. The next poll can submit it again. Nothing stores a retry delay.

`poll` lists through `pages`, dedups, drops rows where `in_scope` is false, and calls `submit` for the rest. One candidate's checkout error, comment error, or search-item pull GET error does not `?` out of the poll.

`drain` stops new admits, waits until in-flight is empty (each finished review has applied), and returns. It does not cancel Codex. `main` joins that task. It does not abort the poller.

## Discovery

One function for newest-created pulls, recently-updated pulls, and public-author search.

Repo and team pages are the pulls request they already are: `per_page=50`, `direction=desc`, plus `page` and `head.sha`. A row whose SHA does not parse is dropped. The rest of the page is kept. There is no separate head-lookup request on this path.

Public-author pages are the existing issue search (`per_page=100`) and the existing per-repo visibility GET. Issue search has no commit, so each kept hit is completed with GET of the pull URL on that hit, reading `head.sha`. A failed GET drops that hit. This GET is how the row comes to exist. It happens before coverage is known, so a covered public head pays it on every poll. It does not take a slot, a checkout, or a task allowance. Repo and team heads do not pay it: their SHA is on the pulls page.

`continuous_review` defaults false and is serde-defaulted. Off means `pages` returns page 1 even when that page is full. On means it follows until the response is short. `MAX_SEARCH_RESULTS` is not reintroduced. Search stops when GitHub returns a short page or refuses the next one; the pages already read stay. No new knob.

## Shutdown

On SIGTERM or ctrl-c the poll stops admitting and drains. The process exits after that. `just stop` is unchanged: one SIGTERM, 20 waits of 0.25 seconds, exit 1 if the pid is still there. No sibling file. No new timeout. `restart` and `update` still run `stop` first.

I8's in-process half and its "drain finishes the record" half hold. The sentence "`just stop` waits for a drain that takes longer than 5 seconds" does not. A review that outlives those 5 seconds makes `stop` exit 1, and `restart` / `update` then refuse to start. That is the current give-up. The sibling file was a protocol added to dodge it, and it had no test.

## Invariants

| | Holds |
| --- | --- |
| I1 | By structure. The covered set is loaded from the file and only grows when a comment was posted. Admit reads it before starting work. The set lives on the one owner. |
| I2 | By structure. `ListedPull.head` is the checkout argument and the key that gets inserted. The commit checkout has no `pull/N/head` input. `rev-parse` must match before the insert. |
| I3 | By structure. Finished results are applied, then the sets are read, then the slot, then the task ledger, then the insert, with no await in between. |
| I4 | By structure for slots, checkout, task allowance, and any GitHub call after the row exists. Weakened for public-author discovery only: REST search has no SHA, so each hit pays one pull GET before the row exists. That GET cannot take the mention's slot. |
| I5 | By structure. A spawn happens only after an in-flight insert, and the count is the bound. The poll does not await checkout. |
| I6 | By structure. `pages` is the only loop. The bool is its argument. Repo, team, and public search call it. |
| I7 | By structure for "no retry queue": failure only removes the in-flight SHA. By code for isolation: a source error or a per-hit error is matched, and the candidate loop does not use `?`. |
| I8 | By structure for this process: the covered insert happens before the file write, so a failed write does not start another Codex run. By structure for a clean shutdown inside the process: `drain` waits until in-flight work has applied. Weakened for `just stop`: it still gives up after 5 seconds. A kill before the write leaves the SHA out of the file. |
| I9 | By code. `in_scope` false skips `submit` and does not record. The result type was cut; the test locks the behavior. |
| I10 | The five suppression tests named above are deleted. The rest of the old auto-review tests change with the shell. |

## Failure matrix

"Die" means the process is killed immediately after.

- Posted-heads file missing: empty covered set, process starts.
- Posted-heads file unreadable or unparseable: `open` fails, process does not start, file untouched.
- `notifications()` error: poll returns before listing.
- The first page of a repo list or an author search fails: that source adds no rows. A later page fails: the walk stops and the pages already read stay. Other sources and the mention-marker retry still run. Next poll retries the source.
- One repo-visibility GET fails: that search page fails. The rule above decides whether earlier pages of that author stay.
- One search hit's pull GET fails, or a SHA does not parse: that row is dropped, the rest are kept, nothing is recorded.
- `in_scope` is false: no submit, no record, no slot.
- SHA covered or in flight, or the slot count is at the max: `Skip`. No task-ledger start, no checkout, no further GitHub call.
- Task ledger at its limit: `Skip`, SHA not inserted. Ledger error: `Fail`, SHA not inserted. A timestamp that landed anyway burns one task slot and leaves the head uncovered.
- Fetch, worktree, or `rev-parse` mismatch: in-flight entry removed, no comment, no record. Next poll can retry. The task slot stays consumed. Die during fetch: next process retries.
- Codex error: same as a checkout failure. Die during Codex: next process retries. `drain` waits for the child.
- Comment post error: in-flight entry removed, no record. Next poll can retry. Die during the POST: the comment may exist and the file does not; the next process may review once.
- Comment posted: owner inserts the SHA, then writes the file. A later admit in this process does not start Codex.
- File write succeeds: the next process loads the SHA.
- File write fails: logged. The SHA stays in the covered set until this process exits. No second Codex run here. Die, or exit, before a later successful write: the next process may review once. `drain` does not retry the write.
- Cleanup error after the owner was told the post succeeded: logged. The insert still happens.
- Worker dropped before it reports: the in-flight entry is removed when the task ends. Next poll can retry. A task that already reported success does not also report failure.
- Mention thread read error: that mention slot is dropped, the poll continues.
- Mention eyes error after the task slot was taken: the slot is dropped, the task slot stays consumed, no Codex.
- Mention post succeeded: the owner writes the mention marker, then tries the mention `+1`. A `+1` failure leaves the marker for the existing retry, which takes no slot.
- Observed-notification load, or the initial mention-marker load, fails: the poll fails, as today.

## Tests

| Test | Invariant | Naive failure |
| --- | --- | --- |
| `posted_head_is_not_reviewed_again_after_restart` | I1 | admit returns `Started` |
| `checkout_uses_the_sha_discovery_selected_after_the_ref_moves` | I2 | detaches and records the moved ref |
| `completing_worker_report_is_applied_before_a_second_admit` | I3 | the second admit is `Started` after the result was queued |
| `covered_head_does_not_take_the_only_slot_from_a_mention` | I4 | `(Started, 1, Skipped, false)` |
| `stalled_checkout_does_not_block_the_poll_and_is_bounded` | I5 | the poll does not return (`returned=false`, `entered=1`) |
| `public_author_pages_follow_the_flag` | I6 | the flag on still returns only page 1 |
| `dropped_row_does_not_end_a_full_page` | I6 | a dropped row makes a full response look short |
| `later_page_failure_keeps_pages_already_read` | I6 | the failed next page throws away the pages already read |
| `a_missing_head_does_not_shrink_the_page_github_returned` | I6 | the pulls adapter counts rows it kept |
| `one_candidate_checkout_failure_does_not_stop_the_other` | I7 | `(false, 0, 1)` |
| `clean_shutdown_drains_the_posted_record` | I8 | `drain` returns before the SHA is in the file |
| `out_of_scope_author_is_not_reviewed_or_recorded` | I9 | the outsider is reviewed and the file receives the SHA |

## Diff

This branch is that shell. `justfile` is not edited. `continuous_review` defaults false. Reviewed heads live in `~/.maid/posted-heads.json`.
