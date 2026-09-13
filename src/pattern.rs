use regex::Regex;

/// Returns true when `pattern` contains regex metacharacters and should be
/// interpreted as a full regex rather than a plain substring.
fn has_regex_metacharacters(pattern: &str) -> bool {
    pattern.chars().any(|c| {
        matches!(
            c,
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$' | '\\'
        )
    })
}

/// Case-insensitive match of `haystack` against `pattern`.
///
/// Plain text (no metacharacters) is treated as a case-insensitive substring.
/// Patterns with metacharacters are compiled as full regexes.
pub fn regex_matches(haystack: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let re_str = if has_regex_metacharacters(pattern) {
        pattern.to_string()
    } else {
        format!("(?i).*{}.*", regex::escape(pattern))
    };
    Regex::new(&re_str)
        .map(|re| re.is_match(haystack))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_matches_substring_case_insensitive() {
        assert!(regex_matches("WENDYS #1234", "wendy"));
        assert!(!regex_matches("McDonalds", "wendy"));
    }

    #[test]
    fn empty_pattern_never_matches() {
        assert!(!regex_matches("anything", ""));
    }

    #[test]
    fn regex_metacharacters_used_as_full_pattern() {
        assert!(regex_matches("Restaurant A", "Rest.*ant"));
        assert!(!regex_matches("Cafe", "Rest.*ant"));
    }
}
