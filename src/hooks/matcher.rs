//! Tool-name glob matching for `PreToolUse` / `PostToolUse` matchers.
//!
//! Follows Claude Code's matcher semantics: a pattern is a `|`-separated
//! list of alternatives, each alternative being an exact name or a glob
//! using `*` at the start and/or end.

/// A single configured hook entry: the `matcher` glob (defaults to `"*"`)
/// and the shell `command` to run for matching tool calls.
#[derive(Debug, Clone)]
pub struct MatchedHook {
    /// The raw matcher pattern, e.g. `"Read|Write"`.
    pub matcher: String,
    /// The shell command to execute.
    pub command: String,
}

/// Match a single glob against arbitrary text.
///
/// `*` may sit at the start, the end, or both; without one the match is
/// exact. Case-sensitive. Shared by hook matchers and `[permissions]` rule
/// globs.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix('*') {
        if let Some(needle) = rest.strip_suffix('*') {
            return text.contains(needle);
        }
        return text.ends_with(rest);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    text == pattern
}

/// Match a single alternative glob against a tool name.
fn single_match(pattern: &str, name: &str) -> bool {
    glob_match(pattern, name)
}

/// Return `true` when `name` matches the (possibly `|`-separated) pattern.
pub fn tool_name_matches(pattern: &str, name: &str) -> bool {
    pattern.split('|').any(|alt| single_match(alt.trim(), name))
}

/// All hooks whose matcher matches `name`, in configuration order.
pub fn matching_hooks<'a>(hooks: &'a [MatchedHook], name: &str) -> Vec<&'a MatchedHook> {
    hooks
        .iter()
        .filter(|h| tool_name_matches(&h.matcher, name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(matcher: &str, command: &str) -> MatchedHook {
        MatchedHook {
            matcher: matcher.to_string(),
            command: command.to_string(),
        }
    }

    #[test]
    fn test_wildcard_matches_everything() {
        assert!(tool_name_matches("*", "read"));
        assert!(tool_name_matches("*", "shell"));
        assert!(tool_name_matches("*", "anything_else"));
    }

    #[test]
    fn test_prefix_glob() {
        assert!(tool_name_matches("Read*", "Read"));
        assert!(tool_name_matches("Read*", "ReadFile"));
        assert!(!tool_name_matches("Read*", "WriteFile"));
    }

    #[test]
    fn test_suffix_glob() {
        assert!(tool_name_matches("*Read", "FileRead"));
        assert!(!tool_name_matches("*Read", "Reader"));
    }

    #[test]
    fn test_contains_glob() {
        assert!(tool_name_matches("*Read*", "WebFetchRead"));
        assert!(!tool_name_matches("*Read*", "Writer"));
    }

    #[test]
    fn test_exact_match() {
        assert!(tool_name_matches("read", "read"));
        assert!(!tool_name_matches("read", "reader"));
    }

    #[test]
    fn test_alternates() {
        assert!(tool_name_matches("Read|Write", "Read"));
        assert!(tool_name_matches("Read|Write", "Write"));
        assert!(!tool_name_matches("Read|Write", "Exec"));
    }

    #[test]
    fn test_alternate_globs() {
        assert!(tool_name_matches("Read*|Write*", "ReadFile"));
        assert!(tool_name_matches("Read*|Write*", "WriteFile"));
        assert!(!tool_name_matches("Read*|Write*", "Exec"));
    }

    #[test]
    fn test_matching_hooks_preserves_order() {
        let hooks = vec![
            hook("*", "first"),
            hook("rea*", "second"),
            hook("read", "third"),
        ];
        let hits = matching_hooks(&hooks, "read");
        let commands: Vec<&str> = hits.iter().map(|h| h.command.as_str()).collect();
        // "read" matches all three patterns; order preserved.
        assert_eq!(commands, vec!["first", "second", "third"]);
    }

    #[test]
    fn test_matching_hooks_is_case_sensitive() {
        // CC matchers are case-sensitive: "Read*" does not match "read".
        let hooks = vec![hook("*", "all"), hook("Read*", "capitalized")];
        let commands: Vec<&str> = matching_hooks(&hooks, "read")
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, vec!["all"]);
        assert!(matching_hooks(&hooks, "ReadFile").len() == 2);
    }

    #[test]
    fn test_matching_hooks_no_match() {
        let hooks = vec![hook("Read*", "first")];
        assert!(matching_hooks(&hooks, "Exec").is_empty());
    }
}
