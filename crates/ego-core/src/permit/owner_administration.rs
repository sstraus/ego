//! Commands that change ego's own authority or configuration.
//!
//! A model that runs `ego roots add`, `ego config set` or `ego rules allow` through
//! `exec` widens its own perimeter, and nothing in the file or network policy sees
//! it: the call is an ordinary process. The owner decided (2026-11-09) that outside
//! `yolo` such a call follows the rules and the owner, like any other: it asks, or a
//! deny rule refuses it. No mode, wildcard allow rule or judge may approve it.
//!
//! This is a policy about direct use, not a sandbox. It reads the argv and shell
//! command lines for a command named `ego`, in the position where a program runs. It
//! cannot see an interpreter that builds the command at run time (`python3 -c`,
//! `node -e`), a script that calls `ego`, an alias, a renamed binary, a program run
//! through `cargo run`, or `sudo -u user ego`. Those stay under the file and exec
//! policy: the control files of ego are protected paths, and an unknown program is
//! asked about anyway. `docs/design-limits.md` lists the same limits.

use super::SpawnSpec;

/// Command groups of `ego` and the verbs that only read. Any other verb, or none,
/// changes ego's state. A verb added later is administrative until listed here.
const GROUPS: &[(&str, &[&str])] = &[
    ("roots", &["ls", "help"]),
    ("config", &["capabilities", "snapshot", "get", "ls", "help"]),
    ("rules", &["ls", "explain", "help"]),
    ("mcp", &["ls", "help"]),
    ("auth", &["status", "help"]),
    ("setup", &[]),
];

/// Options that give a nested `ego` run more authority than the one that started it.
const WIDENING_FLAGS: &[&str] = &[
    "--yolo",
    "--allow-dir",
    "--allow",
    "--set",
    "-c",
    "--mcp",
    "--trust-workspace",
    "--trust-development-programs",
];

/// Programs whose argument is a script or a command line that another shell reads.
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "fish", "eval"];

/// Programs and shell words after which the next word is still the program that runs.
const LAUNCHERS: &[&str] = &[
    "env",
    "sudo",
    "nice",
    "nohup",
    "time",
    "timeout",
    "xargs",
    "command",
    "exec",
    "builtin",
    "stdbuf",
    "setsid",
    "ionice",
    "caffeinate",
    "rtk",
    "{",
    "!",
    "if",
    "then",
    "do",
    "else",
    "elif",
    "while",
    "until",
];

const MAX_DEPTH: usize = 6;

/// True for a process operation that would change ego's authority or configuration.
pub(super) fn is_owner_administration_op(op: &super::Op) -> bool {
    matches!(op, super::Op::Spawn { spec } if is_owner_administration(spec))
}

/// True when the process this spec runs would change ego's authority or configuration.
pub(super) fn is_owner_administration(spec: &SpawnSpec) -> bool {
    let words: Vec<Option<String>> = spec
        .argv()
        .iter()
        .map(|word| Some(word.to_string_lossy().into_owned()))
        .collect();
    commands_administer(&words, 0)
}

/// `words` hold simple commands separated by `None`.
fn commands_administer(words: &[Option<String>], depth: usize) -> bool {
    if depth > MAX_DEPTH {
        // Nesting this deep is not ordinary work; ask.
        return true;
    }
    words.split(Option::is_none).any(|command| {
        let command: Vec<&str> = command.iter().flatten().map(String::as_str).collect();
        command_administers(&command, depth)
    })
}

fn command_administers(command: &[&str], depth: usize) -> bool {
    // A shell reads the words after it as a script, however many launchers stand before it.
    if let Some(shell) = command
        .iter()
        .position(|word| SHELLS.contains(&basename(word)))
    {
        for script in &command[shell + 1..] {
            if commands_administer(&script_words(script), depth + 1) {
                return true;
            }
        }
    }
    let start = program_position(command);
    let Some(program) = command.get(start) else {
        return false;
    };
    let rest = &command[start + 1..];
    if is_ego(program) {
        administers(rest, false) || widens(rest)
    } else if program.starts_with('$') {
        // The program is named by a substitution or a variable: it may be ego.
        administers(rest, true)
    } else {
        false
    }
}

