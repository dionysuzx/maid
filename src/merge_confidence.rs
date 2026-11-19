//! Whether a reviewed pull request can merge without another reviewer.
//!
//! Jev reads the panel's reviews and the diff, then answers two questions.
//! Skipping a reviewer needs both answers to favor it: the reviews leave
//! nothing that needs a person (no must or should finding), and a defect the
//! reviews missed would stay contained. The merge confidence is the chance
//! that both hold. Jev must also have every agent's review and the whole
//! diff, since what it missed could hide the defect that should have stopped
//! the merge.

use crate::{
    agent::AgentLabel,
    domain::{CommitSha, PullRequestDiff},
    jev::{Answer, NoulCriteria, Question, Questions, SystemOne},
    review_tier::ReviewTier,
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
};

const WORST_FINDING: &str = "worst_finding";
const CONTAINED: &str = "contained";
const NO_FINDING: &str = "none";
/// Patches beyond this many bytes reach Jev as paths and line counts
/// only, which keeps the state inside Jev's context.
const PATCH_BUDGET: usize = 48_000;

/// A number from 0 to 1. Never NaN, which makes equality lawful.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Eq for Probability {}

impl Probability {
    pub fn new(value: f64) -> Result<Self> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            bail!("a probability must be from 0 to 1, not {value}")
        }
    }

    pub fn value(self) -> f64 {
        self.0
    }

    /// Treats the two as independent.
    fn and(self, other: Self) -> Self {
        Self(self.0 * other.0)
    }

    /// For mutually exclusive outcomes. Clamped, since probabilities Jev
    /// reports can sum a hair past 1.
    fn or(self, other: Self) -> Self {
        Self((self.0 + other.0).min(1.0))
    }
}

impl fmt::Display for Probability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:.2}", self.0)
    }
}

/// Jev, and the merge confidence a pull request needs to skip another
/// reviewer.
#[derive(Clone)]
pub struct MergeJudge {
    pub jev: Arc<dyn SystemOne>,
    pub threshold: Probability,
}

/// Whether Jev saw everything a merge decision rests on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Coverage {
    Complete,
    /// An agent failed, so its review is missing.
    MissingReview,
    /// Some files or patches were left out.
    PartialDiff,
}

/// What Jev reads: the reviews to judge, and the change they judged.
pub struct MergeEvidence<'a> {
    pub pr_url: &'a str,
    /// Every agent in the panel, with no review when it failed.
    pub reviews: Vec<(&'a AgentLabel, Option<&'a str>)>,
    pub diff: &'a PullRequestDiff,
}

impl<'a> MergeEvidence<'a> {
    pub fn coverage(&self) -> Coverage {
        let every_file_listed = self.diff.files.len() as u64 == self.diff.changed_files;
        let every_patch_shown = self.shown_patches().iter().all(Option::is_some);
        if self.reviews.iter().any(|(_, review)| review.is_none()) {
            Coverage::MissingReview
        } else if !(every_file_listed && every_patch_shown) {
            Coverage::PartialDiff
        } else {
            Coverage::Complete
        }
    }

    pub fn state(&self) -> Value {
        let reviews: Vec<_> = self
            .reviews
            .iter()
            .map(|(reviewer, review)| json!({"reviewer": reviewer.to_string(), "review": review}))
            .collect();
        let changed_files: Vec<_> = self
            .diff
            .files
            .iter()
            .zip(self.shown_patches())
            .map(|(file, patch)| {
                json!({
                    "path": file.path,
                    "status": file.status,
                    "additions": file.additions,
                    "deletions": file.deletions,
                    "patch": patch,
                })
            })
            .collect();

        json!({
            "pull_request": self.pr_url,
            "reviews": reviews,
            "changed_files": changed_files,
        })
    }

    /// Each file's patch, in order, while the budget lasts.
    fn shown_patches(&self) -> Vec<Option<&'a str>> {
        let mut budget = PATCH_BUDGET;
        self.diff
            .files
            .iter()
            .map(|file| {
                let patch = file.patch.as_deref().filter(|patch| patch.len() <= budget);
                budget -= patch.map_or(0, str::len);
                patch
            })
            .collect()
    }
}

