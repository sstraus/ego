//! Where the diagnostics this workspace already emits actually go.
//!
//! `ARCHITECTURE.md`.
//!
//! Seven crates depend on `tracing` and there are seventy-four call sites. Until
//! this module existed, every one of those events was constructed and dropped:
//! no binary installed a subscriber, so `RUST_LOG` did nothing and code that
//! looked like it was reporting was not. Story `regression_case` had already moved real
//! evidence — upstream response bodies, removed from operator-visible errors
//! because those errors reach an ACP host in `error.data` — behind that curtain,
//! on the assumption that somebody would eventually draw it.
//!
//! ## Why an environment variable and not `-v`
//!
//! `CLI.md` §3 is the argument grammar of a *run*, and story `regression_case` froze
//! it with `-v`/`--verbose` recorded as excluded. A diagnostic level is not an
//! argument of a run: it changes nothing the run does, is recorded in no
//! evidence, and belongs in neither the generated reference nor the five
//! completion files. Reading it from the environment is what lets the capability
//! ship without amending a frozen contract — and it is the shape a *host* can
//! use, since something spawning `ego acp` sets variables for its child and
//! cannot inject arguments into a command line it did not compose.
//!
//! ## Why nothing is installed by default
//!
//! Because at any level that is not silent, this would print credentials.
//! `ego-ai`'s two body-logging sites may echo the operator's key back from a
//! failing endpoint or a proxy's interception page, and for a served endpoint
//! stderr is the host's log file — read, as the `regression_case` session put it, by an
//! operator who is not necessarily the one who owns the key. So an unset
//! `EGO_LOG` installs *nothing*, not a subscriber at level `off`, and the two
//! body sites are additionally held behind a target of their own: see
//! [`BODY_OFF`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

/// The filter every resolution starts from, before the operator's own is
/// appended.
///
/// `ego::body` is the target of provider request body logging and upstream
/// refusal bodies (`ego-ai`'s `http::refusal` and `oauth`'s login refusal). The
/// ordering is the mechanism: a bare `EGO_LOG=info`, or even `trace`, leaves
/// them off, because a directive naming a target beats a global level;
/// `EGO_LOG=ego::body=debug` turns them on, because a later directive for an
/// identical target replaces an earlier one.
///
/// The rule this generalises, and the reason it is a target rather than a
/// higher level: a level an operator picks to watch one subsystem must never
/// disclose another subsystem's secrets. An operator debugging why a client
/// cannot connect asks for `ego::listen`; they have not asked to write
/// credentials into a log, and must not receive that as a side effect.
const BODY_OFF: &str = "ego::body=off";

/// The variable that turns diagnostics on, in `EnvFilter` syntax.
pub const LOG: &str = "EGO_LOG";
/// The variable that sends them to a file instead of stderr.
pub const LOG_FILE: &str = "EGO_LOG_FILE";

/// What the environment asked for, once it has been understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to install, and nothing installed — not even a silent
    /// subscriber. Every stream is byte-for-byte what it was before this
    /// module existed.
    Off,
    /// Structured lines to stderr. Headless commands and the protocol servers,
    /// where nothing else owns that stream and stdout is protocol-clean.
    Stderr { filter: String },
    /// Structured lines appended to a file the operator named.
    File { filter: String, path: PathBuf },
}

