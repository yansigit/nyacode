//! A deliberately small common subset of ECMAScript and Rust regex syntax.
//! Matching is restricted to ASCII script URL metadata, never arbitrary text.

use super::Budget;
use super::Result;
use super::SentinelFailure as Error;
use regex::RegexBuilder;
use regex_automata::Anchored;
use regex_automata::Input;
use regex_automata::dfa::Automaton;
use regex_automata::dfa::StartKind;
use regex_automata::dfa::dense;
use regex_automata::nfa::thompson;
use regex_automata::util::syntax;

struct Assertion {
    behind: bool,
    positive: bool,
    pattern: String,
}

struct CompiledAssertion {
    behind: bool,
    positive: bool,
    dfa: dense::DFA<Vec<u32>>,
}

impl CompiledAssertion {
    fn new(assertion: Assertion, budget: &mut Budget) -> Result<Self> {
        let pattern = translate(&assertion.pattern, budget)?;
        budget.allocate(512 * 1024)?;
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .start_kind(StartKind::Anchored)
                    .dfa_size_limit(Some(128 * 1024))
                    .determinize_size_limit(Some(128 * 1024)),
            )
            .syntax(
                syntax::Config::new()
                    .unicode(false)
                    .utf8(false)
                    .nest_limit(32),
            )
            .thompson(
                thompson::Config::new()
                    .reverse(assertion.behind)
                    .utf8(false)
                    .nfa_size_limit(Some(128 * 1024)),
            )
            .build(&pattern)
            .map_err(|error| {
                use std::error::Error as _;
                let nfa_limit = error
                    .source()
                    .and_then(|e| e.downcast_ref::<thompson::BuildError>())
                    .is_some_and(|e| e.size_limit().is_some());
                if error.is_size_limit_exceeded() || nfa_limit {
                    Error::AllocationLimit
                } else {
                    Error::MalformedScriptRegex
                }
            })?;
        budget.check()?;
        Ok(Self {
            behind: assertion.behind,
            positive: assertion.positive,
            dfa,
        })
    }

    fn matches(&self, input: &str, at: usize, budget: &mut Budget) -> Result<bool> {
        // A DFA consumes at most one transition per byte plus its final
        // boundary. Charge the full possible scan before entering the engine.
        let span = if self.behind { 0..at } else { at..input.len() };
        budget.work(span.len() + 1, 0)?;
        let query = Input::new(input)
            .span(span)
            .anchored(Anchored::Yes)
            .earliest(true);
        let found = if self.behind {
            self.dfa.try_search_rev(&query)
        } else {
            self.dfa.try_search_fwd(&query)
        }
        .map_err(|_| Error::UnsupportedScriptRegexGroup)?
        .is_some();
        budget.check()?;
        Ok(found == self.positive)
    }
}

