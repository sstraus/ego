//! The strict, versioned agent-file input of `CLI.md` §5.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use ego_core::config::{ConfigLayer, Source, Sourced};
use yaml_rust2::parser::{Event, EventReceiver, Parser};
use yaml_rust2::scanner::TScalarStyle;

use crate::cli::UsageError;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_COLLECTION_ENTRIES: usize = 256;
const MAX_SCALAR_BYTES: usize = 16 * 1024;

/// One fully checked agent file, ready to enter prompt and config resolution.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AgentFile {
    path: PathBuf,
    body: String,
    supplied_vars: Vec<String>,
    layer_toml: String,
    schema: Option<String>,
}

impl std::fmt::Debug for AgentFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentFile")
            .field("path", &self.path)
            .field("supplied_vars", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl AgentFile {
    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    pub(crate) fn layer(&self) -> Result<Sourced<ConfigLayer>, UsageError> {
        ConfigLayer::parse(Source::AgentFile, &self.layer_toml).map_err(|error| {
            file_error(
                &self.path,
                format!("contains invalid configuration: {error}"),
            )
        })
    }

    pub(crate) fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
enum Node {
    Scalar { value: String, plain: bool },
    Sequence(Vec<Node>),
    Mapping(Vec<(Node, Node)>),
}

#[derive(Debug)]
enum Frame {
    Sequence(Vec<Node>),
    Mapping {
        entries: Vec<(Node, Node)>,
        key: Option<Node>,
    },
}

#[derive(Default)]
struct StrictReceiver {
    stack: Vec<Frame>,
    root: Option<Node>,
    documents: usize,
    error: Option<String>,
}

impl StrictReceiver {
    fn fail(&mut self, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }

    fn start(&mut self, frame: Frame, anchor: usize, tagged: bool) {
        if anchor != 0 {
            self.fail("anchors are not allowed");
        }
        if tagged {
            self.fail("tags are not allowed");
        }
        // The root collection is depth zero. A value nested through sixteen
        // parent collections is admitted; the next collection is not.
        if self.stack.len() > MAX_DEPTH {
            self.fail(format!("YAML nesting exceeds {MAX_DEPTH}"));
        }
        self.stack.push(frame);
    }

    fn push(&mut self, node: Node) {
        let full = match self.stack.last() {
            Some(Frame::Sequence(items)) => items.len() >= MAX_COLLECTION_ENTRIES,
            Some(Frame::Mapping { entries, key }) => {
                key.is_some() && entries.len() >= MAX_COLLECTION_ENTRIES
            }
            None => false,
        };
        if full {
            self.fail(format!(
                "a YAML collection contains more than {MAX_COLLECTION_ENTRIES} entries"
            ));
            return;
        }
        match self.stack.last_mut() {
            Some(Frame::Sequence(items)) => items.push(node),
            Some(Frame::Mapping { entries, key }) => {
                if let Some(key_node) = key.take() {
                    entries.push((key_node, node));
                } else {
                    *key = Some(node);
                }
            }
            None if self.root.is_none() => self.root = Some(node),
            None => self.fail("the frontmatter contains more than one root value"),
        }
    }

    fn end_sequence(&mut self) {
        match self.stack.pop() {
            Some(Frame::Sequence(items)) => self.push(Node::Sequence(items)),
            _ => self.fail("malformed YAML sequence"),
        }
    }

    fn end_mapping(&mut self) {
        let Some(Frame::Mapping { entries, key }) = self.stack.pop() else {
            self.fail("malformed YAML mapping");
            return;
        };
        if key.is_some() {
            self.fail("a YAML mapping key has no value");
            return;
        }
        let mut names = BTreeSet::new();
        for (key, _) in &entries {
            let Node::Scalar { value, plain } = key else {
                self.fail("YAML mapping keys must be strings");
                return;
            };
            if *plain && plain_is_non_string(value) {
                self.fail("YAML mapping keys must be strings");
                return;
            }
            if *plain && value == "<<" {
                self.fail("YAML merge keys are not allowed");
                return;
            }
            if !names.insert(value.clone()) {
                self.fail(format!("duplicate YAML key `{value}`"));
                return;
            }
        }
        self.push(Node::Mapping(entries));
    }
}

impl EventReceiver for StrictReceiver {
    fn on_event(&mut self, event: Event) {
        match event {
            Event::DocumentStart => self.documents += 1,
            Event::Alias(_) => self.fail("aliases are not allowed"),
            Event::Scalar(value, style, anchor, tag) => {
                if anchor != 0 {
                    self.fail("anchors are not allowed");
                }
                if tag.is_some() {
                    self.fail("tags are not allowed");
                }
                if value.len() > MAX_SCALAR_BYTES {
                    self.fail(format!("a YAML scalar exceeds {MAX_SCALAR_BYTES} bytes"));
                }
                self.push(Node::Scalar {
                    value,
                    plain: matches!(style, TScalarStyle::Plain),
                });
            }
            Event::SequenceStart(anchor, tag) => {
                self.start(Frame::Sequence(Vec::new()), anchor, tag.is_some());
            }
            Event::SequenceEnd => self.end_sequence(),
            Event::MappingStart(anchor, tag) => self.start(
                Frame::Mapping {
                    entries: Vec::new(),
                    key: None,
                },
                anchor,
                tag.is_some(),
            ),
            Event::MappingEnd => self.end_mapping(),
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
        }
    }
}

#[derive(Default)]
struct Fields {
    model: Option<String>,
    mode: Option<String>,
    sandbox: Option<String>,
    allow: Vec<String>,
    schema: Option<String>,
    vars: Vec<String>,
}

/// Open, bound, parse, and substitute one executable agent file.
pub(crate) fn load(requested_path: &Path, supplied: &[String]) -> Result<AgentFile, UsageError> {
    let path = std::fs::canonicalize(requested_path).map_err(|error| {
        UsageError(format!(
            "agent file {} could not be resolved: {error}",
            requested_path.display()
        ))
    })?;
    let mut file = std::fs::File::open(&path).map_err(|error| {
        UsageError(format!(
            "agent file {} could not be read: {error}",
            path.display()
        ))
    })?;
    if file
        .metadata()
        .map_err(|error| {
            UsageError(format!(
                "agent file {} could not be read: {error}",
                path.display()
            ))
        })?
        .len()
        > MAX_FILE_BYTES
    {
        return Err(file_error(&path, format!("exceeds {MAX_FILE_BYTES} bytes")));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            UsageError(format!(
                "agent file {} could not be read: {error}",
                path.display()
            ))
        })?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(file_error(&path, format!("exceeds {MAX_FILE_BYTES} bytes")));
    }
    let text = String::from_utf8(bytes).map_err(|_| file_error(&path, "is not valid UTF-8"))?;
    let (header, body) = envelope(&path, &text)?;
    let fields = fields(&path, header)?;
    let supplied_vars = supplied_vars(&path, supplied)?;
    let body = substitute(&path, body, &fields.vars, &supplied_vars)?;
    let mut toml = String::new();
    for (key, value) in [
        ("model", fields.model.as_deref()),
        ("mode", fields.mode.as_deref()),
        ("sandbox", fields.sandbox.as_deref()),
    ] {
        if let Some(value) = value {
            toml.push_str(&format!("{key} = {}\n", quote(value)));
        }
    }
    if !fields.allow.is_empty() {
        let rules = fields
            .allow
            .iter()
            .map(|rule| quote(rule))
            .collect::<Vec<_>>();
        toml.push_str(&format!("permissions.allow = [{}]\n", rules.join(", ")));
    }
    ConfigLayer::parse(Source::AgentFile, &toml)
        .map_err(|error| file_error(&path, format!("contains invalid configuration: {error}")))?;
    let schema = fields.schema.map(|schema| relative_resource(&path, schema));

    Ok(AgentFile {
        path,
        body,
        supplied_vars: supplied.to_vec(),
        layer_toml: toml,
        schema,
    })
}