/// Decide, without touching the filesystem or the process.
///
/// Split from [`install`] on purpose: every rule worth testing is here, and a
/// table of environment spellings does not need a subscriber installed — nor
/// could it have one, since installation is process-wide and one-shot.
///
/// `paints` is whether this invocation owns a prompt. It comes from the
/// invocation rather than from `isatty`, because what makes stderr unsafe is
/// not that a terminal is attached but that Reedline is repainting on it.
pub fn plan(log: Option<&str>, file: Option<&str>, paints: bool) -> Result<Plan, String> {
    let Some(filter) = log.map(str::trim).filter(|raw| !raw.is_empty()) else {
        // An empty value is the same as an unset one, deliberately: `EGO_LOG=`
        // is how a shell script disables an inherited setting for one command,
        // and treating it as a parse error would refuse to start a process the
        // operator was trying to make quieter.
        return Ok(Plan::Off);
    };
    // Validated here rather than at install time so a typo is a usage error
    // before any run work, and never a diagnostic that silently never arrives.
    validate(filter)?;

    let path = match file.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => {
            let path = Path::new(raw);
            if !path.is_absolute() {
                return Err(format!(
                    "{LOG_FILE} must be an absolute path, and {raw} is not. A \
                     relative path resolves against the working directory of \
                     whatever started this process, which for 'ego acp' is the \
                     host's and not yours"
                ));
            }
            Some(path.to_path_buf())
        }
        None => None,
    };

    match (paints, path) {
        (_, Some(path)) => Ok(Plan::File {
            filter: filter.to_owned(),
            path,
        }),
        (false, None) => Ok(Plan::Stderr {
            filter: filter.to_owned(),
        }),
        // Refused rather than half-delivered. The interactive surface is framed
        // through Reedline's sole terminal writer, and stderr lands on the same
        // terminal: a subscriber writing there races the prompt repaint. The
        // right answer is a `MakeWriter` that routes records through
        // `SurfaceOutput`, which is real work and is not built. Until it is,
        // the honest options are to refuse or to corrupt the display.
        (true, None) => Err(format!(
            "{LOG} is set for an interactive session, which paints its own \
             prompt: writing diagnostics to the same terminal would corrupt \
             it. Set {LOG_FILE} to an absolute path to collect them, or run \
             the command headlessly with 'ego run'"
        )),
    }
}

/// Whether every directive in `raw` is one `EnvFilter` understands.
fn validate(raw: &str) -> Result<(), String> {
    for directive in raw.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        directive.parse::<Directive>().map_err(|error| {
            format!(
                "{LOG} is not a valid filter: `{directive}` {error}. Use a \
                     level (`warn`, `debug`) or target directives \
                     (`ego::listen=info,ego_ai=debug`)"
            )
        })?;
    }
    Ok(())
}

/// Build the filter the plan describes, with [`BODY_OFF`] underneath it.
fn filter_of(raw: &str) -> Result<EnvFilter, String> {
    let mut filter = EnvFilter::new(BODY_OFF);
    for directive in raw.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        let parsed = directive
            .parse::<Directive>()
            .map_err(|error| format!("{LOG} is not a valid filter: `{directive}` {error}"))?;
        filter = filter.add_directive(parsed);
    }
    Ok(filter)
}

/// Install the plan, once, for the life of the process.
///
/// A failure is reported and never a panic: a logging failure must not change
/// what a run exits with. `try_init` and not `init` for the same reason — a
/// second installation attempt in a process that already has a subscriber is
/// something a test harness does routinely and is not an error worth ending a
/// run over.
pub fn install(plan: &Plan) -> Result<(), String> {
    let (raw, writer) = match plan {
        Plan::Off => return Ok(()),
        Plan::Stderr { filter } => (filter, BoxMakeWriter::new(std::io::stderr)),
        Plan::File { filter, path } => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|error| {
                    format!("{LOG_FILE} {} could not be opened: {error}", path.display())
                })?;
            (filter, BoxMakeWriter::new(Mutex::new(file)))
        }
    };

    // No ANSI, on either destination. A log file has no terminal to interpret
    // it, and on stderr the colour capability gate (`render::caps`) owns that
    // decision for output the operator asked for — a subscriber is not entitled
    // to reach around it.
    tracing_subscriber::fmt()
        .with_env_filter(filter_of(raw)?)
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .map_err(|error| format!("diagnostics could not be installed: {error}"))
}