// Assertions are regular expressions themselves, but nested assertions and
// backreferences stay outside the supported subset. Reverse DFAs preserve the
// original input context for word boundaries and anchors in look-behind.
fn leading_assertions<'a>(
    mut pattern: &'a str,
    budget: &mut Budget,
) -> Result<(Vec<Assertion>, &'a str)> {
    let mut assertions = Vec::new();
    loop {
        let (prefix, behind, positive) = if pattern.starts_with("(?<=") {
            (4, true, true)
        } else if pattern.starts_with("(?<!") {
            (4, true, false)
        } else if pattern.starts_with("(?=") {
            (3, false, true)
        } else if pattern.starts_with("(?!") {
            (3, false, false)
        } else {
            break;
        };
        if assertions.len() == 8 {
            return Err(Error::ScriptPatternLimit);
        }
        let mut i = prefix;
        let bytes = pattern.as_bytes();
        let mut depth = 1;
        let mut class = false;
        while depth > 0 {
            budget.step(depth)?;
            let byte = *bytes.get(i).ok_or(Error::MalformedScriptRegex)?;
            match byte {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'[' if !class => class = true,
                b']' if class => class = false,
                b'(' if !class => depth += 1,
                b')' if !class => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let inner = &pattern[prefix..i - 1];
        budget.allocate(inner.len() + 64)?;
        assertions.push(Assertion {
            behind,
            positive,
            pattern: inner.to_owned(),
        });
        pattern = &pattern[i..];
    }
    Ok((assertions, pattern))
}

// Locate a trailing positive assertion only when it applies to the
// whole preceding sequence. A top-level alternation changes that scope.
fn trailing_assertion<'a>(
    pattern: &'a str,
    has_leading: bool,
    budget: &mut Budget,
) -> Result<(&'a str, Option<String>)> {
    let bytes = pattern.as_bytes();
    let mut depth = 0usize;
    let mut class = false;
    let mut alternative = false;
    let mut candidate = None;
    let mut i = 0;
    while i < bytes.len() {
        budget.step(depth)?;
        match bytes[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'[' if !class => class = true,
            b']' if class => class = false,
            b'(' if !class => {
                if depth == 0 && bytes[i..].starts_with(b"(?=") {
                    candidate = Some(i);
                }
                depth += 1;
            }
            b')' if !class => depth = depth.checked_sub(1).ok_or(Error::MalformedScriptRegex)?,
            b'|' if !class && depth == 0 => alternative = true,
            _ => {}
        }
        i += 1;
    }
    if depth != 0 || class {
        return Err(Error::MalformedScriptRegex);
    }
    if alternative && has_leading {
        return Err(Error::UnsupportedScriptRegexGroup);
    }
    if !alternative && let Some(at) = candidate {
        let (mut assertions, rest) = leading_assertions(&pattern[at..], budget)?;
        if rest.is_empty() && assertions.len() == 1 {
            let assertion = assertions.remove(0);
            return Ok((&pattern[..at], Some(assertion.pattern)));
        }
    }
    Ok((pattern, None))
}

fn shared_group(bytes: &[u8], at: usize) -> bool {
    let rest = &bytes[at + 2..];
    if rest.starts_with(b":") {
        return true;
    }
    if rest.starts_with(b"<") {
        let Some(end) = rest.iter().position(|b| *b == b'>') else {
            return false;
        };
        let name = &rest[1..end];
        return !name.is_empty()
            && name.len() <= 32
            && (name[0].is_ascii_alphabetic() || name[0] == b'_')
            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    }
    // Scoped ECMAScript i/m/s modifiers only; Rust-only and unscoped flags
    // would give different semantics and are rejected before compilation.
    let Some(end) = rest.iter().position(|b| *b == b':') else {
        return false;
    };
    let flags = &rest[..end];
    if flags.is_empty() || flags.len() > 7 {
        return false;
    }
    let mut seen = [false; 4];
    for byte in flags {
        let slot = match byte {
            b'i' => 0,
            b'm' => 1,
            b's' => 2,
            b'-' => 3,
            _ => return false,
        };
        if seen[slot] {
            return false;
        }
        seen[slot] = true;
    }
    flags != b"-" && flags.last() != Some(&b'-')
}

fn translate(pattern: &str, budget: &mut Budget) -> Result<String> {
    // Rust character-class set operations, nonliteral look-around, flags, backreferences,
    // Unicode escapes and Unicode properties are outside this shared subset.
    if ["&&", "~~", "--", "[[", "++", "*+", "?+"]
        .iter()
        .any(|s| pattern.contains(s))
    {
        return Err(Error::UnsupportedScriptRegexSyntax);
    }
    let bytes = pattern.as_bytes();
    let mut translated = String::with_capacity(pattern.len());
    let mut i = 0;
    let mut class = false;
    while i < bytes.len() {
        budget.step(0)?;
        if !class && bytes[i] == b'(' && bytes.get(i + 1) == Some(&b'?') && !shared_group(bytes, i)
        {
            return Err(Error::UnsupportedScriptRegexGroup);
        }
        if bytes[i] == b'\\' {
            let escaped = *bytes.get(i + 1).ok_or(Error::MalformedScriptRegex)?;
            if !matches!(
                escaped,
                b'.' | b'*'
                    | b'+'
                    | b'?'
                    | b'['
                    | b']'
                    | b'('
                    | b')'
                    | b'{'
                    | b'}'
                    | b'|'
                    | b'^'
                    | b'$'
                    | b'-'
                    | b'/'
                    | b'\\'
                    | b'd'
                    | b'D'
                    | b'w'
                    | b'W'
                    | b's'
                    | b'S'
                    | b'b'
                    | b'B'
                    | b'n'
                    | b'r'
                    | b't'
                    | b'f'
                    | b'v'
            ) {
                return Err(Error::UnsupportedScriptRegexEscape);
            }
            if escaped != b'/' {
                translated.push('\\');
            }
            translated.push(char::from(escaped));
            i += 2;
        } else {
            if bytes[i] == b'[' {
                class = true;
            }
            if bytes[i] == b']' {
                class = false;
            }
            translated.push(char::from(bytes[i]));
            i += 1;
        }
    }
    Ok(translated)
}

pub(super) fn find(
    pattern: &str,
    scripts: &[String],
    budget: &mut Budget,
) -> Result<Option<String>> {
    budget.check()?;
    if pattern.len() > 512 || !pattern.is_ascii() || scripts.len() > 64 {
        return Err(Error::ScriptPatternLimit);
    }
    let (assertions, pattern) = leading_assertions(pattern, budget)?;
    let (pattern, trailing) = trailing_assertion(pattern, !assertions.is_empty(), budget)?;
    let translated = translate(pattern, budget)?;
    let assertions = assertions
        .into_iter()
        .map(|assertion| CompiledAssertion::new(assertion, budget))
        .collect::<Result<Vec<_>>>()?;
    let trailing = trailing
        .as_deref()
        .map(|pattern| translate(pattern, budget))
        .transpose()?;
    // Charge compilation and matching scratch space up front. Both engines are
    // bounded separately; no backtracking VM or untrusted JS is executed.
    budget.allocate(512 * 1024)?;
    // Capturing the body lets a trailing positive assertion constrain Rust's
    // ordinary greedy/lazy matching without returning the assertion's bytes.
    // Backreferences are excluded, so this extra group cannot renumber an input
    // backreference or change the guest-visible result.
    let translated = format!(
        "({translated}){}",
        trailing.map(|p| format!("(?:{p})")).unwrap_or_default()
    );
    let regex = RegexBuilder::new(&translated)
        .size_limit(128 * 1024)
        .dfa_size_limit(128 * 1024)
        .nest_limit(32)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => Error::AllocationLimit,
            _ => Error::MalformedScriptRegex,
        })?;
    budget.check()?;
    budget.allocate(regex.captures_len().saturating_mul(32))?;
    let mut captures = regex.capture_locations();
    for script in scripts {
        budget.step(0)?;
        if !script.is_ascii() || script.len() > 2048 {
            return Err(Error::UnsupportedScriptMetadata);
        }
        let mut at = 0;
        while at <= script.len() {
            budget.step(0)?;
            if regex.captures_read_at(&mut captures, script, at).is_none() {
                break;
            }
            let (start, end) = captures.get(1).ok_or(Error::MalformedScriptRegex)?;
            budget.check()?;
            let mut matches = true;
            for assertion in &assertions {
                if !assertion.matches(script, start, budget)? {
                    matches = false;
                    break;
                }
            }
            if matches {
                budget.allocate(end - start)?;
                return Ok(Some(script[start..end].to_owned()));
            }
            // ASCII was checked above, so every byte is a character boundary.
            // Searching the original string preserves ^, $ and word boundaries.
            at = start + 1;
        }
    }
    // A completed search with no match is distinct from invalid metadata. The
    // caller must preserve that distinction without inventing a script value.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::Limits;
    use tokio_util::sync::CancellationToken;

    fn budget() -> Budget {
        Budget::new(Limits::default(), CancellationToken::new())
    }

    #[test]
    fn matches_only_the_shared_ascii_subset_against_observed_metadata() {
        let scripts = vec!["https://chatgpt.com/app.js".into()];
        assert_eq!(
            find(
                r"https:\/\/chatgpt\.com/(?:app|bundle)\.js",
                &scripts,
                &mut budget()
            ),
            Ok(Some(scripts[0].clone()))
        );
        assert_eq!(find("missing", &scripts, &mut budget()), Ok(None));
        assert_eq!(
            find("app", &["https://example.test/é.js".into()], &mut budget()),
            Err(Error::UnsupportedScriptMetadata)
        );
    }

    #[test]
    fn literal_assertions_preserve_match_positions_and_original_input_anchors() {
        let scripts = vec!["https://chatgpt.com/app.js".into()];
        for (pattern, expected) in [
            (r"(?<=chatgpt\.com/)app\.js", "app.js"),
            (r"(?<!other/)app\.js", "app.js"),
            (r"(?=https://)https://chatgpt\.com", "https://chatgpt.com"),
            (r"(?!http:)https://.+", "https://chatgpt.com/app.js"),
            (r"(?<=/)(?=app)app\.js", "app.js"),
        ] {
            assert_eq!(
                find(pattern, &scripts, &mut budget()),
                Ok(Some(expected.into()))
            );
        }
        for pattern in [
            r"(?<=/)\bapp\b",
            r"(?<=/)^app",
            r"(?<!/)app",
            r"(?=http:)https",
        ] {
            let expected = if pattern.contains("\\b") {
                Ok(Some("app".into()))
            } else {
                Ok(None)
            };
            assert_eq!(find(pattern, &scripts, &mut budget()), expected);
        }
    }

    #[test]
    fn literal_assertions_share_the_instruction_and_input_limits() {
        let scripts = vec!["a".repeat(2048)];
        let mut limited = Budget::new(
            Limits {
                instructions: 40,
                ..Limits::default()
            },
            CancellationToken::new(),
        );
        assert_eq!(
            find(r"(?<=z)a", &scripts, &mut limited),
            Err(Error::InstructionLimit)
        );
        let too_many = format!("{}app", "(?=app)".repeat(9));
        assert_eq!(
            find(&too_many, &[], &mut budget()),
            Err(Error::ScriptPatternLimit)
        );
        assert_eq!(find(r"(?<=a+)app", &[], &mut budget()), Ok(None));
    }

    #[test]
    fn trailing_literals_and_shared_groups_preserve_greedy_and_lazy_boundaries() {
        let scripts = vec!["https://chatgpt.com/app.js".into()];
        for (pattern, expected) in [
            (r"(?<=/)app(?=\.js)", "app"),
            (r"(?<=chatgpt\.com/).*(?=\.js)", "app"),
            (r"(?<=chatgpt\.com/).*?(?=p)", "a"),
            (r"(?<asset>app)\.js", "app.js"),
            (r"(?i:APP)\.js", "app.js"),
            (r"(?:app|bundle)(?=\.js)", "app"),
        ] {
            assert_eq!(
                find(pattern, &scripts, &mut budget()),
                Ok(Some(expected.into()))
            );
        }
        for pattern in [
            r"(?<=/)missing|app",
            r"missing|app(?=\.js)",
            r"(?i)app",
            r"(?u:app)",
            r"(?i-:app)",
            r"(?ii:app)",
        ] {
            assert_eq!(
                find(pattern, &scripts, &mut budget()),
                Err(Error::UnsupportedScriptRegexGroup)
            );
        }
    }

    #[test]
    fn regular_assertions_keep_real_input_context_at_both_boundaries() {
        let scripts = vec!["https://chatgpt.com/app-v42.js".into()];
        for (pattern, expected) in [
            (r"(?<=app-v)[0-9]+(?=\.js$)", Some("42")),
            (r"(?<=/[a-z]+-v)\d+", Some("42")),
            (r"(?<=^https://chatgpt\.com/)app", Some("app")),
            (r"(?<=^chatgpt\.com/)app", None),
            (r"(?<=app\b)-v", Some("-v")),
            (r"(?<=app$)-v", None),
            (r"(?<=app\B)-v", None),
            (r"(?<=/app-[a-z]\B)\d+", Some("42")),
            (r"(?<![0-9])\d+(?=\.js$)", Some("42")),
            (r"(?=https?://).*?chatgpt\.com", Some("https://chatgpt.com")),
        ] {
            let expected = Ok(expected.map(str::to_owned));
            assert_eq!(
                find(pattern, &scripts, &mut budget()),
                expected,
                "{pattern}"
            );
        }
    }

    #[test]
    fn assertion_compilation_cannot_exceed_its_reserved_memory() {
        assert_eq!(
            find(r"(?<=(?:a{1000}){1000})b", &[], &mut budget()),
            Err(Error::AllocationLimit)
        );
        assert_eq!(
            find(r"(?<=(?=a))b", &[], &mut budget()),
            Err(Error::UnsupportedScriptRegexGroup)
        );
    }

    #[test]
    fn unsupported_syntax_is_rejected_and_empty_matches_remain_distinct_from_absence() {
        let scripts = vec!["https://chatgpt.com/app.js".into()];
        for (pattern, error) in [
            (r"app(?=a.*)suffix", Error::UnsupportedScriptRegexGroup),
            (r"(app)\1", Error::UnsupportedScriptRegexEscape),
            (r"\p{L}", Error::UnsupportedScriptRegexEscape),
            ("(?i)app", Error::UnsupportedScriptRegexGroup),
            ("[[a-z]--x]", Error::UnsupportedScriptRegexSyntax),
            ("é", Error::ScriptPatternLimit),
        ] {
            assert_eq!(find(pattern, &scripts, &mut budget()), Err(error));
        }
        for pattern in [".*?", "^", "$", ""] {
            assert_eq!(
                find(pattern, &scripts, &mut budget()),
                Ok(Some(String::new()))
            );
        }
        assert_eq!(find("no-match", &scripts, &mut budget()), Ok(None));
    }

    #[test]
    fn expanded_patterns_and_cancelled_matching_are_bounded() {
        assert_eq!(
            find("(?:a{1000}){1000}", &[], &mut budget()),
            Err(Error::AllocationLimit)
        );
        let token = CancellationToken::new();
        token.cancel();
        let mut budget = Budget::new(Limits::default(), token);
        assert_eq!(find("app", &[], &mut budget), Err(Error::Cancelled));
    }
}
