use crate::{OutputKind, inline};

pub(crate) fn render(source: &str, kind: OutputKind) -> String {
    let source = strip_comments(&source.replace("\r\n", "\n").replace('\r', "\n"));
    let mut renderer = Renderer::new(kind);
    for line in source.lines() {
        renderer.line(line);
    }
    renderer.finish()
}

pub(crate) fn search_headings(source: &str) -> String {
    let source = strip_comments(&source.replace("\r\n", "\n").replace('\r', "\n"));
    let mut headings = source
        .lines()
        .filter_map(heading)
        .map(|(_, value)| normalized_inline(value, OutputKind::PlainText))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !headings.is_empty() {
        headings.push('\n');
    }
    headings
}

#[derive(Debug)]
struct Renderer {
    kind: OutputKind,
    lines: Vec<String>,
    paragraph: String,
    in_table: bool,
    table_has_header: bool,
}

impl Renderer {
    fn new(kind: OutputKind) -> Self {
        Self {
            kind,
            lines: Vec::new(),
            paragraph: String::new(),
            in_table: false,
            table_has_header: false,
        }
    }

    fn line(&mut self, raw_line: &str) {
        let line = raw_line.trim_end();
        if self.in_table {
            self.table_line(line);
            return;
        }
        if line.trim_start().starts_with("{|") {
            self.flush_paragraph();
            self.in_table = true;
            self.table_has_header = false;
            return;
        }
        if line.trim().is_empty() {
            self.flush_paragraph();
            self.blank();
            return;
        }
        if let Some((level, heading)) = heading(line) {
            self.flush_paragraph();
            let heading = normalized_inline(heading, self.kind);
            if !heading.is_empty() {
                match self.kind {
                    OutputKind::PlainText => self.push(heading),
                    OutputKind::Markdown => self.push(format!("{} {heading}", "#".repeat(level))),
                }
                self.blank();
            }
            return;
        }
        if let Some((markers, content)) = list_item(line) {
            self.flush_paragraph();
            let content = normalized_inline(content, self.kind);
            if !content.is_empty() {
                self.push(format_list(markers, &content, self.kind));
            }
            return;
        }
        if let Some(code) = line.strip_prefix(' ') {
            self.flush_paragraph();
            match self.kind {
                OutputKind::PlainText => self.push(format!("    {code}")),
                OutputKind::Markdown => self.push(format!("    {code}")),
            }
            return;
        }
        if line.trim().bytes().all(|byte| byte == b'-') && line.trim().len() >= 4 {
            self.flush_paragraph();
            self.push(match self.kind {
                OutputKind::PlainText => "────────".to_owned(),
                OutputKind::Markdown => "---".to_owned(),
            });
            return;
        }

        let source = line.trim();
        if !source.is_empty() {
            if !self.paragraph.is_empty() {
                self.paragraph.push(' ');
            }
            self.paragraph.push_str(source);
        }
    }