fn envelope<'a>(path: &Path, text: &'a str) -> Result<(&'a str, &'a str), UsageError> {
    let mut offset = 0;
    if text.starts_with("#!") {
        offset = text
            .find('\n')
            .map(|index| index + 1)
            .ok_or_else(|| file_error(path, "has a shebang but no YAML frontmatter"))?;
    }
    let (opening, after_opening) = line(text, offset);
    if opening != "---" {
        return Err(file_error(
            path,
            "must begin with a standalone `---` delimiter",
        ));
    }
    let header_start = after_opening;
    let mut cursor = header_start;
    while cursor < text.len() {
        let (content, next) = line(text, cursor);
        if content == "---" {
            let header = &text[header_start..cursor];
            if header.len() > MAX_FRONTMATTER_BYTES {
                return Err(file_error(
                    path,
                    format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"),
                ));
            }
            let body = &text[next..];
            if body.is_empty() {
                return Err(file_error(path, "has an empty prompt body"));
            }
            return Ok((header, body));
        }
        if next == cursor {
            break;
        }
        cursor = next;
        if cursor.saturating_sub(header_start) > MAX_FRONTMATTER_BYTES {
            return Err(file_error(
                path,
                format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"),
            ));
        }
    }
    Err(file_error(
        path,
        "has no closing standalone `---` delimiter",
    ))
}

