use crate::OutputKind;

const MAX_INLINE_DEPTH: usize = 32;

pub(crate) fn render(source: &str, kind: OutputKind) -> String {
    render_nested(source, kind, 0)
}

fn render_nested(source: &str, kind: OutputKind, depth: usize) -> String {
    if depth >= MAX_INLINE_DEPTH {
        return literal(source, kind);
    }

    let mut output = String::with_capacity(source.len());
    let mut offset = 0;
    while offset < source.len() {
        let remaining = &source[offset..];

        if remaining.starts_with("<!--") {
            offset += remaining.find("-->").map_or(remaining.len(), |end| end + 3);
            continue;
        }
        if remaining.starts_with("[[")
            && let Some((body, consumed)) = balanced(remaining, "[[", "]]")
        {
            output.push_str(&internal_link(body, kind, depth + 1));
            offset += consumed;
            continue;
        }
        if remaining.starts_with("{{")
            && let Some((body, consumed)) = balanced(remaining, "{{", "}}")
        {
            output.push_str(&template(body, kind, depth + 1));
            offset += consumed;
            continue;
        }
        if remaining.starts_with("[http")
            && let Some(end) = remaining.find(']')
        {
            output.push_str(&external_link(&remaining[1..end], kind, depth + 1));
            offset += end + 1;
            continue;
        }
        if remaining.starts_with("<nowiki>")
            && let Some(end) = find_ascii_case_insensitive(remaining, "</nowiki>")
        {
            let body = &remaining[8..end];
            output.push_str(&literal(body, kind));
            offset += end + 9;
            continue;
        }
        if starts_with_tag(remaining, "ref") {
            let tag_end = remaining.find('>').unwrap_or(remaining.len() - 1);
            if remaining[..=tag_end].trim_end().ends_with("/>") {
                offset += tag_end + 1;
                continue;
            }
            if let Some(end) = find_ascii_case_insensitive(remaining, "</ref>") {
                let body = render_nested(&remaining[tag_end + 1..end], kind, depth + 1);
                let body = collapse_whitespace(&body);
                if !body.is_empty() {
                    output.push_str(" [ref: ");
                    output.push_str(&body);
                    output.push(']');
                }
                offset += end + 6;
                continue;
            }
        }
        if (starts_with_tag(remaining, "code") || starts_with_tag(remaining, "tt"))
            && let Some(tag_end) = remaining.find('>')
        {
            let tag_name = if starts_with_tag(remaining, "code") {
                "code"
            } else {
                "tt"
            };
            let closing = format!("</{tag_name}>");
            if let Some(end) = find_ascii_case_insensitive(remaining, &closing) {
                let body = collapse_whitespace(&remaining[tag_end + 1..end]);
                match kind {
                    OutputKind::PlainText => output.push_str(&body),
                    OutputKind::Markdown => {
                        let fence = if body.contains('`') { "``" } else { "`" };
                        output.push_str(fence);
                        output.push_str(&body);
                        output.push_str(fence);
                    }
                }
                offset += end + closing.len();
                continue;
            }
        }
        if remaining.starts_with("'''")
            && let Some(end) = remaining[3..].find("'''")
        {
            let body = render_nested(&remaining[3..end + 3], kind, depth + 1);
            if kind == OutputKind::Markdown {
                output.push_str("**");
            }
            output.push_str(&body);
            if kind == OutputKind::Markdown {
                output.push_str("**");
            }
            offset += end + 6;
            continue;
        }
        if remaining.starts_with("''")
            && let Some(end) = remaining[2..].find("''")
        {
            let body = render_nested(&remaining[2..end + 2], kind, depth + 1);
            if kind == OutputKind::Markdown {
                output.push('*');
            }
            output.push_str(&body);
            if kind == OutputKind::Markdown {
                output.push('*');
            }
            offset += end + 4;
            continue;
        }
        if remaining.starts_with("__")
            && let Some(end) = remaining[2..].find("__")
            && remaining[2..end + 2]
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        {
            offset += end + 4;
            continue;
        }
        if remaining.starts_with('<')
            && let Some(end) = remaining.find('>')
        {
            let tag = &remaining[..=end];
            if is_break_tag(tag) {
                output.push('\n');
            }
            offset += end + 1;
            continue;
        }
        if remaining.starts_with('&')
            && let Some(end) = remaining.find(';')
            && end <= 12
            && let Some(decoded) = decode_entity(&remaining[1..end])
        {
            output.push(decoded);
            offset += end + 1;
            continue;
        }

        let character = remaining.chars().next().expect("non-empty remainder");
        push_literal_character(&mut output, character, kind);
        offset += character.len_utf8();
    }
    output
}

