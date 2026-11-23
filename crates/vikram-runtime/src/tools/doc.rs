use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;

/// A document is read once and its text enters the transcript, so the file ceiling is about the
/// extraction helper's memory, not about disk.
const MAX_DOC_BYTES: u64 = 16 * 1024 * 1024;
/// What one call may lift out of a document before the elision marker takes over.
const MAX_TEXT_BYTES: usize = 256 * 1024;
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pdf,
    Docx,
    Xlsx,
}

impl Kind {
    fn of(path: &Path) -> Option<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())?
            .to_ascii_lowercase();
        match extension.as_str() {
            "pdf" => Some(Self::Pdf),
            "docx" => Some(Self::Docx),
            "xlsx" => Some(Self::Xlsx),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
        }
    }
}

pub(crate) struct DocRead;

impl Tool for DocRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "doc.read".into(),
            description: "Read the text of a PDF, DOCX or XLSX file from the workspace. PDFs need \
                          pdftotext; DOCX and XLSX need unzip. For .xlsx, `range` reads one named \
                          range by its sheet and bounds."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Document to read; a relative path is resolved against the workspace root."
                    },
                    "range": {
                        "type": "string",
                        "description": "For .xlsx: the name of one defined range; returns its sheet, bounds and cell values."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "doc.read",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let (path, display) = canonical_resource(&ctx.workspace_root, &raw)?;
        let Some(kind) = Kind::of(&path) else {
            return Ok(refused(format!(
                "doc.read reads .pdf, .docx and .xlsx; `{display}` is none of those"
            )));
        };
        let length = match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => return Ok(refused(format!("`{display}` is not a file"))),
            Err(error) => return Ok(refused(format!("cannot read `{display}`: {error}"))),
        };
        if length > MAX_DOC_BYTES {
            return Ok(refused(format!(
                "`{display}` is {:.1} MiB, above the {} MiB ceiling doc.read will read",
                length as f64 / (1024.0 * 1024.0),
                MAX_DOC_BYTES / (1024 * 1024)
            )));
        }

        let requested_range = args
            .get("range")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some(name) = requested_range {
            if kind != Kind::Xlsx {
                return Ok(refused(format!(
                    "`range` reads a named range from .xlsx workbooks; `{display}` is {}",
                    kind.label()
                )));
            }
            return Ok(match xlsx_named_range(&path, name, ctx) {
                Ok(text) => ToolOutput {
                    content: truncated(format!("xlsx `{display}`\n\n{text}")),
                    ok: true,
                },
                Err(message) => refused(format!("xlsx `{display}`: {message}")),
            });
        }

        let body = match kind {
            Kind::Pdf => pdf_text(&path, ctx),
            Kind::Docx => docx_text(&path, ctx),
            Kind::Xlsx => xlsx_text(&path, ctx),
        };
        match body {
            Ok(text) if text.trim().is_empty() => {
                let why = match kind {
                    Kind::Pdf => {
                        "a scanned PDF has no text layer, and computer.ocr reads images rather than PDFs"
                    }
                    _ => "the file holds no text this tool can read",
                };
                Ok(ToolOutput {
                    content: format!("{} `{display}`: no text extracted — {why}", kind.label()),
                    ok: true,
                })
            }
            Ok(text) => Ok(ToolOutput {
                content: truncated(format!("{} `{display}`\n\n{text}", kind.label())),
                ok: true,
            }),
            Err(error) => Ok(refused(format!("{} `{display}`: {error}", kind.label()))),
        }
    }
}

fn refused(content: String) -> ToolOutput {
    ToolOutput { content, ok: false }
}

