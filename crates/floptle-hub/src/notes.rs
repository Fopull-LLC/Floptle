//! Rendering release notes — the Markdown in `docs/releases/vX.Y.Z.md` — into egui.
//!
//! Deliberately small. This renders one author's prose, written to a house style we
//! control, not arbitrary Markdown off the internet: headings, bullets, numbered items,
//! fenced code, block quotes, tables and rules, with `**bold**`, `` `code` `` and links
//! inline. That covers every construct in `docs/releases/` (counted, not guessed) and
//! stops there — a full CommonMark dependency to render our own changelog would be a
//! larger surface than the feature.
//!
//! Anything unrecognised falls through as a paragraph, so a construct we don't know
//! prints its own source rather than vanishing. **Nothing here can fail**: notes arrive
//! over the network, and a version whose notes are odd must still show its version.

use eframe::egui;
use egui::{Color32, FontId, RichText, TextFormat, Ui, text::LayoutJob};

/// One parsed block. The whole document is a flat list — nesting isn't in the house style.
#[derive(Clone, Debug, PartialEq)]
enum Block {
    Heading(u8, String),
    Para(String),
    Bullet(String),
    Numbered(String, String),
    Quote(String),
    Code(String),
    Table(Vec<Vec<String>>),
    Rule,
}

/// Split Markdown into blocks. Line-oriented, and a fence swallows everything up to the
/// closing one — including lines that would otherwise look like headings, which is the
/// point of a fence.
fn parse(md: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    let mut table: Vec<Vec<String>> = Vec::new();

    // A paragraph ends when anything else starts; flushing here keeps that in one place.
    macro_rules! flush {
        ($out:expr) => {
            if !para.is_empty() {
                $out.push(Block::Para(para.join(" ")));
                para.clear();
            }
            if !table.is_empty() {
                $out.push(Block::Table(std::mem::take(&mut table)));
            }
        };
    }

    for line in md.lines() {
        if let Some(body) = code.as_mut() {
            if line.trim_start().starts_with("```") {
                out.push(Block::Code(body.join("\n")));
                code = None;
            } else {
                body.push(line);
            }
            continue;
        }
        let t = line.trim();
        if t.starts_with("```") {
            flush!(out);
            code = Some(Vec::new());
            continue;
        }
        if t.is_empty() {
            flush!(out);
            continue;
        }
        // A table row. The `|---|---|` separator carries no content, so it is dropped
        // rather than drawn as a row of dashes.
        if t.starts_with('|') {
            if !para.is_empty() {
                out.push(Block::Para(para.join(" ")));
                para.clear();
            }
            let cells: Vec<String> =
                t.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect();
            if !cells.iter().all(|c| c.chars().all(|ch| ch == '-' || ch == ':') && !c.is_empty()) {
                table.push(cells);
            }
            continue;
        }
        if !table.is_empty() {
            out.push(Block::Table(std::mem::take(&mut table)));
        }
        if t.chars().all(|c| c == '-') && t.len() >= 3 {
            flush!(out);
            out.push(Block::Rule);
            continue;
        }
        if let Some(rest) = t.strip_prefix("#### ").map(|r| (4u8, r))
            .or_else(|| t.strip_prefix("### ").map(|r| (3, r)))
            .or_else(|| t.strip_prefix("## ").map(|r| (2, r)))
            .or_else(|| t.strip_prefix("# ").map(|r| (1, r)))
        {
            flush!(out);
            out.push(Block::Heading(rest.0, rest.1.trim().to_string()));
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ").or_else(|| t.strip_prefix(">")) {
            flush!(out);
            out.push(Block::Quote(rest.trim().to_string()));
            continue;
        }
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            flush!(out);
            out.push(Block::Bullet(rest.trim().to_string()));
            continue;
        }
        if let Some((num, rest)) = numbered(t) {
            flush!(out);
            out.push(Block::Numbered(num, rest));
            continue;
        }
        // An indented line continuing a bullet reads as part of it, not as a new
        // paragraph — the notes wrap their lists at 80 columns.
        if line.starts_with("  ") && matches!(out.last(), Some(Block::Bullet(_) | Block::Numbered(..)))
            && para.is_empty()
        {
            match out.last_mut() {
                Some(Block::Bullet(b)) | Some(Block::Numbered(_, b)) => {
                    b.push(' ');
                    b.push_str(t);
                }
                _ => unreachable!(),
            }
            continue;
        }
        para.push(t);
    }
    if let Some(body) = code {
        out.push(Block::Code(body.join("\n")));
    }
    flush!(out);
    out
}

