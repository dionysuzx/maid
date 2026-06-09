use crate::domain::{CodexTask, CodexTaskOrigin, CommentMention, MentionRequest, ReviewState};
use crate::handled_marker::PendingHandledMarker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MentionThreadRead {
    LatestOnly,
    RecentComments,
}

pub fn choose_mention_thread_read(
    latest: &CommentMention,
    request: Option<&MentionRequest>,
    state: Option<ReviewState>,
    bot_login: &str,
) -> MentionThreadRead {
    if latest.author.eq_ignore_ascii_case(bot_login) {
        return MentionThreadRead::RecentComments;
    }

    match (request, state) {
        (Some(_), Some(ReviewState::Handled)) => MentionThreadRead::LatestOnly,
        _ => MentionThreadRead::RecentComments,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MentionThread {
    observations: Vec<MentionObservation>,
}

impl MentionThread {
    pub fn from_observations(observations: Vec<MentionObservation>) -> Self {
        Self { observations }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionObservation {
    mention: CommentMention,
    request: MentionRequest,
    state: ReviewState,
    has_pending_marker: bool,
}

impl MentionObservation {
    pub fn new(
        mention: CommentMention,
        request: MentionRequest,
        state: ReviewState,
        has_pending_marker: bool,
    ) -> Self {
        Self {
            mention,
            request,
            state,
            has_pending_marker,
        }
    }

    fn disposition(&self) -> MentionDisposition {
        match (self.state, self.has_pending_marker) {
            (ReviewState::Handled, _) => MentionDisposition::AlreadyHandled,
            (ReviewState::Pending, true) => MentionDisposition::PendingHandledMarker,
            (ReviewState::Pending, false) => MentionDisposition::PendingRequest,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MentionDisposition {
    AlreadyHandled,
    PendingHandledMarker,
    PendingRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionThreadPlan {
    pub actions: Vec<MentionThreadAction>,
    pub notification: MentionNotificationPlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MentionNotificationPlan {
    MarkRead,
    LeaveUnread,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MentionThreadAction {
    StartTask {
        mention: CommentMention,
        task: CodexTask,
    },
    MarkHandled {
        mention: CommentMention,
        marker: PendingHandledMarker,
    },
    ForgetHandledMarker {
        mention: CommentMention,
        marker: PendingHandledMarker,
    },
}

pub fn plan_mention_thread(thread: MentionThread, bot_login: &str) -> MentionThreadPlan {
    let actions = thread
        .observations
        .into_iter()
        .map(|observation| mention_action_for(observation, bot_login))
        .collect::<Vec<_>>();
    let notification = if actions
        .iter()
        .any(|action| matches!(action, MentionThreadAction::StartTask { .. }))
    {
        MentionNotificationPlan::LeaveUnread
    } else {
        MentionNotificationPlan::MarkRead
    };

    MentionThreadPlan {
        actions,
        notification,
    }
}

fn mention_action_for(observation: MentionObservation, bot_login: &str) -> MentionThreadAction {
    match observation.disposition() {
        MentionDisposition::AlreadyHandled => {
            let marker = PendingHandledMarker::for_mention(&observation.mention);
            MentionThreadAction::ForgetHandledMarker {
                mention: observation.mention,
                marker,
            }
        }
        MentionDisposition::PendingHandledMarker => {
            let marker = PendingHandledMarker::for_mention(&observation.mention);
            MentionThreadAction::MarkHandled {
                mention: observation.mention,
                marker,
            }
        }
        MentionDisposition::PendingRequest => MentionThreadAction::StartTask {
            task: CodexTask {
                pr_url: observation.mention.target.html_url().to_string(),
                origin: mention_task_origin(&observation.mention, observation.request, bot_login),
            },
            mention: observation.mention,
        },
    }
}

fn mention_task_origin(
    mention: &CommentMention,
    request: MentionRequest,
    bot_login: &str,
) -> CodexTaskOrigin {
    if let Some(operator_text) = request.operator_text() {
        return CodexTaskOrigin::OperatorMention {
            mention_url: mention.html_url.clone(),
            raw_body: request.raw_body,
            request_text: operator_text,
            trigger_author: mention.author.clone(),
            bot_login: bot_login.to_string(),
        };
    }

    CodexTaskOrigin::Mention {
        mention_url: mention.html_url.clone(),
        raw_body: request.raw_body,
        cleaned_text: request.cleaned_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PullRequest, WorkTarget};

    #[test]
    fn handled_latest_trusted_request_does_not_scan_old_comments() {
        let latest = mention("dionysuzx", "@maid-bot done", "4");
        let request = MentionRequest::parse(&latest.body, "maid-bot")
            .unwrap()
            .unwrap();

        assert_eq!(
            choose_mention_thread_read(
                &latest,
                Some(&request),
                Some(ReviewState::Handled),
                "maid-bot"
            ),
            MentionThreadRead::LatestOnly
        );
    }

    #[test]
    fn latest_bot_comment_scans_recent_comments() {
        let latest = mention("maid-bot", "codex response", "4");

        assert_eq!(
            choose_mention_thread_read(&latest, None, None, "maid-bot"),
            MentionThreadRead::RecentComments
        );
    }

    #[test]
    fn pending_marker_is_marked_without_starting_task() {
        let mention = mention("dionysuzx", "@maid-bot review", "2");
        let request = MentionRequest::parse(&mention.body, "maid-bot")
            .unwrap()
            .unwrap();

        let plan = plan_mention_thread(
            MentionThread::from_observations(vec![MentionObservation::new(
                mention.clone(),
                request,
                ReviewState::Pending,
                true,
            )]),
            "maid-bot",
        );

        assert_eq!(plan.notification, MentionNotificationPlan::MarkRead);
        assert_eq!(
            plan.actions,
            vec![MentionThreadAction::MarkHandled {
                marker: PendingHandledMarker::for_mention(&mention),
                mention,
            }]
        );
    }

    #[test]
    fn pending_request_starts_task_and_leaves_notification_unread() {
        let mention = mention("dionysuzx", "@maid-bot review", "2");
        let request = MentionRequest::parse(&mention.body, "maid-bot")
            .unwrap()
            .unwrap();

        let plan = plan_mention_thread(
            MentionThread::from_observations(vec![MentionObservation::new(
                mention.clone(),
                request,
                ReviewState::Pending,
                false,
            )]),
            "maid-bot",
        );

        assert_eq!(plan.notification, MentionNotificationPlan::LeaveUnread);
        assert!(matches!(
            plan.actions.as_slice(),
            [MentionThreadAction::StartTask { mention: started, .. }] if started == &mention
        ));
    }

    #[test]
    fn pending_requests_each_start_tasks() {
        let older = mention("dionysuzx", "@maid-bot review old head", "2");
        let newer = mention("dionysuzx", "@maid-bot review latest head", "3");
        let older_request = MentionRequest::parse(&older.body, "maid-bot")
            .unwrap()
            .unwrap();
        let newer_request = MentionRequest::parse(&newer.body, "maid-bot")
            .unwrap()
            .unwrap();

        let plan = plan_mention_thread(
            MentionThread::from_observations(vec![
                MentionObservation::new(older.clone(), older_request, ReviewState::Pending, false),
                MentionObservation::new(newer.clone(), newer_request, ReviewState::Pending, false),
            ]),
            "maid-bot",
        );

        assert_eq!(plan.notification, MentionNotificationPlan::LeaveUnread);
        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(
            &plan.actions[0],
            MentionThreadAction::StartTask { mention: started, .. } if started == &older
        ));
        assert!(matches!(
            &plan.actions[1],
            MentionThreadAction::StartTask { mention: started, .. } if started == &newer
        ));
    }

    fn mention(author: &str, body: &str, comment_id: &str) -> CommentMention {
        CommentMention {
            author: author.to_string(),
            body: body.to_string(),
            api_url: format!("https://api.github.com/repos/o/r/issues/comments/{comment_id}"),
            html_url: format!("https://github.com/o/r/pull/1#issuecomment-{comment_id}"),
            target: WorkTarget::PullRequest(PullRequest {
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 1,
                author: "contributor".to_string(),
                api_url: "https://api.github.com/repos/o/r/pulls/1".to_string(),
                html_url: "https://github.com/o/r/pull/1".to_string(),
                clone_url: "https://github.com/o/r.git".to_string(),
            }),
        }
    }
}