/// The index of the word that names the program: after assignments and launchers.
fn program_position(command: &[&str]) -> usize {
    let mut launched = false;
    let mut index = 0;
    while let Some(word) = command.get(index) {
        if is_assignment(word) || LAUNCHERS.contains(&basename(word)) {
            launched = true;
        } else if !(launched
            && (word.starts_with('-') || word.starts_with(|c: char| c.is_ascii_digit())))
        {
            break;
        }
        index += 1;
    }
    index
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `ego`, a versioned copy such as `ego-0.4.3` or `ego.previous`, in any letter case
/// (the default macOS file system does not tell them apart).
fn is_ego(word: &str) -> bool {
    let name = basename(word);
    name.eq_ignore_ascii_case("ego")
        || name
            .get(..4)
            .is_some_and(|head| head.eq_ignore_ascii_case("ego."))
        || name
            .get(..4)
            .is_some_and(|head| head.eq_ignore_ascii_case("ego-"))
            && name[4..].starts_with(|c: char| c.is_ascii_digit())
}

/// `rest` is what follows the program word of one simple command. With `explicit_verb`
/// the group must be followed by a verb that is not read-only: an unknown program has
/// to name what it changes before it is asked about.
fn administers(rest: &[&str], explicit_verb: bool) -> bool {
    let before_separator: &[&str] = match rest.iter().position(|word| *word == "--") {
        Some(end) => &rest[..end],
        None => rest,
    };
    if before_separator
        .iter()
        .any(|word| matches!(*word, "--help" | "-h"))
    {
        return false;
    }
    // Brace expansion writes several words as one: `ego {roots,add,/p}`.
    let words: Vec<&str> = rest
        .iter()
        .flat_map(|word| {
            if word.contains('{') && word.contains(',') {
                word.split(['{', '}', ','])
                    .filter(|part| !part.is_empty())
                    .collect()
            } else {
                vec![*word]
            }
        })
        .collect();
    let Some((position, read_only)) = words.iter().enumerate().find_map(|(position, word)| {
        GROUPS
            .iter()
            .find(|(name, _)| name == word)
            .map(|(_, read_only)| (position, *read_only))
    }) else {
        return false;
    };
    match words[position + 1..]
        .iter()
        .find(|word| !word.starts_with('-'))
    {
        Some(verb) => !read_only.contains(verb),
        None => !explicit_verb,
    }
}

/// A nested `ego` run that asks for more authority than its parent has.
fn widens(rest: &[&str]) -> bool {
    let before_separator: &[&str] = match rest.iter().position(|word| *word == "--") {
        Some(end) => &rest[..end],
        None => rest,
    };
    before_separator.iter().enumerate().any(|(index, word)| {
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (*word, None),
        };
        if WIDENING_FLAGS.contains(&name) {
            return true;
        }
        if matches!(name, "--mode" | "--sandbox") {
            let value = inline.or_else(|| before_separator.get(index + 1).copied());
            return matches!(value, Some("yolo" | "off"));
        }
        false
    })
}

/// Split a shell script into words, with `None` between simple commands. Quotes group a
/// word and are not expanded. A command substitution is one placeholder word in its
/// command and its content follows as commands of their own. A here-document with a
/// quoted delimiter is data, not commands.
fn script_words(script: &str) -> Vec<Option<String>> {
    let chars: Vec<char> = script.chars().collect();
    let mut out: Vec<Option<String>> = Vec::new();
    let mut substitutions: Vec<String> = Vec::new();
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut index = 0;
    let end_word = |out: &mut Vec<Option<String>>, word: &mut String, started: &mut bool| {
        if *started {
            out.push(Some(std::mem::take(word)));
            *started = false;
        }
    };
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '\n' => {
                end_word(&mut out, &mut word, &mut started);
                out.push(None);
                for heredoc in std::mem::take(&mut heredocs) {
                    index = skip_heredoc(&chars, index + 1, &heredoc, &mut substitutions) - 1;
                }
            }
            ch if ch.is_whitespace() => end_word(&mut out, &mut word, &mut started),
            ';' | '|' | '&' | '(' | ')' => {
                end_word(&mut out, &mut word, &mut started);
                out.push(None);
            }
            '<' if chars.get(index + 1) == Some(&'<') && chars.get(index + 2) != Some(&'<') => {
                end_word(&mut out, &mut word, &mut started);
                let (heredoc, next) = read_heredoc_header(&chars, index + 2);
                heredocs.push(heredoc);
                index = next - 1;
            }
            '<' | '>' => end_word(&mut out, &mut word, &mut started),
            '`' => {
                let close = find_unescaped(&chars, index + 1, '`');
                substitutions.push(chars[index + 1..close].iter().collect());
                word.push_str("$SUBST");
                started = true;
                index = close;
            }
            '$' if chars.get(index + 1) == Some(&'(') => {
                let close = matching_paren(&chars, index + 1);
                substitutions.push(chars[index + 2..close.min(chars.len())].iter().collect());
                word.push_str("$SUBST");
                started = true;
                index = close;
            }
            // ANSI-C quoting: `$'ego'` is the word `ego`.
            '$' if chars.get(index + 1) == Some(&'\'') => {}
            '\'' => {
                started = true;
                index += 1;
                while index < chars.len() && chars[index] != '\'' {
                    word.push(chars[index]);
                    index += 1;
                }
            }
            '"' => {
                started = true;
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    match chars[index] {
                        '\\' => {
                            index += 1;
                            if let Some(escaped) = chars.get(index) {
                                word.push(*escaped);
                            }
                        }
                        '`' => {
                            let close = find_unescaped(&chars, index + 1, '`');
                            substitutions.push(chars[index + 1..close].iter().collect());
                            word.push_str("$SUBST");
                            index = close;
                        }
                        '$' if chars.get(index + 1) == Some(&'(') => {
                            let close = matching_paren(&chars, index + 1);
                            substitutions
                                .push(chars[index + 2..close.min(chars.len())].iter().collect());
                            word.push_str("$SUBST");
                            index = close;
                        }
                        other => word.push(other),
                    }
                    index += 1;
                }
            }
            '\\' => {
                // A backslash before a newline joins the lines.
                if chars.get(index + 1) == Some(&'\n') {
                    index += 1;
                } else if let Some(escaped) = chars.get(index + 1) {
                    started = true;
                    word.push(*escaped);
                    index += 1;
                }
            }
            other => {
                started = true;
                word.push(other);
            }
        }
        index += 1;
    }
    end_word(&mut out, &mut word, &mut started);
    for inner in substitutions {
        out.push(None);
        out.extend(script_words(&inner));
    }
    out
}