/// The two questions. Their wording is the classifier's whole prompt, so the
/// review comment shows it.
pub fn questions() -> Questions {
    let mut worst_finding: BTreeMap<_, _> = ReviewTier::ALL
        .iter()
        .map(|tier| (tier.as_str(), tier.meaning()))
        .collect();
    worst_finding.insert(NO_FINDING, "The reviews find nothing to change.");

    Questions::from([
        (
            WORST_FINDING,
            Question::Choice {
                instructions: "Which tier is the most serious finding the reviews raise about \
                               this pull request? When reviewers disagree, take the more \
                               serious finding unless another reviewer shows it is wrong.",
                criteria: worst_finding,
            },
        ),
        (
            CONTAINED,
            Question::Noul {
                instructions: "If this change hides a defect the reviews missed, the damage \
                               stays inside the changed code and is easy to notice and undo.",
                criteria: NoulCriteria {
                    yes: "Tests, docs, tooling, or an isolated feature whose failure is loud, \
                          local, and fixed by a revert.",
                    no: "Stored data or its shape, auth or permissions, secrets, money, \
                         concurrency, deploys or infrastructure, interfaces other code depends \
                         on, or a failure that would go unnoticed.",
                },
            },
        ),
    ])
}

/// For each tier, the chance that it is the most serious finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorstFinding {
    pub must: Probability,
    pub should: Probability,
    pub could: Probability,
    pub none: Probability,
}

impl WorstFinding {
    pub fn of(&self, tier: ReviewTier) -> Probability {
        match tier {
            ReviewTier::Must => self.must,
            ReviewTier::Should => self.should,
            ReviewTier::Could => self.could,
        }
    }

    /// Polish never needs a reviewer, so only must and should findings count.
    fn needs_no_reviewer(&self) -> Probability {
        self.could.or(self.none)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MergeConfidence {
    pub worst_finding: WorstFinding,
    /// The chance that a defect the reviews missed would stay contained.
    pub contained: Probability,
}

impl MergeConfidence {
    pub fn from_answers(answers: &HashMap<String, Answer>) -> Result<Self> {
        let Some(Answer::Choice { probabilities }) = answers.get(WORST_FINDING) else {
            bail!("Jev gave no choice for {WORST_FINDING}");
        };
        let Some(Answer::Noul { noul }) = answers.get(CONTAINED) else {
            bail!("Jev gave no noul for {CONTAINED}");
        };
        let probability_of = |option: &str| {
            probabilities
                .get(option)
                .ok_or_else(|| anyhow!("Jev gave no probability for {option}"))
                .and_then(|value| Probability::new(*value))
        };

        Ok(Self {
            worst_finding: WorstFinding {
                must: probability_of(ReviewTier::Must.as_str())?,
                should: probability_of(ReviewTier::Should.as_str())?,
                could: probability_of(ReviewTier::Could.as_str())?,
                none: probability_of(NO_FINDING)?,
            },
            contained: Probability::new(*noul)?,
        })
    }

    pub fn score(&self) -> Probability {
        self.worst_finding.needs_no_reviewer().and(self.contained)
    }
}

/// What Jev concluded about one commit of a pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Judgment {
    /// The concrete model Jev resolved to.
    pub model: String,
    pub commit: CommitSha,
    /// The branch the diff was taken against, since retargeting a pull
    /// request changes its diff without a new commit.
    pub base_ref: String,
    pub confidence: MergeConfidence,
    pub coverage: Coverage,
}

impl Judgment {
    /// Incomplete evidence could hide the defect that should have stopped
    /// the merge, so it always needs a reviewer.
    pub fn needs_reviewer(&self, threshold: Probability) -> bool {
        self.coverage != Coverage::Complete || self.confidence.score() < threshold
    }

