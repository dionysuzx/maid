use crate::domain::{CodexTask, CodexTaskOrigin, CommitSha, PullRequest, TeamSlug};
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

/// How often Maid reviews a pull request that is in its auto-review scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReviewCadence {
    /// Review each pull request once.
    #[default]
    Once,
    /// Review each new head commit of a pull request.
    EveryHead,
}

impl ReviewCadence {
    pub fn from_continuous_review(continuous_review: bool) -> Self {
        if continuous_review {
            Self::EveryHead
        } else {
            Self::Once
        }
    }

    pub fn needs_review(self, head: &CommitSha, last_reviewed: Option<&CommitSha>) -> bool {
        match (self, last_reviewed) {
            (_, None) => true,
            (Self::Once, Some(_)) => false,
            (Self::EveryHead, Some(last_reviewed)) => last_reviewed != head,
        }
    }
}

pub fn auto_review_task(pr: &PullRequest) -> CodexTask {
    CodexTask {
        pr_url: pr.html_url.clone(),
        origin: CodexTaskOrigin::PullRequestOpened {
            author: pr.author.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_decides_whether_a_head_needs_review() {
        let first = sha('a');
        let second = sha('b');

        for cadence in [ReviewCadence::Once, ReviewCadence::EveryHead] {
            assert!(cadence.needs_review(&first, None));
        }
        assert!(!ReviewCadence::Once.needs_review(&second, Some(&first)));
        assert!(!ReviewCadence::EveryHead.needs_review(&first, Some(&first)));
        assert!(ReviewCadence::EveryHead.needs_review(&second, Some(&first)));
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

    fn sha(digit: char) -> CommitSha {
        CommitSha::parse(&digit.to_string().repeat(40)).unwrap()
    }
}
