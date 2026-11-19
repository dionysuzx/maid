use std::fmt;

/// How much a review finding obliges the author, named for the kind of harm
/// the finding does. The agents label their findings with these tiers, and
/// Jev reads the same definitions when it judges the reviews, so both speak
/// one vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewTier {
    /// Correctness or intent: the change is wrong.
    Must,
    /// Design: the change works but costs the codebase.
    Should,
    /// Polish: the change could read better.
    Could,
}

impl ReviewTier {
    /// Most serious first.
    pub const ALL: [Self; 3] = [Self::Must, Self::Should, Self::Could];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Must => "must",
            Self::Should => "should",
            Self::Could => "could",
        }
    }

    pub fn meaning(self) -> &'static str {
        match self {
            Self::Must => {
                "Correctness or intent. The change does not do what its author means, or it \
                 breaks something that worked: a bug, a regression, a security hole, lost data, \
                 or a broken contract. Fix before merging."
            }
            Self::Should => {
                "Design. The change works but adds complexity the codebase carries forward: \
                 tangled responsibilities, leaky boundaries, duplicated logic, or state that is \
                 hard to reason about. Fix unless the author gives a reason not to."
            }
            Self::Could => {
                "Polish. Naming, comments, wording, or small style choices. Improves the code \
                 but never blocks a merge."
            }
        }
    }

    /// The tier definitions as a Markdown list, for the `{{review_tiers}}`
    /// prompt placeholder.
    pub fn rubric() -> String {
        Self::ALL
            .iter()
            .map(|tier| format!("- **{tier}**: {}", tier.meaning()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl fmt::Display for ReviewTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rubric_lists_every_tier_most_serious_first() {
        let rubric = ReviewTier::rubric();
        let lines: Vec<_> = rubric.lines().collect();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("- **must**: Correctness or intent."));
        assert!(lines[1].starts_with("- **should**: Design."));
        assert!(lines[2].starts_with("- **could**: Polish."));
    }
}