/// Read the environment and install, in one call for the composition root.
pub fn install_from_environment(paints: bool) -> Result<(), String> {
    let log = std::env::var(LOG).ok();
    let file = std::env::var(LOG_FILE).ok();
    install(&plan(log.as_deref(), file.as_deref(), paints)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is not "quiet", it is *absent* — and the distinction is the
    /// whole security argument, so it is asserted rather than assumed.
    #[test]
    fn an_unset_or_emptied_variable_installs_nothing() {
        let log_file = std::env::current_dir()
            .expect("current directory")
            .join("ego.log");
        for (log, what) in [(None, "unset"), (Some(""), "empty"), (Some("  "), "blank")] {
            assert_eq!(
                plan(log, None, false),
                Ok(Plan::Off),
                "an {what} {LOG} must install nothing at all"
            );
        }
        // Naming a file without naming a level is still off: the destination is
        // not the switch.
        assert_eq!(plan(None, log_file.to_str(), false), Ok(Plan::Off));
    }

    /// A session that paints its own prompt is told to name a file, not left to
    /// discover a corrupted display.
    #[test]
    fn an_interactive_session_refuses_stderr_and_says_what_to_do_instead() {
        let log_file = std::env::current_dir()
            .expect("current directory")
            .join("ego.log");
        let refusal = plan(Some("info"), None, true).expect_err("interactive stderr is refused");
        assert!(
            refusal.contains(LOG_FILE),
            "the refusal must name the variable that fixes it: {refusal}"
        );
        assert_eq!(
            plan(Some("info"), log_file.to_str(), true),
            Ok(Plan::File {
                filter: "info".to_owned(),
                path: log_file,
            }),
            "naming a file is what makes an interactive session collectable"
        );
    }

    /// Headless and the protocol servers get stderr, because nothing else owns
    /// it there and stdout is protocol-clean.
    #[test]
    fn a_headless_session_writes_to_stderr() {
        assert_eq!(
            plan(Some("warn"), None, false),
            Ok(Plan::Stderr {
                filter: "warn".to_owned()
            })
        );
    }

    /// A relative path is refused because the process's working directory is
    /// not necessarily the operator's — least of all under a host.
    #[test]
    fn a_relative_log_file_is_refused_by_name() {
        let refusal =
            plan(Some("info"), Some("ego.log"), false).expect_err("a relative path is refused");
        assert!(
            refusal.contains("ego.log") && refusal.contains("absolute"),
            "the refusal must name the path and what is wrong with it: {refusal}"
        );
    }

    /// A typo disables diagnostics silently unless it is refused, which is the
    /// one failure mode an operator cannot detect by looking.
    #[test]
    fn a_malformed_filter_is_a_usage_error_and_not_a_silent_off() {
        let refusal = plan(Some("info,ego::listen=lousy"), None, false)
            .expect_err("an unknown level is refused");
        assert!(
            refusal.contains("ego::listen=lousy"),
            "the refusal must quote the directive that was wrong: {refusal}"
        );
        assert!(
            plan(Some("ego::listen=info,ego_ai=debug"), None, false).is_ok(),
            "a well-formed target directive is accepted"
        );
    }

    /// The security property, asserted against the filter itself rather than
    /// against prose: no ordinary level reaches `ego::body`, and naming the
    /// target does.
    ///
    /// `EnvFilter`'s `Display` renders the directives it resolved, so this
    /// reads what the process would actually enforce.
    #[test]
    fn no_bare_level_reaches_the_response_body_target() {
        for level in ["error", "warn", "info", "debug", "trace"] {
            let rendered = filter_of(level).expect("a level is a filter").to_string();
            assert!(
                rendered.contains("ego::body=off"),
                "`EGO_LOG={level}` must leave the body target off, or an \
                 operator watching connections starts logging credentials. \
                 filter: {rendered}"
            );
        }
        let asked = filter_of("info,ego::body=debug")
            .expect("naming the target is a filter")
            .to_string();
        assert!(
            !asked.contains("ego::body=off"),
            "an operator who names the target by hand has asked for it and \
             must get it: {asked}"
        );
    }
}