fn balanced<'a>(source: &'a str, open: &str, close: &str) -> Option<(&'a str, usize)> {
    let mut depth = 1_usize;
    let mut cursor = open.len();
    while cursor < source.len() {
        let remaining = &source[cursor..];
        if remaining.starts_with(open) {
            depth += 1;
            cursor += open.len();
        } else if remaining.starts_with(close) {
            depth -= 1;
            if depth == 0 {
                return Some((&source[open.len()..cursor], cursor + close.len()));
            }
            cursor += close.len();
        } else {
            cursor += remaining
                .chars()
                .next()
                .expect("non-empty remainder")
                .len_utf8();
        }
    }
    None
}

fn internal_link(body: &str, kind: OutputKind, depth: usize) -> String {
    let parts = split_top_level(body, '|');
    let target = parts.first().map_or("", |value| value.trim());
    let namespace = target
        .trim_start_matches(':')
        .split_once(':')
        .map(|(prefix, _)| prefix.to_ascii_lowercase());

    if !target.starts_with(':') && matches!(namespace.as_deref(), Some("category")) {
        return String::new();
    }

    if !target.starts_with(':') && matches!(namespace.as_deref(), Some("file" | "image")) {
        let caption = parts
            .iter()
            .skip(1)
            .rev()
            .map(|part| part.trim())
            .find(|part| is_visible_media_argument(part));
        return caption.map_or_else(String::new, |caption| {
            let caption = render_nested(caption, kind, depth);
            match kind {
                OutputKind::PlainText => format!("[Image: {caption}]"),
                OutputKind::Markdown => format!("*Image: {caption}*"),
            }
        });
    }

    let label = parts.last().copied().unwrap_or(target).trim();
    let label = if parts.len() == 1 {
        target.split('#').next().unwrap_or(target)
    } else {
        label
    };
    let rendered_label = render_nested(label, kind, depth);
    if kind == OutputKind::PlainText || target.is_empty() {
        return rendered_label;
    }

    format!("[{rendered_label}]({})", markdown_target(target))
}

fn is_visible_media_argument(argument: &str) -> bool {
    let normalized = argument.to_ascii_lowercase();
    !argument.is_empty()
        && !matches!(
            normalized.as_str(),
            "thumb"
                | "thumbnail"
                | "frame"
                | "frameless"
                | "border"
                | "left"
                | "right"
                | "center"
                | "none"
                | "upright"
        )
        && !normalized.ends_with("px")
        && !normalized.starts_with("alt=")
        && !normalized.starts_with("link=")
        && !normalized.starts_with("class=")
        && !normalized.starts_with("lang=")
        && !normalized.starts_with("page=")
}

fn markdown_target(target: &str) -> String {
    let mut output = String::with_capacity(target.len());
    for byte in target.trim_start_matches(':').bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b'#'
            | b':' => output.push(char::from(byte)),
            b' ' => output.push('_'),
            _ => {
                use std::fmt::Write as _;
                write!(output, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
    }
    output
}

fn external_link(body: &str, kind: OutputKind, depth: usize) -> String {
    let (url, label) = body
        .split_once(char::is_whitespace)
        .map_or((body.trim(), None), |(url, label)| {
            (url.trim(), Some(label.trim()))
        });
    let label = label.filter(|label| !label.is_empty()).unwrap_or(url);
    let rendered_label = render_nested(label, kind, depth);
    match kind {
        OutputKind::PlainText if label == url => url.to_owned(),
        OutputKind::PlainText => format!("{rendered_label} ({url})"),
        OutputKind::Markdown => format!("[{rendered_label}]({url})"),
    }
}

fn template(body: &str, kind: OutputKind, depth: usize) -> String {
    let parts = split_top_level(body, '|');
    let name = parts.first().map_or("", |value| value.trim());
    let normalized = name
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let positional = parts
        .iter()
        .skip(1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !is_named_argument(value))
        .collect::<Vec<_>>();

    let visible = match normalized.as_str() {
        "lang" | "langue" => positional.last().copied(),
        "nowrap" | "nobr" | "small" | "big" | "center" | "plainlist" | "ubl" => {
            positional.first().copied()
        }
        "quote" | "blockquote" | "pull quote" => positional.first().copied(),
        "abbr" => positional.first().copied(),
        "convert" => return render_convert(&positional, kind, depth),
        "frac" | "fraction" => return render_fraction(&positional, kind, depth),
        "!" => return "|".to_owned(),
        "=" => return "=".to_owned(),
        _ => None,
    };
    if let Some(visible) = visible {
        return render_nested(visible, kind, depth);
    }

    let label = if name.is_empty() { "unnamed" } else { name };
    match kind {
        OutputKind::PlainText => format!("[Template: {label}]"),
        OutputKind::Markdown => format!("`[Template: {}]`", label.replace('`', "'")),
    }
}

fn render_convert(parts: &[&str], kind: OutputKind, depth: usize) -> String {
    let visible = parts.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    render_nested(&visible, kind, depth)
}

fn render_fraction(parts: &[&str], kind: OutputKind, depth: usize) -> String {
    let value = match parts {
        [whole, numerator, denominator, ..] => format!("{whole} {numerator}/{denominator}"),
        [numerator, denominator] => format!("{numerator}/{denominator}"),
        [value] => (*value).to_owned(),
        [] => String::new(),
    };
    render_nested(&value, kind, depth)
}

fn is_named_argument(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(name, _)| !name.trim().is_empty())
}

