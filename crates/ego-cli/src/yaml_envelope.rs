//! Shared bounded strict-YAML envelope for CLI-owned Markdown inputs.

use std::collections::{BTreeMap, BTreeSet};

use yaml_rust2::parser::{Event, EventReceiver, Parser};
use yaml_rust2::scanner::TScalarStyle;

pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_COLLECTION_ENTRIES: usize = 256;
const MAX_SCALAR_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) enum Node {
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

pub(crate) fn split(text: &str, shebang: bool) -> Result<(&str, &str), String> {
    let mut offset = 0;
    if shebang && text.starts_with("#!") {
        offset = text
            .find('\n')
            .map(|index| index + 1)
            .ok_or_else(|| "has a shebang but no YAML frontmatter".to_owned())?;
    }
    let (opening, after_opening) = line(text, offset);
    if opening != "---" {
        return Err("must begin with a standalone `---` delimiter".to_owned());
    }
    let header_start = after_opening;
    let mut cursor = header_start;
    while cursor < text.len() {
        let (content, next) = line(text, cursor);
        if content == "---" {
            let header = &text[header_start..cursor];
            if header.len() > MAX_FRONTMATTER_BYTES {
                return Err(format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"));
            }
            let body = &text[next..];
            if body.is_empty() {
                return Err("has an empty prompt body".to_owned());
            }
            return Ok((header, body));
        }
        if next == cursor {
            break;
        }
        cursor = next;
        if cursor.saturating_sub(header_start) > MAX_FRONTMATTER_BYTES {
            return Err(format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"));
        }
    }
    Err("has no closing standalone `---` delimiter".to_owned())
}

pub(crate) fn mapping(header: &str) -> Result<BTreeMap<String, Node>, String> {
    let mut receiver = StrictReceiver::default();
    Parser::new_from_str(header)
        .load(&mut receiver, true)
        .map_err(|error| format!("contains invalid YAML: {error}"))?;
    if let Some(error) = receiver.error {
        return Err(error);
    }
    if receiver.documents != 1 {
        return Err("must contain exactly one YAML document".to_owned());
    }
    let Node::Mapping(entries) = receiver
        .root
        .ok_or_else(|| "has empty frontmatter".to_owned())?
    else {
        return Err("frontmatter must be a mapping".to_owned());
    };
    entries
        .into_iter()
        .map(|(key, value)| match key {
            Node::Scalar { value: key, .. } => Ok((key, value)),
            _ => Err("frontmatter keys must be strings".to_owned()),
        })
        .collect()
}

pub(crate) fn scalar(node: Node) -> Option<String> {
    match node {
        Node::Scalar { value, plain }
            if !value.is_empty() && (!plain || !plain_is_non_string(&value)) =>
        {
            Some(value)
        }
        _ => None,
    }
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

pub(crate) fn plain_is_non_string(value: &str) -> bool {
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
