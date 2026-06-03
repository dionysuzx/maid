use crate::domain::{CodexTask, CodexTaskOrigin, PullRequest, ReviewState};
use crate::handled_marker::PendingHandledMarker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoReviewAuthor {
    Bot,
    Allowed,
    NotAllowed,
}

pub fn classify_auto_review_author(
    pr: &PullRequest,
    bot_login: &str,
    is_allowed_author: bool,
) -> AutoReviewAuthor {
    if pr.author.eq_ignore_ascii_case(bot_login) {
        AutoReviewAuthor::Bot
    } else if is_allowed_author {
        AutoReviewAuthor::Allowed
    } else {
        AutoReviewAuthor::NotAllowed
    }
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
