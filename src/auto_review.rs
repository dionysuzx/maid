use crate::domain::TeamSlug;
use chrono::{DateTime, Duration, Utc};

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
