//! Text in a snip: read by Windows' own OCR engine (`Windows.Media.Ocr`, part of every Windows 10
//! and 11, so there is no model to ship), then put back together as plain text, Markdown, rich text
//! (HTML) or WhatsApp's markup.

use eframe::egui::{pos2, Color32, ColorImage, Pos2, Rect};
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// A recognised word and where it is, in image pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
}

/// The words of one line of text, left to right.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub words: Vec<Word>,
}

impl Line {
    pub fn rect(&self) -> Rect {
        self.words.iter().fold(Rect::NOTHING, |r, w| r.union(w.rect))
    }

    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    /// The tallest word: a word's box varies with its letters, the line's capitals don't.
    fn height(&self) -> f32 {
        self.words.iter().map(|w| w.rect.height()).fold(0.0, f32::max)
    }
}

/// How "Copy formatted" lays the text out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Markdown,
    RichText,
    WhatsApp,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Markdown, Format::RichText, Format::WhatsApp];

    pub fn label(self) -> &'static str {
        match self {
            Format::Markdown => "Markdown",
            Format::RichText => "Rich text",
            Format::WhatsApp => "WhatsApp",
        }
    }
}

/// At most this many languages read a snip, at once.
const MAX_LANGUAGES: usize = 3;

/// Reads the text in `img` with every OCR language Windows has installed (up to three, at once).
/// Each line is taken from the English reading unless another language read accented letters in
/// it: an engine reads letters it doesn't know as the nearest ones it does ("Ação" comes out of
/// the English one as "Aqäo"), while text without accents reads best in English. Takes as long as
/// the slowest language (tens of milliseconds for a window, ~100 ms for a 4K desktop), so call it
/// off the UI thread.
pub fn recognize(img: &ColorImage) -> Result<Vec<Line>, String> {
    let langs = languages()?;
    if langs.is_empty() {
        return Err("Windows has no text recognition installed (add a language in Settings > Time & language > Language & region)".into());
    }
    let (bgra, size, scale) = prepare(img, OcrEngine::MaxImageDimension().map_err(|e| e.message())? as usize);
    if size[0] == 0 || size[1] == 0 {
        return Ok(Vec::new());
    }
    let readings: Vec<(String, Result<Vec<Line>, String>)> = std::thread::scope(|s| {
        let runs: Vec<_> = langs.iter().map(|tag| (tag, s.spawn(|| read(tag, &bgra, size, scale)))).collect();
        runs.into_iter().map(|(tag, h)| (tag.clone(), h.join().unwrap_or_else(|_| Err("text recognition stopped".into())))).collect()
    });
    let mut ok: Vec<(bool, Vec<Line>)> = Vec::new();
    let mut first_err = None;
    for (tag, r) in readings {
        match r {
            Ok(lines) => ok.push((tag.to_ascii_lowercase().starts_with("en"), lines)),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    if ok.is_empty() {
        return Err(first_err.unwrap_or_else(|| "text recognition failed".into()));
    }
    // The English reading leads (its line order, and the lines only it found); without one, the first language's.
    let lead = ok.iter().position(|(en, _)| *en).unwrap_or(0);
    let (_, base) = ok.remove(lead);
    let others: Vec<Vec<Line>> = ok.into_iter().map(|(_, l)| l).collect();
    Ok(merge(base, &others))
}

/// The installed OCR languages, the user profile's first.
fn languages() -> Result<Vec<String>, String> {
    let err = |e: windows::core::Error| e.message();
    let mut tags: Vec<String> = Vec::new();
    if let Ok(tag) = OcrEngine::TryCreateFromUserProfileLanguages().and_then(|e| e.RecognizerLanguage()).and_then(|l| l.LanguageTag()) {
        tags.push(tag.to_string());
    }
    for lang in OcrEngine::AvailableRecognizerLanguages().map_err(err)? {
        let tag = lang.LanguageTag().map_err(err)?.to_string();
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    keep_english(&mut tags);
    Ok(tags)
}

/// Cuts `tags` to `MAX_LANGUAGES`, never leaving out English: it leads the merge (see `recognize`).
fn keep_english(tags: &mut Vec<String>) {
    if let Some(i) = tags.iter().position(|t| t.to_ascii_lowercase().starts_with("en")).filter(|&i| i >= MAX_LANGUAGES) {
        let en = tags.remove(i);
        tags.insert(MAX_LANGUAGES - 1, en);
    }
    tags.truncate(MAX_LANGUAGES);
}

/// One language's reading of the prepared picture, in image pixels.
fn read(tag: &str, bgra: &[u8], size: [usize; 2], scale: f32) -> Result<Vec<Line>, String> {
    let err = |e: windows::core::Error| format!("{tag}: {}", e.message());
    let lang = windows::Globalization::Language::CreateLanguage(&windows::core::HSTRING::from(tag)).map_err(err)?;
    let engine = OcrEngine::TryCreateFromLanguage(&lang).map_err(err)?;
    let writer = DataWriter::new().map_err(err)?;
    writer.WriteBytes(bgra).map_err(err)?;
    let buffer = writer.DetachBuffer().map_err(err)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, size[0] as i32, size[1] as i32).map_err(err)?;
    let result = engine.RecognizeAsync(&bitmap).map_err(err)?.join().map_err(err)?;
    let mut lines = Vec::new();
    for line in result.Lines().map_err(err)? {
        let mut words = Vec::new();
        for word in line.Words().map_err(err)? {
            let r = word.BoundingRect().map_err(err)?;
            let rect = Rect::from_min_max(pos2(r.X * scale, r.Y * scale), pos2((r.X + r.Width) * scale, (r.Y + r.Height) * scale));
            words.push(Word { text: word.Text().map_err(err)?.to_string(), rect });
        }
        if !words.is_empty() {
            lines.push(Line { words });
        }
    }
    Ok(lines)
}

/// `base`'s lines, each swapped for another reading of the same place that has accented letters.
fn merge(base: Vec<Line>, others: &[Vec<Line>]) -> Vec<Line> {
    base.into_iter()
        .map(|line| {
            let r = line.rect();
            match others.iter().flatten().filter(|o| overlap(r, o.rect()) > 0.6).find(|o| has_accents(o)) {
                Some(o) => o.clone(),
                None => line,
            }
        })
        .collect()
}

/// Intersection over union.
fn overlap(a: Rect, b: Rect) -> f32 {
    let i = a.intersect(b);
    if !i.is_positive() {
        return 0.0;
    }
    let area = |r: Rect| r.width() * r.height();
    area(i) / (area(a) + area(b) - area(i))
}

fn has_accents(line: &Line) -> bool {
    line.words.iter().any(|w| w.text.chars().any(|c| c.is_alphabetic() && !c.is_ascii()))
}

/// The picture as the engine takes it: BGRA bytes, their size, and image pixels per bitmap pixel.
/// Transparent areas (a freeform snip's outside) become white, and a picture longer than the
/// engine's `limit` on a side is shrunk by a whole factor, averaging each block of pixels.
fn prepare(img: &ColorImage, limit: usize) -> (Vec<u8>, [usize; 2], f32) {
    let [w, h] = img.size;
    let f = w.max(h).div_ceil(limit.max(1)).max(1);
    let (ow, oh) = (w / f, h / f);
    let mut out = Vec::with_capacity(ow * oh * 4);
    for y in 0..oh {
        for x in 0..ow {
            let mut sum = [0u32; 3];
            for yy in y * f..(y + 1) * f {
                for xx in x * f..(x + 1) * f {
                    let [r, g, b] = on_white(img.pixels[yy * w + xx]);
                    sum = [sum[0] + r as u32, sum[1] + g as u32, sum[2] + b as u32];
                }
            }
            let n = (f * f) as u32;
            out.extend([(sum[2] / n) as u8, (sum[1] / n) as u8, (sum[0] / n) as u8, 255]);
        }
    }
    (out, [ow, oh], f as f32)
}

fn on_white(c: Color32) -> [u8; 3] {
    let [r, g, b, a] = c.to_array();
    // Premultiplied: what's missing of the alpha is white showing through.
    let add = 255 - a;
    [r.saturating_add(add), g.saturating_add(add), b.saturating_add(add)]
}

/// How many words the lines hold.
pub fn word_count(lines: &[Line]) -> usize {
    lines.iter().map(|l| l.words.len()).sum()
}

/// The index (over all words, line by line) of the word at `p`, in image pixels: within `reach` of
/// its box, or, when `loose`, the nearest word of the nearest line, so a drag that runs past the
/// text still picks up to its end.
pub fn word_at(lines: &[Line], p: Pos2, reach: f32, loose: bool) -> Option<usize> {
    let mut best: Option<(f32, f32, usize)> = None;
    let mut i = 0;
    for line in lines {
        let r = line.rect();
        let dy = (r.top() - p.y).max(p.y - r.bottom()).max(0.0);
        for (k, w) in line.words.iter().enumerate() {
            let dx = (w.rect.left() - p.x).max(p.x - w.rect.right()).max(0.0);
            if best.is_none_or(|(by, bx, _)| (dy, dx) < (by, bx)) {
                best = Some((dy, dx, i + k));
            }
        }
        i += line.words.len();
    }
    let (dy, dx, index) = best?;
    (loose || (dy <= reach && dx <= reach)).then_some(index)
}

/// The words `from..=to` (indices over all words, line by line), as lines.
pub fn select(lines: &[Line], from: usize, to: usize) -> Vec<Line> {
    let (from, to) = (from.min(to), from.max(to));
    let mut i = 0;
    let mut out = Vec::new();
    for line in lines {
        let n = line.words.len();
        let (a, b) = (from.max(i), to.min(i + n - 1));
        if a <= b {
            out.push(Line { words: line.words[a - i..=b - i].to_vec() });
        }
        i += n;
    }
    out
}

/// One line per line of text, words single-spaced.
pub fn plain(lines: &[Line]) -> String {
    lines.iter().map(Line::text).collect::<Vec<_>>().join("\n")
}

/// The text laid out in `format`. For rich text this is an HTML fragment; plain text goes beside it.
pub fn formatted(lines: &[Line], format: Format) -> String {
    let blocks = blocks(lines);
    match format {
        Format::Markdown => markdown(&blocks),
        Format::RichText => html(&blocks),
        Format::WhatsApp => whatsapp(&blocks),
    }
}

/// What a stretch of the text looks like it is.
#[derive(Clone, Debug, PartialEq)]
enum Block {
    Heading(String),
    Paragraph(String),
    /// A list entry, with its number ("3") when the list is numbered.
    Item(Option<String>, String),
    Table(Vec<Vec<String>>),
}

/// A gap between words wider than this many line heights splits a line into table cells.
const CELL_GAP: f32 = 2.0;
/// Lines closer than this many line heights, one under the other, are one stretch of text.
const RUN_GAP: f32 = 0.8;
/// A line this much taller than the text's usual height is a heading.
const HEADING_RATIO: f32 = 1.3;
/// Table cells are short; longer runs side by side are columns of text.
const MAX_CELL_CHARS: usize = 40;
/// Wrapped text fills its lines: a line wraps into the next only if it reaches this share of its
/// stretch's widest line and is at least `WRAP_MIN` line heights long (about 35 letters), so a
/// short address or a list of names keeps its lines.
const WRAP_SHARE: f32 = 0.85;
const WRAP_MIN: f32 = 20.0;

/// The lines grouped into headings, paragraphs, list entries and tables, in reading order.
// ceiling: wrapped text and a block of long lines one under another look alike (the first two lines
// of an address join when the first is long); a font-metrics or language model would tell them apart.
fn blocks(lines: &[Line]) -> Vec<Block> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut heights: Vec<f32> = lines.iter().map(Line::height).collect();
    heights.sort_by(f32::total_cmp);
    let usual = heights[heights.len() / 2];
    let tables = tables(lines);
    let mut out = Vec::new();
    // Lines one under another, the same size and left edge, close together.
    let mut run: Vec<&Line> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(t) = tables.iter().find(|t| t.lines.contains(&i)) {
            flush(&mut run, &mut out);
            if t.lines.iter().min() == Some(&i) {
                out.push(Block::Table(t.rows.clone()));
            }
            continue;
        }
        let text = line.text();
        if line.height() >= usual * HEADING_RATIO && text.chars().count() <= 80 {
            flush(&mut run, &mut out);
            out.push(Block::Heading(text));
            continue;
        }
        let close = run.last().is_some_and(|prev| {
            let (p, r) = (prev.rect(), line.rect());
            let h = prev.height().max(line.height());
            r.top() >= p.top() + h * 0.5
                && r.top() - p.bottom() < h * RUN_GAP
                && (r.left() - p.left()).abs() < h * 2.0
                && (line.height() / prev.height()).max(prev.height() / line.height()) < 1.25
        });
        if !close {
            flush(&mut run, &mut out);
        }
        run.push(line);
    }
    flush(&mut run, &mut out);
    out
}

