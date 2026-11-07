//! Fence-aware message chunking and markdown atom splitting (module 5 of the foundation).
//!
//! Port of `truncate_message` and `_split_markdown_atoms` from hermes-agent
//! `gateway/platforms/base.py`. Adapters with `splits_long_messages` never get truncated by the
//! delivery router; those that cannot split natively (Telegram, Discord, ...) are chunked here so
//! the per-chunk measured length always stays under the platform cap.
//!
//! Guarantees, in order of precedence:
//!
//! 1. every returned chunk measures `<= max_len` under the caller's `len_fn` (Telegram counts
//!    UTF-16 code units, most platforms count code points, HTTP-ish platforms count bytes);
//! 2. a fenced code block that fits in a chunk on its own is never split — it is moved whole to
//!    the next chunk instead of being sliced;
//! 3. when a block is too big even alone, it is cut at line boundaries and every produced chunk
//!    closes the fence and re-opens it (` ```lang ` … ` ``` `) so no platform renders the rest of
//!    the message as broken markdown;
//! 4. text that already fits is returned untouched (no normalization, no re-encoded newlines).
//!
//! Line endings are preserved: a CRLF message keeps CRLF in every chunk, including the fence
//! lines this module synthesizes. The only content the chunker may drop is blank lines that would
//! otherwise open a chunk, and the single whitespace character a hard split breaks on.

/// Python `len()` measurement: Unicode scalar values. The hermes default for `message_len_fn`.
pub fn codepoint_len(text: &str) -> usize {
    text.chars().count()
}

/// Spaces tolerated before a fence marker (CommonMark allows three).
const FENCE_INDENT: usize = 3;
/// Shortest run that counts as a fence marker (CommonMark).
const MIN_FENCE_RUN: usize = 3;

/// An open fenced code block: the marker run (` ``` ` / `~~~`) plus the info string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fence {
    marker: String,
    info: String,
}

impl Fence {
    /// The synthesized opening line, e.g. ` ```rust `.
    fn open_line(&self) -> String {
        let mut out = self.marker.clone();
        out.push_str(&self.info);
        out
    }

    /// The synthesized closing line: the same marker run with no info string.
    fn close_line(&self) -> String {
        self.marker.clone()
    }

    fn marker_char(&self) -> char {
        self.marker.chars().next().unwrap_or('`')
    }

    fn marker_len(&self) -> usize {
        self.marker.chars().count()
    }
}

/// Strip up to [`FENCE_INDENT`] leading spaces so indented fences are still recognized.
fn trim_fence_indent(content: &str) -> &str {
    let mut stripped = content;
    for _ in 0..FENCE_INDENT {
        match stripped.strip_prefix(' ') {
            Some(rest) => stripped = rest,
            None => break,
        }
    }
    stripped
}

/// Parse a line as a fence opener. Returns `None` for content lines, short marker runs and
/// backtick fences whose info string contains a backtick (CommonMark forbids it).
fn parse_fence_open(content: &str) -> Option<Fence> {
    let body = trim_fence_indent(content);
    let marker_char = body.chars().next()?;
    if marker_char != '`' && marker_char != '~' {
        return None;
    }
    let marker_byte = if marker_char == '`' { b'`' } else { b'~' };
    let run = body.bytes().take_while(|byte| *byte == marker_byte).count();
    if run < MIN_FENCE_RUN {
        return None;
    }
    let info = body[run..].trim();
    if marker_char == '`' && info.contains('`') {
        return None;
    }
    Some(Fence {
        marker: marker_char.to_string().repeat(run),
        info: info.to_string(),
    })
}

/// Is `content` the closing line of `open`? A closer is the same character repeated at least as
/// many times as the opener, with nothing else on the line.
fn is_fence_close(content: &str, open: &Fence) -> bool {
    let body = trim_fence_indent(content);
    let marker_char = open.marker_char();
    let run = body.chars().take_while(|ch| *ch == marker_char).count();
    run >= open.marker_len() && body.chars().skip(run).all(char::is_whitespace)
}

/// How appending one line changes the fence state of a chunk.
///
/// A step owns a fence only on the line that opens a block, so the common case — a body line
/// inside a fenced block — is [`FenceStep::Keep`] and clones nothing. Returning an owned
/// `Option<Fence>` instead would clone the fence once per accepted line.
#[derive(Debug)]
enum FenceStep {
    /// The line neither opens nor closes a fence: the chunk keeps whatever fence it had.
    Keep,
    /// The line closes the open fence.
    Close,
    /// The line opens a fence that later lines must close.
    Open(Fence),
}

impl FenceStep {
    /// What the chunk must append to close its fence *after* this line, measured: `None` when the
    /// chunk would end outside a code block.
    fn close_len(&self, gauge: &Gauge, current: Option<&Fence>) -> Option<usize> {
        match self {
            FenceStep::Keep => current.map(|open| gauge.close_len(open)),
            FenceStep::Close => None,
            FenceStep::Open(open) => Some(gauge.close_len(open)),
        }
    }

    /// Install this step into the chunk's fence state.
    fn apply(self, slot: &mut Option<Fence>) {
        match self {
            FenceStep::Keep => {}
            FenceStep::Close => *slot = None,
            FenceStep::Open(open) => *slot = Some(open),
        }
    }
}

/// Fence state after appending `line` to a chunk that is currently inside `current`.
///
/// `track` is false for absurdly small limits where a fence pair cannot fit at all; blocks are
/// then treated as plain lines rather than emitting malformed fences.
fn fence_step(current: Option<&Fence>, line: &str, track: bool) -> FenceStep {
    if !track {
        // Untracked chunks always end with no fence.
        return if current.is_some() {
            FenceStep::Close
        } else {
            FenceStep::Keep
        };
    }
    match current {
        Some(open) => {
            if is_fence_close(line, open) {
                FenceStep::Close
            } else {
                FenceStep::Keep
            }
        }
        None => match parse_fence_open(line) {
            Some(open) => FenceStep::Open(open),
            None => FenceStep::Keep,
        },
    }
}

