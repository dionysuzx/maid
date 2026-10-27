const WITHHELD_NOTICE: &str =
    "Maid withheld this response because it contained what looks like a credential.";
const GITHUB_TOKEN_PREFIXES: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
const GITHUB_TOKEN_LENGTH: usize = 36;
const FINE_GRAINED_TOKEN_PREFIX: &str = "github_pat_";
const FINE_GRAINED_TOKEN_MIN_LENGTH: usize = 22;
const JWT_SEGMENT_MIN_LENGTH: usize = 10;

/// A comment Maid may post. The only constructor checks Codex's response for
/// credentials, so a prompt-injected task cannot leak one through a comment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentBody {
    text: String,
    withheld: bool,
}

impl CommentBody {
    pub fn from_response(response: &str) -> Self {
        if contains_credential(response) {
            Self {
                text: WITHHELD_NOTICE.to_string(),
                withheld: true,
            }
        } else {
            Self {
                text: response.to_string(),
                withheld: false,
            }
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn withheld(&self) -> bool {
        self.withheld
    }
}

/// GitHub tokens, which carry the bot's access, and JWTs, which Codex's own
/// login stores.
fn contains_credential(text: &str) -> bool {
    GITHUB_TOKEN_PREFIXES.iter().any(|prefix| {
        tails_after(text, prefix).any(|tail| {
            run_length(tail, |byte| byte.is_ascii_alphanumeric()) >= GITHUB_TOKEN_LENGTH
        })
    }) || tails_after(text, FINE_GRAINED_TOKEN_PREFIX).any(|tail| {
        run_length(tail, |byte| byte.is_ascii_alphanumeric() || byte == b'_')
            >= FINE_GRAINED_TOKEN_MIN_LENGTH
    }) || text
        .match_indices("eyJ")
        .any(|(start, _)| is_jwt(&text[start..]))
}

fn tails_after<'a>(text: &'a str, prefix: &'a str) -> impl Iterator<Item = &'a str> {
    text.match_indices(prefix)
        .map(move |(start, _)| &text[start + prefix.len()..])
}

fn run_length(text: &str, allowed: impl Fn(u8) -> bool) -> usize {
    text.bytes().take_while(|byte| allowed(*byte)).count()
}

fn is_jwt(text: &str) -> bool {
    let mut segments = text.splitn(3, '.');
    let (Some(header), Some(payload), Some(rest)) =
        (segments.next(), segments.next(), segments.next())
    else {
        return false;
    };
    [header, payload]
        .iter()
        .all(|segment| segment.len() >= JWT_SEGMENT_MIN_LENGTH && segment.bytes().all(is_base64url))
        && run_length(rest, is_base64url) >= JWT_SEGMENT_MIN_LENGTH
}

fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn withholds_responses_that_contain_a_credential() {
        let classic = format!("ghp_{}", "a1".repeat(18));
        let fine_grained = format!("github_pat_{}", "A_1".repeat(10));
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJtYWlkIn0.c2lnbmF0dXJlLXZhbHVl";

        for response in [
            format!("Found `{classic}`."),
            format!("token={fine_grained}"),
            format!("auth: {jwt}"),
        ] {
            let body = CommentBody::from_response(&response);
            assert!(body.withheld(), "{response}");
            assert_eq!(body.as_str(), WITHHELD_NOTICE);
        }
    }

    #[test]
    fn publishes_ordinary_review_text() {
        let response = "Looks good. `ghp_` tokens are revoked in 0123456789abcdef0123456789abcdef01234567; \
                        see https://github.com/o/r/pull/1 and eyJ-prefixed examples. Two more sentences. Done.";

        let body = CommentBody::from_response(response);

        assert!(!body.withheld());
        assert_eq!(body.as_str(), response);
    }
}