/// The message a caller sees when an extraction helper is missing: the package, not just the name.
fn missing_helper(name: &str) -> String {
    let hint = if cfg!(target_os = "macos") {
        match name {
            "pdftotext" => "brew install poppler".to_owned(),
            "unzip" => "brew install unzip".to_owned(),
            _ => "install it and keep it on PATH".to_owned(),
        }
    } else if cfg!(target_os = "windows") {
        match name {
            "pdftotext" => "install poppler and keep pdftotext.exe on PATH".to_owned(),
            "unzip" => "install unzip and keep unzip.exe on PATH".to_owned(),
            _ => "install it and keep it on PATH".to_owned(),
        }
    } else {
        match name {
            "pdftotext" => "apt install poppler-utils".to_owned(),
            "unzip" => "apt install unzip".to_owned(),
            _ => "install it and keep it on PATH".to_owned(),
        }
    };
    format!("{name} not found; install it: {hint}")
}

fn require_helper(name: &str) -> Result<(), String> {
    match crate::computer::which(name) {
        Some(_) => Ok(()),
        None => Err(missing_helper(name)),
    }
}

/// Run an extraction helper and keep only its stdout, on success.
///
/// The output is capped before it becomes a `String`: a document with a huge text layer must not
/// turn one tool call into an allocation the transcript will never use.
fn run_helper(
    program: &str,
    args: &[std::ffi::OsString],
    ctx: &ToolContext,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    ctx.sandbox.wrap_command(&mut command, &ctx.workspace_root);
    match crate::verification::wait_with_timeout(&mut command, HELPER_TIMEOUT, ctx.cancel.as_ref())
    {
        Ok(crate::verification::WaitOutcome::Finished(output)) => {
            if output.status.success() {
                return Ok(capped(output.stdout));
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stderr.is_empty() {
                format!("exit {:?}", output.status.code())
            } else {
                stderr
            };
            Err(format!("`{program}` failed: {detail}"))
        }
        Ok(crate::verification::WaitOutcome::TimedOut(_)) => Err(format!(
            "`{program}` timed out after {}s",
            HELPER_TIMEOUT.as_secs()
        )),
        Err(error) => Err(format!("`{program}` could not run: {error}")),
    }
}

fn capped(bytes: Vec<u8>) -> String {
    let cut = bytes.len() > MAX_TEXT_BYTES;
    let prefix = &bytes[..bytes.len().min(MAX_TEXT_BYTES)];
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    if cut {
        text.push_str(&format!(
            "\n… [text cut at {MAX_TEXT_BYTES} bytes of {} extracted; read a page range instead]",
            bytes.len()
        ));
    }
    text
}

fn pdf_text(path: &Path, ctx: &ToolContext) -> Result<String, String> {
    require_helper("pdftotext")?;
    let args = [
        std::ffi::OsString::from("-layout"),
        path.as_os_str().to_owned(),
        std::ffi::OsString::from("-"),
    ];
    let text = run_helper("pdftotext", &args, ctx)?;
    Ok(text.replace('\u{c}', "\n\n"))
}

fn unzip_entry(path: &Path, entry: &str, ctx: &ToolContext) -> Result<String, String> {
    require_helper("unzip")?;
    let args = [
        std::ffi::OsString::from("-p"),
        path.as_os_str().to_owned(),
        std::ffi::OsString::from(entry),
    ];
    run_helper("unzip", &args, ctx)
}

fn docx_text(path: &Path, ctx: &ToolContext) -> Result<String, String> {
    let xml = unzip_entry(path, "word/document.xml", ctx)?;
    Ok(xml_to_text(&xml))
}

fn xlsx_text(path: &Path, ctx: &ToolContext) -> Result<String, String> {
    require_helper("unzip")?;
    let workbook = unzip_entry(path, "xl/workbook.xml", ctx).unwrap_or_default();
    let sheets = sheet_names(&workbook);
    let defined = defined_names(&workbook);
    let shared = unzip_entry(path, "xl/sharedStrings.xml", ctx).unwrap_or_default();
    let strings = xml_to_text(&shared);
    let count = strings
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    let mut out = String::new();
    out.push_str(
        "cell addressing is available only through `range` for one named range; without it this \
         lists the workbook's shared strings and sheet names, not rows, columns or cells\n",
    );
    if sheets.is_empty() {
        out.push_str("sheets: no sheet names found in xl/workbook.xml\n");
    } else {
        out.push_str(&format!("sheets: {}\n", sheets.join(", ")));
    }
    if defined.is_empty() {
        out.push_str("named ranges: this workbook declares no named ranges\n");
    } else {
        out.push_str(&format!("named ranges ({}):\n", defined.len()));
        for name in &defined {
            out.push_str(&format!(
                "  {} — {} — {}\n",
                name.name,
                target_sheet(name, &sheets),
                name.reference
            ));
        }
    }
    if count == 0 {
        out.push_str(
            "shared strings: none — the workbook stores its text inline or has no text cells\n",
        );
    } else {
        out.push_str(&format!("shared strings ({count}):\n"));
    }
    out.push_str(&strings);
    Ok(out)
}

struct DefinedName {
    name: String,
    local_sheet: Option<usize>,
    reference: String,
}

fn defined_names(workbook: &str) -> Vec<DefinedName> {
    let mut names = Vec::new();
    let mut rest = workbook;
    while let Some(found) = rest.find("<definedName") {
        let after = &rest[found + "<definedName".len()..];
        if after.starts_with('s') {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else {
            break;
        };
        let tag = &after[..gt];
        if tag.trim_end().ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        let Some(end) = body.find("</definedName>") else {
            break;
        };
        let reference = decode_entities(body[..end].trim());
        if let Some(name) = attribute(tag, "name").filter(|name| !name.trim().is_empty()) {
            names.push(DefinedName {
                name,
                local_sheet: attribute(tag, "localSheetId")
                    .and_then(|value| value.trim().parse::<usize>().ok()),
                reference,
            });
        }
        rest = &body[end + "</definedName>".len()..];
    }
    names
}

fn target_sheet(defined: &DefinedName, sheets: &[String]) -> String {
    if let Some(index) = defined.local_sheet {
        return match sheets.get(index) {
            Some(name) => format!("sheet `{name}`"),
            None => format!("sheet index {index} (no such sheet)"),
        };
    }
    match sheet_prefix(&defined.reference) {
        Some(name) => format!("workbook-scoped, sheet `{name}`"),
        None => "workbook-scoped".to_owned(),
    }
}

fn sheet_prefix(reference: &str) -> Option<String> {
    let (sheet, _) = reference.rsplit_once('!')?;
    if sheet.contains(['!', ':']) {
        return None;
    }
    unquote_sheet(sheet)
}

fn xlsx_named_range(path: &Path, name: &str, ctx: &ToolContext) -> Result<String, String> {
    require_helper("unzip")?;
    let workbook = unzip_entry(path, "xl/workbook.xml", ctx)?;
    let sheets = sheet_entries(&workbook);
    let names = defined_names(&workbook);
    if names.is_empty() {
        return Err("this workbook declares no named ranges".to_owned());
    }
    let Some(defined) = names
        .iter()
        .find(|defined| defined.name.eq_ignore_ascii_case(name))
    else {
        let mut available: Vec<&str> = names.iter().map(|defined| defined.name.as_str()).collect();
        available.sort_unstable();
        return Err(format!(
            "no named range `{name}`; this workbook declares: {}",
            available.join(", ")
        ));
    };
    let range = parse_named_reference(&defined.reference)?;
    let Some(index) = sheets.iter().position(|sheet| sheet.name == range.sheet) else {
        let available: Vec<&str> = sheets.iter().map(|sheet| sheet.name.as_str()).collect();
        return Err(format!(
            "`{}` points at sheet `{}`, which this workbook does not have; its sheets are: {}",
            defined.name,
            range.sheet,
            available.join(", ")
        ));
    };
    let rels = unzip_entry(path, "xl/_rels/workbook.xml.rels", ctx).unwrap_or_default();
    let entry = worksheet_entry(&sheets[index], &relationship_targets(&rels), index);
    let xml = unzip_entry(path, &entry, ctx)?;
    let shared =
        shared_string_values(&unzip_entry(path, "xl/sharedStrings.xml", ctx).unwrap_or_default());
    let cells = sheet_cells(&xml, &shared);

    let label = rectangle_label((range.start.0, range.start.1), (range.end.0, range.end.1));
    let total = (range.end.0 - range.start.0 + 1).saturating_mul(range.end.1 - range.start.1 + 1);
    let mut out = format!(
        "named range `{}`: sheet `{}`, bounds {label}\n",
        defined.name, range.sheet
    );
    let mut written = 0usize;
    'rows: for row in range.start.0..=range.end.0 {
        let mut line = format!("{row}:");
        for column in range.start.1..=range.end.1 {
            if written >= total || out.len() > MAX_TEXT_BYTES {
                break 'rows;
            }
            line.push('\t');
            line.push_str(cells.get(&(row, column)).map(String::as_str).unwrap_or(""));
            written += 1;
        }
        out.push_str(&line);
        out.push('\n');
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    if written < total {
        out.push_str(&format!(
            "… [range cut after {written} of {total} cells at the {MAX_TEXT_BYTES}-byte ceiling; ask for a smaller rectangle]\n"
        ));
    }
    Ok(out)
}

struct ParsedRange {
    sheet: String,
    start: (usize, usize),
    end: (usize, usize),
}

fn parse_named_reference(reference: &str) -> Result<ParsedRange, String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("the named range has an empty reference".to_owned());
    }
    if reference.to_ascii_uppercase().contains("#REF!") {
        return Err(format!(
            "`{reference}` is a broken reference (#REF!): the cells it pointed at no longer exist"
        ));
    }
    if reference.contains(['[', ']']) {
        return Err(format!(
            "`{reference}` points into another workbook; only a rectangle in this workbook is read"
        ));
    }
    let Some((sheet_part, bounds_part)) = reference.rsplit_once('!') else {
        return Err(format!(
            "`{reference}` is not a sheet!A1 reference, so it does not name a rectangle on one sheet"
        ));
    };
    if sheet_part.contains('!') || bounds_part.contains('!') {
        return Err(format!(
            "`{reference}` spans several sheets; only a single-sheet rectangle is read"
        ));
    }
    if sheet_part.contains(':') {
        return Err(format!(
            "`{reference}` is a 3-D reference across sheets; only a single-sheet rectangle is read"
        ));
    }
    let sheet = unquote_sheet(sheet_part)
        .ok_or_else(|| format!("`{reference}` has an unclosed quoted sheet name"))?;
    if sheet.trim().is_empty() {
        return Err(format!("`{reference}` names no sheet"));
    }
    if bounds_part.contains([',', ' ', '(', ')']) || bounds_part.matches(':').count() > 1 {
        return Err(format!(
            "`{reference}` is not a simple rectangle (it uses a union, intersection or a dynamic reference)"
        ));
    }
    let (start_text, end_text) = match bounds_part.split_once(':') {
        Some((start, end)) => (start, end),
        None => (bounds_part, bounds_part),
    };
    let start = a1_cell(start_text).ok_or_else(|| {
        format!("`{reference}` has bound `{start_text}`, which is not one A1 cell")
    })?;
    let end = a1_cell(end_text)
        .ok_or_else(|| format!("`{reference}` has bound `{end_text}`, which is not one A1 cell"))?;
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Ok(ParsedRange { sheet, start, end })
}