    /// The comment section. It names the commit and base it judged, since a
    /// later push or retarget is a diff Jev never saw.
    pub fn section(&self, threshold: Probability, questions: &Questions) -> String {
        let score = self.confidence.score();
        let verdict = if self.coverage == Coverage::MissingReview {
            format!(
                "🔴 **Merge confidence {score} / 1.00**, but an agent's review failed: \
                 ask a reviewer."
            )
        } else if self.coverage == Coverage::PartialDiff {
            format!(
                "🔴 **Merge confidence {score} / 1.00**, but Jev saw only part of the diff: \
                 ask a reviewer."
            )
        } else if self.needs_reviewer(threshold) {
            format!(
                "🔴 **Merge confidence {score} / 1.00**, below the {threshold} threshold: \
                 ask a reviewer."
            )
        } else {
            format!(
                "🟢 **Merge confidence {score} / 1.00**, at or above the {threshold} threshold: \
                 this commit can merge without another reviewer."
            )
        };
        let worst = &self.confidence.worst_finding;
        let tiers: Vec<_> = ReviewTier::ALL
            .iter()
            .map(|tier| format!("{tier} {}", worst.of(*tier)))
            .chain([format!("{NO_FINDING} {}", worst.none)])
            .collect();

        format!(
            "### jev · {}\nAt {} against `{}`.\n\n{verdict}\n\n\
             - Most serious finding: {}\n\
             - A missed defect stays contained: {}\n\n{}",
            self.model,
            self.commit,
            self.base_ref,
            tiers.join(" · "),
            self.confidence.contained,
            questions_details(questions)
        )
    }
}

/// The comment section when Jev could not judge the pull request.
pub fn failed_section(questions: &Questions) -> String {
    format!(
        "### jev\n\nFailed. The error is in Maid's logs.\n\n{}",
        questions_details(questions)
    )
}

fn questions_details(questions: &Questions) -> String {
    let questions =
        serde_json::to_string_pretty(questions).expect("Jev questions should serialize to JSON");
    format!(
        "<details>\n<summary>Jev questions</summary>\n\n```json\n{questions}\n```\n\n</details>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentKind;
    use crate::domain::ChangedFile;

    fn p(value: f64) -> Probability {
        Probability::new(value).unwrap()
    }

    fn confidence(
        must: f64,
        should: f64,
        could: f64,
        none: f64,
        contained: f64,
    ) -> MergeConfidence {
        MergeConfidence {
            worst_finding: WorstFinding {
                must: p(must),
                should: p(should),
                could: p(could),
                none: p(none),
            },
            contained: p(contained),
        }
    }

    fn assert_close(actual: Probability, expected: f64) {
        assert!(
            (actual.value() - expected).abs() < 1e-9,
            "{actual:?} is not {expected}"
        );
    }

    fn answers(probabilities: &[(&str, f64)], contained: f64) -> HashMap<String, Answer> {
        HashMap::from([
            (
                WORST_FINDING.to_string(),
                Answer::Choice {
                    probabilities: probabilities
                        .iter()
                        .map(|(option, value)| (option.to_string(), *value))
                        .collect(),
                },
            ),
            (CONTAINED.to_string(), Answer::Noul { noul: contained }),
        ])
    }

    fn changed_file(path: &str, patch: Option<&str>) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            status: "modified".to_string(),
            additions: 1,
            deletions: 0,
            patch: patch.map(str::to_string),
        }
    }

    fn diff(files: Vec<ChangedFile>) -> PullRequestDiff {
        PullRequestDiff {
            base_ref: "main".to_string(),
            changed_files: files.len() as u64,
            files,
        }
    }