struct Heredoc {
    delimiter: String,
    quoted: bool,
    strip_tabs: bool,
}

/// Read `<<[-]['"]WORD['"]` starting after the `<<`; returns the header and the index after it.
fn read_heredoc_header(chars: &[char], mut index: usize) -> (Heredoc, usize) {
    let strip_tabs = chars.get(index) == Some(&'-');
    if strip_tabs {
        index += 1;
    }
    while chars.get(index).is_some_and(|c| *c == ' ' || *c == '\t') {
        index += 1;
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    while let Some(&c) = chars.get(index) {
        match c {
            '\'' | '"' => {
                quoted = true;
                index += 1;
                while chars.get(index).is_some_and(|inner| *inner != c) {
                    delimiter.push(chars[index]);
                    index += 1;
                }
            }
            '\\' => quoted = true,
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>') => break,
            c => delimiter.push(c),
        }
        index += 1;
    }
    (
        Heredoc {
            delimiter,
            quoted,
            strip_tabs,
        },
        index,
    )
}

/// Skip the body of a here-document that starts at `index`; returns the index after the
/// delimiter line. The body of an unquoted one still runs command substitutions.
fn skip_heredoc(
    chars: &[char],
    mut index: usize,
    heredoc: &Heredoc,
    substitutions: &mut Vec<String>,
) -> usize {
    while index < chars.len() {
        let end = chars[index..]
            .iter()
            .position(|c| *c == '\n')
            .map_or(chars.len(), |offset| index + offset);
        let line: String = chars[index..end].iter().collect();
        let line_to_compare = if heredoc.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        index = (end + 1).min(chars.len());
        if line_to_compare == heredoc.delimiter {
            break;
        }
        if !heredoc.quoted && (line.contains("$(") || line.contains('`')) {
            substitutions.push(line);
        }
    }
    index
}

/// The index of the next `target` that is not escaped, or the end of the text.
fn find_unescaped(chars: &[char], mut index: usize, target: char) -> usize {
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 1,
            c if c == target => return index,
            _ => {}
        }
        index += 1;
    }
    chars.len()
}