fn unquote_sheet(part: &str) -> Option<String> {
    match part.strip_prefix('\'') {
        Some(inner) => inner
            .strip_suffix('\'')
            .map(|inner| inner.replace("''", "'")),
        None => Some(part.to_owned()),
    }
}

fn a1_cell(text: &str) -> Option<(usize, usize)> {
    let mut row = 0usize;
    let mut column = 0usize;
    let mut digits = false;
    for character in text.trim().chars() {
        match character {
            '$' => {}
            'A'..='Z' => {
                if digits {
                    return None;
                }
                column = column
                    .checked_mul(26)?
                    .checked_add((character as u8 - b'A' + 1) as usize)?;
            }
            'a'..='z' => {
                if digits {
                    return None;
                }
                column = column
                    .checked_mul(26)?
                    .checked_add((character as u8 - b'a' + 1) as usize)?;
            }
            '0'..='9' => {
                digits = true;
                row = row.checked_mul(10)? + (character as u8 - b'0') as usize;
            }
            _ => return None,
        }
    }
    (column > 0 && row > 0).then_some((row, column))
}

fn cell_name(row: usize, column: usize) -> String {
    let mut letters = String::new();
    let mut column = column;
    while column > 0 {
        let remainder = (column - 1) % 26;
        letters.insert(0, (b'A' + remainder as u8) as char);
        column = (column - 1) / 26;
    }
    format!("{letters}{row}")
}

