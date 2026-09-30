//! Facebook group posters write Markdown they mean as formatting. This one
//! tokenizer decides what counts as formatting; `embed` renders the tokens
//! for Discord and `activity` renders them as HTML, so the two never disagree.
//!
//! Supported: `# heading` markers (stripped), `> quote` and `* bullet` lines,
//! `**bold**`, `*italic*` / `_italic_`, `` `code` ``, and `\X` escapes.
//! Markers pair first-with-second; unmatched ones stay literal text.

pub enum Block {
    Plain,
    Quote,
    Bullet,
}

pub struct Line<'a> {
    /// Leading whitespace, kept verbatim.
    pub indent: &'a str,
    pub block: Block,
    pub spans: Vec<Span>,
}

#[derive(Debug, PartialEq)]
pub enum Span {
    Text(String),
    /// `\X` where X is a Markdown special: the author asked for a literal X.
    Escaped(char),
    /// A backslash that escapes nothing.
    Backslash,
    BoldOpen,
    BoldClose,
    /// Carries the author's delimiter (`*` or `_`).
    ItalicOpen(char),
    ItalicClose(char),
    CodeOpen,
    CodeClose,
}

/// Chars a Facebook-authored `\X` escape may protect.
fn is_escapable(c: char) -> bool {
    matches!(c, '*' | '_' | '~' | '|' | '`' | '>' | '#' | '\\')
}

pub fn lines(text: &str) -> impl Iterator<Item = Line<'_>> {
    text.split('\n').map(parse_line)
}

fn parse_line(line: &str) -> Line<'_> {
    let indent_end = line
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(line.len());
    let (indent, mut rest) = line.split_at(indent_end);
    let hashes = rest.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        rest = rest[hashes..].trim_start_matches(' ');
    }
    let (block, rest) = if rest == ">" || rest.starts_with("> ") {
        (Block::Quote, rest.get(2..).unwrap_or(""))
    } else if rest == "*" || rest.starts_with("* ") {
        // Facebook renders `* item` lines as bullets; `**` is bold, inline.
        (Block::Bullet, rest.get(2..).unwrap_or(""))
    } else {
        (Block::Plain, rest)
    };
    Line {
        indent,
        block,
        spans: spans(rest),
    }
}

fn spans(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let (mut bold, mut code) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '*' if chars.get(i + 1) == Some(&'*') => {
                bold.push(i);
                i += 1;
            }
            '`' => code.push(i),
            _ => {}
        }
        i += 1;
    }
    let (bold_opens, bold_closes) = pair(&bold);
    let (code_opens, code_closes) = pair(&code);
    let (italic_opens, italic_closes) = italic_markers(&chars);

    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at = |markers: &[usize]| markers.binary_search(&i).is_ok();
        let (span, width) = if c == '\\' {
            match chars.get(i + 1) {
                Some(&next) if is_escapable(next) => (Span::Escaped(next), 2),
                _ => (Span::Backslash, 1),
            }
        } else if at(&bold_opens) {
            (Span::BoldOpen, 2)
        } else if at(&bold_closes) {
            (Span::BoldClose, 2)
        } else if at(&italic_opens) {
            (Span::ItalicOpen(c), 1)
        } else if at(&italic_closes) {
            (Span::ItalicClose(c), 1)
        } else if at(&code_opens) {
            (Span::CodeOpen, 1)
        } else if at(&code_closes) {
            (Span::CodeClose, 1)
        } else {
            text.push(c);
            i += 1;
            continue;
        };
        // Discord only bolds `**x**`, not `** x **`; trim the padding.
        if span == Span::BoldClose {
            text.truncate(text.trim_end_matches(' ').len());
        }
        if !text.is_empty() {
            out.push(Span::Text(std::mem::take(&mut text)));
        }
        i += width;
        if span == Span::BoldOpen {
            while chars.get(i) == Some(&' ') {
                i += 1;
            }
        }
        out.push(span);
    }
    if !text.is_empty() {
        out.push(Span::Text(text));
    }
    out
}

/// Pair markers first-with-second; an odd trailing marker stays literal.
fn pair(markers: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let paired = &markers[..markers.len() - markers.len() % 2];
    (
        paired.iter().step_by(2).copied().collect(),
        paired.iter().skip(1).step_by(2).copied().collect(),
    )
}

/// Pair single, unescaped emphasis markers. Keep code, bold runs, intraword
/// underscores, unmatched delimiters, and bullet stars literal.
fn italic_markers(chars: &[char]) -> (Vec<usize>, Vec<usize>) {
    let (mut opens, mut closes) = (Vec::new(), Vec::new());
    let mut opening = None;
    let mut in_code = false;
    let mut index = 0;
    while index < chars.len() {
        let marker = chars[index];
        if marker == '\\' {
            index += 2;
            continue;
        }
        if marker == '`' {
            in_code = !in_code;
        }
        let previous = index.checked_sub(1).and_then(|i| chars.get(i));
        let next = chars.get(index + 1);
        if !in_code
            && matches!(marker, '*' | '_')
            && previous != Some(&marker)
            && next != Some(&marker)
            && !(marker == '_'
                && previous.is_some_and(|c| c.is_alphanumeric())
                && next.is_some_and(|c| c.is_alphanumeric()))
        {
            match opening {
                Some((start, delimiter))
                    if marker == delimiter && previous.is_some_and(|c| !c.is_whitespace()) =>
                {
                    opens.push(start);
                    closes.push(index);
                    opening = None;
                }
                None if next.is_some_and(|c| !c.is_whitespace()) => {
                    opening = Some((index, marker));
                }
                _ => {}
            }
        }
        index += 1;
    }
    (opens, closes)
}