/// The measurement one [`truncate_message`] call works in: the caller's `len_fn` plus the line
/// ending the chunker synthesizes, with that ending measured once per call.
///
/// The accounting below assumes `len_fn` is additive over concatenation —
/// `len_fn(a) + len_fn(b) == len_fn(a + b)` — which every measure a platform passes here satisfies,
/// because each one counts units that a character contributes to on its own: Unicode scalar values
/// ([`codepoint_len`]), UTF-16 code units (`crate::platform::utf16_len`) or bytes (`str::len`).
/// No unit straddles a line boundary, so summing per-line measures plus the separators is exact.
#[derive(Debug, Clone, Copy)]
struct Gauge {
    len_fn: fn(&str) -> usize,
    /// The line ending synthesized for this message, preserved from it.
    eol: &'static str,
    /// `len_fn(eol)`, measured once instead of once per candidate.
    eol_len: usize,
}

impl Gauge {
    fn new(len_fn: fn(&str) -> usize, eol: &'static str) -> Self {
        Self {
            len_fn,
            eol,
            eol_len: len_fn(eol),
        }
    }

    /// `len_fn(text)`.
    fn len(self, text: &str) -> usize {
        (self.len_fn)(text)
    }

    /// `len_fn` of the synthesized closing line. [`Fence::close_line`] is the marker run verbatim,
    /// so its measure is the measure of the marker itself.
    fn close_len(self, fence: &Fence) -> usize {
        (self.len_fn)(&fence.marker)
    }
}

/// The accounting rule for a rendered chunk, without rendering it.
///
/// [`Frame::render`] joins `count` lines with `eol` and, when the chunk is cut inside a code block,
/// appends `eol` plus the closing marker. All measures are additive (see [`Gauge`]), so the
/// rendered length is exactly
///
/// `body_len + eol_len * (count - 1) + if fence { eol_len + close_len } else { 0 }`
///
/// where `body_len` is `sum(len_fn(line))` over the lines. That makes measuring a candidate O(1)
/// arithmetic instead of a re-join and a re-measure.
///
/// One arm exists only to stay exactly equal to `render`: its separator is emitted only when the
/// joined text is non-empty, so a frame whose joined text is empty contributes the closing marker
/// alone. `body_len == 0` is that case (`count == 0`, or a single empty line, since every supported
/// measure counts at least one unit for a non-empty line). `truncate_message` never reaches it —
/// a chunk holding a fence always holds the non-empty marker line — but the rule is total, so a
/// candidate length can be trusted in every state a frame can be in.
fn measured_chunk(gauge: &Gauge, body_len: usize, count: usize, close_len: Option<usize>) -> usize {
    if body_len == 0 && count <= 1 {
        return close_len.unwrap_or(0);
    }
    let mut total = body_len + gauge.eol_len * (count - 1);
    if let Some(marker_len) = close_len {
        total += gauge.eol_len + marker_len;
    }
    total
}

/// A chunk under construction: the lines written so far, and their summed measure.
///
/// `body_len` is the incremental accounting behind [`Frame::measured_with`]: it is kept in step by
/// [`Frame::push_line`], so no candidate ever has to be cloned and re-rendered to be measured.
/// The fence the chunk must close lives beside the frame, in [`truncate_message`], because a
/// candidate line only changes it on a handful of lines and keeping one authoritative copy avoids
/// a fence clone per accepted line.
#[derive(Debug, Clone, Default)]
struct Frame {
    lines: Vec<String>,
    /// `sum(gauge.len(line))` over `lines`.
    body_len: usize,
}

impl Frame {
    /// Append `line`, keeping the cached measure in step.
    fn push_line(&mut self, gauge: &Gauge, line: String) {
        self.body_len += gauge.len(&line);
        self.lines.push(line);
    }

    /// The text this frame becomes: joined lines, then a synthesized closing fence when the chunk
    /// is cut inside a code block.
    fn render(&self, gauge: &Gauge, fence: Option<&Fence>) -> String {
        let mut out = self.lines.join(gauge.eol);
        if let Some(fence) = fence {
            if !out.is_empty() {
                out.push_str(gauge.eol);
            }
            out.push_str(&fence.close_line());
        }
        out
    }

    /// The measured length of this frame plus one more line, already measured as `line_len`, whose
    /// chunk would cost `close_len` to close (`None` for a chunk that ends outside a code block).
    ///
    /// This is [`measured_chunk`] for `count + 1` lines: no clone, no render.
    fn measured_with(&self, gauge: &Gauge, line_len: usize, close_len: Option<usize>) -> usize {
        measured_chunk(
            gauge,
            self.body_len + line_len,
            self.lines.len() + 1,
            close_len,
        )
    }

    /// The measured length of this frame plus the whole lines `lines[from..to]`, with no open
    /// fence — the length the whole-block look-ahead would render.
    fn measured_with_block(
        &self,
        gauge: &Gauge,
        lines: &[String],
        from: usize,
        to: usize,
    ) -> usize {
        let body_len = self.body_len
            + lines[from..to]
                .iter()
                .map(|line| gauge.len(line))
                .sum::<usize>();
        measured_chunk(gauge, body_len, self.lines.len() + (to - from), None)
    }
}

/// [`Frame::measured_with_block`] for a chunk that holds nothing but `lines[from..to]`: the "would
/// this block fit in a chunk of its own?" length the look-ahead compares against `max_len`.
fn block_measured(gauge: &Gauge, lines: &[String], from: usize, to: usize) -> usize {
    Frame::default().measured_with_block(gauge, lines, from, to)
}