/// `"3. text"` → `("3", "text")`.
fn numbered(t: &str) -> Option<(String, String)> {
    let dot = t.find(". ")?;
    let head = &t[..dot];
    (!head.is_empty() && head.chars().all(|c| c.is_ascii_digit()))
        .then(|| (head.to_string(), t[dot + 2..].trim().to_string()))
}

/// One run of inline text and how it is set.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Style {
    Plain,
    Bold,
    Italic,
    Code,
    Link,
}

/// Split a line into styled runs. `**bold**`, `` `code` ``, `[text](url)`; the URL is
/// dropped because these pages open in a Hub with no browser of its own and a link that
/// looks clickable and isn't is worse than plain emphasis.
fn runs(s: &str) -> Vec<(Style, String)> {
    let mut out: Vec<(Style, String)> = Vec::new();
    let mut plain = String::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let push = |out: &mut Vec<(Style, String)>, plain: &mut String| {
        if !plain.is_empty() {
            out.push((Style::Plain, std::mem::take(plain)));
        }
    };
    while i < b.len() {
        // `code` — first, so `**` inside a span stays literal.
        if b[i] == '`'
            && let Some(end) = (i + 1..b.len()).find(|&j| b[j] == '`')
        {
            push(&mut out, &mut plain);
            out.push((Style::Code, b[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        if b[i] == '*'
            && b.get(i + 1) == Some(&'*')
            && let Some(end) =
                (i + 2..b.len().saturating_sub(1)).find(|&j| b[j] == '*' && b[j + 1] == '*')
        {
            push(&mut out, &mut plain);
            out.push((Style::Bold, b[i + 2..end].iter().collect()));
            i = end + 2;
            continue;
        }
        // A LONE asterisk is italic, and must be tried after `**` so bold still wins.
        // Guarded on a non-space neighbour so `2 * 3` and a bare `*` stay literal.
        if b[i] == '*'
            && b.get(i + 1).is_some_and(|c| !c.is_whitespace())
            && let Some(end) = (i + 1..b.len()).find(|&j| b[j] == '*' && !b[j - 1].is_whitespace())
        {
            push(&mut out, &mut plain);
            out.push((Style::Italic, b[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        if b[i] == '['
            && let Some(close) = (i + 1..b.len()).find(|&j| b[j] == ']')
            && b.get(close + 1) == Some(&'(')
            && let Some(end) = (close + 2..b.len()).find(|&j| b[j] == ')')
        {
            push(&mut out, &mut plain);
            out.push((Style::Link, b[i + 1..close].iter().collect()));
            i = end + 1;
            continue;
        }
        plain.push(b[i]);
        i += 1;
    }
    push(&mut out, &mut plain);
    out
}

/// Draw a line of inline Markdown at `size`, wrapping to the available width.
fn inline(ui: &mut Ui, text: &str, size: f32, color: Color32) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    let mono = FontId::monospace(size - 1.0);
    for (style, run) in runs(text) {
        let (font, col) = match style {
            Style::Plain => (FontId::proportional(size), color),
            Style::Bold => (FontId::proportional(size), ui.visuals().strong_text_color()),
            // egui's default fonts ship no italic face, so emphasis is carried by colour.
            // Better than printing the asterisks, which is what happened before.
            Style::Italic => (FontId::proportional(size), ui.visuals().strong_text_color()),
            Style::Code => (mono.clone(), ui.visuals().hyperlink_color.gamma_multiply(0.9)),
            Style::Link => (FontId::proportional(size), ui.visuals().hyperlink_color),
        };
        job.append(&run, 0.0, TextFormat { font_id: font, color: col, ..Default::default() });
    }
    ui.label(job);
}

/// Render release notes into `ui`. Never panics and never returns an error: an empty or
/// unparseable body simply draws nothing, and the caller has already drawn the version.
pub fn render(ui: &mut Ui, md: &str) {
    let dim = ui.visuals().weak_text_color();
    let body = ui.visuals().text_color();
    for block in parse(md) {
        match block {
            Block::Heading(level, text) => {
                ui.add_space(if level <= 2 { 12.0 } else { 8.0 });
                let size = match level {
                    1 => 20.0,
                    2 => 16.5,
                    _ => 14.5,
                };
                inline(ui, &text, size, ui.visuals().strong_text_color());
                ui.add_space(3.0);
            }
            Block::Para(text) => {
                inline(ui, &text, 13.5, body);
                ui.add_space(6.0);
            }
            Block::Bullet(text) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("•").color(dim));
                    inline(ui, &text, 13.5, body);
                });
                ui.add_space(2.0);
            }
            Block::Numbered(n, text) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("{n}.")).color(dim));
                    inline(ui, &text, 13.5, body);
                });
                ui.add_space(2.0);
            }
            Block::Quote(text) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new("▏").color(ui.visuals().hyperlink_color));
                    inline(ui, &text, 13.5, dim);
                });
                ui.add_space(4.0);
            }
            Block::Code(text) => {
                // Its own scroll area: a wide code line must not widen the page and force
                // the whole panel to scroll sideways.
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::horizontal().id_salt(egui::Id::new(&text)).show(ui, |ui| {
                            ui.label(RichText::new(&text).monospace().size(12.0));
                        });
                    });
                ui.add_space(8.0);
            }
            Block::Table(rows) => {
                egui::Grid::new(egui::Id::new(("notes-table", rows.len(), rows.first().cloned())))
                    .num_columns(rows.first().map_or(1, |r| r.len()))
                    .spacing([14.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for (i, row) in rows.iter().enumerate() {
                            for cell in row {
                                inline(ui, cell, 13.0, if i == 0 { ui.visuals().strong_text_color() } else { body });
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(8.0);
            }
            Block::Rule => {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_house_style_parses_into_the_blocks_it_looks_like() {
        let md = "\
# Floptle v0.9.0

An opening line
that wraps.

## A heading

- a bullet
  continued on the next line
- another

1. first
2. second

> quoted

```lua
account.signIn()
```

| a | b |
|---|---|
| 1 | 2 |
";
        let blocks = parse(md);
        assert_eq!(blocks[0], Block::Heading(1, "Floptle v0.9.0".into()));
        // A wrapped paragraph is ONE paragraph — joined, not two lines with a break.
        assert_eq!(blocks[1], Block::Para("An opening line that wraps.".into()));
        assert_eq!(blocks[2], Block::Heading(2, "A heading".into()));
        assert_eq!(blocks[3], Block::Bullet("a bullet continued on the next line".into()));
        assert_eq!(blocks[4], Block::Bullet("another".into()));
        assert_eq!(blocks[5], Block::Numbered("1".into(), "first".into()));
        assert_eq!(blocks[7], Block::Quote("quoted".into()));
        assert_eq!(blocks[8], Block::Code("account.signIn()".into()));
        // The |---| separator row carries nothing and is not drawn.
        assert_eq!(blocks[9], Block::Table(vec![vec!["a".into(), "b".into()], vec!["1".into(), "2".into()]]));
    }

    #[test]
    fn a_fence_swallows_markup_instead_of_obeying_it() {
        let blocks = parse("```\n# not a heading\n- not a bullet\n```\n");
        assert_eq!(blocks, vec![Block::Code("# not a heading\n- not a bullet".into())]);
        // An unterminated fence still yields its content rather than dropping it.
        assert_eq!(parse("```\nstuck\n"), vec![Block::Code("stuck".into())]);
    }

    #[test]
    fn inline_runs_split_on_emphasis_code_and_links() {
        assert_eq!(
            runs("a **bold** and `code` and [text](http://x)"),
            vec![
                (Style::Plain, "a ".into()),
                (Style::Bold, "bold".into()),
                (Style::Plain, " and ".into()),
                (Style::Code, "code".into()),
                (Style::Plain, " and ".into()),
                (Style::Link, "text".into()),
            ]
        );
        // Unclosed markers stay literal rather than eating the rest of the line.
        assert_eq!(runs("2 * 3 and a `stray"), vec![(Style::Plain, "2 * 3 and a `stray".into())]);
        // A `**` inside a code span is not emphasis.
        assert_eq!(runs("`a ** b`"), vec![(Style::Code, "a ** b".into())]);
        // Single asterisks are italic, but only around something.
        assert_eq!(
            runs("launched *from* the Hub"),
            vec![
                (Style::Plain, "launched ".into()),
                (Style::Italic, "from".into()),
                (Style::Plain, " the Hub".into()),
            ]
        );
        assert_eq!(runs("2 * 3 * 4"), vec![(Style::Plain, "2 * 3 * 4".into())]);
        // Bold still wins over italic when both could match.
        assert_eq!(runs("**both**"), vec![(Style::Bold, "both".into())]);
    }

    /// The notes we actually ship, parsed. Not an assertion about their content — an
    /// assertion that no shipped release note hits a case this parser drops on the floor.
    #[test]
    fn every_shipped_release_note_parses_to_something() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/releases");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut seen = 0;
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let blocks = parse(&text);
            assert!(!blocks.is_empty(), "{} parsed to nothing", p.display());
            // Every fence closed: an odd count means a Code block ran to the end of the
            // file and swallowed the rest of the notes.
            assert_eq!(
                text.lines().filter(|l| l.trim_start().starts_with("```")).count() % 2,
                0,
                "{} has an unclosed code fence",
                p.display()
            );
            seen += 1;
        }
        assert!(seen > 10, "expected the release notes to be there, found {seen}");
    }
}