fn rectangle_label(start: (usize, usize), end: (usize, usize)) -> String {
    let first = cell_name(start.0, start.1);
    if start == end {
        first
    } else {
        format!("{first}:{}", cell_name(end.0, end.1))
    }
}

fn sheet_cells(xml: &str, shared: &[String]) -> HashMap<(usize, usize), String> {
    let mut cells = HashMap::new();
    let mut rest = xml;
    while let Some(found) = rest.find("<c ") {
        let after = &rest[found + 3..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let tag = &after[..gt];
        let body = &after[gt + 1..];
        let (inner, next) = if tag.trim_end().ends_with('/') {
            ("", body)
        } else {
            match body.find("</c>") {
                Some(end) => (&body[..end], &body[end + "</c>".len()..]),
                None => ("", ""),
            }
        };
        rest = next;
        let kind = attribute(tag, "t").unwrap_or_default();
        let value = match kind.as_str() {
            "s" => element_value(inner, "v")
                .and_then(|index| index.trim().parse::<usize>().ok())
                .and_then(|index| shared.get(index).cloned())
                .unwrap_or_default(),
            "inlineStr" => element_texts(inner),
            "b" => match element_value(inner, "v").as_deref() {
                Some("1") => "TRUE".to_owned(),
                Some("0") => "FALSE".to_owned(),
                _ => String::new(),
            },
            _ => element_value(inner, "v").unwrap_or_default(),
        };
        if let Some((row, column)) = attribute(tag, "r").and_then(|r| a1_cell(&r)) {
            cells.insert((row, column), value);
        }
        if rest.is_empty() {
            break;
        }
    }
    cells
}

fn shared_string_values(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut rest = xml;
    while let Some(found) = rest.find("<si>") {
        let body = &rest[found + "<si>".len()..];
        let Some(end) = body.find("</si>") else {
            break;
        };
        strings.push(element_texts(&body[..end]));
        rest = &body[end + "</si>".len()..];
    }
    strings
}

fn element_texts(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(found) = rest.find("<t") {
        let after = &rest[found + "<t".len()..];
        if !matches!(after.chars().next(), Some('>') | Some(' ')) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else {
            break;
        };
        let body = &after[gt + 1..];
        let Some(end) = body.find("</t>") else {
            break;
        };
        out.push_str(&decode_entities(&body[..end]));
        rest = &body[end + "</t>".len()..];
    }
    out
}

fn element_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)? + open.len();
    let gt = xml[start..].find('>')? + start;
    let body = &xml[gt + 1..];
    let end = body.find(&format!("</{tag}>"))?;
    Some(decode_entities(&body[..end]))
}

