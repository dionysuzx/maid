use crate::{agent::AgentLabel, domain::CommitSha};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentReply<'a> {
    Responded(&'a str),
    /// Maid logs the error rather than posting it, since stderr can hold
    /// details that do not belong in a public comment.
    Failed,
}

/// One section per agent, in panel order, then the exact prompt every agent
/// received. Each section names its commit, since the author may push again
/// before the comment posts. GitHub links the bare SHA.
pub fn agent_comment(
    commit: Option<&CommitSha>,
    replies: &[(&AgentLabel, AgentReply<'_>)],
    prompt: &str,
) -> String {
    let sections: Vec<String> = replies
        .iter()
        .map(|(label, reply)| agent_section(label, commit, *reply))
        .collect();

    format!(
        "{}\n\n{}",
        sections.join("\n\n---\n\n"),
        prompt_details(prompt)
    )
}

fn agent_section(label: &AgentLabel, commit: Option<&CommitSha>, reply: AgentReply<'_>) -> String {
    let at = commit
        .map(|commit| format!("\nAt {commit}."))
        .unwrap_or_default();
    let body = match reply {
        AgentReply::Responded(response) => response,
        AgentReply::Failed => "Failed. The error is in Maid's logs.",
    };
    format!("### {label}{at}\n\n{body}")
}

/// Collapsed, and fenced longer than any backtick run inside the prompt, so a
/// quoted code block cannot close the fence early.
fn prompt_details(prompt: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(prompt).max(2) + 1);
    format!(
        "<details>\n<summary>Input prompt</summary>\n\n{fence}text\n{}\n{fence}\n\n</details>",
        prompt.trim_end()
    )
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentKind;

    fn label(kind: AgentKind, model: &str) -> AgentLabel {
        AgentLabel {
            kind,
            model: model.to_string(),
            effort: "high".to_string(),
        }
    }

    #[test]
    fn stitches_each_agent_section_above_the_collapsed_prompt() {
        let commit = CommitSha::parse(&"a".repeat(40)).unwrap();
        let codex = label(AgentKind::Codex, "gpt-5.5");
        let claude = label(AgentKind::Claude, "claude-opus-5-5");

        let comment = agent_comment(
            Some(&commit),
            &[
                (&codex, AgentReply::Responded("1. **[P1] Bug.**")),
                (&claude, AgentReply::Failed),
            ],
            "please review\n",
        );

        let sha = "a".repeat(40);
        assert_eq!(
            comment,
            format!(
                "### codex · gpt-5.5 · high\nAt {sha}.\n\n1. **[P1] Bug.**\n\n---\n\n\
                 ### claude · claude-opus-5-5 · high\nAt {sha}.\n\nFailed. The error is in Maid's logs.\n\n\
                 <details>\n<summary>Input prompt</summary>\n\n```text\nplease review\n```\n\n</details>"
            )
        );
    }

    #[test]
    fn omits_the_commit_when_the_target_has_none() {
        let codex = label(AgentKind::Codex, "gpt-5.5");

        let comment = agent_comment(None, &[(&codex, AgentReply::Responded("done"))], "p");

        assert!(comment.starts_with("### codex · gpt-5.5 · high\n\ndone\n\n<details>"));
    }

    #[test]
    fn fences_the_prompt_beyond_any_backtick_run_inside_it() {
        let codex = label(AgentKind::Codex, "gpt-5.5");

        let comment = agent_comment(
            None,
            &[(&codex, AgentReply::Responded("done"))],
            "quoted:\n````rust\nfn main() {}\n````",
        );

        assert!(comment.contains("\n`````text\nquoted:\n````rust\nfn main() {}\n````\n`````\n"));
    }
}