/// A stretch of lines as paragraphs and list entries: a line that fills the stretch's width wraps
/// into the next, unless that one starts a list entry.
fn flush(run: &mut Vec<&Line>, out: &mut Vec<Block>) {
    let widest = run.iter().map(|l| l.rect().width()).fold(0.0, f32::max);
    let mut text = String::new();
    for (k, line) in run.iter().enumerate() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&line.text());
        let w = line.rect().width();
        let wraps = run.get(k + 1).is_some_and(|next| w >= widest * WRAP_SHARE && w >= line.height() * WRAP_MIN && item(&next.text()).is_none());
        if !wraps {
            out.push(match item(&text) {
                Some((n, rest)) => Block::Item(n, rest),
                None => Block::Paragraph(std::mem::take(&mut text)),
            });
            text.clear();
        }
    }
    run.clear();
}

const BULLETS: [char; 9] = ['•', '·', '●', '▪', '◦', '‣', '-', '–', '*'];

/// A list entry's number (none for a bullet) and its text.
fn item(text: &str) -> Option<(Option<String>, String)> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if BULLETS.contains(&first) {
        let rest = chars.as_str();
        return rest.starts_with(' ').then(|| (None, rest.trim_start().to_string())).filter(|(_, r)| !r.is_empty());
    }
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    if (1..=3).contains(&digits.len()) {
        let rest = &text[digits.len()..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            if !r.trim().is_empty() {
                return Some((Some(digits), r.trim_start().to_string()));
            }
        }
    }
    None
}