fn attribute(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(decode_entities(&tag[start..start + end]))
}

struct SheetEntry {
    name: String,
    rel_id: Option<String>,
}

fn sheet_entries(workbook: &str) -> Vec<SheetEntry> {
    let mut entries = Vec::new();
    let mut rest = workbook;
    while let Some(found) = rest.find("<sheet ") {
        let after = &rest[found + "<sheet ".len()..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let tag = &after[..gt];
        if let Some(name) = attribute(tag, "name").filter(|name| !name.trim().is_empty()) {
            entries.push(SheetEntry {
                name,
                rel_id: attribute(tag, "r:id"),
            });
        }
        rest = &after[gt + 1..];
    }
    entries
}

fn relationship_targets(rels: &str) -> HashMap<String, String> {
    let mut targets = HashMap::new();
    let mut rest = rels;
    while let Some(found) = rest.find("<Relationship ") {
        let after = &rest[found + "<Relationship ".len()..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let tag = &after[..gt];
        if let (Some(id), Some(target)) = (attribute(tag, "Id"), attribute(tag, "Target")) {
            targets.insert(id, target);
        }
        rest = &after[gt + 1..];
    }
    targets
}

fn worksheet_entry(sheet: &SheetEntry, targets: &HashMap<String, String>, index: usize) -> String {
    let target = sheet
        .rel_id
        .as_ref()
        .and_then(|id| targets.get(id))
        .cloned()
        .unwrap_or_else(|| format!("worksheets/sheet{}.xml", index + 1));
    if let Some(stripped) = target.strip_prefix('/') {
        stripped.to_owned()
    } else if target.starts_with("xl/") {
        target
    } else {
        format!("xl/{target}")
    }
}

/// Sheet display names, in workbook order. Attribute scanning keeps this in one pass and needs no
/// XML parser the workspace does not have.
fn sheet_names(workbook: &str) -> Vec<String> {
    sheet_entries(workbook)
        .into_iter()
        .map(|sheet| sheet.name)
        .collect()
}

/// Turn WordprocessingML/SpreadsheetML into text without an XML crate.
///
/// Tags are dropped, paragraph and row ends become breaks, and every run of whitespace collapses
/// to one space so a document's formatting does not arrive as a wall of tabs.
fn xml_to_text(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 3);
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        if open > 0 {
            out.push_str(&decode_entities(&rest[..open]));
        }
        let Some(close) = rest[open..].find('>') else {
            return collapse_whitespace(&out);
        };
        let tag = &rest[open + 1..open + close];
        match tag {
            "/w:p" | "/row" => out.push_str("\n\n"),
            "/si" => out.push('\n'),
            "w:br/" | "w:br /" => out.push('\n'),
            "w:tab/" | "w:tab /" => out.push(' '),
            _ => {}
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(&decode_entities(rest));
    collapse_whitespace(&out)
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let Some(length) = rest[start..].find(';').filter(|length| *length <= 10) else {
            out.push('&');
            rest = &rest[start + 1..];
            continue;
        };
        let entity = &rest[start + 1..start + length];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            "nbsp" => out.push(' '),
            _ => match numeric_entity(entity) {
                Some(character) => out.push(character),
                None => out.push_str(&rest[start..start + length + 1]),
            },
        }
        rest = &rest[start + length + 1..];
    }
    out.push_str(rest);
    out
}

