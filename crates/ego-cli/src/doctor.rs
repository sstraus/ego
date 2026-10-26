//! `ego doctor` — the one read-only health snapshot both surfaces render.
//!
//! Every probe here reads what its subsystem's own authority already knows.
//! Nothing logs in, refreshes a credential or a catalogue, reloads an
//! extension, rewrites config, resolves a hosted model, sends a provider
//! request, or executes anything. Where a fact cannot be had under that rule it
//! is reported `not_checked` with the reason and the command that would get it
//! — which is the whole difference between a diagnosis and a guess.
//!
//! See `ARCHITECTURE.md`.

use std::path::PathBuf;

use ego_core::config::Trust;
use ego_core::health::{
    HealthDetail, HealthItem, HealthItemId, HealthProbe, HealthSection, HealthSnapshot,
    HealthStatus, RedactionRefusal, VisibleRoots,
};
use ego_core::hook::HookCatalogue;
use ego_core::run::RunTermination;

use crate::auth::AuthStore;
use crate::cli::DoctorArgs;
use crate::config;
use crate::output::Stdout;

/// The text rendering, for the surface that has nowhere to put a `--json`.
///
/// `/doctor` calls this and `ego doctor` calls [`execute`]; both go through
/// [`snapshot`], which is the point.
///
/// The workspace is the caller's, not the process working directory: a session
/// opened with `ego --directory X` must be diagnosed against X.
pub fn observed(workspace: PathBuf) -> String {
    let home = config::ego_home();
    let roots = visible_roots(&workspace, home.as_ref());
    text(&snapshot(workspace, home, &roots))
}

/// The same health snapshot rendered for a bounded interactive terminal.
pub fn observed_with_width(workspace: PathBuf, width: usize) -> String {
    let home = config::ego_home();
    let roots = visible_roots(&workspace, home.as_ref());
    text_with_width(&snapshot(workspace, home, &roots), width)
}

/// Ask every probe once.
///
/// `pub` because both surfaces and the tests that bind them together need to
/// hold *one* snapshot taken over explicit roots. A test that had to change
/// the process working directory to reach this would be a test that cannot run
/// beside another one.
pub fn snapshot(workspace: PathBuf, home: Option<PathBuf>, roots: &VisibleRoots) -> HealthSnapshot {
    let perimeter = crate::perimeter::observe(&workspace, home.as_deref(), None).ok();
    snapshot_with_perimeter(workspace, home, roots, perimeter.as_ref())
}

/// Inspect an existing session without re-resolving its admitted perimeter.
pub fn observed_session(
    workspace: PathBuf,
    width: usize,
    perimeter: &crate::perimeter::Perimeter,
) -> String {
    observed_session_view(workspace, width, perimeter, true)
}

pub(crate) fn observed_session_view(
    workspace: PathBuf,
    width: usize,
    perimeter: &crate::perimeter::Perimeter,
    details: bool,
) -> String {
    let home = config::ego_home();
    let roots = visible_roots(&workspace, home.as_ref());
    let snapshot = snapshot_with_perimeter(workspace, home, &roots, Some(perimeter));
    if details {
        text_with_width(&snapshot, width)
    } else {
        let mut rows = vec![format!(
            "doctor: {} — inspect the results with '/doctor details'",
            status_word(snapshot.status())
        )];
        for report in &snapshot.sections {
            rows.push(format!(
                "{}: {}",
                report.section.label(),
                status_word(report.status())
            ));
            for item in report
                .items
                .iter()
                .filter(|item| matches!(item.status, HealthStatus::Warning | HealthStatus::Error))
            {
                rows.push(format!(
                    "  {}: {} — /doctor details",
                    item.id,
                    status_word(item.status)
                ));
            }
        }
        rows.join("\n")
    }
}