/// The newline convention to synthesize: preserved from the message.
fn detect_eol(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Split into lines without their terminator. A trailing newline yields no extra empty line.
fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Can a fence pair plus one character of body fit inside `max_len`?
fn fence_tracking_feasible(max_len: usize, len_fn: fn(&str) -> usize, eol: &str) -> bool {
    let overhead = len_fn("```") + len_fn(eol) + len_fn(eol) + len_fn("```");
    max_len > overhead
}

/// Exclusive end index of the fenced block that starts at `lines[pos]`, or `None` when the line
/// is not a fence opener or the block never closes.
fn closed_block_span(lines: &[String], pos: usize) -> Option<usize> {
    let open = parse_fence_open(lines.get(pos)?)?;
    let mut idx = pos + 1;
    while idx < lines.len() {
        if is_fence_close(&lines[idx], &open) {
            return Some(idx + 1);
        }
        idx += 1;
    }
    None
}

/// Split one over-long line into the largest head that fits the (empty) `frame` and the remainder.
///
/// Breaks on whitespace when that keeps at least half the available width, otherwise at a
/// character boundary. A single character is emitted even if it does not fit, so callers always
/// make progress instead of looping on an impossible limit.
///
/// `fence` is the fence the surrounding chunk would have to close, so a head is only accepted when
/// the finished chunk — head plus synthesized closer — still measures `<= max_len`.
fn split_line_to_fit(
    line: &str,
    frame: &Frame,
    max_len: usize,
    gauge: &Gauge,
    fence: Option<&Fence>,
) -> (String, Option<String>) {
    // `bounds[k]` is the byte index after the first `k` characters; `bounds[0]` is 0.
    let mut bounds: Vec<usize> = Vec::with_capacity(line.len() + 1);
    bounds.push(0);
    bounds.extend(line.char_indices().map(|(idx, _)| idx).skip(1));
    bounds.push(line.len());
    let char_count = bounds.len() - 1;

    // The candidate chunk keeps the fence the frame is cut inside, so its closing cost is fixed and
    // each probe is one `measured_chunk` instead of a frame clone plus a re-render.
    let close_len = fence.map(|open| gauge.close_len(open));
    let fits = |prefix: &str| frame.measured_with(gauge, gauge.len(prefix), close_len) <= max_len;

    // Largest number of characters whose prefix still fits (binary search: `fits` is monotone).
    let mut lo = 0usize;
    let mut hi = char_count;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(&line[..bounds[mid]]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }

    if lo >= char_count {
        return (line.to_string(), None);
    }
    if lo == 0 {
        let first = line.chars().next().map(|ch| ch.len_utf8()).unwrap_or(0);
        let (head, tail) = line.split_at(first);
        return (head.to_string(), non_empty(tail));
    }

    // Prefer a whitespace break so words stay whole.
    let head = &line[..bounds[lo]];
    if let Some((ws_byte, _)) = head.char_indices().rev().find(|(_, ch)| ch.is_whitespace()) {
        let ws_chars = head[..ws_byte].chars().count();
        if ws_chars > 0 && ws_chars * 2 >= lo {
            return (
                line[..ws_byte].to_string(),
                non_empty(line[ws_byte..].trim_start()),
            );
        }
    }
    let (head, tail) = line.split_at(bounds[lo]);
    (head.to_string(), non_empty(tail))
}

fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Split `text` into the fewest chunks whose measured length stays under `max_len`.
///
/// `len_fn` is the platform's measurement (see [`codepoint_len`]; Telegram passes
/// `crate::platform::utf16_len`). Empty input or `max_len == 0` produce no chunks. Text that
/// already fits is returned verbatim as a single chunk.
///
/// Blank lines that would open a chunk are dropped, and splits inside a fenced block close the
/// block at the end of one chunk and re-open it at the start of the next.
///
/// Runs in O(n) over the input: every candidate chunk is measured by [`measured_chunk`] from the
/// running per-line totals, so the greedy loop neither clones the growing frame nor re-joins and
/// re-measures the lines it already accepted.
pub fn truncate_message(text: &str, max_len: usize, len_fn: fn(&str) -> usize) -> Vec<String> {
    if text.is_empty() || max_len == 0 {
        return Vec::new();
    }
    if len_fn(text) <= max_len {
        return vec![text.to_string()];
    }

    let gauge = Gauge::new(len_fn, detect_eol(text));
    let track_fences = fence_tracking_feasible(max_len, len_fn, gauge.eol);
    let mut lines: Vec<String> = split_lines(text).into_iter().map(str::to_string).collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut fence: Option<Fence> = None;
    let mut pos = 0usize;

    while pos < lines.len() {
        // A chunk starts either fresh or by re-opening the fence it was cut inside. `frame` holds
        // the lines and their running measure; `fence` is the one the chunk must close.
        let mut frame = Frame::default();
        let mut seeded = 0usize;
        if let Some(open) = &fence {
            frame.push_line(&gauge, open.open_line());
            seeded = 1;
        }

        // Greedily take whole lines while the rendered chunk still fits. Each candidate is priced
        // from the cached totals plus this one line, so a rejected line costs no clone.
        while pos < lines.len() {
            // Blank lines never open a chunk.
            if frame.lines.len() == seeded && fence.is_none() && lines[pos].trim().is_empty() {
                pos += 1;
                continue;
            }
            // A fenced block that fits in an empty chunk is never sliced: move it whole.
            if frame.lines.len() > seeded
                && let Some(end) = closed_block_span(&lines, pos)
                && frame.measured_with_block(&gauge, &lines, pos, end) > max_len
                && block_measured(&gauge, &lines, pos, end) <= max_len
            {
                break;
            }
            let step = fence_step(fence.as_ref(), &lines[pos], track_fences);
            let close_len = step.close_len(&gauge, fence.as_ref());
            if frame.measured_with(&gauge, gauge.len(&lines[pos]), close_len) <= max_len {
                step.apply(&mut fence);
                frame.push_line(&gauge, lines[pos].clone());
                pos += 1;
            } else {
                break;
            }
        }

        // Nothing but the re-opened fence head fit: hard-split the over-long line.
        if frame.lines.len() == seeded && pos < lines.len() {
            let (head, tail) =
                split_line_to_fit(&lines[pos], &frame, max_len, &gauge, fence.as_ref());
            frame.push_line(&gauge, head);
            match tail {
                Some(rest) => lines[pos] = rest,
                None => pos += 1,
            }
        }

        if frame.lines.is_empty() {
            // Degenerate limit (nothing at all fits): drop the line to guarantee progress.
            pos += 1;
            continue;
        }
        chunks.push(frame.render(&gauge, fence.as_ref()));
    }
    chunks
}

/// Byte spans `(start, end_including_terminator)` for every line of `text`.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (idx, _) in text.match_indices('\n') {
        spans.push((start, idx + 1));
        start = idx + 1;
    }
    if start < text.len() {
        spans.push((start, text.len()));
    }
    spans
}