/// The index of the `)` that closes the `(` at `open`, or the end of the text.
fn matching_paren(chars: &[char], open: usize) -> usize {
    let mut depth = 0_usize;
    let mut index = open;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            quote @ ('\'' | '"') => {
                index = find_unescaped(chars, index + 1, quote);
            }
            _ => {}
        }
        index += 1;
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permit::EnvPolicy;

    fn spec(argv: &[&str]) -> SpawnSpec {
        SpawnSpec::new(
            argv.iter().map(|word| (*word).into()).collect(),
            None,
            None,
            EnvPolicy::Inherited,
        )
        .unwrap()
    }

    // Catches: text that only mentions ego, a path that ends in `/ego`, a crate name or a
    // heredoc body is treated as an administrative command and asks the owner each time.
    #[test]
    fn text_that_only_mentions_ego_is_not_a_command() {
        for argv in [
            &["git", "commit", "-m", "docs: ego roots add needs the owner"][..],
            &["bash", "-c", "git commit -m 'fix: ego config set mode'"],
            &["bash", "-c", "echo \"ego roots add\""],
            &["bash", "-c", "echo ego roots add"],
            &[
                "bash",
                "-c",
                "git commit -m \"feat: \\`ego roots add\\` floor\"",
            ],
            &[
                "bash",
                "-c",
                "git commit -F - <<'EOF'\nfeat: ego roots add floor\nego config set x\nEOF",
            ],
            &["bash", "-c", "cat <<EOF\nego roots add /p\nEOF"],
            &[
                "git",
                "-C",
                "/Users/x/Gits/personal/ego",
                "config",
                "--get",
                "user.name",
            ],
            &["find", "/x/ego", "-name", "config"],
            &["grep", "ego", "config"],
            &[
                "cargo",
                "nextest",
                "run",
                "-p",
                "ego-core",
                "-E",
                "test(roots)",
            ],
            &["bash", "-lc", "cargo test -p ego-tools config"],
            &["ego", "run", "fix the roots bug"],
            &["ego", "run", "--sandbox", "ro", "look at it"],
            &["ego", "ls", "--all"],
            &["ego", "roots", "add", "--help"],
            &["bash", "-c", "ego ls | grep roots"],
        ] {
            assert!(!is_owner_administration(&spec(argv)), "{argv:?}");
        }
    }

    // Catches: a wrapper, a nested shell, a substitution, a quote trick or an option with a
    // value hides an administrative command from the floor.
    #[test]
    fn shell_structure_does_not_hide_an_admin_command() {
        for argv in [
            &["bash", "-c", "echo $(ego roots add /p)"][..],
            &["bash", "-c", "echo \"$(ego config set x=1)\""],
            &["bash", "-c", "true && 'ego' roots add /p"],
            &["bash", "-c", "ls | xargs ego roots add"],
            &["sh", "-c", "(cd /tmp; ego rules allow 'exec(*)')"],
            &["bash", "-c", "`ego auth logout`"],
            &["/usr/bin/timeout", "5", "ego", "mcp", "rm", "x"],
            &["env", "bash", "-c", "ego roots add /p"],
            &["/usr/bin/env", "sh", "-c", "ego roots add /p"],
            &["sudo", "bash", "-c", "ego roots add /p"],
            &["timeout", "5", "bash", "-c", "ego roots add /p"],
            &["nice", "-n", "10", "ego", "roots", "add", "/p"],
            &["xargs", "sh", "-c", "ego roots add \"$1\"", "_"],
            &["bash", "-c", "bash -c 'ego roots add /p'"],
            &["bash", "-c", "eval 'ego roots add /p'"],
            &["bash", "-c", "echo \"$(echo '\"'; ego roots add /p)\""],
            &["bash", "-c", "ego \\\nroots add /p"],
            &["bash", "-c", "ego {roots,add,/p}"],
            &["bash", "-c", "$'ego' roots add /p"],
            &["bash", "-c", "$(command -v ego) roots add /p"],
            &["bash", "-c", "{ ego roots add /p; }"],
            &["bash", "-c", "cat <<EOF\n$(ego roots add /p)\nEOF"],
            &["EGO", "roots", "add", "/p"],
            &["ego", "mcp", "add", "x", "--stdio", "--", "npx", "-h"],
        ] {
            assert!(is_owner_administration(&spec(argv)), "{argv:?}");
        }
    }

    // Catches: a nested `ego run` with `--yolo`, `--sandbox off` or `--allow-dir` is not an
    // administrative command, so a model starts a more powerful agent under a wildcard rule.
    #[test]
    fn a_nested_ego_that_widens_its_own_authority_is_administrative() {
        for argv in [
            &["ego", "run", "--yolo", "task"][..],
            &["ego", "--sandbox", "off", "run", "task"],
            &["ego", "--sandbox=off", "task"],
            &["ego", "--mode", "yolo", "task"],
            &["ego", "--allow-dir", "/:rw", "task"],
            &["ego", "-c", "sandbox=\"off\"", "task"],
            &["ego", "--trust-workspace", "task"],
        ] {
            assert!(is_owner_administration(&spec(argv)), "{argv:?}");
        }
    }

    // Catches: a verb added to ego later is read-only by default, so a new way to change
    // the configuration escapes the floor.
    #[test]
    fn an_unlisted_verb_of_a_known_group_is_administrative() {
        assert!(is_owner_administration(&spec(&[
            "ego", "config", "migrate"
        ])));
        assert!(is_owner_administration(&spec(&["ego", "roots"])));
        assert!(!is_owner_administration(&spec(&[
            "ego", "config", "get", "mode"
        ])));
    }
}