fn snapshot_with_perimeter(
    workspace: PathBuf,
    home: Option<PathBuf>,
    roots: &VisibleRoots,
    perimeter: Option<&crate::perimeter::Perimeter>,
) -> HealthSnapshot {
    let configuration = Configuration;
    let credentials = Credentials { home };
    let sessions = SessionStore {
        workspace: workspace.clone(),
        home: credentials.home.clone(),
    };
    let sandbox = Sandbox { perimeter };
    let catalogue = Catalogue;
    let mcp = Mcp;
    let skills = Skills { workspace };
    let hooks = Hooks;
    let acp = Acp;
    let probes: [&dyn HealthProbe; 9] = [
        &configuration,
        &credentials,
        &sessions,
        &sandbox,
        &catalogue,
        &mcp,
        &skills,
        &hooks,
        &acp,
    ];
    let mut snapshot = HealthSnapshot::observe(&probes, roots);
    let item = match perimeter {
        Some(perimeter) => {
            // The 2026-10-04 amendment names admitted roots only in this item.
            // The typed constructor does not alter any probe's visible roots.
            HealthItem::new(
                HealthItemId::probe("sandbox.perimeter"),
                if perimeter.measured_capabilities().is_some() {
                    HealthStatus::Ok
                } else {
                    HealthStatus::NotChecked
                },
                HealthDetail::perimeter(perimeter),
                HealthDetail::fixed("the admitted perimeter"),
            )
        }
        None => HealthItem::new(
            HealthItemId::probe("sandbox.perimeter"),
            HealthStatus::Error,
            HealthDetail::fixed("the effective perimeter could not be resolved or admitted"),
            HealthDetail::fixed("configuration resolution"),
        ),
    };
    if let Some(section) = snapshot
        .sections
        .iter_mut()
        .find(|section| section.section == HealthSection::Sandbox)
    {
        section.items.push(item);
    }
    snapshot
}

/// Read the facts and print them.
///
/// Exit 1 comes from the snapshot rather than from any single probe failing:
/// a subsystem that could not be read is one `error` item among others, and
/// the command that reported it did its job.
pub fn execute(args: &DoctorArgs, out: &Stdout) -> RunTermination {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = config::ego_home();
    let roots = visible_roots(&workspace, home.as_ref());
    let snapshot = snapshot(workspace, home, &roots);
    let rendered = match render(&snapshot, args.json) {
        Ok(rendered) => rendered,
        Err(error) => {
            return RunTermination::Failed {
                message: format!("the health snapshot could not be rendered: {error}"),
            };
        }
    };
    out.line(&rendered);
    // `TaskFailed` and not `Failed`: the command worked, and what it looked at
    // did not. `Failed` is reserved for ego's own defects, and an operator
    // whose MCP server is down has not found one.
    if snapshot.exit_code() == 0 {
        RunTermination::Done
    } else {
        RunTermination::TaskFailed
    }
}

/// The directories doctor may name a path inside.
///
/// The workspace and `EGO_HOME` because they are what the operator asked ego
/// to work on, and `/etc/ego` because a system administrator put it there on
/// purpose. Everything else on the machine stays out of output people paste
/// into issues.
pub fn visible_roots(workspace: &std::path::Path, home: Option<&PathBuf>) -> VisibleRoots {
    let mut roots = vec![
        workspace.to_path_buf(),
        PathBuf::from(config::SYSTEM_CONFIG_DIR),
    ];
    if let Some(home) = home {
        roots.push(home.clone());
    }
    VisibleRoots::new(roots)
}

/// One rendering of the snapshot, chosen by the caller and not by the terminal.
pub fn render(snapshot: &HealthSnapshot, json: bool) -> Result<String, serde_json::Error> {
    if json {
        serde_json::to_string(snapshot)
    } else {
        Ok(text(snapshot))
    }
}

