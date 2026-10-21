use crate::domain::{CodexTask, CodexTaskOrigin, PullRequest, ReviewState, TeamSlug};
use crate::handled_marker::PendingHandledMarker;
use chrono::{DateTime, Duration, Utc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoReviewAuthor {
    Bot,
    Allowed,
    NotAllowed,
}

pub fn classify_auto_review_author(
    pr: &PullRequest,
    bot_login: &str,
    is_authorized: bool,
) -> AutoReviewAuthor {
    if pr.author.eq_ignore_ascii_case(bot_login) {
        AutoReviewAuthor::Bot
    } else if is_authorized {
        AutoReviewAuthor::Allowed
    } else {
        AutoReviewAuthor::NotAllowed
    }
}

/// The configured team that authorizes review of this pull request, if any.
///
/// `updated_at` is the pull request's last activity. Requesting a team and
/// marking a draft ready both move it, so a request is eligible when it
/// happens rather than when the pull request was opened. Activity outside
/// `window` (measured from `now`; a future timestamp counts as inside)
/// authorizes nothing, which leaves an untouched backlog alone. A missing
/// timestamp, an empty team list, or a non-positive window authorizes nothing.
pub fn authorizing_team<'a>(
    requested_teams: &[TeamSlug],
    configured_teams: &'a [TeamSlug],
    updated_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    window: Duration,
) -> Option<&'a TeamSlug> {
    if window <= Duration::zero() {
        return None;
    }
    let updated_at = updated_at?;
    if now.signed_duration_since(updated_at) > window {
        return None;
    }

    configured_teams
        .iter()
        .find(|team| requested_teams.contains(*team))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoReviewObservation {
    pr: PullRequest,
    author: AutoReviewAuthor,
    state: Option<ReviewState>,
    has_pending_marker: bool,
}

impl AutoReviewObservation {
    pub fn new(
        pr: PullRequest,
        author: AutoReviewAuthor,
        state: Option<ReviewState>,
        has_pending_marker: bool,
    ) -> Self {
        Self {
            pr,
            author,
            state,
            has_pending_marker,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutoReviewAction {
    StartTask {
        pr: PullRequest,
        task: CodexTask,
    },
    MarkHandled {
        pr: PullRequest,
        marker: PendingHandledMarker,
    },
    ForgetHandledMarker {
        pr: PullRequest,
        marker: PendingHandledMarker,
    },
    SkipSelfAuthored,
    SkipUnauthorized {
        pr: PullRequest,
    },
}

pub fn plan_auto_review(observation: AutoReviewObservation) -> AutoReviewAction {
    match observation.author {
        AutoReviewAuthor::Bot => AutoReviewAction::SkipSelfAuthored,
        AutoReviewAuthor::NotAllowed => AutoReviewAction::SkipUnauthorized { pr: observation.pr },
        AutoReviewAuthor::Allowed => plan_allowed_auto_review(observation),
    }
}

fn plan_allowed_auto_review(observation: AutoReviewObservation) -> AutoReviewAction {
    let marker = PendingHandledMarker::for_pull_request(&observation.pr);
    match observation.state {
        Some(ReviewState::Handled) => AutoReviewAction::ForgetHandledMarker {
            pr: observation.pr,
            marker,
        },
        Some(ReviewState::Pending) if observation.has_pending_marker => {
            AutoReviewAction::MarkHandled {
                pr: observation.pr,
                marker,
            }
        }
        Some(ReviewState::Pending) => AutoReviewAction::StartTask {
            task: CodexTask {
                pr_url: observation.pr.html_url.clone(),
                origin: CodexTaskOrigin::PullRequestOpened {
                    author: observation.pr.author.clone(),
                },
            },
            pr: observation.pr,
        },
        None => AutoReviewAction::SkipUnauthorized { pr: observation.pr },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_author_is_skipped_without_review_state() {
        let pr = pr("contributor");
        let action = plan_auto_review(AutoReviewObservation::new(
            pr.clone(),
            AutoReviewAuthor::NotAllowed,
            None,
            false,
        ));

        assert_eq!(action, AutoReviewAction::SkipUnauthorized { pr });
    }

    #[test]
    fn pending_marker_is_marked_without_starting_task() {
        let pr = pr("dionysuzx");
        let action = plan_auto_review(AutoReviewObservation::new(
            pr.clone(),
            AutoReviewAuthor::Allowed,
            Some(ReviewState::Pending),
            true,
        ));

        assert_eq!(
            action,
            AutoReviewAction::MarkHandled {
                marker: PendingHandledMarker::for_pull_request(&pr),
                pr,
            }
        );
    }

    #[test]
    fn pending_allowed_pr_starts_review_task() {
        let pr = pr("dionysuzx");
        let action = plan_auto_review(AutoReviewObservation::new(
            pr.clone(),
            AutoReviewAuthor::Allowed,
            Some(ReviewState::Pending),
            false,
        ));

        assert!(matches!(
            action,
            AutoReviewAction::StartTask { pr: started, task }
                if started == pr
                    && task.pr_url == "https://github.com/o/r/pull/1"
                    && matches!(task.origin, CodexTaskOrigin::PullRequestOpened { .. })
        ));
    }

    #[test]
    fn team_review_authorizes_only_a_configured_team_inside_the_window() {
        let now = DateTime::parse_from_rfc3339("2026-10-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let window = Duration::hours(24);
        let configured = [
            TeamSlug::parse("some-org/docs").unwrap(),
            TeamSlug::parse("Some-Org/eng-infra").unwrap(),
        ];
        let requested = [
            TeamSlug::parse("some-org/eng-infra").unwrap(),
            TeamSlug::parse("some-org/docs").unwrap(),
        ];

        assert_eq!(
            authorizing_team(&requested, &configured, Some(now - window), now, window)
                .map(ToString::to_string)
                .as_deref(),
            Some("some-org/docs")
        );
        assert!(
            authorizing_team(
                &requested,
                &configured,
                Some(now - window - Duration::seconds(1)),
                now,
                window,
            )
            .is_none()
        );
        assert!(authorizing_team(&requested, &configured, None, now, window).is_none());
        assert!(authorizing_team(&[], &configured, Some(now), now, window).is_none());
        assert!(authorizing_team(&requested, &[], Some(now), now, window).is_none());
        assert!(
            authorizing_team(
                &[TeamSlug::parse("other-org/eng-infra").unwrap()],
                &configured,
                Some(now),
                now,
                window,
            )
            .is_none()
        );
        assert!(
            authorizing_team(
                &requested,
                &configured,
                Some(now + Duration::minutes(5)),
                now,
                window,
            )
            .is_some()
        );
        assert!(
            authorizing_team(&requested, &configured, Some(now), now, Duration::zero()).is_none()
        );
    }

    fn pr(author: &str) -> PullRequest {
        PullRequest {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
            author: author.to_string(),
            api_url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
            html_url: "https://github.com/o/r/pull/1".to_string(),
            clone_url: "https://github.com/o/r.git".to_string(),
        }
    }
}