    fn table_line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("|}") {
            self.in_table = false;
            self.blank();
            return;
        }
        if trimmed.starts_with("|-") || trimmed.is_empty() {
            return;
        }
        if let Some(caption) = trimmed.strip_prefix("|+") {
            let caption = normalized_inline(caption.trim(), self.kind);
            if !caption.is_empty() {
                self.push(match self.kind {
                    OutputKind::PlainText => format!("Table: {caption}"),
                    OutputKind::Markdown => format!("*Table: {caption}*"),
                });
            }
            return;
        }
        let (header, cells) = if let Some(cells) = trimmed.strip_prefix('!') {
            (true, split_cells(cells, "!!"))
        } else if let Some(cells) = trimmed.strip_prefix('|') {
            (false, split_cells(cells, "||"))
        } else {
            return;
        };
        let cells = cells
            .into_iter()
            .map(|cell| {
                let value = cell
                    .split_once('|')
                    .filter(|(attributes, _)| looks_like_attributes(attributes))
                    .map_or(cell, |(_, value)| value);
                normalized_inline(value.trim(), self.kind)
            })
            .collect::<Vec<_>>();
        if cells.iter().all(String::is_empty) {
            return;
        }
        match self.kind {
            OutputKind::PlainText => self.push(cells.join(" | ")),
            OutputKind::Markdown => {
                self.push(format!("| {} |", cells.join(" | ")));
                if header && !self.table_has_header {
                    self.push(format!(
                        "| {} |",
                        cells.iter().map(|_| "---").collect::<Vec<_>>().join(" | ")
                    ));
                    self.table_has_header = true;
                }
            }
        }
    }

    fn flush_paragraph(&mut self) {
        if !self.paragraph.is_empty() {
            let source = std::mem::take(&mut self.paragraph);
            let paragraph = normalized_inline(&source, self.kind);
            if !paragraph.is_empty() {
                self.push(paragraph);
            }
        }
    }

    fn push(&mut self, line: String) {
        self.lines.push(line.trim_end().to_owned());
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
    }

    fn finish(mut self) -> String {
        self.flush_paragraph();
        while self.lines.last().is_some_and(String::is_empty) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            String::new()
        } else {
            self.lines.join("\n") + "\n"
        }
    }
}

fn strip_comments(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut remaining = source;
    while let Some(start) = remaining.find("<!--") {
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start + 4..];
        let Some(end) = after_start.find("-->") else {
            return output;
        };
        remaining = &after_start[end + 3..];
    }
    output.push_str(remaining);
    output
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim();
    let leading = trimmed.bytes().take_while(|byte| *byte == b'=').count();
    let trailing = trimmed
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if !(2..=6).contains(&leading) || trailing < leading || trimmed.len() < leading * 2 {
        return None;
    }
    Some((leading, trimmed[leading..trimmed.len() - leading].trim()))
}

fn list_item(line: &str) -> Option<(&str, &str)> {
    let marker_length = line
        .bytes()
        .take_while(|byte| matches!(byte, b'*' | b'#' | b';' | b':'))
        .count();
    (marker_length > 0).then(|| (&line[..marker_length], line[marker_length..].trim_start()))
}

fn format_list(markers: &str, content: &str, kind: OutputKind) -> String {
    let depth = markers.len();
    let marker = markers.as_bytes()[depth - 1];
    let indent = "  ".repeat(depth.saturating_sub(1));
    match (kind, marker) {
        (OutputKind::Markdown, b'#') => format!("{indent}1. {content}"),
        (OutputKind::Markdown, b':') => format!("{}> {content}", "> ".repeat(depth - 1)),
        (OutputKind::Markdown, b';') => format!("{indent}- **{content}**"),
        (OutputKind::Markdown, _) | (OutputKind::PlainText, b'*' | b';') => {
            format!("{indent}- {content}")
        }
        (OutputKind::PlainText, b'#') => format!("{indent}1. {content}"),
        (OutputKind::PlainText, b':') => format!("{indent}{content}"),
        (OutputKind::PlainText, _) => format!("{indent}- {content}"),
    }
}

fn normalized_inline(source: &str, kind: OutputKind) -> String {
    inline::render(source, kind)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_cells<'a>(source: &'a str, delimiter: &str) -> Vec<&'a str> {
    source.split(delimiter).collect()
}

fn looks_like_attributes(source: &str) -> bool {
    source.contains('=')
        || matches!(
            source.trim().to_ascii_lowercase().as_str(),
            "left" | "right" | "center"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_source_lines_but_preserves_paragraphs() {
        assert_eq!(
            render("one\r\ntwo\r\n\r\nthree", OutputKind::PlainText),
            "one two\n\nthree\n"
        );
    }

    #[test]
    fn removes_multiline_comments() {
        assert_eq!(
            render(
                "before <!-- hidden\nstill hidden --> after",
                OutputKind::PlainText
            ),
            "before after\n"
        );
    }
}
