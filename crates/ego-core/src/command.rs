//! Single-pass Markdown command expansion. Inserted data is never executable syntax.

use crate::skill::SkillError;

pub const MAX_PROMPT_BYTES: usize = 256 * 1024;
pub const MAX_SHELL_BYTES: usize = 64 * 1024;
pub const MAX_SHELLS: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    File(String),
    Shell(String),
}

/// Parse executable syntax before inserting arguments or reading external data.
pub fn prepare(template: &str, arguments: &str) -> Result<Vec<Part>, SkillError> {
    let values = shell_words::split(arguments)
        .map_err(|error| SkillError::new(format!("command arguments: {error}")))?;
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = template;
    let mut used = false;
    let mut shells = 0;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("!`") {
            let end = tail
                .find('`')
                .ok_or_else(|| SkillError::new("unclosed command shell marker"))?;
            shells += 1;
            if shells > MAX_SHELLS {
                return Err(SkillError::new("command exceeds eight shell markers"));
            }
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            let command = substitute(&tail[..end], arguments, &values, true, &mut used)?;
            if command.trim().is_empty() {
                return Err(SkillError::new("empty command shell marker"));
            }
            parts.push(Part::Shell(command));
            rest = &tail[end + 1..];
        } else if rest.starts_with('@') && (text.is_empty() || text.ends_with(char::is_whitespace))
        {
            let tail = &rest[1..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            if end == 0 {
                text.push('@');
                rest = tail;
                continue;
            }
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::File(substitute(
                &tail[..end],
                arguments,
                &values,
                false,
                &mut used,
            )?));
            rest = &tail[end..];
        } else if rest.starts_with('$') {
            let (value, length) = token(rest, arguments, &values, false)?;
            if length > 0 {
                used = true;
                text.push_str(&value);
                rest = &rest[length..];
            } else {
                text.push('$');
                rest = &rest[1..];
            }
        } else {
            let ch = rest.chars().next().expect("nonempty template");
            text.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
        if text.len() > MAX_PROMPT_BYTES {
            return Err(SkillError::new("command prompt exceeds 256 KiB"));
        }
    }
    if !used && !arguments.is_empty() {
        text.push_str("\n\n");
        text.push_str(arguments);
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

fn substitute(
    text: &str,
    raw: &str,
    values: &[String],
    shell: bool,
    used: &mut bool,
) -> Result<String, SkillError> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        let (value, length) = token(rest, raw, values, shell)?;
        if length == 0 {
            out.push('$');
            rest = &rest[1..];
        } else {
            *used = true;
            out.push_str(&value);
            rest = &rest[length..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn token(
    text: &str,
    raw: &str,
    values: &[String],
    shell: bool,
) -> Result<(String, usize), SkillError> {
    if let Some(tail) = text.strip_prefix("$ARGUMENTS")
        && !tail.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
    {
        if shell {
            return Err(SkillError::new(
                "raw $ARGUMENTS is not permitted inside shell markers; use positional parameters",
            ));
        }
        return Ok((raw.to_owned(), "$ARGUMENTS".len()));
    }
    let digits = text[1..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Ok((String::new(), 0));
    }
    let index = text[1..1 + digits]
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1));
    let value = index.and_then(|n| values.get(n)).map_or("", String::as_str);
    // Shell arguments are supplied via positional parameters by the caller,
    // so template quoting cannot turn inserted bytes back into shell syntax.
    Ok((
        if shell {
            index.map_or_else(|| "''".to_owned(), |n| format!("${{{}}}", n + 1))
        } else {
            value.to_owned()
        },
        digits + 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_arguments_cannot_create_shell_execution() {
        assert_eq!(
            prepare("Review $1 / $ARGUMENTS", "'!`touch nope`'").unwrap(),
            vec![Part::Text("Review !`touch nope` / '!`touch nope`'".into())]
        );
    }

    #[test]
    fn quoted_positions_missing_values_and_raw_fallback_are_distinct() {
        assert_eq!(
            prepare("$1|$2|$10", "'one two' three").unwrap(),
            vec![Part::Text("one two|three|".into())]
        );
        assert_eq!(
            prepare("Review", "'one two'").unwrap(),
            vec![Part::Text("Review\n\n'one two'".into())]
        );
        assert!(prepare("$1", "'unclosed").is_err());
    }

    #[test]
    fn shell_sources_are_parsed_before_file_and_argument_data() {
        assert_eq!(
            prepare("@$1 !`git diff -- $2`", "src/file 'some file'").unwrap(),
            vec![
                Part::File("src/file".into()),
                Part::Text(" ".into()),
                Part::Shell("git diff -- ${2}".into())
            ]
        );
        assert!(prepare("!`echo $ARGUMENTS`", "unsafe").is_err());
        assert!(prepare("!`unclosed", "").is_err());
        assert!(prepare(&"!`true`".repeat(9), "").is_err());
    }
}