struct Table {
    /// The lines it was made from (indices into the scan's lines).
    lines: Vec<usize>,
    rows: Vec<Vec<String>>,
}

/// Runs of two or more rows, one under the other, that each hold the same number (two or more) of
/// short cells side by side. A cell is a line, or the part of one between wide gaps: the engine
/// reads a table either way.
fn tables(lines: &[Line]) -> Vec<Table> {
    // Rows: lines sharing a band of height.
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[a].rect().center().y.total_cmp(&lines[b].rect().center().y));
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for i in order {
        let r = lines[i].rect();
        let same = rows.last().is_some_and(|row| {
            let first = lines[row[0]].rect();
            (r.center().y - first.center().y).abs() < first.height().min(r.height()) * 0.5
        });
        match rows.last_mut() {
            Some(row) if same => row.push(i),
            _ => rows.push(vec![i]),
        }
    }
    let cells = |row: &Vec<usize>| -> Vec<String> {
        let mut row = row.clone();
        row.sort_by(|&a, &b| lines[a].rect().left().total_cmp(&lines[b].rect().left()));
        row.iter().flat_map(|&i| split_cells(&lines[i])).collect()
    };
    let row_cells: Vec<Vec<String>> = rows.iter().map(cells).collect();
    let tabular = |c: &Vec<String>| c.len() >= 2 && c.iter().all(|s| s.chars().count() <= MAX_CELL_CHARS);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while tabular(&row_cells[i]) && j + 1 < rows.len() && row_cells[j + 1].len() == row_cells[i].len() && tabular(&row_cells[j + 1]) {
            j += 1;
        }
        if j > i {
            out.push(Table { lines: rows[i..=j].concat(), rows: row_cells[i..=j].to_vec() });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// A line's text cut at gaps wider than `CELL_GAP` line heights.
fn split_cells(line: &Line) -> Vec<String> {
    let h = line.height();
    let mut cells: Vec<Vec<&str>> = vec![Vec::new()];
    for (k, w) in line.words.iter().enumerate() {
        if k > 0 && w.rect.left() - line.words[k - 1].rect.right() > h * CELL_GAP {
            cells.push(Vec::new());
        }
        cells.last_mut().expect("never empty").push(&w.text);
    }
    cells.into_iter().map(|c| c.join(" ")).collect()
}

/// Blocks one after another: entries of one list (bulleted or numbered) on consecutive lines,
/// everything else a blank line apart.
fn join_blocks(blocks: &[Block], one: impl Fn(&Block) -> String) -> String {
    let mut out = String::new();
    for (k, b) in blocks.iter().enumerate() {
        if k > 0 {
            let list = matches!((&blocks[k - 1], b), (Block::Item(a, _), Block::Item(b, _)) if a.is_some() == b.is_some());
            out.push_str(if list { "\n" } else { "\n\n" });
        }
        out.push_str(&one(b));
    }
    out
}

fn list_item(n: &Option<String>, t: &str) -> String {
    match n {
        Some(n) => format!("{n}. {t}"),
        None => format!("- {t}"),
    }
}

fn markdown(blocks: &[Block]) -> String {
    join_blocks(blocks, |b| match b {
        Block::Heading(t) => format!("## {t}"),
        Block::Paragraph(t) => t.clone(),
        Block::Item(n, t) => list_item(n, t),
        Block::Table(rows) => {
            let row = |r: &Vec<String>| format!("| {} |", r.iter().map(|c| c.replace('|', "\\|")).collect::<Vec<_>>().join(" | "));
            let mut lines = vec![row(&rows[0]), format!("|{}", " --- |".repeat(rows[0].len()))];
            lines.extend(rows[1..].iter().map(row));
            lines.join("\n")
        }
    })
}

/// WhatsApp's markup: *bold* headings, "- " and "1. " lists, and tables (which it lacks) as
/// monospaced blocks with their columns padded to line up.
fn whatsapp(blocks: &[Block]) -> String {
    join_blocks(blocks, |b| match b {
        Block::Heading(t) => format!("*{t}*"),
        Block::Paragraph(t) => t.clone(),
        Block::Item(n, t) => list_item(n, t),
        Block::Table(rows) => {
            let widths: Vec<usize> = (0..rows[0].len()).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
            let lines: Vec<String> = rows.iter().map(|r| r.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect::<Vec<_>>().join("  ").trim_end().to_string()).collect();
            format!("```\n{}\n```", lines.join("\n"))
        }
    })
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn html(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut k = 0;
    while k < blocks.len() {
        match &blocks[k] {
            Block::Heading(t) => out.push_str(&format!("<h2>{}</h2>", escape(t))),
            Block::Paragraph(t) => out.push_str(&format!("<p>{}</p>", escape(t))),
            Block::Item(first, _) => {
                let tag = if first.is_some() { "ol" } else { "ul" };
                let start = first.as_ref().filter(|n| *n != "1").map(|n| format!(" start=\"{n}\"")).unwrap_or_default();
                out.push_str(&format!("<{tag}{start}>"));
                while let Some(Block::Item(n, t)) = blocks.get(k) {
                    if n.is_some() != first.is_some() {
                        break;
                    }
                    out.push_str(&format!("<li>{}</li>", escape(t)));
                    k += 1;
                }
                out.push_str(&format!("</{tag}>"));
                continue;
            }
            Block::Table(rows) => {
                let cell = |tag: &str, c: &String| format!("<{tag} style=\"border:1px solid #999;padding:4px 8px\">{}</{tag}>", escape(c));
                out.push_str("<table style=\"border-collapse:collapse\">");
                for (r, row) in rows.iter().enumerate() {
                    let tag = if r == 0 { "th" } else { "td" };
                    out.push_str(&format!("<tr>{}</tr>", row.iter().map(|c| cell(tag, c)).collect::<String>()));
                }
                out.push_str("</table>");
            }
        }
        k += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    /// A line of words at `(x, y)`, each `h` tall, a quarter of `h` apart, 0.55 `h` per letter.
    fn line(x: f32, y: f32, h: f32, text: &str) -> Line {
        let mut at = x;
        let words = text
            .split(' ')
            .map(|t| {
                let rect = Rect::from_min_size(pos2(at, y), vec2(t.chars().count() as f32 * h * 0.55, h));
                at = rect.right() + h * 0.25;
                Word { text: t.to_string(), rect }
            })
            .collect();
        Line { words }
    }

    #[test]
    fn selecting_words_keeps_their_lines_apart() {
        let lines = [line(0.0, 0.0, 10.0, "one two three"), line(0.0, 20.0, 10.0, "four five"), line(0.0, 40.0, 10.0, "six")];
        assert_eq!(word_count(&lines), 6);
        assert_eq!(plain(&select(&lines, 1, 3)), "two three\nfour");
        assert_eq!(plain(&select(&lines, 5, 2)), "three\nfour five\nsix", "a drag upwards selects the same words");
        assert_eq!(plain(&select(&lines, 0, 5)), "one two three\nfour five\nsix");
        assert_eq!(select(&lines, 9, 12), Vec::new());
    }

    #[test]
    fn a_word_is_picked_near_its_box_and_a_loose_drag_finds_the_nearest() {
        // "one two" on a line at y 0..10, "three" under it at y 20..30.
        let lines = [line(0.0, 0.0, 10.0, "one two"), line(0.0, 20.0, 10.0, "three")];
        assert_eq!(word_at(&lines, pos2(25.0, 5.0), 2.0, false), Some(1));
        assert_eq!(word_at(&lines, pos2(5.0, 31.0), 2.0, false), Some(2), "just below a word");
        assert_eq!(word_at(&lines, pos2(200.0, 5.0), 2.0, false), None, "far right of the line");
        assert_eq!(word_at(&lines, pos2(200.0, 5.0), 2.0, true), Some(1), "a drag past the line's end takes its last word");
        assert_eq!(word_at(&lines, pos2(5.0, 300.0), 2.0, true), Some(2));
        assert_eq!(word_at(&[], pos2(0.0, 0.0), 2.0, true), None);
    }

    #[test]
    fn wrapped_lines_become_one_paragraph_and_a_gap_starts_another() {
        let lines = [
            line(0.0, 0.0, 10.0, "The first paragraph runs right across the whole width"),
            line(0.0, 14.0, 10.0, "and wraps here."),
            line(0.0, 40.0, 10.0, "A second paragraph."),
        ];
        assert_eq!(formatted(&lines, Format::Markdown), "The first paragraph runs right across the whole width and wraps here.\n\nA second paragraph.");
    }

    #[test]
    fn short_lines_one_under_another_stay_apart() {
        let lines = [line(0.0, 0.0, 10.0, "Maria Silva"), line(0.0, 14.0, 10.0, "Rua das Flores, 123"), line(0.0, 28.0, 10.0, "São Paulo")];
        assert_eq!(formatted(&lines, Format::Markdown), "Maria Silva\n\nRua das Flores, 123\n\nSão Paulo");
    }

    #[test]
    fn taller_lines_are_headings() {
        let lines = [line(0.0, 0.0, 20.0, "Release notes"), line(0.0, 40.0, 10.0, "Fixed the save."), line(0.0, 60.0, 10.0, "Faster copy.")];
        assert_eq!(formatted(&lines, Format::Markdown), "## Release notes\n\nFixed the save.\n\nFaster copy.");
        assert_eq!(formatted(&lines, Format::WhatsApp), "*Release notes*\n\nFixed the save.\n\nFaster copy.");
        assert_eq!(formatted(&lines, Format::RichText), "<h2>Release notes</h2><p>Fixed the save.</p><p>Faster copy.</p>");
    }

    #[test]
    fn bullets_and_numbers_become_lists() {
        let lines = [line(0.0, 0.0, 10.0, "• Milk"), line(0.0, 14.0, 10.0, "• Eggs & bread"), line(0.0, 40.0, 10.0, "2) Then"), line(0.0, 54.0, 10.0, "3. Done")];
        assert_eq!(formatted(&lines, Format::Markdown), "- Milk\n- Eggs & bread\n\n2. Then\n3. Done");
        assert_eq!(formatted(&lines, Format::WhatsApp), "- Milk\n- Eggs & bread\n\n2. Then\n3. Done");
        assert_eq!(formatted(&lines, Format::RichText), "<ul><li>Milk</li><li>Eggs &amp; bread</li></ul><ol start=\"2\"><li>Then</li><li>Done</li></ol>");
    }

    #[test]
    fn a_wrapped_list_entry_stays_one_entry() {
        let lines = [line(0.0, 0.0, 10.0, "• The first entry is long enough to run across the list"), line(14.0, 14.0, 10.0, "and wraps."), line(0.0, 28.0, 10.0, "• Second")];
        assert_eq!(formatted(&lines, Format::Markdown), "- The first entry is long enough to run across the list and wraps.\n- Second");
    }

    #[test]
    fn a_number_or_dash_that_is_not_a_list_stays_text() {
        assert_eq!(item("2026 was a year"), None);
        assert_eq!(item("-5 degrees"), None);
        assert_eq!(item("1."), None);
        assert_eq!(item("12. Twelfth"), Some((Some("12".into()), "Twelfth".into())));
    }

    #[test]
    fn columns_in_rows_become_a_table_whichever_way_the_engine_split_them() {
        // The header row read as one line with a wide gap, the body read as one line per cell.
        let header = Line { words: [line(0.0, 0.0, 10.0, "Name").words, line(100.0, 0.0, 10.0, "Price").words].concat() };
        let lines = [
            line(0.0, -30.0, 10.0, "Prices this week"),
            header,
            line(0.0, 20.0, 10.0, "Apple"),
            line(0.0, 40.0, 10.0, "Pear | ripe"),
            line(100.0, 20.0, 10.0, "1,20"),
            line(100.0, 40.0, 10.0, "2,10"),
        ];
        assert_eq!(formatted(&lines, Format::Markdown), "Prices this week\n\n| Name | Price |\n| --- | --- |\n| Apple | 1,20 |\n| Pear \\| ripe | 2,10 |");
        assert_eq!(formatted(&lines, Format::WhatsApp), "Prices this week\n\n```\nName         Price\nApple        1,20\nPear | ripe  2,10\n```");
        assert!(formatted(&lines, Format::RichText).starts_with("<p>Prices this week</p><table style=\"border-collapse:collapse\"><tr><th style=\"border:1px solid #999;padding:4px 8px\">Name</th>"));
    }

    #[test]
    fn two_columns_of_prose_are_not_a_table() {
        let left = "This is a long sentence in the left column of a page";
        let right = "And this one runs along the right column of the page";
        let lines = [line(0.0, 0.0, 10.0, left), line(400.0, 0.0, 10.0, right), line(0.0, 14.0, 10.0, left), line(400.0, 14.0, 10.0, right)];
        assert!(!formatted(&lines, Format::Markdown).contains('|'));
    }

    #[test]
    fn an_accented_reading_replaces_the_english_one_only_where_it_overlaps() {
        let en = vec![line(0.0, 0.0, 10.0, "Aqäo rapida"), line(0.0, 20.0, 10.0, "I'll test")];
        let pt = vec![line(0.5, 0.5, 10.0, "Ação rápida"), line(0.0, 20.0, 10.0, "1'11 test")];
        assert_eq!(plain(&merge(en, &[pt])), "Ação rápida\nI'll test");
    }

    #[test]
    fn english_is_kept_when_more_languages_are_installed_than_are_run() {
        let mut tags: Vec<String> = ["pt-BR", "de-DE", "fr-FR", "es-ES", "en-US"].map(String::from).to_vec();
        keep_english(&mut tags);
        assert_eq!(tags, ["pt-BR", "de-DE", "en-US"]);
        let mut tags: Vec<String> = ["en-GB", "pt-BR", "de-DE", "fr-FR"].map(String::from).to_vec();
        keep_english(&mut tags);
        assert_eq!(tags, ["en-GB", "pt-BR", "de-DE"]);
    }

    #[test]
    fn transparent_pixels_read_as_white_and_huge_pictures_shrink() {
        let mut img = ColorImage::new([4, 2], vec![Color32::BLACK; 8]);
        img.pixels[0] = Color32::TRANSPARENT;
        let (bgra, size, scale) = prepare(&img, 10);
        assert_eq!((size, scale), ([4, 2], 1.0));
        assert_eq!(&bgra[..8], &[255, 255, 255, 255, 0, 0, 0, 255]);
        let (bgra, size, scale) = prepare(&img, 3);
        assert_eq!((size, scale), ([2, 1], 2.0));
        // The first 2x2 block: one white pixel among three black ones.
        assert_eq!(&bgra[..4], &[63, 63, 63, 255]);
    }

    /// Reads the PNGs in `SNIPSCREEN_OCR_DIR` and prints what comes out, with timings.
    #[test]
    #[ignore = "report on real pictures, not a check"]
    fn ocr_report() {
        let Ok(dir) = std::env::var("SNIPSCREEN_OCR_DIR") else { return };
        println!("languages: {:?}", languages());
        for entry in std::fs::read_dir(dir).expect("dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|e| e != "png") {
                continue;
            }
            let img = crate::notify::load_png(&path).expect("png");
            let t = std::time::Instant::now();
            let lines = recognize(&img).expect("recognized");
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            println!("== {} {}x{}: {} lines, {} words, {ms:.0} ms", path.display(), img.size[0], img.size[1], lines.len(), word_count(&lines));
            println!("{}\n-- markdown --\n{}", plain(&lines), formatted(&lines, Format::Markdown));
        }
    }
}