/// A line's text without its terminator.
fn line_content(text: &str, span: (usize, usize)) -> &str {
    text[span.0..span.1].trim_end_matches(&['\n', '\r'][..])
}

fn is_blank(content: &str) -> bool {
    content.trim().is_empty()
}

fn is_heading(content: &str) -> bool {
    let trimmed = content.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ')
}

fn is_list_item(content: &str) -> bool {
    let body = trim_fence_indent(content);
    for marker in ["- ", "* ", "+ "] {
        if body.starts_with(marker) {
            return true;
        }
    }
    // Ordered list: `<digits>. ` or `<digits>) `.
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let rest = &body[digits..];
    (rest.starts_with(". ") || rest.starts_with(") ")) && digits <= 9
}

fn is_table_row(content: &str) -> bool {
    content.trim_start().starts_with('|')
}

/// Split markdown into the human-meaningful atoms the gateway may reorder or move whole:
/// fenced code blocks, paragraphs, list items, headings, tables and standalone lines.
///
/// Every atom is a borrow of `text` and the atoms concatenated in order reproduce `text`
/// exactly. Blank runs are their own atoms so the invariant holds.
pub fn split_markdown_atoms(text: &str) -> Vec<&str> {
    let spans = line_spans(text);
    let mut atoms = Vec::new();
    let mut idx = 0usize;

    while idx < spans.len() {
        let start = idx;
        let content = line_content(text, spans[idx]);
        if let Some(open) = parse_fence_open(content) {
            idx += 1;
            while idx < spans.len() {
                let closing = is_fence_close(line_content(text, spans[idx]), &open);
                idx += 1;
                if closing {
                    break;
                }
            }
        } else if is_blank(content) {
            while idx < spans.len() && is_blank(line_content(text, spans[idx])) {
                idx += 1;
            }
        } else if is_table_row(content) {
            while idx < spans.len() && is_table_row(line_content(text, spans[idx])) {
                idx += 1;
            }
        } else if is_list_item(content) || is_heading(content) {
            idx += 1;
        } else {
            idx += 1;
            while idx < spans.len() {
                let next = line_content(text, spans[idx]);
                if is_blank(next)
                    || is_list_item(next)
                    || is_heading(next)
                    || is_table_row(next)
                    || parse_fence_open(next).is_some()
                {
                    break;
                }
                idx += 1;
            }
        }
        atoms.push(&text[spans[start].0..spans[idx - 1].1]);
    }
    atoms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::utf16_len;

    fn assert_within(chunks: &[String], max_len: usize, len_fn: fn(&str) -> usize) {
        for (idx, chunk) in chunks.iter().enumerate() {
            assert!(
                len_fn(chunk) <= max_len,
                "chunk {idx} measures {} > {max_len}: {chunk:?}",
                len_fn(chunk)
            );
        }
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn empty_and_zero_limit_produce_no_chunks() {
        assert!(truncate_message("", 100, codepoint_len).is_empty());
        assert!(truncate_message("hello", 0, codepoint_len).is_empty());
    }

    #[test]
    fn text_at_or_below_limit_is_untouched() {
        let text = "a".repeat(100);
        assert_eq!(
            truncate_message(&text, 100, codepoint_len),
            vec![text.clone()]
        );
        assert_eq!(truncate_message(&text, 101, codepoint_len), vec![text]);
    }

    #[test]
    fn one_over_the_limit_splits() {
        let text = "x".repeat(101);
        let chunks = truncate_message(&text, 100, codepoint_len);
        assert_eq!(chunks.len(), 2);
        assert_within(&chunks, 100, codepoint_len);
    }

    #[test]
    fn long_prose_splits_on_line_boundaries() {
        let lines: Vec<String> = (0..40)
            .map(|idx| format!("line {idx:02} of the long prose sample text"))
            .collect();
        let text = lines.join("\n");
        let chunks = truncate_message(&text, 120, codepoint_len);
        assert!(chunks.len() > 1);
        assert_within(&chunks, 120, codepoint_len);
        for chunk in &chunks {
            for line in chunk.split('\n') {
                assert!(
                    lines.iter().any(|source| source == line),
                    "chunk contains a partial line: {line:?}"
                );
            }
        }
        // Nothing is lost when no fences or blank lines are involved.
        let rejoined: Vec<&str> = chunks.iter().flat_map(|chunk| chunk.split('\n')).collect();
        assert_eq!(
            rejoined,
            lines.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn oversized_fenced_block_closes_and_reopens() {
        let body: Vec<String> = (0..30)
            .map(|idx| format!("fn line_{idx}() {{ }}"))
            .collect();
        let text = format!("before\n```rust\n{}\n```\nafter", body.join("\n"));
        let max_len = 120;
        let block_len = format!("```rust\n{}\n```", body.join("\n")).chars().count();
        assert!(
            block_len > max_len * 3,
            "block must be at least 3x the limit"
        );

        let chunks = truncate_message(&text, max_len, codepoint_len);
        assert!(chunks.len() >= 4, "expected several chunks: {chunks:#?}");
        assert_within(&chunks, max_len, codepoint_len);

        // Every chunk that carries block content opens and closes the fence itself.
        for chunk in &chunks {
            let opens = chunk.matches("```rust").count();
            let closes = chunk.lines().filter(|line| *line == "```").count();
            assert_eq!(opens, closes, "unbalanced fences in chunk: {chunk:?}");
            assert_eq!(closes, 1, "chunk should close exactly one block: {chunk:?}");
        }
        // Each body line survives exactly once and never broken.
        let joined = chunks.join("\n");
        for line in &body {
            assert_eq!(
                joined.matches(line.as_str()).count(),
                1,
                "line {line:?} lost or duplicated"
            );
        }
        assert!(chunks.first().unwrap().starts_with("before"));
        assert!(chunks.last().unwrap().ends_with("after"));
    }

    #[test]
    fn small_fenced_block_moves_whole_instead_of_slicing() {
        let text = format!("{}\n```py\nprint(1)\n```\ntail", "x".repeat(30));
        let chunks = truncate_message(&text, 40, codepoint_len);
        assert_within(&chunks, 40, codepoint_len);
        // The prose line fills the first chunk on its own; the block is moved whole.
        assert_eq!(chunks[0], "x".repeat(30));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.contains("```py\nprint(1)\n```")),
            "block must not be split: {chunks:#?}"
        );
        let fences: usize = chunks
            .iter()
            .map(|chunk| chunk.matches("```").count())
            .sum();
        assert_eq!(fences, 2, "the block is opened and closed exactly once");
    }

    #[test]
    fn crlf_input_keeps_crlf_and_fence_style() {
        let body: Vec<String> = (0..20).map(|idx| format!("crlf line {idx}")).collect();
        let text = format!("intro\r\n```sh\r\n{}\r\n```\r\noutro", body.join("\r\n"));
        let chunks = truncate_message(&text, 90, utf16_len);
        assert!(chunks.len() > 1);
        assert_within(&chunks, 90, utf16_len);
        for chunk in &chunks {
            assert!(
                !chunk.contains("\n")
                    || chunk.matches("\r\n").count() == chunk.matches('\n').count(),
                "mixed line endings in {chunk:?}"
            );
            assert!(chunk.contains("\r\n"), "CRLF lost in {chunk:?}");
        }
        assert!(chunks.iter().any(|chunk| chunk.contains("```sh\r\n")));
    }

    #[test]
    fn unicode_measured_in_utf16_units() {
        let text = "😀🚀🎉 line with emoji\n".repeat(20);
        let max_len = 64;
        let chunks = truncate_message(&text, max_len, utf16_len);
        assert!(chunks.len() > 1);
        assert_within(&chunks, max_len, utf16_len);
        // A code-point limit would have let these chunks grow past the UTF-16 cap; verify the
        // measurement really is UTF-16 by checking at least one chunk would violate codepoints.
        let codepoint_chunks = truncate_message(&text, max_len, codepoint_len);
        assert!(codepoint_chunks.len() < chunks.len());
    }

    #[test]
    fn single_long_line_is_hard_split_within_limit() {
        let text = "word ".repeat(200);
        let max_len = 50;
        let chunks = truncate_message(&text, max_len, codepoint_len);
        assert!(chunks.len() > 1);
        assert_within(&chunks, max_len, codepoint_len);
        assert_eq!(non_whitespace(&chunks.concat()), non_whitespace(&text));
        for chunk in &chunks {
            assert!(!chunk.trim().is_empty());
        }
    }

    #[test]
    fn invariant_holds_across_shapes_and_measures() {
        let shapes = [
            "plain text without any newlines at all, just prose that has to wrap somewhere"
                .to_string(),
            "para one\n\npara two\n\n- item a\n- item b\n1. first\n2. second".to_string(),
            "# heading\nbody text\n```toml\nkey = 1\nother = 2\n```\n| a | b |\n| - | - |"
                .to_string(),
            "\n\n\nleading blanks\n\n\n```\n{}\n```\n\n".replace("{}", &"z".repeat(300)),
        ];
        for (idx, text) in shapes.iter().enumerate() {
            for max_len in [1usize, 8, 24, 64, 200] {
                let chunks = truncate_message(text, max_len, codepoint_len);
                let overhead = fence_tracking_feasible(max_len, codepoint_len, "\n");
                for chunk in &chunks {
                    // At absurdly small limits a single character can exceed the cap; the
                    // impossible case is documented, everything else must hold.
                    if max_len >= 2 || !overhead {
                        assert_within(&chunks, max_len, codepoint_len);
                    }
                    assert!(!chunk.is_empty(), "shape {idx} produced an empty chunk");
                }
                assert_within(
                    &truncate_message(text, max_len, utf16_len),
                    max_len,
                    utf16_len,
                );
            }
        }
    }

    #[test]
    fn unclosed_fence_is_left_as_is() {
        let body: Vec<String> = (0..15)
            .map(|idx| format!("line {idx} of an unclosed block"))
            .collect();
        let text = format!("```\n{}", body.join("\n"));
        let chunks = truncate_message(&text, 100, codepoint_len);
        assert!(chunks.len() > 1);
        assert_within(&chunks, 100, codepoint_len);
        assert!(chunks[0].starts_with("```\n"));
    }

    #[test]
    fn atoms_cover_paragraphs_fences_lists_and_lines() {
        let text = "# Title\nfirst line\nsecond line\n\n- one\n- two\n```rust\ncode\n```\n| a | b |\n| c | d |\n";
        let atoms = split_markdown_atoms(text);
        assert_eq!(
            atoms,
            vec![
                "# Title\n",
                "first line\nsecond line\n",
                "\n",
                "- one\n",
                "- two\n",
                "```rust\ncode\n```\n",
                "| a | b |\n| c | d |\n",
            ]
        );
        assert_eq!(atoms.concat(), text);
    }

    #[test]
    fn atoms_reproduce_input_exactly() {
        for text in [
            "",
            "no trailing newline",
            "one\n\n\ntwo\n",
            "\r\n\r\nwindows\r\n",
            "```\nunclosed\n",
            "  ```\nindented fence\n  ```\n",
        ] {
            let atoms = split_markdown_atoms(text);
            assert_eq!(atoms.concat(), text, "atom round trip failed for {text:?}");
            if text.is_empty() {
                assert!(atoms.is_empty());
            } else {
                assert!(!atoms.is_empty());
            }
        }
    }

    #[test]
    fn fence_parsing_rejects_false_markers() {
        assert!(parse_fence_open("``not a fence``").is_none());
        assert!(parse_fence_open("text ```").is_none());
        assert!(parse_fence_open("```rust").is_some());
        assert!(parse_fence_open("~~~~python").is_some());
        let open = parse_fence_open("   ```js").expect("indented fence");
        assert_eq!(open.open_line(), "```js");
        assert!(is_fence_close("```", &open));
        assert!(is_fence_close("  ````  ", &open));
        assert!(!is_fence_close("``` x", &open));
    }

    #[test]
    fn list_item_and_heading_detection() {
        assert!(is_list_item("- item"));
        assert!(is_list_item("   * item"));
        assert!(is_list_item("10) item"));
        assert!(!is_list_item("-item"));
        assert!(!is_list_item("text - item"));
        assert!(is_heading("## heading"));
        assert!(!is_heading("#nope"));
        assert!(!is_heading("####### too deep"));
    }

    // ------------------------------------------------------------ incremental accounting

    /// Byte measure, the third of the three measures the chunker is used with.
    fn byte_len(text: &str) -> usize {
        text.len()
    }

    /// The three measures a platform passes, so every accounting equality below is checked against
    /// each of the codepoint / UTF-16 / byte semantics the caller may hold the chunker to.
    const MEASURES: [fn(&str) -> usize; 3] = [codepoint_len, utf16_len, byte_len];

    fn test_fence(marker: &str, info: &str) -> Fence {
        Fence {
            marker: marker.to_string(),
            info: info.to_string(),
        }
    }

    /// The accounting rule must equal the measured length of the actual render — that is the only
    /// thing the optimized loop relies on, so it is checked directly: every measure, both line
    /// endings, an empty frame, frames with and without a fence to close, the one-line extension
    /// the greedy loop prices, and the whole-block form the look-ahead prices.
    #[test]
    fn accounting_rule_equals_the_measured_render() {
        let source = [
            "",
            "plain line",
            "😀 CJK 漢字",
            "```rust",
            "| a | b |",
            "   ",
        ];
        let fences = [
            None,
            Some(test_fence("```", "rust")),
            Some(test_fence("~~~~~", "")),
        ];
        for len_fn in MEASURES {
            for eol in ["\n", "\r\n"] {
                let gauge = Gauge::new(len_fn, eol);
                for count in 0..=source.len() {
                    let mut frame = Frame::default();
                    for line in source.iter().take(count) {
                        frame.push_line(&gauge, (*line).to_string());
                    }
                    for fence in &fences {
                        let close_len = fence.as_ref().map(|open| gauge.close_len(open));
                        // The direct rule, for the frame as it stands (including the empty frame,
                        // which is the `count == 0` state of `measured_chunk`).
                        assert_eq!(
                            len_fn(&frame.render(&gauge, fence.as_ref())),
                            measured_chunk(&gauge, frame.body_len, count, close_len),
                            "frame measure {len_fn:?} eol {eol:?} count {count} fence {fence:?}"
                        );
                        // The greedy loop's rule: this frame plus one more line.
                        for extra in source {
                            let mut extended = frame.clone();
                            extended.push_line(&gauge, extra.to_string());
                            assert_eq!(
                                len_fn(&extended.render(&gauge, fence.as_ref())),
                                frame.measured_with(&gauge, gauge.len(extra), close_len),
                                "line {extra:?} measure {len_fn:?} eol {eol:?} count {count}"
                            );
                        }
                    }
                }
            }
        }

        // Whole-block form: a frame plus a run of lines, with no fence to close.
        let block: Vec<String> = source.iter().map(|line| (*line).to_string()).collect();
        for len_fn in MEASURES {
            for eol in ["\n", "\r\n"] {
                let gauge = Gauge::new(len_fn, eol);
                for prefix in 0..=3usize {
                    let mut frame = Frame::default();
                    for line in source.iter().take(prefix) {
                        frame.push_line(&gauge, (*line).to_string());
                    }
                    for from in 0..=block.len() {
                        for to in from..=block.len() {
                            let mut combined = frame.clone();
                            for line in &block[from..to] {
                                combined.push_line(&gauge, line.clone());
                            }
                            assert_eq!(
                                len_fn(&combined.render(&gauge, None)),
                                frame.measured_with_block(&gauge, &block, from, to),
                                "prefix {prefix} block {from}..{to} measure {len_fn:?} eol {eol:?}"
                            );
                            // A frame that holds nothing renders exactly the block.
                            let mut fresh = Frame::default();
                            for line in &block[from..to] {
                                fresh.push_line(&gauge, line.clone());
                            }
                            assert_eq!(
                                len_fn(&fresh.render(&gauge, None)),
                                block_measured(&gauge, &block, from, to),
                                "block {from}..{to} measure {len_fn:?} eol {eol:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ differential test

    // A verbatim copy of the pre-optimization chunker, kept only so the optimized one can be
    // differential-tested against it. Do not "fix" anything in here: it is the specification the
    // fast path has to reproduce chunk for chunk, byte for byte.

    #[derive(Debug, Clone, Default)]
    struct NaiveFrame {
        lines: Vec<String>,
        fence: Option<Fence>,
    }

    impl NaiveFrame {
        fn render(&self, eol: &str) -> String {
            let mut out = self.lines.join(eol);
            if let Some(fence) = &self.fence {
                if !out.is_empty() {
                    out.push_str(eol);
                }
                out.push_str(&fence.close_line());
            }
            out
        }

        fn measured(&self, len_fn: fn(&str) -> usize, eol: &str) -> usize {
            len_fn(&self.render(eol))
        }
    }

    fn naive_advance_fence(current: Option<&Fence>, line: &str, track: bool) -> Option<Fence> {
        if !track {
            return None;
        }
        match current {
            Some(open) => {
                if is_fence_close(line, open) {
                    None
                } else {
                    Some(open.clone())
                }
            }
            None => parse_fence_open(line),
        }
    }

    fn naive_split_line_to_fit(
        line: &str,
        frame: &NaiveFrame,
        max_len: usize,
        len_fn: fn(&str) -> usize,
        eol: &str,
    ) -> (String, Option<String>) {
        let mut bounds: Vec<usize> = Vec::with_capacity(line.len() + 1);
        bounds.push(0);
        bounds.extend(line.char_indices().map(|(idx, _)| idx).skip(1));
        bounds.push(line.len());
        let char_count = bounds.len() - 1;

        let fits = |prefix: &str| {
            let mut candidate = frame.clone();
            candidate.lines.push(prefix.to_string());
            candidate.measured(len_fn, eol) <= max_len
        };

        let mut lo = 0usize;
        let mut hi = char_count;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if fits(&line[..bounds[mid]]) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }

        if lo >= char_count {
            return (line.to_string(), None);
        }
        if lo == 0 {
            let first = line.chars().next().map(|ch| ch.len_utf8()).unwrap_or(0);
            let (head, tail) = line.split_at(first);
            return (head.to_string(), non_empty(tail));
        }

        let head = &line[..bounds[lo]];
        if let Some((ws_byte, _)) = head.char_indices().rev().find(|(_, ch)| ch.is_whitespace()) {
            let ws_chars = head[..ws_byte].chars().count();
            if ws_chars > 0 && ws_chars * 2 >= lo {
                return (
                    line[..ws_byte].to_string(),
                    non_empty(line[ws_byte..].trim_start()),
                );
            }
        }
        let (head, tail) = line.split_at(bounds[lo]);
        (head.to_string(), non_empty(tail))
    }

    fn naive_truncate_message(
        text: &str,
        max_len: usize,
        len_fn: fn(&str) -> usize,
    ) -> Vec<String> {
        if text.is_empty() || max_len == 0 {
            return Vec::new();
        }
        if len_fn(text) <= max_len {
            return vec![text.to_string()];
        }

        let eol = detect_eol(text);
        let track_fences = fence_tracking_feasible(max_len, len_fn, eol);
        let mut lines: Vec<String> = split_lines(text).into_iter().map(str::to_string).collect();
        let mut chunks: Vec<String> = Vec::new();
        let mut fence: Option<Fence> = None;
        let mut pos = 0usize;

        while pos < lines.len() {
            let mut frame = NaiveFrame::default();
            let mut seeded = 0usize;
            if let Some(open) = fence.clone() {
                frame.lines.push(open.open_line());
                frame.fence = Some(open);
                seeded = 1;
            }

            while pos < lines.len() {
                let line = lines[pos].clone();
                if frame.lines.len() == seeded && frame.fence.is_none() && line.trim().is_empty() {
                    pos += 1;
                    continue;
                }
                if frame.lines.len() > seeded
                    && let Some(end) = closed_block_span(&lines, pos)
                {
                    let mut candidate = frame.clone();
                    candidate.lines.extend(lines[pos..end].iter().cloned());
                    candidate.fence = None;
                    if candidate.measured(len_fn, eol) > max_len {
                        let mut fresh = NaiveFrame::default();
                        fresh.lines.extend(lines[pos..end].iter().cloned());
                        if fresh.measured(len_fn, eol) <= max_len {
                            break;
                        }
                    }
                }
                let next_fence = naive_advance_fence(fence.as_ref(), &line, track_fences);
                let mut candidate = frame.clone();
                candidate.lines.push(line);
                candidate.fence = next_fence.clone();
                if candidate.measured(len_fn, eol) <= max_len {
                    frame = candidate;
                    fence = next_fence;
                    pos += 1;
                } else {
                    break;
                }
            }

            if frame.lines.len() == seeded && pos < lines.len() {
                let (head, tail) =
                    naive_split_line_to_fit(&lines[pos], &frame, max_len, len_fn, eol);
                frame.lines.push(head);
                match tail {
                    Some(rest) => lines[pos] = rest,
                    None => pos += 1,
                }
            }

            if frame.lines.is_empty() {
                pos += 1;
                continue;
            }
            chunks.push(frame.render(eol));
        }
        chunks
    }

    /// Deterministic xorshift, so the differential test covers exactly the same inputs every run
    /// (a flaky-but-passing fuzz would not be a regression test).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut state = self.0;
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            self.0 = state;
            state
        }

        /// A value in `0..bound` (`0` for `bound == 0`).
        fn below(&mut self, bound: usize) -> usize {
            if bound == 0 {
                0
            } else {
                (self.next() % bound as u64) as usize
            }
        }

        fn pick<'a>(&mut self, options: &[&'a str]) -> &'a str {
            let idx = self.below(options.len());
            options.get(idx).copied().unwrap_or_default()
        }
    }

    fn repeat(text: &str, count: usize) -> String {
        text.repeat(count)
    }

    /// A generated document that mixes every shape the chunker branches on: prose, unbreakable
    /// long words, fenced blocks (plain, infostring, tilde, indented, unclosed), blank runs,
    /// headings, list items, emoji and CJK, with either line ending and with or without a trailing
    /// newline.
    fn generated_markdown(rng: &mut Rng) -> String {
        let eol = if rng.below(4) == 0 { "\r\n" } else { "\n" };
        let line_count = 1 + rng.below(26);
        let mut lines: Vec<String> = Vec::with_capacity(line_count + 8);
        while lines.len() < line_count {
            match rng.below(12) {
                0 => lines.push(String::new()),
                1 => {
                    // Fence opener, sometimes indented, sometimes a tilde fence.
                    let indent = repeat(" ", rng.below(FENCE_INDENT + 1));
                    let marker = if rng.below(3) == 0 { "~" } else { "`" };
                    let run = repeat(marker, MIN_FENCE_RUN + rng.below(3));
                    let info = rng.pick(&["", "rust", "py", "toml", " a`b", "text extra"]);
                    lines.push(format!("{indent}{run}{info}"));
                }
                2 => {
                    let indent = repeat(" ", rng.below(FENCE_INDENT + 1));
                    let marker = if rng.below(3) == 0 { "~" } else { "`" };
                    let run = repeat(marker, MIN_FENCE_RUN + rng.below(3));
                    lines.push(format!("{indent}{run}"));
                }
                3 => {
                    let hashes = repeat("#", 1 + rng.below(6));
                    lines.push(format!("{hashes} {}", rng.pick(&["heading", "gateway"])));
                }
                4 => lines.push(format!(
                    "{}{}",
                    rng.pick(&["- ", "* ", "1. ", "12) "]),
                    rng.pick(&["item", "😀 emoji", "漢字"])
                )),
                5 => {
                    let ch = rng.pick(&["x", "W", "😀", "漢"]);
                    lines.push(repeat(ch, 5 + rng.below(80)));
                }
                6 => lines.push(format!("| a{} | b |", rng.below(4))),
                7 => lines.push(repeat(" ", 1 + rng.below(4))),
                _ => {
                    let words = 1 + rng.below(8);
                    let mut line = String::new();
                    for word in 0..words {
                        if word > 0 {
                            line.push(' ');
                        }
                        line.push_str(rng.pick(&[
                            "alpha",
                            "gateway",
                            "chunk",
                            "🎉",
                            "漢字mixed",
                            "x",
                            "``",
                            "fence",
                        ]));
                    }
                    lines.push(line);
                }
            }
        }
        let mut text = lines.join(eol);
        if rng.below(2) == 0 {
            text.push_str(eol);
        }
        text
    }

    /// The optimized chunker must return exactly what the pre-optimization implementation returns:
    /// same chunks, same order, same bytes — for every measure and a spread of limits.
    #[test]
    fn optimized_chunker_matches_the_naive_reference() {
        let mut rng = Rng(0x5EED_1234_ABCD_9876);
        let mut texts: Vec<String> = vec![
            String::new(),
            "\n".to_string(),
            "\n\n\n".to_string(),
            "no trailing newline".to_string(),
            "a".to_string(),
            "😀".to_string(),
            "```\nunclosed".to_string(),
            "   ```js\nindented\n  ````\n".to_string(),
            "```\n```\n```\n```\n".to_string(),
            "~~~~\na\n~~~\n".to_string(),
            "\r\n\r\nwindows\r\n".to_string(),
            "word ".repeat(60),
            format!(
                "{}\n```\n{}\n```\n{}",
                "p ".repeat(30),
                "z".repeat(120),
                "q ".repeat(30)
            ),
        ];
        for _ in 0..48 {
            texts.push(generated_markdown(&mut rng));
        }

        let limits = [
            0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 21, 28, 40, 64, 100, 200, 4096,
        ];
        let mut compared = 0usize;
        let mut multi_chunk = 0usize;
        let mut fenced_multi_chunk = 0usize;
        for text in &texts {
            for max_len in limits {
                for len_fn in MEASURES {
                    let expected = naive_truncate_message(text, max_len, len_fn);
                    let actual = truncate_message(text, max_len, len_fn);
                    assert_eq!(
                        actual,
                        expected,
                        "divergence for max_len {max_len}, text {:?}",
                        &text[..text.len().min(200)]
                    );
                    compared += 1;
                    if expected.len() > 2 {
                        multi_chunk += 1;
                        if expected.iter().any(|chunk| chunk.contains("```")) {
                            fenced_multi_chunk += 1;
                        }
                    }
                }
            }
        }
        // Guard against a vacuous pass: the comparison must have exercised real chunking, and
        // real fence re-opening, not only inputs that come back as one chunk.
        assert!(compared > 3000, "only {compared} comparisons ran");
        assert!(
            multi_chunk > compared / 4,
            "only {multi_chunk} of {compared} comparisons produced more than two chunks"
        );
        assert!(
            fenced_multi_chunk > compared / 10,
            "only {fenced_multi_chunk} of {compared} comparisons chunked a fenced block"
        );
    }

    /// The same text under every measure must satisfy the cap. The lower limit is set above the
    /// worst fence overhead this generator can produce (a five-character marker, a CRLF line
    /// ending, and a four-byte body character) because `fence_tracking_feasible` only budgets for
    /// a three-character marker: below that, no chunking can honour the cap and the existing
    /// invariant test documents the same caveat.
    #[test]
    fn every_chunk_within_limit_across_measures() {
        let mut rng = Rng(0xC0DE_F00D_1234_5678);
        for _ in 0..24 {
            let text = generated_markdown(&mut rng);
            for max_len in [24usize, 40, 100, 1000] {
                for len_fn in MEASURES {
                    assert_within(&truncate_message(&text, max_len, len_fn), max_len, len_fn);
                }
            }
        }
    }

    /// ~64 KiB of markdown with fenced blocks, i.e. the shape the bench harness chunks.
    fn fenced_markdown_64k() -> String {
        let mut text = String::with_capacity(64 * 1024 + 512);
        let mut section = 0;
        while text.len() < 64 * 1024 {
            section += 1;
            text.push_str(&format!("## section {section}\n"));
            text.push_str("A gateway chunk must keep a fenced block whole whenever it fits.\n");
            if section % 5 == 0 {
                text.push_str("```rust\n");
                for line in 0..40 {
                    text.push_str(&format!("    let value_{line} = compute({line});\n"));
                }
                text.push_str("```\n");
            }
            text.push('\n');
        }
        text
    }

    /// Regression guard for the superlinear greedy loop: 64 KiB of fenced markdown used to take
    /// milliseconds because every accepted line cloned the growing frame and re-rendered it. The
    /// bound is deliberately generous (an order of magnitude above the observed debug-build cost)
    /// so a loaded CI box cannot fail it — it only catches a return to quadratic behaviour.
    #[test]
    fn sixty_four_kib_fenced_markdown_chunks_well_under_a_second() {
        let text = fenced_markdown_64k();
        assert!(text.len() >= 64 * 1024);

        let started = std::time::Instant::now();
        let chunks = truncate_message(&text, 4096, codepoint_len);
        let elapsed = started.elapsed();

        assert!(chunks.len() >= 16, "expected many chunks: {}", chunks.len());
        assert_within(&chunks, 4096, codepoint_len);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "64 KiB of fenced markdown took {elapsed:?}; the chunker is superlinear again"
        );
        // The content survives: chunking must not have silently dropped the body.
        let kept: usize = chunks.iter().map(String::len).sum();
        assert!(
            kept >= text.len(),
            "kept only {kept} of {} bytes",
            text.len()
        );
    }
}
