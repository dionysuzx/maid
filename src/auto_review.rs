use crate::domain::{PullRequest, TeamSlug};
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