    fn evidence(diff: &PullRequestDiff) -> MergeEvidence<'_> {
        MergeEvidence {
            pr_url: "https://github.com/o/r/pull/1",
            reviews: Vec::new(),
            diff,
        }
    }

    #[test]
    fn probabilities_stay_from_zero_to_one() {
        assert!(Probability::new(0.0).is_ok());
        assert!(Probability::new(1.0).is_ok());
        assert!(Probability::new(-0.1).is_err());
        assert!(Probability::new(1.1).is_err());
        assert!(Probability::new(f64::NAN).is_err());
    }

    #[test]
    fn confidence_needs_clean_reviews_and_a_contained_change() {
        let polish_only = confidence(0.02, 0.08, 0.5, 0.4, 0.9);
        let design_concern = confidence(0.05, 0.7, 0.15, 0.1, 0.9);
        let risky_change = confidence(0.0, 0.0, 0.1, 0.9, 0.2);

        assert_close(polish_only.score(), 0.81);
        assert_close(design_concern.score(), 0.225);
        assert_close(risky_change.score(), 0.2);
    }

    fn judgment(confidence: MergeConfidence, coverage: Coverage) -> Judgment {
        Judgment {
            model: "jev-1.13.0".to_string(),
            commit: CommitSha::parse(&"a".repeat(40)).unwrap(),
            base_ref: "main".to_string(),
            confidence,
            coverage,
        }
    }

    #[test]
    fn a_score_at_the_threshold_merges_without_a_reviewer() {
        let at = confidence(0.0, 0.0, 0.0, 1.0, 0.8);

        assert!(!judgment(at, Coverage::Complete).needs_reviewer(p(0.8)));
        assert!(judgment(at, Coverage::Complete).needs_reviewer(p(0.81)));
        assert!(judgment(at, Coverage::PartialDiff).needs_reviewer(p(0.0)));
        assert!(judgment(at, Coverage::MissingReview).needs_reviewer(p(0.0)));
    }

    #[test]
    fn reads_both_answers() {
        let parsed = MergeConfidence::from_answers(&answers(
            &[
                ("must", 0.1),
                ("should", 0.2),
                ("could", 0.3),
                ("none", 0.4),
            ],
            0.7,
        ))
        .unwrap();

        assert_eq!(parsed, confidence(0.1, 0.2, 0.3, 0.4, 0.7));
    }

    #[test]
    fn rejects_incomplete_or_mistyped_answers() {
        let missing_tier = answers(&[("must", 0.5), ("should", 0.5), ("could", 0.0)], 0.7);
        let out_of_range = answers(
            &[
                ("must", 0.0),
                ("should", 0.0),
                ("could", 0.0),
                ("none", 1.0),
            ],
            1.5,
        );
        let mut swapped = answers(&[], 0.7);
        swapped.insert(WORST_FINDING.to_string(), Answer::Noul { noul: 0.5 });

        assert_eq!(
            MergeConfidence::from_answers(&missing_tier)
                .unwrap_err()
                .to_string(),
            "Jev gave no probability for none"
        );
        assert!(MergeConfidence::from_answers(&out_of_range).is_err());
        assert_eq!(
            MergeConfidence::from_answers(&swapped)
                .unwrap_err()
                .to_string(),
            "Jev gave no choice for worst_finding"
        );
    }

    #[test]
    fn asks_about_every_tier_and_no_finding() {
        let questions = questions();
        let Question::Choice { criteria, .. } = &questions[WORST_FINDING] else {
            panic!("worst_finding should be a choice");
        };

        assert_eq!(
            criteria.keys().copied().collect::<Vec<_>>(),
            vec!["could", "must", "none", "should"]
        );
        assert_eq!(criteria["must"], ReviewTier::Must.meaning());
        assert!(matches!(questions[CONTAINED], Question::Noul { .. }));
    }

    #[test]
    fn state_carries_reviews_and_patches_within_budget() {
        let codex = AgentLabel {
            kind: AgentKind::Codex,
            model: "gpt-5.5".to_string(),
            effort: "high".to_string(),
        };
        let huge = "+".repeat(PATCH_BUDGET);
        let diff = diff(vec![
            changed_file("src/a.rs", Some("+a")),
            changed_file("src/huge.rs", Some(&huge)),
            changed_file("src/b.rs", Some("+b")),
        ]);

        let state = MergeEvidence {
            pr_url: "https://github.com/o/r/pull/1",
            reviews: vec![(&codex, Some("No findings."))],
            diff: &diff,
        }
        .state();

        assert_eq!(state["pull_request"], "https://github.com/o/r/pull/1");
        assert_eq!(
            state["reviews"],
            json!([{"reviewer": "codex · gpt-5.5 · high", "review": "No findings."}])
        );
        let patches: Vec<_> = state["changed_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| (file["path"].as_str().unwrap(), file["patch"].as_str()))
            .collect();
        assert_eq!(
            patches,
            vec![
                ("src/a.rs", Some("+a")),
                ("src/huge.rs", None),
                ("src/b.rs", Some("+b"))
            ]
        );
    }

    #[test]
    fn coverage_is_complete_only_when_jev_sees_every_patch() {
        let complete = diff(vec![
            changed_file("src/a.rs", Some("+a")),
            changed_file("src/b.rs", Some("+b")),
        ]);
        // GitHub omits the patch of a binary or a renamed file, whose
        // contents may still have changed.
        let binary = diff(vec![changed_file("logo.png", None)]);
        let over_budget = diff(vec![changed_file(
            "src/huge.rs",
            Some(&"+".repeat(PATCH_BUDGET + 1)),
        )]);
        let unlisted = PullRequestDiff {
            changed_files: 301,
            ..diff(vec![changed_file("src/a.rs", Some("+a"))])
        };

        assert_eq!(evidence(&complete).coverage(), Coverage::Complete);
        assert_eq!(evidence(&binary).coverage(), Coverage::PartialDiff);
        assert_eq!(evidence(&over_budget).coverage(), Coverage::PartialDiff);
        assert_eq!(evidence(&unlisted).coverage(), Coverage::PartialDiff);
    }

    #[test]
    fn a_failed_agent_leaves_the_evidence_incomplete() {
        let codex = AgentLabel {
            kind: AgentKind::Codex,
            model: "gpt-5.5".to_string(),
            effort: "high".to_string(),
        };
        let claude = AgentLabel {
            kind: AgentKind::Claude,
            model: "claude-opus-5-5".to_string(),
            effort: "high".to_string(),
        };
        let diff = diff(vec![changed_file("src/a.rs", Some("+a"))]);
        let evidence = MergeEvidence {
            pr_url: "https://github.com/o/r/pull/1",
            reviews: vec![(&codex, None), (&claude, Some("No findings."))],
            diff: &diff,
        };

        assert_eq!(evidence.coverage(), Coverage::MissingReview);
        assert_eq!(evidence.state()["reviews"][0]["review"], Value::Null);
    }

    #[test]
    fn judged_section_shows_the_verdict_and_what_it_rests_on() {
        let questions = questions();

        let green = judgment(confidence(0.02, 0.08, 0.3, 0.6, 0.9), Coverage::Complete)
            .section(p(0.8), &questions);
        let red = judgment(confidence(0.6, 0.2, 0.1, 0.1, 0.9), Coverage::Complete)
            .section(p(0.8), &questions);
        let partial = judgment(confidence(0.0, 0.0, 0.0, 1.0, 1.0), Coverage::PartialDiff)
            .section(p(0.8), &questions);
        let missing = judgment(confidence(0.0, 0.0, 0.0, 1.0, 1.0), Coverage::MissingReview)
            .section(p(0.8), &questions);

        let sha = "a".repeat(40);
        assert!(green.starts_with(&format!(
            "### jev · jev-1.13.0\nAt {sha} against `main`.\n\n🟢 **Merge confidence 0.81 / 1.00**, at or above \
             the 0.80 threshold: this commit can merge without another reviewer.\n\n\
             - Most serious finding: must 0.02 · should 0.08 · could 0.30 · none 0.60\n\
             - A missed defect stays contained: 0.90\n\n\
             <details>\n<summary>Jev questions</summary>\n\n```json\n{{"
        )));
        assert!(red.contains(
            "🔴 **Merge confidence 0.18 / 1.00**, below the 0.80 threshold: ask a reviewer."
        ));
        assert!(partial.contains(
            "🔴 **Merge confidence 1.00 / 1.00**, but Jev saw only part of the diff: ask a reviewer."
        ));
        assert!(missing.contains(
            "🔴 **Merge confidence 1.00 / 1.00**, but an agent's review failed: ask a reviewer."
        ));
        assert!(failed_section(&questions).starts_with("### jev\n\nFailed."));
    }
}