/// The text rendering, shared with `/doctor`.
///
/// Every section appears, including the ones with nothing in them: a silent
/// subsystem reads as a healthy one, and that is the confusion this whole
/// projection exists to remove.
pub fn text(snapshot: &HealthSnapshot) -> String {
    let mut lines = vec![format!("doctor: {}", status_word(snapshot.status()))];
    for report in &snapshot.sections {
        lines.push(format!(
            "{}\t{}",
            report.section.label(),
            status_word(report.status())
        ));
        for item in &report.items {
            let mut line = format!(
                "  {}\t{}\t{}\t{}",
                item.id,
                status_word(item.status),
                item.detail,
                item.source
            );
            if let Some(remediation) = &item.remediation {
                line.push_str(&format!("\t{remediation}"));
            }
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Wrap the canonical text projection without removing semantic bytes.
pub fn text_with_width(snapshot: &HealthSnapshot, width: usize) -> String {
    text(snapshot)
        .lines()
        .flat_map(|line| crate::render::Line::plain(line.replace('\t', "  ")).wrap(width.max(1)))
        .map(|line| line.text())
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_word(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Ok => "ok",
        HealthStatus::NotChecked => "not_checked",
        HealthStatus::Warning => "warning",
        HealthStatus::Error => "error",
    }
}

/// An item whose detail came from somewhere on this machine.
fn fact(
    id: HealthItemId,
    status: HealthStatus,
    probe: &'static str,
    detail: String,
    source: &'static str,
    roots: &VisibleRoots,
) -> Result<HealthItem, RedactionRefusal> {
    Ok(HealthItem::new(
        id,
        status,
        HealthDetail::sanitized(probe, detail, roots)?,
        HealthDetail::fixed(source),
    ))
}

struct Configuration;

impl HealthProbe for Configuration {
    fn section(&self) -> HealthSection {
        HealthSection::Configuration
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        // The same loader `ego hooks ls` uses, which is the point: doctor must
        // not grow a second reading of `config.toml` that can disagree with
        // the one every command actually obeys.
        let item = match config::hook_listing_layers() {
            Ok(layers) => fact(
                HealthItemId::probe("config.layers"),
                HealthStatus::Ok,
                "config.layers",
                format!("{} configuration layer(s) parsed", layers.len()),
                "the system and user config.toml",
                roots,
            )?,
            Err(error) => fact(
                HealthItemId::probe("config.layers"),
                HealthStatus::Error,
                "config.layers",
                error.to_string(),
                "the system and user config.toml",
                roots,
            )?
            .remediation(HealthDetail::fixed("Run 'ego config edit'")),
        };
        Ok(vec![item])
    }
}

struct Credentials {
    home: Option<PathBuf>,
}

impl HealthProbe for Credentials {
    fn section(&self) -> HealthSection {
        HealthSection::Credentials
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        let Some(home) = &self.home else {
            return Ok(vec![
                HealthItem::new(
                    HealthItemId::probe("credentials.home"),
                    HealthStatus::Error,
                    HealthDetail::fixed(
                        "neither EGO_HOME nor HOME is set, so no credential store can be found",
                    ),
                    HealthDetail::fixed("the process environment"),
                )
                .remediation(HealthDetail::fixed("set EGO_HOME")),
            ]);
        };
        // An id, a kind and an expiry — the same three facts `ego auth status`
        // publishes. Nothing here reads secret material: the store has a
        // separate call for that and doctor does not make it.
        // `read_observed` and not `read`: the latter creates the home
        // directory and a lock file to take a write lock with, and doctor is
        // not allowed to bring into existence any part of what it diagnoses.
        let entries = match AuthStore::new(home).read_observed() {
            Ok(entries) => entries,
            Err(error) => {
                return Ok(vec![
                    fact(
                        HealthItemId::probe("credentials.store"),
                        HealthStatus::Error,
                        "credentials.store",
                        error.to_string(),
                        "the credential store",
                        roots,
                    )?
                    .remediation(HealthDetail::fixed("Run 'ego auth status'")),
                ]);
            }
        };
        if entries.is_empty() {
            return Ok(vec![
                HealthItem::new(
                    HealthItemId::probe("credentials.store"),
                    HealthStatus::NotChecked,
                    HealthDetail::fixed("no provider credentials are stored"),
                    HealthDetail::fixed("the credential store"),
                )
                .remediation(HealthDetail::fixed(
                    "List sign-in providers with 'ego auth login'",
                )),
            ]);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        entries
            .into_iter()
            .map(|(id, entry)| {
                // An expired credential is a warning and not an error: the
                // refresh that would fix it is a thing the next run does by
                // itself, and doctor refuses to perform it here.
                let (status, detail) = match entry.expires() {
                    Some(expires) if expires <= now => (
                        HealthStatus::Warning,
                        format!("{} credential expired", entry.kind()),
                    ),
                    Some(_) => (
                        HealthStatus::Ok,
                        format!("{} credential stored and unexpired", entry.kind()),
                    ),
                    None => (
                        HealthStatus::Ok,
                        format!("{} credential stored", entry.kind()),
                    ),
                };
                fact(
                    HealthItemId::about("credentials.entry", &id),
                    status,
                    "credentials.entry",
                    detail,
                    "the credential store",
                    roots,
                )
                .map(|item| item.remediation(HealthDetail::fixed("Run 'ego auth status'")))
            })
            .collect()
    }
}

struct SessionStore {
    workspace: PathBuf,
    home: Option<PathBuf>,
}

impl HealthProbe for SessionStore {
    fn section(&self) -> HealthSection {
        HealthSection::SessionStore
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        // The home the snapshot was taken with, not one re-read from the
        // environment: doctor must diagnose the store the runs it is being
        // asked about actually write to.
        let Some(home) = &self.home else {
            return Ok(vec![
                HealthItem::new(
                    HealthItemId::probe("sessions.root"),
                    HealthStatus::Error,
                    HealthDetail::fixed(
                        "no home directory to keep sessions in: set EGO_HOME or pass --session-dir",
                    ),
                    HealthDetail::fixed("the process environment"),
                )
                .remediation(HealthDetail::fixed("set EGO_HOME")),
            ]);
        };
        let root = crate::session::sessions_in(home);
        // Listing reads the manifest and the directory; it takes no lease and
        // repairs nothing, which is what makes it legal here.
        let store = crate::session::jsonl(root, self.workspace.clone());
        let item = match crate::catalogue::listing(&store, &self.workspace, true) {
            Ok(metas) => fact(
                HealthItemId::probe("sessions.listing"),
                HealthStatus::Ok,
                "sessions.listing",
                format!("{} durable session(s) readable", metas.len()),
                "the session store",
                roots,
            )?,
            Err(error) => fact(
                HealthItemId::probe("sessions.listing"),
                HealthStatus::Error,
                "sessions.listing",
                error.to_string(),
                "the session store",
                roots,
            )?
            .remediation(HealthDetail::fixed("Run 'ego ls --all'")),
        };
        Ok(vec![item])
    }
}

struct Sandbox<'a> {
    perimeter: Option<&'a crate::perimeter::Perimeter>,
}

impl HealthProbe for Sandbox<'_> {
    fn section(&self) -> HealthSection {
        HealthSection::Sandbox
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        if let Some(capabilities) = self.perimeter.and_then(|view| view.measured_capabilities()) {
            return Ok(vec![HealthItem::new(
                HealthItemId::probe("sandbox.capabilities"),
                if capabilities == ego_core::permit::ExecCaps::empty() {
                    HealthStatus::Warning
                } else {
                    HealthStatus::Ok
                },
                HealthDetail::sanitized("sandbox.capabilities", capabilities.to_string(), roots)?,
                HealthDetail::fixed("the existing execution backend measurement"),
            )]);
        }
        // Not an omission and not laziness. What a backend can actually do is
        // *measured* — the probe spawns real children against scratch and
        // out-of-workspace fixtures and watches which mutations the kernel
        // refuses. A headless inspection has no session verdict to read, and
        // executing processes
        // is exactly what doctor promised not to do. Reporting a guess from
        // the presence of `sandbox-exec` or a Landlock ABI number would be
        // worse than this: it would look like evidence.
        Ok(vec![
            HealthItem::new(
                HealthItemId::probe("sandbox.capabilities"),
                HealthStatus::NotChecked,
                HealthDetail::fixed(
                    "confinement capability is measured by executing probe children, which doctor does not do",
                ),
                HealthDetail::fixed("the execution backend"),
            )
            .remediation(HealthDetail::fixed(
                "Run a command with '--sandbox' to check the sandbox backend",
            )),
        ])
    }
}

struct Catalogue;

impl HealthProbe for Catalogue {
    fn section(&self) -> HealthSection {
        HealthSection::Catalogue
    }

    fn observe(&self, _roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        // The snapshot builder enumerates from each provider whenever its
        // slug is not already cached, and doctor cannot tell in advance
        // whether that will reach the network. A diagnosis that sometimes
        // spends a key and sometimes does not is not one an operator can run
        // safely, so it never runs it.
        Ok(vec![
            HealthItem::new(
                HealthItemId::probe("catalogue.snapshot"),
                HealthStatus::NotChecked,
                HealthDetail::fixed(
                    "a model catalogue snapshot may enumerate from a provider, and doctor sends no provider request",
                ),
                HealthDetail::fixed("the model catalogue"),
            )
            .remediation(HealthDetail::fixed("Run 'ego models'")),
        ])
    }
}

struct Mcp;

impl HealthProbe for Mcp {
    fn section(&self) -> HealthSection {
        HealthSection::Mcp
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        // What is *configured*, not what is admitted. Admission reconciles —
        // it starts servers and opens connections — so the live registry is
        // out of reach here by the same rule that keeps the sandbox out.
        let management = match config::mcp_management() {
            Ok(management) => management,
            Err(error) => {
                return Ok(vec![fact(
                    HealthItemId::probe("mcp.config"),
                    HealthStatus::Error,
                    "mcp.config",
                    error.to_string(),
                    "the resolved configuration",
                    roots,
                )?]);
            }
        };
        let configured = config::configured_mcp(&management.effective);
        if configured.is_empty() {
            return Ok(vec![HealthItem::new(
                HealthItemId::probe("mcp.config"),
                HealthStatus::Ok,
                HealthDetail::fixed("no MCP servers are configured"),
                HealthDetail::fixed("the resolved configuration"),
            )]);
        }
        configured
            .into_iter()
            .map(|(name, _config, _provenance)| {
                fact(
                    HealthItemId::about("mcp.server", name.to_string()),
                    HealthStatus::NotChecked,
                    "mcp.server",
                    "configured; connection not checked because that would start the server"
                        .to_owned(),
                    "the resolved configuration",
                    roots,
                )
                .map(|item| item.remediation(HealthDetail::fixed("Run 'ego mcp ls'")))
            })
            .collect()
    }
}

struct Skills {
    workspace: PathBuf,
}

impl HealthProbe for Skills {
    fn section(&self) -> HealthSection {
        HealthSection::Skills
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        // `false` for project trust: doctor is not the place a workspace
        // earns it, and reading an untrusted project's skills as admitted
        // would report a catalogue no run would actually use.
        let Some(discovery) = crate::skill::SkillDiscovery::for_workspace(&self.workspace, false)
        else {
            return Ok(vec![HealthItem::new(
                HealthItemId::probe("skills.discovery"),
                HealthStatus::Ok,
                HealthDetail::fixed("no skill directories exist"),
                HealthDetail::fixed("the skill catalogue"),
            )]);
        };
        let catalogue = match crate::skill::SkillCatalogue::discover(discovery, &[]) {
            Ok(catalogue) => catalogue,
            Err(error) => {
                return Ok(vec![fact(
                    HealthItemId::probe("skills.discovery"),
                    HealthStatus::Error,
                    "skills.discovery",
                    error.to_string(),
                    "the skill catalogue",
                    roots,
                )?]);
            }
        };
        let capture = match catalogue.capture() {
            Ok(capture) => capture,
            Err(error) => {
                return Ok(vec![fact(
                    HealthItemId::probe("skills.capture"),
                    HealthStatus::Error,
                    "skills.capture",
                    error.to_string(),
                    "the skill catalogue",
                    roots,
                )?]);
            }
        };
        let mut items = vec![fact(
            HealthItemId::probe("skills.catalogue"),
            HealthStatus::Ok,
            "skills.catalogue",
            format!(
                "{} skill(s) admitted, snapshot {}",
                capture.descriptors().len(),
                capture.snapshot_digest()
            ),
            "the skill catalogue",
            roots,
        )?];
        for (id, message) in crate::capability_roots::UNSUPPORTED_SKILL_SOURCES {
            items.push(
                HealthItem::new(
                    HealthItemId::probe(id),
                    HealthStatus::NotChecked,
                    HealthDetail::fixed(message),
                    HealthDetail::fixed("the skill root compatibility inventory"),
                )
                .remediation(HealthDetail::fixed(
                    "install reviewed standalone skills in an ego skill root",
                )),
            );
        }
        // A diagnostic is a skill that was rejected. It is a warning and not
        // an error because the rest of the catalogue is admitted and usable,
        // and it is never silent because a skill an operator wrote and cannot
        // invoke is exactly what they came here to find out about.
        for diagnostic in capture.diagnostics() {
            items.push(
                fact(
                    HealthItemId::probe("skills.diagnostic"),
                    HealthStatus::Warning,
                    "skills.diagnostic",
                    diagnostic.to_string(),
                    "the skill catalogue",
                    roots,
                )?
                .remediation(HealthDetail::fixed("Run 'ego run /skills'")),
            );
        }
        Ok(items)
    }
}

struct Hooks;

impl HealthProbe for Hooks {
    fn section(&self) -> HealthSection {
        HealthSection::Hooks
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        let layers = match config::hook_listing_layers() {
            Ok(layers) => layers,
            Err(error) => {
                return Ok(vec![fact(
                    HealthItemId::probe("hooks.config"),
                    HealthStatus::Error,
                    "hooks.config",
                    error.to_string(),
                    "the system and user config.toml",
                    roots,
                )?]);
            }
        };
        let item = match HookCatalogue::admit_typed(&layers, &[], Trust::Untrusted) {
            Ok(catalogue) => fact(
                HealthItemId::probe("hooks.catalogue"),
                HealthStatus::Ok,
                "hooks.catalogue",
                format!("{} hook(s) admitted", catalogue.specs().len()),
                "the admitted hook catalogue",
                roots,
            )?,
            Err(error) => fact(
                HealthItemId::probe("hooks.catalogue"),
                HealthStatus::Error,
                "hooks.catalogue",
                error.to_string(),
                "the admitted hook catalogue",
                roots,
            )?
            .remediation(HealthDetail::fixed("Run 'ego hooks ls'")),
        };
        Ok(vec![item])
    }
}

struct Acp;

impl HealthProbe for Acp {
    fn section(&self) -> HealthSection {
        HealthSection::Acp
    }

    fn observe(&self, roots: &VisibleRoots) -> Result<Vec<HealthItem>, RedactionRefusal> {
        let mut items = vec![HealthItem::new(
            HealthItemId::probe("acp.protocol"),
            HealthStatus::Ok,
            HealthDetail::fixed("this build serves ACP protocol version 1"),
            HealthDetail::fixed("the ACP router"),
        )];
        // The exact `_meta.ego` object a host reads out of `initialize`, taken
        // from the composer rather than restated. A host that cannot find
        // `_ego/pause` can now be told what this side actually publishes,
        // which is the fact regression case had to discover by connecting.
        let advertised = ego_acp::serve::durable_advertisement();
        let extensions = advertised
            .meta
            .as_ref()
            .and_then(|meta| meta.get("ego"))
            .and_then(serde_json::Value::as_object);
        match extensions {
            Some(extensions) if !extensions.is_empty() => {
                for (name, value) in extensions {
                    items.push(
                        fact(
                            HealthItemId::about("acp.extension", name),
                            HealthStatus::Ok,
                            "acp.extension",
                            value.to_string(),
                            "the ACP initialize answer",
                            roots,
                        )?
                        .remediation(HealthDetail::fixed(
                            "a host reads these from agentCapabilities._meta.ego",
                        )),
                    );
                }
            }
            _ => items.push(HealthItem::new(
                HealthItemId::probe("acp.extensions"),
                HealthStatus::Warning,
                HealthDetail::fixed("this build advertises no _ego extensions"),
                HealthDetail::fixed("the ACP initialize answer"),
            )),
        }
        Ok(items)
    }
}