fn line(text: &str, start: usize) -> (&str, usize) {
    let rest = &text[start..];
    match rest.find('\n') {
        Some(relative_end) => {
            let end = start + relative_end;
            (
                text[start..end]
                    .strip_suffix('\r')
                    .unwrap_or(&text[start..end]),
                end + 1,
            )
        }
        None => (
            text[start..].strip_suffix('\r').unwrap_or(&text[start..]),
            text.len(),
        ),
    }
}

fn fields(path: &Path, header: &str) -> Result<Fields, UsageError> {
    let mut receiver = StrictReceiver::default();
    Parser::new_from_str(header)
        .load(&mut receiver, true)
        .map_err(|error| file_error(path, format!("contains invalid YAML: {error}")))?;
    if let Some(error) = receiver.error {
        return Err(file_error(path, error));
    }
    if receiver.documents != 1 {
        return Err(file_error(path, "must contain exactly one YAML document"));
    }
    let Node::Mapping(entries) = receiver
        .root
        .ok_or_else(|| file_error(path, "has empty frontmatter"))?
    else {
        return Err(file_error(path, "frontmatter must be a mapping"));
    };
    let mut entries = entries
        .into_iter()
        .map(|(key, value)| match key {
            Node::Scalar { value: key, .. } => Ok((key, value)),
            _ => Err(file_error(path, "frontmatter keys must be strings")),
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let version = entries
        .remove("version")
        .ok_or_else(|| file_error(path, "is missing required integer `version: 1`"))?;
    match version {
        Node::Scalar { value, plain: true } if value == "1" => {}
        _ => return Err(file_error(path, "`version` must be the integer 1")),
    }
    for key in entries.keys() {
        if !matches!(
            key.as_str(),
            "model" | "mode" | "sandbox" | "allow" | "schema" | "vars"
        ) {
            return Err(file_error(path, format!("contains unknown field `{key}`")));
        }
    }
    Ok(Fields {
        model: take_string(path, &mut entries, "model")?,
        mode: take_string(path, &mut entries, "mode")?,
        sandbox: take_string(path, &mut entries, "sandbox")?,
        allow: take_strings(path, &mut entries, "allow")?,
        schema: take_string(path, &mut entries, "schema")?,
        vars: take_strings(path, &mut entries, "vars")?,
    })
}

fn take_string(
    path: &Path,
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Option<String>, UsageError> {
    let Some(node) = entries.remove(key) else {
        return Ok(None);
    };
    match node {
        Node::Scalar { value, plain }
            if !value.is_empty() && (!plain || !plain_is_non_string(&value)) =>
        {
            Ok(Some(value))
        }
        _ => Err(file_error(
            path,
            format!("`{key}` must be a non-empty string"),
        )),
    }
}

fn take_strings(
    path: &Path,
    entries: &mut BTreeMap<String, Node>,
    key: &str,
) -> Result<Vec<String>, UsageError> {
    let Some(node) = entries.remove(key) else {
        return Ok(Vec::new());
    };
    let Node::Sequence(nodes) = node else {
        return Err(file_error(
            path,
            format!("`{key}` must be a sequence of strings"),
        ));
    };
    let mut values = Vec::with_capacity(nodes.len());
    let mut unique = BTreeSet::new();
    for node in nodes {
        let Node::Scalar { value, plain } = node else {
            return Err(file_error(
                path,
                format!("`{key}` must contain only strings"),
            ));
        };
        if value.is_empty() || plain && plain_is_non_string(&value) {
            return Err(file_error(path, format!("`{key}` contains an empty value")));
        }
        if key == "vars" && !valid_name(&value) {
            return Err(file_error(path, format!("invalid variable name `{value}`")));
        }
        if !unique.insert(value.clone()) {
            return Err(file_error(
                path,
                format!("`{key}` contains duplicate `{value}`"),
            ));
        }
        values.push(value);
    }
    Ok(values)
}

fn supplied_vars(path: &Path, supplied: &[String]) -> Result<BTreeMap<String, String>, UsageError> {
    let mut values = BTreeMap::new();
    for item in supplied {
        let (name, value) = item.split_once('=').ok_or_else(|| {
            file_error(path, format!("`--var {item}` must be written as KEY=VALUE"))
        })?;
        if !valid_name(name) {
            return Err(file_error(path, format!("invalid --var name `{name}`")));
        }
        if value.is_empty() {
            return Err(file_error(
                path,
                format!("--var `{name}` has an empty value"),
            ));
        }
        if values.insert(name.to_string(), value.to_string()).is_some() {
            return Err(file_error(
                path,
                format!("--var `{name}` was supplied more than once"),
            ));
        }
    }
    Ok(values)
}

fn substitute(
    path: &Path,
    body: &str,
    declared: &[String],
    supplied: &BTreeMap<String, String>,
) -> Result<String, UsageError> {
    let mut placeholders = BTreeSet::new();
    let mut spans = Vec::new();
    let mut cursor = 0;
    loop {
        let opening = body[cursor..].find("{{");
        let closing = body[cursor..].find("}}");
        if closing.is_some_and(|closing| opening.is_none_or(|opening| closing < opening)) {
            return Err(file_error(
                path,
                "contains an unmatched `}}` placeholder delimiter",
            ));
        }
        let Some(relative) = opening else {
            break;
        };
        let start = cursor + relative;
        let value_start = start + 2;
        let relative_end = body[value_start..]
            .find("}}")
            .ok_or_else(|| file_error(path, "contains an unclosed `{{placeholder}}`"))?;
        let end = value_start + relative_end;
        let name = &body[value_start..end];
        if !valid_name(name) {
            return Err(file_error(
                path,
                format!("contains invalid placeholder `{{{{{name}}}}}`"),
            ));
        }
        placeholders.insert(name.to_string());
        spans.push((start, end + 2, name));
        cursor = end + 2;
    }
    let declared = declared.iter().cloned().collect::<BTreeSet<_>>();
    let supplied_names = supplied.keys().cloned().collect::<BTreeSet<_>>();
    exact_set(
        path,
        "declared variables",
        &declared,
        "body placeholders",
        &placeholders,
    )?;
    exact_set(
        path,
        "declared variables",
        &declared,
        "supplied --var values",
        &supplied_names,
    )?;

    let mut rendered = String::with_capacity(body.len());
    let mut copied = 0;
    for (start, end, name) in spans {
        rendered.push_str(&body[copied..start]);
        rendered.push_str(
            supplied
                .get(name)
                .expect("equal sets contain every placeholder"),
        );
        copied = end;
    }
    rendered.push_str(&body[copied..]);
    if rendered.is_empty() {
        return Err(file_error(
            path,
            "has an empty prompt body after substitution",
        ));
    }
    Ok(rendered)
}

fn exact_set(
    path: &Path,
    left_name: &str,
    left: &BTreeSet<String>,
    right_name: &str,
    right: &BTreeSet<String>,
) -> Result<(), UsageError> {
    if left == right {
        return Ok(());
    }
    let missing = left.difference(right).cloned().collect::<Vec<_>>();
    let foreign = right.difference(left).cloned().collect::<Vec<_>>();
    Err(file_error(
        path,
        format!(
            "{left_name} and {right_name} differ (missing: {}; foreign: {})",
            names(&missing),
            names(&foreign)
        ),
    ))
}

fn names(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values
            .iter()
            .map(|value| format!("`{value}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char.is_ascii_alphanumeric())
}

fn plain_is_non_string(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "~" | "null" | "true" | "false" | ".inf" | "+.inf" | "-.inf" | ".nan"
    ) {
        return true;
    }
    let unsigned = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value)
        .replace('_', "");
    unsigned.starts_with("0x")
        || unsigned.starts_with("0o")
        || unsigned.parse::<i64>().is_ok()
        || unsigned.parse::<f64>().is_ok()
}

fn relative_resource(agent_path: &Path, schema: String) -> String {
    let Some(resource) = schema.strip_prefix('@') else {
        return schema;
    };
    let resource = Path::new(resource);
    let resolved = if resource.is_absolute() {
        resource.to_path_buf()
    } else {
        agent_path
            .parent()
            .expect("a canonical file has a parent")
            .join(resource)
    };
    format!("@{}", resolved.display())
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn file_error(path: &Path, fact: impl Into<String>) -> UsageError {
    UsageError(format!("agent file {} {}", path.display(), fact.into()))
}
