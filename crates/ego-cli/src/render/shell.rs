//! Presentation-only shell syntax. Raw script offsets never refer to quoted argv.

use serde_json::Value;
use tree_sitter::{Node, Parser};

use super::caps::Depth;
use super::line::{Line, Span};
use super::style::{Palette, Style};

const MAX_SOURCE_BYTES: usize = 64 * 1024;

/// The caller supplies the existing display as the oracle and the fallback.
/// No parser or source map is built for colour-off output.
pub(super) fn command(args: &Value, display: &str, depth: Depth) -> Line {
    if depth == Depth::None {
        return Line::plain(display);
    }
    coloured(args)
        .filter(|line| line.text() == display)
        .unwrap_or_else(|| Line::plain(display))
}

fn coloured(args: &Value) -> Option<Line> {
    let argv = args.get("argv")?.as_array()?;
    let argv = argv.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
    let bytes = argv
        .iter()
        .try_fold(0usize, |size, arg| size.checked_add(arg.len() + 1))?;
    if bytes > MAX_SOURCE_BYTES {
        return None;
    }
    let executable = argv.first()?.rsplit('/').next()?;
    // Do not claim Bash semantics for a known different interpreter.
    if matches!(
        executable,
        "fish" | "csh" | "tcsh" | "ksh" | "dash" | "pwsh" | "powershell" | "cmd" | "cmd.exe"
    ) {
        return None;
    }
    let script_index = if matches!(executable, "bash" | "sh" | "zsh") {
        argv.iter()
            .position(|arg| matches!(*arg, "-c" | "-lc"))
            .map(|index| index + 1)
    } else {
        None
    };
    // Parse before constructing any spans: a bad script also clears wrapper colours.
    let script_styles = match script_index {
        Some(index) => Some(syntax(argv.get(index).copied()?)?),
        None => None,
    };
    let mut line = Line::empty();
    for (index, argument) in argv.iter().enumerate() {
        if index > 0 {
            line.push(Span::plain(" "));
        }
        if script_index == Some(index) {
            project(&mut line, argument, script_styles.as_deref()?)?;
        } else {
            let display = ego_core::permit::shell_word(argument);
            let colour = if index == 0 {
                Some(Palette::TOOL)
            } else if display.starts_with('\'') {
                Some(Palette::ADDED_FG)
            } else if argument.starts_with('-') && argument.len() > 1 {
                Some(Palette::WARNING)
            } else {
                None
            };
            line.push(Span::new(display, colour.map_or(Style::PLAIN, Style::fg)));
        }
    }
    Some(line)
}

/// Project each raw character through the shared encoding. The full word is
/// checked against shell_word so an encoding change cannot silently drift.
fn project(line: &mut Line, source: &str, styles: &[Style]) -> Option<()> {
    let oracle = ego_core::permit::shell_word(source);
    let quoted = oracle != source;
    let mut projected = Line::empty();
    if quoted {
        projected.push(Span::plain("'"));
    }
    for (offset, character) in source.char_indices() {
        let encoded = if quoted {
            ego_core::permit::terminal_safe(&character.to_string().replace('\'', "'\\''"))
        } else {
            character.to_string()
        };
        projected.push(Span::new(encoded, *styles.get(offset)?));
    }
    if quoted {
        projected.push(Span::plain("'"));
    }
    if projected.text() != oracle {
        return None;
    }
    for span in projected.spans {
        line.push(span);
    }
    Some(())
}

/// Each byte is assigned once, in source order. Iterative traversal avoids a
/// Rust stack frame per nested shell expression and retains specific child roles.
fn syntax(source: &str) -> Option<Vec<Style>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut styles = vec![Style::PLAIN; source.len()];
    let mut inherited = vec![Style::PLAIN];
    let mut cursor = tree.walk();
    let mut offset = 0;
    loop {
        let node = cursor.node();
        if node.is_error()
            || node.is_missing()
            || !source.is_char_boundary(node.start_byte())
            || !source.is_char_boundary(node.end_byte())
        {
            return None;
        }
        let parent_style = *inherited.last()?;
        styles
            .get_mut(offset..node.start_byte())?
            .fill(parent_style);
        offset = node.start_byte();
        inherited.push(role(node, source).unwrap_or(parent_style));
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            let end = cursor.node().end_byte();
            styles.get_mut(offset..end)?.fill(inherited.pop()?);
            offset = end;
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(styles);
            }
        }
    }
}

fn role(node: Node<'_>, source: &str) -> Option<Style> {
    let colour = match node.kind() {
        "command_name" => Palette::TOOL,
        "string" | "raw_string" | "ansi_c_string" => Palette::ADDED_FG,
        "simple_expansion" | "expansion" | "variable_name" | "special_variable_name" => {
            Palette::CODE
        }
        "comment" => Palette::NOTE,
        "word"
            if node
                .parent()
                .is_some_and(|parent| parent.kind() == "command")
                && source
                    .get(node.byte_range())
                    .is_some_and(|word| word.starts_with('-') && word.len() > 1) =>
        {
            Palette::WARNING
        }
        "|" | "|&" | "||" | "&&" | ";" | "&" | ";;" | ";&" | ";;&" | ">" | ">>" | "<" | "<<"
        | "<<-" | "<<<" | ">&" | "<&" | ">&-" | "<&-" | "&>" | "&>>" | ">|" | "="
            if !node.is_named() =>
        {
            Palette::LINK
        }
        _ => return None,
    };
    Some(Style::fg(colour))
}