fn split_top_level(source: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut braces = 0_i32;
    let mut brackets = 0_i32;
    let mut cursor = 0;
    while cursor < source.len() {
        let remaining = &source[cursor..];
        if remaining.starts_with("{{") {
            braces += 1;
            cursor += 2;
        } else if remaining.starts_with("}}") {
            braces = (braces - 1).max(0);
            cursor += 2;
        } else if remaining.starts_with("[[") {
            brackets += 1;
            cursor += 2;
        } else if remaining.starts_with("]]") {
            brackets = (brackets - 1).max(0);
            cursor += 2;
        } else {
            let character = remaining.chars().next().expect("non-empty remainder");
            if character == separator && braces == 0 && brackets == 0 {
                parts.push(&source[start..cursor]);
                start = cursor + character.len_utf8();
            }
            cursor += character.len_utf8();
        }
    }
    parts.push(&source[start..]);
    parts
}

fn starts_with_tag(source: &str, name: &str) -> bool {
    let Some(prefix) = source.get(..name.len() + 1) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(&format!("<{name}")) {
        return false;
    }
    source[name.len() + 1..]
        .chars()
        .next()
        .is_some_and(|character| character == '>' || character == '/' || character.is_whitespace())
}

fn find_ascii_case_insensitive(source: &str, needle: &str) -> Option<usize> {
    source
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn is_break_tag(tag: &str) -> bool {
    let tag = tag.trim_matches(['<', '>', '/', ' ']);
    tag.eq_ignore_ascii_case("br")
        || tag.eq_ignore_ascii_case("p")
        || tag.eq_ignore_ascii_case("div")
        || tag.eq_ignore_ascii_case("li")
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" | "#160" => Some(' '),
        "ndash" => Some('–'),
        "mdash" => Some('—'),
        "hellip" => Some('…'),
        _ => entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .or_else(|| {
                entity
                    .strip_prefix('#')
                    .and_then(|digits| digits.parse().ok())
            })
            .and_then(char::from_u32),
    }
}

fn collapse_whitespace(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn literal(source: &str, kind: OutputKind) -> String {
    let mut output = String::with_capacity(source.len());
    for character in source.chars() {
        push_literal_character(&mut output, character, kind);
    }
    output
}

fn push_literal_character(output: &mut String, character: char, kind: OutputKind) {
    if kind == OutputKind::Markdown
        && matches!(
            character,
            '\\' | '*' | '_' | '[' | ']' | '`' | '<' | '>' | '#'
        )
    {
        output.push('\\');
    }
    output.push(character);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_delimiters_do_not_split_link_labels() {
        assert_eq!(
            render("[[Target|A {{small|nested}} label]]", OutputKind::PlainText),
            "A nested label"
        );
    }

    #[test]
    fn excessive_nesting_falls_back_to_literal_text() {
        let source = format!("{}x{}", "{{small|".repeat(40), "}}".repeat(40));
        let output = render(&source, OutputKind::PlainText);
        assert!(output.contains('x'));
    }
}