fn numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code).filter(|character| !character.is_control() || *character == '\n')
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut line_breaks = 0u8;
    for character in text.chars() {
        if character == '\n' {
            pending_space = false;
            line_breaks = line_breaks.saturating_add(1);
            if line_breaks <= 2 {
                out.push('\n');
            }
        } else if character.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            pending_space = false;
            line_breaks = 0;
            out.push(character);
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &Path) -> ToolContext {
        ToolContext {
            workspace_root: root.to_path_buf(),
            ..ToolContext::default()
        }
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let directory = tempfile::tempdir().unwrap();
        let output = DocRead
            .run(&json!({"path": "nope.pdf"}), &workspace(directory.path()))
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("cannot read"), "{}", output.content);
    }

    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let escaped = outside.path().join("notes.pdf");
        let error = DocRead
            .run(
                &json!({"path": escaped.to_string_lossy()}),
                &workspace(directory.path()),
            )
            .unwrap_err();
        assert!(
            matches!(error, VikramError::PolicyDenied { .. }),
            "a path outside the root must be a policy denial: {error:?}"
        );
    }

    #[test]
    fn an_unsupported_extension_is_named_in_the_refusal() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.txt"), "plain").unwrap();
        let output = DocRead
            .run(&json!({"path": "notes.txt"}), &workspace(directory.path()))
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains(".pdf"), "{}", output.content);
        assert!(output.content.contains("notes.txt"), "{}", output.content);
    }

    #[test]
    fn a_file_over_the_ceiling_is_refused_with_its_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("huge.pdf");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_DOC_BYTES + 1).unwrap();
        let output = DocRead
            .run(&json!({"path": "huge.pdf"}), &workspace(directory.path()))
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(output.content.contains("ceiling"), "{}", output.content);
    }

    #[test]
    fn a_missing_helper_names_the_package_to_install() {
        assert_eq!(
            missing_helper("pdftotext"),
            if cfg!(target_os = "macos") {
                "pdftotext not found; install it: brew install poppler"
            } else if cfg!(target_os = "windows") {
                "pdftotext not found; install it: install poppler and keep pdftotext.exe on PATH"
            } else {
                "pdftotext not found; install it: apt install poppler-utils"
            }
        );
        assert!(missing_helper("unzip").contains("install it: "));
    }

    #[test]
    fn a_helper_that_cannot_start_is_an_error_not_a_panic() {
        let error = run_helper(
            "vikram-definitely-not-installed-9f3c",
            &[],
            &ToolContext::default(),
        )
        .expect_err("an absent binary must not panic");
        assert!(error.contains("could not run"), "{error}");
    }

    #[test]
    fn a_present_helper_returns_its_stdout() {
        if crate::computer::which("unzip").is_none() {
            return;
        }
        let text = run_helper(
            "unzip",
            &[std::ffi::OsString::from("-v")],
            &ToolContext::default(),
        )
        .expect("unzip -v runs on a machine that has unzip");
        assert!(!text.trim().is_empty());
    }

    #[test]
    fn wordprocessingml_becomes_paragraphs_with_entities_decoded() {
        let xml = "<?xml version=\"1.0\"?><w:document><w:body>\
                   <w:p><w:r><w:t>Hello &amp; welcome</w:t></w:r></w:p>\
                   <w:p><w:r><w:t>Second</w:t><w:tab/><w:t>cell</w:t><w:br/><w:t>line</w:t></w:r></w:p>\
                   </w:body></w:document>";
        assert_eq!(xml_to_text(xml), "Hello & welcome\n\nSecond cell\nline");
    }

    #[test]
    fn numeric_entities_decode_and_unknown_ones_survive_verbatim() {
        assert_eq!(decode_entities("&#65;&#x42; &bogus; &lt;"), "AB &bogus; <");
        assert_eq!(collapse_whitespace("a \t b\n\n\n\nc"), "a b\n\nc");
    }

    #[test]
    fn xlsx_sheet_names_are_read_in_workbook_order() {
        let workbook = "<workbook><sheets>\
                        <sheet name=\"Data &amp; Notes\" sheetId=\"1\"/>\
                        <sheet name=\"Summary\" sheetId=\"2\"/>\
                        </sheets></workbook>";
        assert_eq!(sheet_names(workbook), vec!["Data & Notes", "Summary"]);
        assert!(sheet_names("<workbook/>").is_empty());
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut count = 0u16;
        for (name, body) in entries {
            let name = name.as_bytes();
            let body = body.as_bytes();
            let crc = crc32(body);
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(body);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
            count += 1;
        }
        let central_offset = out.len() as u32;
        let central_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&central_size.to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        std::fs::write(path, out).unwrap();
    }

    fn xlsx_fixture(path: &Path, workbook: &str, sheet: &str, shared: &str) {
        write_zip(
            path,
            &[
                ("xl/workbook.xml", workbook),
                (
                    "xl/_rels/workbook.xml.rels",
                    "<?xml version=\"1.0\"?><Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>",
                ),
                ("xl/worksheets/sheet1.xml", sheet),
                ("xl/sharedStrings.xml", shared),
            ],
        );
    }

    #[test]
    fn a_named_range_is_read_by_its_sheet_and_bounds() {
        if crate::computer::which("unzip").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let workbook = "<workbook><sheets>\
                        <sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/>\
                        <sheet name=\"Other\" sheetId=\"2\" r:id=\"rId2\"/>\
                        </sheets><definedNames>\
                        <definedName name=\"Revenue\" localSheetId=\"0\">Data!$A$1:$AB$2</definedName>\
                        </definedNames></workbook>";
        let sheet = "<worksheet><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c></row>\
                     <row r=\"2\"><c r=\"A2\"><v>42</v></c>\
                     <c r=\"B2\" t=\"inlineStr\"><is><t>inline</t></is></c></row>\
                     </sheetData></worksheet>";
        let shared = "<sst><si><t>Jan</t></si><si><t>Feb</t></si></sst>";
        xlsx_fixture(&directory.path().join("book.xlsx"), workbook, sheet, shared);
        let ctx = workspace(directory.path());

        let output = DocRead
            .run(&json!({"path": "book.xlsx", "range": "Revenue"}), &ctx)
            .unwrap();
        assert!(output.ok, "{}", output.content);
        assert!(
            output.content.contains("sheet `Data`"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("bounds A1:AB2"),
            "{}",
            output.content
        );
        assert!(output.content.contains("Jan\tFeb"), "{}", output.content);
        assert!(output.content.contains("\t42"), "{}", output.content);
        assert!(output.content.contains("inline"), "{}", output.content);

        let listing = DocRead.run(&json!({"path": "book.xlsx"}), &ctx).unwrap();
        assert!(listing.ok, "{}", listing.content);
        assert!(
            listing
                .content
                .contains("Revenue — sheet `Data` — Data!$A$1:$AB$2"),
            "{}",
            listing.content
        );
    }

    #[test]
    fn an_unknown_range_name_lists_the_names_that_exist() {
        if crate::computer::which("unzip").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let workbook = "<workbook><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/></sheets>\
                        <definedNames>\
                        <definedName name=\"Revenue\" localSheetId=\"0\">Data!$A$1:$B$2</definedName>\
                        <definedName name=\"Total\" localSheetId=\"0\">Data!$B$9</definedName>\
                        </definedNames></workbook>";
        xlsx_fixture(
            &directory.path().join("book.xlsx"),
            workbook,
            "<worksheet><sheetData/></worksheet>",
            "<sst/>",
        );

        let output = DocRead
            .run(
                &json!({"path": "book.xlsx", "range": "Missing"}),
                &workspace(directory.path()),
            )
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(
            output.content.contains("no named range `Missing`"),
            "{}",
            output.content
        );
        assert!(output.content.contains("Revenue"), "{}", output.content);
        assert!(output.content.contains("Total"), "{}", output.content);
    }

    #[test]
    fn a_workbook_without_named_ranges_says_so() {
        if crate::computer::which("unzip").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let workbook = "<workbook><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>";
        xlsx_fixture(
            &directory.path().join("book.xlsx"),
            workbook,
            "<worksheet><sheetData/></worksheet>",
            "<sst/>",
        );
        let ctx = workspace(directory.path());

        let output = DocRead
            .run(&json!({"path": "book.xlsx", "range": "Anything"}), &ctx)
            .unwrap();
        assert!(!output.ok, "{}", output.content);
        assert!(
            output
                .content
                .contains("this workbook declares no named ranges"),
            "{}",
            output.content
        );

        let listing = DocRead.run(&json!({"path": "book.xlsx"}), &ctx).unwrap();
        assert!(listing.ok, "{}", listing.content);
        assert!(
            listing
                .content
                .contains("this workbook declares no named ranges"),
            "{}",
            listing.content
        );
    }

    #[test]
    fn the_tool_is_registered_and_advertises_its_schema() {
        let registry = ToolRegistry::with_builtins();
        let tool = registry.get("doc.read").expect("doc.read registered");
        assert_eq!(tool.spec().name, "doc.read");
        assert_eq!(tool.spec().schema["type"], "object");
        assert_eq!(tool.spec().schema["properties"]["path"]["type"], "string");
        assert_eq!(tool.spec().schema["required"][0], "path");
        assert!(registry.names().contains(&"doc.read".to_owned()));
    }

    #[test]
    fn the_permission_is_a_read_with_no_side_effect() {
        let request = DocRead.permission(&json!({"path": "a.pdf"})).unwrap();
        assert_eq!(request.action, ActionKind::Read);
        assert_eq!(request.side_effect, SideEffect::None);
        assert_eq!(request.verb, "doc.read");
    }
}
