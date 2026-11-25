//! Recognized development commands: build, test, lint, format, package
//! tools, inspection and local git. Auto mode approves them without the
//! judge: they are the daily work of the user, and the sandbox and the
//! roots still confine what they touch. Anything not named here goes on to
//! the judge, never to a refusal.

use super::SpawnSpec;
use super::shell;

/// True when every command that `spec` runs is a recognized development command.
pub(super) fn is_development_command(spec: &SpawnSpec) -> bool {
    let preload = spec.env_policy().exact_bindings().iter().any(|binding| {
        let name = binding.name().as_os_str().to_string_lossy();
        name.starts_with("LD_") || name.starts_with("DYLD_") || name.starts_with("BASH_FUNC_")
    });
    if preload {
        return false;
    }
    if let Some(commands) = shell::static_commands(spec.argv()) {
        // Output filters may follow a development command in a pipeline, never
        // stand alone: a bare name could resolve to a workspace script.
        return commands.iter().any(|words| recognized(words))
            && commands
                .iter()
                .all(|words| recognized(words) || output_filter(words));
    }
    let argv: Option<Vec<String>> = spec
        .argv()
        .iter()
        .map(|word| word.to_str().map(str::to_owned))
        .collect();
    argv.is_some_and(|words| recognized(&words))
}

fn recognized(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    if basename(first) == "rtk" {
        return recognized(&words[1..]);
    }
    development_tool(words)
}

fn output_filter(words: &[String]) -> bool {
    matches!(
        words.first().map(|word| basename(word)),
        Some("wc" | "head" | "tail")
    ) && words.iter().skip(1).all(|word| !word.contains(['$', '`']))
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn development_tool(words: &[String]) -> bool {
    let program = basename(&words[0]);
    let rest = &words[1..];
    // A configuration override can name any program to run instead.
    if rest
        .iter()
        .any(|word| word == "--config" || word.starts_with("--config="))
    {
        return false;
    }
    match program {
        "cargo" => {
            let rest = match rest.first() {
                Some(word) if word.starts_with('+') => &rest[1..],
                _ => rest,
            };
            subcommand_in(
                rest,
                &[
                    "check", "build", "test", "nextest", "clippy", "fmt", "doc", "bench",
                    "metadata", "tree", "fetch", "run", "clean", "llvm-cov", "deny", "audit",
                    "mutants", "expand",
                ],
            )
        }
        "npm" | "pnpm" | "yarn" | "bun" => subcommand_in(
            rest,
            &[
                "test",
                "run",
                "run-script",
                "ci",
                "install",
                "i",
                "add",
                "build",
                "lint",
                "ls",
                "list",
                "outdated",
                "why",
                "start",
                "typecheck",
                "format",
                "vitest",
                "jest",
                "tsc",
                "eslint",
                "biome",
                "prettier",
            ],
        ),
        "go" => subcommand_in(
            rest,
            &[
                "build", "test", "vet", "fmt", "list", "mod", "run", "generate", "env", "version",
            ],
        ),
        "swift" => subcommand_in(rest, &["build", "test", "package"]),
        "git" => git(rest),
        "python" | "python3" => {
            rest.first().is_some_and(|word| word == "-m")
                && rest.get(1).is_some_and(|module| {
                    matches!(
                        module.as_str(),
                        "pytest" | "unittest" | "ruff" | "mypy" | "black" | "isort" | "flake8"
                    )
                })
        }
        "make" | "gmake" | "just" | "ninja" | "tsc" | "eslint" | "prettier" | "biome"
        | "vitest" | "jest" | "pytest" | "ruff" | "mypy" | "rustfmt" => true,
        _ => false,
    }
}

/// The subcommand is the first word: a global option before it (`git -c`,
/// `npm --prefix`) can change what runs, so it is not recognized.
fn subcommand_in(rest: &[String], allowed: &[&str]) -> bool {
    rest.first()
        .is_some_and(|word| allowed.contains(&word.as_str()))
}

fn git(rest: &[String]) -> bool {
    let Some(subcommand) = rest.first() else {
        return false;
    };
    let args = &rest[1..];
    match subcommand.as_str() {
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree"
        | "blame" | "describe" | "shortlog" | "grep" | "add" | "commit" | "merge-base"
        | "cat-file" | "for-each-ref" => true,
        "branch" => args.iter().all(|word| {
            matches!(
                word.as_str(),
                "--show-current" | "--list" | "-a" | "-r" | "-v" | "-vv"
            )
        }),
        "config" => {
            !args.is_empty()
                && args
                    .iter()
                    .all(|word| word.starts_with('-') || !word.contains('='))
                && args.iter().any(|word| {
                    matches!(
                        word.as_str(),
                        "--list" | "-l" | "--get" | "--get-all" | "--get-regexp" | "--show-origin"
                    )
                })
                && !args.iter().any(|word| {
                    matches!(
                        word.as_str(),
                        "--unset" | "--add" | "--replace-all" | "--edit" | "-e"
                    )
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn daily_development_commands_are_recognized() {
        for line in [
            "cargo check --offline",
            "cargo +nightly fmt --all",
            "cargo nextest run -E test(x)",
            "pnpm vitest run",
            "pnpm run build",
            "git commit -m fix",
            "git config --global --list",
            "git status --short",
            "rtk cargo test",
            "make test",
            "python3 -m pytest -q",
        ] {
            assert!(recognized(&words(line)), "{line}");
        }
    }

    #[test]
    fn only_output_filters_may_follow_a_development_command() {
        assert!(output_filter(&words("wc -l")));
        assert!(!output_filter(&words("ls")));
        assert!(!output_filter(&words("wc $HOME")));
    }

    #[test]
    fn commands_that_widen_what_runs_go_to_the_judge() {
        for line in [
            "cargo install ripgrep",
            "cargo --config build.rustc-wrapper=x check",
            "git -c core.sshCommand=x fetch",
            "git push origin main",
            "git config user.name x",
            "git config --global --unset user.name",
            "npm publish",
            "npx something",
            "curl https://example.com",
            "python3 -c print(1)",
        ] {
            assert!(!recognized(&words(line)), "{line}");
        }
    }
}
