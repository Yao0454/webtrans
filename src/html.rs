//! Source-preserving HTML extraction. Only selected byte ranges are rewritten;
//! the tokenizer never serializes or executes the source document.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use html5gum::{DefaultEmitter, StartTag, State, Token, Tokenizer};
use sha2::{Digest, Sha256};

use crate::model::{EntryKind, TranslationEntry, TranslationTable};

const MARKER_PREFIX: &str = "[[[WT_";

#[derive(Debug)]
struct Frame {
    name: String,
    suppressed: bool,
    hard_suppressed: bool,
    translate_enabled: bool,
    boundary: bool,
}

#[derive(Debug)]
struct Piece {
    start: usize,
    end: usize,
    markup: bool,
}

#[derive(Debug, Default)]
struct Group {
    pieces: Vec<Piece>,
    context: String,
    // Private, freshly parsed bindings. Never trust raw tag matching or offsets
    // supplied by a translation file when composing nested attribute edits.
    markup_bindings: BTreeMap<usize, BTreeMap<String, Range<usize>>>,
}

/// Extract text groups and selected human-readable attributes into a stable map.
/// Inline tags remain part of the surrounding sentence; their human-readable
/// attributes are separate entries whose edits are composed during application.
pub fn extract(html: &str) -> Result<TranslationTable> {
    extract_version(html, 2)
}

/// Recreate the extraction rules associated with a saved translation table.
/// Version 1 keeps its original grouping and suppression semantics so existing
/// checkpoints can still be validated and applied without migration.
pub fn extract_version(html: &str, version: u32) -> Result<TranslationTable> {
    Ok(extract_with_bindings(html, version)?.table)
}

struct Extraction {
    table: TranslationTable,
    markup_bindings: BTreeMap<usize, BTreeMap<String, Range<usize>>>,
}

fn extract_with_bindings(html: &str, version: u32) -> Result<Extraction> {
    ensure!(
        matches!(version, 1 | 2),
        "unsupported translation table version {version}"
    );
    ensure!(
        !html.contains(MARKER_PREFIX),
        "HTML contains the reserved placeholder prefix {MARKER_PREFIX}"
    );
    let mut emitter = DefaultEmitter::<usize>::new_with_span();
    emitter.naively_switch_states(true);
    let mut entries = Vec::new();
    let mut stack = Vec::<Frame>::new();
    let mut group = Group::default();

    for token in Tokenizer::new_with_emitter(html, emitter) {
        match token.expect("string readers cannot fail") {
            Token::StartTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name).into_owned();
                if close_implicit_elements(&name, &mut stack) {
                    finish_group(html, &mut group, &mut entries)?;
                }
                let translate = attr(&tag, "translate");
                let notranslate = attr(&tag, "class")
                    .is_some_and(|v| v.split_ascii_whitespace().any(|v| v == "notranslate"));
                let (suppressed, suppress_attributes, hard_suppressed, translate_enabled) =
                    if version == 1 {
                        let suppressed = stack.last().is_some_and(|f| f.suppressed)
                            || excluded_tag(&name)
                            || translate
                                .as_ref()
                                .is_some_and(|v| v.eq_ignore_ascii_case("no"))
                            || notranslate;
                        (suppressed, suppressed, suppressed, !suppressed)
                    } else {
                        let translate_enabled = match translate.as_deref() {
                            Some(value) if value.eq_ignore_ascii_case("no") => false,
                            Some(value)
                                if value.is_empty() || value.eq_ignore_ascii_case("yes") =>
                            {
                                true
                            }
                            _ => stack.last().is_none_or(|frame| frame.translate_enabled),
                        };
                        let hard_attributes =
                            stack.last().is_some_and(|frame| frame.hard_suppressed)
                                || (excluded_tag(&name) && name != "textarea")
                                || notranslate;
                        let hard_suppressed = hard_attributes || name == "textarea";
                        (
                            hard_suppressed || !translate_enabled,
                            hard_attributes || !translate_enabled,
                            hard_suppressed,
                            translate_enabled,
                        )
                    };
                let attrs = if suppress_attributes {
                    Vec::new()
                } else {
                    extract_attributes(html, &tag, &stack)?
                };
                let boundary =
                    suppressed || !inline_tag(&name) || (version == 1 && !attrs.is_empty());
                if boundary {
                    finish_group(html, &mut group, &mut entries)?;
                } else {
                    push_piece(
                        html,
                        &mut group,
                        &mut entries,
                        &stack,
                        Piece {
                            start: tag.span.start,
                            end: tag.span.end,
                            markup: true,
                        },
                    )?;
                }
                entries.extend(attrs);
                // '/>' closes foreign SVG/MathML elements, but not ordinary HTML.
                let foreign = matches!(name.as_str(), "svg" | "math")
                    || stack
                        .iter()
                        .any(|frame| matches!(frame.name.as_str(), "svg" | "math"));
                if !void_tag(&name) && !(foreign && tag.self_closing) {
                    stack.push(Frame {
                        name,
                        suppressed,
                        hard_suppressed,
                        translate_enabled,
                        boundary,
                    });
                }
            }
            Token::EndTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name);
                let matched = stack.iter().rposition(|frame| frame.name == name);
                let suppressed = stack.last().is_some_and(|f| f.suppressed);
                let boundary = matched.is_none_or(|i| stack[i].boundary);
                if suppressed || boundary {
                    finish_group(html, &mut group, &mut entries)?;
                } else {
                    push_piece(
                        html,
                        &mut group,
                        &mut entries,
                        &stack,
                        Piece {
                            start: tag.span.start,
                            end: tag.span.end,
                            markup: true,
                        },
                    )?;
                }
                if let Some(index) = matched {
                    stack.truncate(index);
                }
            }
            Token::String(text) if !stack.last().is_some_and(|f| f.suppressed) => {
                push_piece(
                    html,
                    &mut group,
                    &mut entries,
                    &stack,
                    Piece {
                        start: text.span.start,
                        end: text.span.end,
                        markup: false,
                    },
                )?;
            }
            // Comments and declarations stay outside translated ranges.
            Token::Comment(_) | Token::Doctype(_) => {
                finish_group(html, &mut group, &mut entries)?;
            }
            Token::String(_) | Token::Error(_) => {}
        }
    }
    finish_group(html, &mut group, &mut entries)?;
    entries.sort_by_key(|entry| entry.start);
    let mut top_end = 0;
    let mut parent: Option<&TranslationEntry> = None;
    for entry in &entries {
        if entry.start >= top_end {
            top_end = entry.end;
            parent = (entry.kind == EntryKind::Text).then_some(entry);
        } else {
            ensure!(
                version == 2
                    && entry.kind == EntryKind::Attribute
                    && parent.is_some_and(|text| entry.end <= text.end
                        && group
                            .markup_bindings
                            .get(&text.start)
                            .is_some_and(|bindings| bindings
                                .values()
                                .any(|span| span.start <= entry.start && entry.end <= span.end))),
                "overlapping HTML ranges outside a protected inline tag"
            );
        }
    }
    Ok(Extraction {
        table: TranslationTable {
            version,
            source_sha256: format!("{:x}", Sha256::digest(html.as_bytes())),
            source_language: None,
            target_language: None,
            entries: entries
                .into_iter()
                .enumerate()
                .map(|(i, entry)| (format!("u{:06}", i + 1), entry))
                .collect(),
        },
        markup_bindings: group.markup_bindings,
    })
}

/// Validate the source binding and metadata, then replace only translated
/// ranges. A table cannot introduce markup by modifying its stored placeholders.
pub fn apply(html: &str, table: &TranslationTable, allow_missing: bool) -> Result<String> {
    let extraction = extract_with_bindings(html, table.version)?;
    let expected = &extraction.table;
    ensure!(
        table.source_sha256 == expected.source_sha256,
        "source HTML changed: extract a new translation table"
    );
    ensure!(
        table.entries.len() == expected.entries.len(),
        "translation table entries do not match source HTML"
    );
    let mut attribute_patches = Vec::new();
    let mut text_translations = Vec::new();
    for (id, original) in &expected.entries {
        let entry = table
            .entries
            .get(id)
            .with_context(|| format!("missing entry {id}"))?;
        let mut metadata = entry.clone();
        metadata.translation = None;
        ensure!(
            &metadata == original,
            "entry {id} source or metadata was modified"
        );
        match &entry.translation {
            Some(translation) => {
                validate_translation(entry, translation).with_context(|| format!("entry {id}"))?;
                // Identity text stays byte-for-byte intact, but attribute edits
                // inside it can still be applied independently below.
                if translation != &entry.source {
                    if entry.kind == EntryKind::Attribute {
                        let quoted = entry.start > 0
                            && matches!(html.as_bytes()[entry.start - 1], b'\'' | b'"');
                        let mut replacement = render(original, translation);
                        if !quoted {
                            // Malformed unquoted source may contain a raw quote
                            // in a protected variable. Never promote it to markup.
                            replacement = format!("\"{}\"", replacement.replace('"', "&quot;"));
                        }
                        attribute_patches.push(Patch {
                            start: entry.start,
                            end: entry.end,
                            replacement,
                        });
                    } else {
                        text_translations.push((original, translation));
                    }
                }
            }
            None => ensure!(
                allow_missing,
                "entry {id} has no translation; use --allow-missing to preserve it"
            ),
        }
    }
    // Entry IDs are display keys, not an ordering contract: beyond six digits
    // their lexical order no longer follows source order. Source ranges are the
    // authority for both composition and efficient lookup.
    attribute_patches.sort_by_key(|patch| patch.start);
    let mut patches = Vec::new();
    let mut absorbed_attributes = BTreeSet::new();
    for (entry, translation) in text_translations {
        let mut render_entry = entry.clone();
        let bindings = extraction.markup_bindings.get(&entry.start);
        let mut marker_patches: BTreeMap<&str, Vec<Patch>> = BTreeMap::new();
        let first_attribute = attribute_patches.partition_point(|patch| patch.start < entry.start);
        let last_attribute = attribute_patches.partition_point(|patch| patch.start < entry.end);
        for (relative_index, attribute) in attribute_patches[first_attribute..last_attribute]
            .iter()
            .enumerate()
        {
            ensure!(
                attribute.end <= entry.end,
                "attribute crosses a text replacement boundary"
            );
            let (marker, span) = bindings
                .and_then(|bindings| {
                    bindings.iter().find(|(_, span)| {
                        span.start <= attribute.start && attribute.end <= span.end
                    })
                })
                .context("nested attribute has no original inline marker binding")?;
            marker_patches.entry(marker).or_default().push(Patch {
                start: attribute.start - span.start,
                end: attribute.end - span.start,
                replacement: attribute.replacement.clone(),
            });
            absorbed_attributes.insert(first_attribute + relative_index);
        }
        for (marker, edits) in marker_patches {
            let raw = render_entry
                .placeholders
                .get_mut(marker)
                .context("missing inline marker")?;
            *raw = apply_patches(raw, edits)?;
        }
        patches.push(Patch {
            start: entry.start,
            end: entry.end,
            replacement: render(&render_entry, translation),
        });
    }
    patches.extend(
        attribute_patches
            .into_iter()
            .enumerate()
            .filter_map(|(index, patch)| (!absorbed_attributes.contains(&index)).then_some(patch)),
    );
    apply_patches(html, patches)
}

#[derive(Debug)]
struct Patch {
    start: usize,
    end: usize,
    replacement: String,
}

fn apply_patches(source: &str, mut patches: Vec<Patch>) -> Result<String> {
    patches.sort_by_key(|patch| patch.start);
    for pair in patches.windows(2) {
        ensure!(
            pair[0].end <= pair[1].start,
            "overlapping replacement ranges"
        );
    }
    let mut result = source.to_owned();
    for patch in patches.into_iter().rev() {
        ensure!(
            source.get(patch.start..patch.end).is_some(),
            "invalid replacement range"
        );
        result.replace_range(patch.start..patch.end, &patch.replacement);
    }
    Ok(result)
}

/// Markup and variable markers must survive exactly once and in source order.
pub fn validate_translation(entry: &TranslationEntry, translation: &str) -> Result<()> {
    ensure!(
        !translation.trim().is_empty(),
        "translation must not be empty"
    );
    ensure!(
        !translation.contains('\0'),
        "translation contains a NUL character"
    );
    let expected = markers(&entry.source)?;
    let received = markers(translation)?;
    ensure!(
        expected == received,
        "translation must preserve every placeholder exactly once and in its original order"
    );
    ensure!(
        expected.len() == entry.placeholders.len()
            && expected
                .iter()
                .all(|marker| entry.placeholders.contains_key(*marker)),
        "invalid source placeholder metadata"
    );
    Ok(())
}

fn markers(text: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(MARKER_PREFIX) {
        let end = rest[start..]
            .find("]]]")
            .context("unclosed translation placeholder")?
            + start
            + 3;
        result.push(&rest[start..end]);
        rest = &rest[end..];
    }
    Ok(result)
}

fn push_piece(
    html: &str,
    group: &mut Group,
    entries: &mut Vec<TranslationEntry>,
    stack: &[Frame],
    piece: Piece,
) -> Result<()> {
    ensure!(
        html.get(piece.start..piece.end).is_some(),
        "invalid HTML tokenizer source range"
    );
    if group
        .pieces
        .last()
        .is_some_and(|last| last.end != piece.start)
    {
        finish_group(html, group, entries)?;
    }
    if group.pieces.is_empty() {
        group.context = context(stack);
    }
    group.pieces.push(piece);
    Ok(())
}

fn finish_group(html: &str, group: &mut Group, entries: &mut Vec<TranslationEntry>) -> Result<()> {
    if group.pieces.is_empty() {
        return Ok(());
    }
    let mut pieces = std::mem::take(&mut group.pieces);
    if let Some(first) = pieces.first_mut().filter(|p| !p.markup) {
        let raw = &html[first.start..first.end];
        first.start += raw.len() - raw.trim_start_matches(char::is_whitespace).len();
    }
    if let Some(last) = pieces.last_mut().filter(|p| !p.markup) {
        last.end = last.start
            + html[last.start..last.end]
                .trim_end_matches(char::is_whitespace)
                .len();
    }
    let start = pieces.first().unwrap().start;
    let end = pieces.last().unwrap().end;
    let mut source = String::new();
    let mut placeholders = BTreeMap::new();
    let mut markup_bindings = BTreeMap::new();
    let mut meaningful = false;
    for piece in pieces {
        let raw = &html[piece.start..piece.end];
        if piece.markup {
            let marker = add_placeholder(&mut placeholders, raw);
            markup_bindings.insert(marker.clone(), piece.start..piece.end);
            source.push_str(&marker);
        } else {
            let (text, has_words) = protected_text(raw, false, &mut placeholders)?;
            source.push_str(&text);
            meaningful |= has_words;
        }
    }
    if meaningful {
        group.markup_bindings.insert(start, markup_bindings);
        entries.push(TranslationEntry {
            kind: EntryKind::Text,
            source,
            translation: None,
            context: std::mem::take(&mut group.context),
            start,
            end,
            placeholders,
        });
    }
    Ok(())
}

fn context(stack: &[Frame]) -> String {
    if stack.is_empty() {
        "#document".to_owned()
    } else {
        stack
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    }
}

/// Support common optional HTML end tags without rewriting the source. This is
/// scoped to text grouping, not a replacement for a browser's full tree builder.
fn close_implicit_elements(name: &str, stack: &mut Vec<Frame>) -> bool {
    if stack
        .iter()
        .any(|frame| matches!(frame.name.as_str(), "svg" | "math"))
    {
        return false;
    }
    let previous_len = stack.len();
    if matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "details"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "search"
            | "section"
            | "table"
            | "ul"
    ) {
        close_in_scope(
            stack,
            &["p"],
            &["table", "td", "th", "button", "select", "template"],
        );
    }
    match name {
        "li" => close_in_scope(stack, &["li"], &["ul", "ol", "menu", "template"]),
        "dt" | "dd" => close_in_scope(stack, &["dt", "dd"], &["dl", "template"]),
        "option" => close_in_scope(stack, &["option"], &["select", "datalist", "template"]),
        "optgroup" => {
            close_in_scope(stack, &["option"], &["select", "datalist", "template"]);
            close_in_scope(stack, &["optgroup"], &["select", "template"]);
        }
        "td" | "th" => close_in_scope(stack, &["td", "th"], &["tr", "table", "template"]),
        "tr" => close_in_scope(stack, &["tr"], &["table", "template"]),
        "thead" | "tbody" | "tfoot" => {
            close_in_scope(stack, &["tr"], &["table", "template"]);
            close_in_scope(stack, &["thead", "tbody", "tfoot"], &["table", "template"]);
        }
        _ => {}
    }
    previous_len != stack.len()
}

fn close_in_scope(stack: &mut Vec<Frame>, names: &[&str], barriers: &[&str]) {
    for index in (0..stack.len()).rev() {
        let name = stack[index].name.as_str();
        if names.contains(&name) {
            stack.truncate(index);
            break;
        }
        if barriers.contains(&name) {
            break;
        }
    }
}

fn attr(tag: &StartTag<usize>, name: &str) -> Option<String> {
    tag.attributes
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

fn extract_attributes(
    html: &str,
    tag: &StartTag<usize>,
    stack: &[Frame],
) -> Result<Vec<TranslationEntry>> {
    let name = String::from_utf8_lossy(&tag.name);
    let input_value = name == "input"
        && attr(tag, "type").is_some_and(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "button" | "submit" | "reset"
            )
        });
    let description =
        name == "meta" && attr(tag, "name").is_some_and(|v| v.eq_ignore_ascii_case("description"));
    let mut entries = Vec::new();
    for (attribute, start, end) in attribute_ranges(html, tag.span.start, tag.span.end)? {
        if !matches!(
            attribute.as_str(),
            "title" | "alt" | "placeholder" | "aria-label" | "aria-description"
        ) && !(input_value && attribute == "value")
            && !(description && attribute == "content")
        {
            continue;
        }
        let mut placeholders = BTreeMap::new();
        let (source, meaningful) = protected_text(&html[start..end], true, &mut placeholders)?;
        if meaningful {
            entries.push(TranslationEntry {
                kind: EntryKind::Attribute,
                source,
                translation: None,
                context: format!("{} > {}@{}", context(stack), name, attribute),
                start,
                end,
                placeholders,
            });
        }
    }
    Ok(entries)
}

/// Locate values within an already-tokenized start tag, preserving quoting.
fn attribute_ranges(html: &str, start: usize, end: usize) -> Result<Vec<(String, usize, usize)>> {
    let raw = html.get(start..end).context("invalid start tag range")?;
    let bytes = raw.as_bytes();
    let mut i = 1;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'>' | b'/') {
        i += 1;
    }
    let mut attributes = Vec::new();
    let mut seen = BTreeSet::new();
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i == bytes.len() || bytes[i] == b'>' {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
        {
            i += 1;
        }
        if i == name_start {
            i += 1;
            continue;
        }
        let name = raw[name_start..i].to_ascii_lowercase();
        let first = seen.insert(name.clone());
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == bytes.len() {
            break;
        }
        let quote = matches!(bytes[i], b'\'' | b'"').then_some(bytes[i]);
        if quote.is_some() {
            i += 1;
        }
        let value_start = i;
        while i < bytes.len()
            && match quote {
                Some(quote) => bytes[i] != quote,
                None => !bytes[i].is_ascii_whitespace() && bytes[i] != b'>',
            }
        {
            i += 1;
        }
        if first {
            attributes.push((name, start + value_start, start + i));
        }
        if quote.is_some() && i < bytes.len() {
            i += 1;
        }
    }
    Ok(attributes)
}

fn decode(raw: &str, attribute: bool) -> Result<String> {
    if attribute {
        let wrapped = format!("<i data-value=\"{}\">", raw.replace('"', "&#34;"));
        for token in Tokenizer::new(wrapped.as_str()) {
            if let Token::StartTag(tag) = token.expect("string readers cannot fail") {
                let value = tag
                    .attributes
                    .iter()
                    .find(|(name, _)| name.as_slice() == b"data-value")
                    .context("cannot decode HTML attribute")?
                    .1;
                return Ok(String::from_utf8_lossy(value).into_owned());
            }
        }
        bail!("cannot decode HTML attribute");
    }
    let mut tokenizer = Tokenizer::new(raw);
    tokenizer.set_state(State::RcData);
    let mut result = String::new();
    for token in tokenizer {
        if let Token::String(text) = token.expect("string readers cannot fail") {
            result.push_str(&String::from_utf8_lossy(&text));
        }
    }
    Ok(result)
}

fn add_placeholder(placeholders: &mut BTreeMap<String, String>, raw: &str) -> String {
    let marker = format!("[[[WT_{:06}]]]", placeholders.len() + 1);
    placeholders.insert(marker.clone(), raw.to_owned());
    marker
}

fn protected_text(
    raw: &str,
    attribute: bool,
    placeholders: &mut BTreeMap<String, String>,
) -> Result<(String, bool)> {
    let mut result = String::new();
    let mut meaningful = false;
    let mut offset = 0;
    while let Some((start, end)) = next_variable(&raw[offset..]) {
        let plain = decode(&raw[offset..offset + start], attribute)?;
        meaningful |= plain.chars().any(char::is_alphanumeric);
        result.push_str(&plain);
        result.push_str(&add_placeholder(
            placeholders,
            &raw[offset + start..offset + end],
        ));
        offset += end;
    }
    let plain = decode(&raw[offset..], attribute)?;
    meaningful |= plain.chars().any(char::is_alphanumeric);
    result.push_str(&plain);
    Ok((result, meaningful))
}

fn next_variable(raw: &str) -> Option<(usize, usize)> {
    for (start, ch) in raw.char_indices() {
        let rest = &raw[start..];
        let delimiter = if rest.starts_with("{{{") {
            Some((3, "}}}"))
        } else if rest.starts_with("{{") {
            Some((2, "}}"))
        } else if rest.starts_with("${") {
            Some((2, "}"))
        } else {
            None
        };
        if let Some((prefix, suffix)) = delimiter {
            if let Some(end) = rest[prefix..].find(suffix) {
                return Some((start, start + prefix + end + suffix.len()));
            }
        } else if ch == '{'
            && let Some(end) = rest.find('}')
        {
            let inside = &rest[1..end];
            if inside
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && inside
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            {
                return Some((start, start + end + 1));
            }
        }
    }
    None
}

fn render(entry: &TranslationEntry, translation: &str) -> String {
    let mut result = String::new();
    let mut rest = translation;
    while let Some(start) = rest.find(MARKER_PREFIX) {
        let end = start + rest[start..].find("]]]").expect("validated marker") + 3;
        escape(
            &rest[..start],
            entry.kind == EntryKind::Attribute,
            &mut result,
        );
        result.push_str(&entry.placeholders[&rest[start..end]]);
        rest = &rest[end..];
    }
    escape(rest, entry.kind == EntryKind::Attribute, &mut result);
    result
}

fn escape(text: &str, attribute: bool, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\'' if attribute => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

fn excluded_tag(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "pre"
            | "code"
            | "noscript"
            | "template"
            | "textarea"
            | "svg"
            | "math"
            | "iframe"
            | "xmp"
            | "noembed"
            | "noframes"
            | "plaintext"
    )
}

fn void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn inline_tag(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "b"
            | "bdi"
            | "bdo"
            | "br"
            | "cite"
            | "data"
            | "del"
            | "dfn"
            | "em"
            | "i"
            | "img"
            | "ins"
            | "kbd"
            | "mark"
            | "q"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strong"
            | "sub"
            | "sup"
            | "time"
            | "u"
            | "var"
            | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_identity(table: &mut TranslationTable) {
        for entry in table.entries.values_mut() {
            entry.translation = Some(entry.source.clone());
        }
    }

    #[test]
    fn inline_groups_unicode_entities_and_identity() {
        let html = "<!doctype html>\r\n<p class='x'>  Hello &amp; <strong>世界</strong>!  </p>\n";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 1);
        assert_eq!(
            table.entries["u000001"].source,
            "Hello & [[[WT_000001]]]世界[[[WT_000002]]]!"
        );
        fill_identity(&mut table);
        assert_eq!(apply(html, &table, false).unwrap(), html);
        table.entries.get_mut("u000001").unwrap().translation =
            Some("你好 & [[[WT_000001]]]world[[[WT_000002]]]!".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<!doctype html>\r\n<p class='x'>  你好 &amp; <strong>world</strong>!  </p>\n"
        );
    }

    #[test]
    fn skips_code_css_and_opted_out_subtrees() {
        let html = "<style>.x::after {content:'Hello'}</style><script>const x = '<p>Hello</p>';</script><pre>Hello</pre><code>Hello</code><template><p>Hello</p></template><textarea>Hello</textarea><p translate=no>Hello <b>World</b></p><div class='x notranslate y'><p>No</p></div><p>Yes</p>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.entries["u000001"].source, "Yes");
        table.entries.get_mut("u000001").unwrap().translation = Some("是".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            html.replace("<p>Yes</p>", "<p>是</p>")
        );
    }

    #[test]
    fn attributes_and_inline_attributes_are_composed_safely() {
        let html = "<p>Hello <a href='/hello' title='Link &amp; help'>world</a>.</p><input placeholder=Search aria-label=Query><img alt=Photo src=a.png>";
        let mut table = extract(html).unwrap();
        fill_identity(&mut table);
        assert_eq!(apply(html, &table, false).unwrap(), html);
        let entry = table
            .entries
            .values_mut()
            .find(|e| e.source == "Search")
            .unwrap();
        entry.translation = Some("搜索 \"内容\" <&>".to_owned());
        let entry = table
            .entries
            .values_mut()
            .find(|e| e.source == "Link & help")
            .unwrap();
        entry.translation = Some("链接 '提示'".to_owned());
        let result = apply(html, &table, false).unwrap();
        assert!(result.contains("placeholder=\"搜索 &quot;内容&quot; &lt;&amp;&gt;\""));
        assert!(result.contains("title='链接 &#39;提示&#39;'"));
        assert!(result.contains("href='/hello'"));
    }

    #[test]
    fn duplicate_text_has_distinct_ids() {
        let mut table = extract("<p>Open</p><button>Open</button>").unwrap();
        assert_eq!(table.entries.len(), 2);
        table.entries.get_mut("u000001").unwrap().translation = Some("开放".to_owned());
        table.entries.get_mut("u000002").unwrap().translation = Some("打开".to_owned());
        assert_eq!(
            apply("<p>Open</p><button>Open</button>", &table, false).unwrap(),
            "<p>开放</p><button>打开</button>"
        );
    }

    #[test]
    fn missing_translations_and_modified_sources_fail_closed() {
        let html = "<p>Hello</p>";
        let mut table = extract(html).unwrap();
        assert!(apply(html, &table, false).is_err());
        assert_eq!(apply(html, &table, true).unwrap(), html);
        fill_identity(&mut table);
        assert!(apply("<p>Changed</p>", &table, false).is_err());
        table.entries.get_mut("u000001").unwrap().start = 0;
        assert!(apply(html, &table, false).is_err());
    }

    #[test]
    fn markers_are_exact_and_cannot_inject_markup() {
        let html = "<p>Hello <b>world</b></p>";
        let mut table = extract(html).unwrap();
        let entry = table.entries.get_mut("u000001").unwrap();
        assert!(validate_translation(entry, "你好 world").is_err());
        assert!(validate_translation(entry, "[[[WT_000002]]]你好[[[WT_000001]]]").is_err());
        assert!(
            validate_translation(entry, "[[[WT_000001]]]你好[[[WT_000002]]][[[WT_000003]]]")
                .is_err()
        );
        entry.translation =
            Some("<script>alert(1)</script> [[[WT_000001]]]世界[[[WT_000002]]]".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<p>&lt;script&gt;alert(1)&lt;/script&gt; <b>世界</b></p>"
        );
        table
            .entries
            .get_mut("u000001")
            .unwrap()
            .placeholders
            .insert("[[[WT_000001]]]".to_owned(), "<script>".to_owned());
        assert!(apply(html, &table, false).is_err());
    }

    #[test]
    fn protects_simple_template_variables() {
        let html = "<p>Hello {{ user.name }}, ${price} / {count} items</p>";
        let mut table = extract(html).unwrap();
        assert_eq!(
            table.entries["u000001"].source,
            "Hello [[[WT_000001]]], [[[WT_000002]]] / [[[WT_000003]]] items"
        );
        table.entries.get_mut("u000001").unwrap().translation =
            Some("你好 [[[WT_000001]]]，[[[WT_000002]]] / [[[WT_000003]]] 件".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<p>你好 {{ user.name }}，${price} / {count} 件</p>"
        );
        assert!(extract("<p>[[[WT_000001]]]</p>").is_err());
    }

    #[test]
    fn keeps_comments_and_whitespace_only_content() {
        let html = "<p>\nHello<!-- hidden --> world\n</p><div>  \t </div>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 2);
        fill_identity(&mut table);
        assert_eq!(apply(html, &table, false).unwrap(), html);
    }

    #[test]
    fn standalone_entities_and_title_are_decoded() {
        let mut table =
            extract("<title>A &amp; B</title><p>&#20320;&#22909; &lt;Test&gt;</p>").unwrap();
        assert_eq!(table.entries["u000001"].source, "A & B");
        assert_eq!(table.entries["u000002"].source, "你好 <Test>");
        fill_identity(&mut table);
        assert_eq!(
            apply(
                "<title>A &amp; B</title><p>&#20320;&#22909; &lt;Test&gt;</p>",
                &table,
                false
            )
            .unwrap(),
            "<title>A &amp; B</title><p>&#20320;&#22909; &lt;Test&gt;</p>"
        );
    }

    #[test]
    fn optional_end_tags_do_not_leak_translation_suppression() {
        let html = "<p translate=no>Keep<p>Translate</p><ul><li translate=no>Keep<li>Translate</ul><select><option translate=no>Keep<option>Translate</select><table><tr><td translate=no>Keep<td>Translate</table>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 4);
        for entry in table.entries.values_mut() {
            assert_eq!(entry.source, "Translate");
            entry.translation = Some("翻译".to_owned());
        }
        assert_eq!(
            apply(html, &table, false).unwrap(),
            html.replace("Translate", "翻译")
        );
    }

    #[test]
    fn self_closing_foreign_elements_do_not_hide_following_html() {
        let html =
            "<svg/><p>Hello</p><math/><p>World</p><svg><path/><text>Skip</text></svg><p>After</p>";
        let table = extract(html).unwrap();
        assert_eq!(
            table
                .entries
                .values()
                .map(|entry| entry.source.as_str())
                .collect::<Vec<_>>(),
            ["Hello", "World", "After"]
        );
    }

    #[test]
    fn quoting_unquoted_attributes_cannot_promote_variable_quotes_to_markup() {
        let html = r#"<input placeholder=Hello${"name"}>"#;
        let mut table = extract(html).unwrap();
        table.entries.values_mut().next().unwrap().translation =
            Some("你好[[[WT_000001]]]".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            r#"<input placeholder="你好${&quot;name&quot;}">"#
        );
    }

    #[test]
    fn repeated_inline_tags_keep_sentence_and_independent_attributes() {
        let html = "<p>Click <a title=Open>here</a> or <a title=Open>there</a>.</p>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.version, 2);
        assert_eq!(table.entries.len(), 3);
        assert_eq!(
            table.entries["u000001"].source,
            "Click [[[WT_000001]]]here[[[WT_000002]]] or [[[WT_000003]]]there[[[WT_000004]]]."
        );
        table.entries.get_mut("u000001").unwrap().translation = Some(
            "点击[[[WT_000001]]]这里[[[WT_000002]]]或[[[WT_000003]]]那里[[[WT_000004]]]。"
                .to_owned(),
        );
        table.entries.get_mut("u000002").unwrap().translation = Some("打开此处".to_owned());
        table.entries.get_mut("u000003").unwrap().translation = Some("打开彼处".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<p>点击<a title=\"打开此处\">这里</a>或<a title=\"打开彼处\">那里</a>。</p>"
        );
        // Trusted source bindings, rather than matching identical raw tags,
        // determine which attribute is rewritten.
        table.entries.get_mut("u000002").unwrap().translation = None;
        assert_eq!(
            apply(html, &table, true).unwrap(),
            "<p>点击<a title=Open>这里</a>或<a title=\"打开彼处\">那里</a>。</p>"
        );
    }

    #[test]
    fn nested_attributes_apply_with_identity_or_missing_text() {
        let html = "<p>Fish &amp; <a title='Open &amp; help'>chips</a>.</p>";
        let mut table = extract(html).unwrap();
        fill_identity(&mut table);
        assert_eq!(apply(html, &table, false).unwrap(), html);
        table.entries.get_mut("u000002").unwrap().translation = Some("打开帮助".to_owned());
        let expected = "<p>Fish &amp; <a title='打开帮助'>chips</a>.</p>";
        assert_eq!(apply(html, &table, false).unwrap(), expected);
        table.entries.get_mut("u000001").unwrap().translation = None;
        assert_eq!(apply(html, &table, true).unwrap(), expected);
        assert!(apply(html, &table, false).is_err());
        // Nested metadata is authenticated even when that entry is untranslated.
        table.entries.get_mut("u000002").unwrap().end += 1;
        assert!(apply(html, &table, true).is_err());
    }

    #[test]
    fn nested_inline_attributes_and_template_variables_keep_bindings() {
        let html = "<p>Hello {{name}}, <a title='Visit {site}' aria-label='Open page'><strong title=Bold>world</strong></a>!</p>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 4);
        let text = table.entries.get_mut("u000001").unwrap();
        assert_eq!(
            text.source,
            "Hello [[[WT_000001]]], [[[WT_000002]]][[[WT_000003]]]world[[[WT_000004]]][[[WT_000005]]]!"
        );
        text.translation = Some("你好 [[[WT_000001]]]，[[[WT_000002]]][[[WT_000003]]]世界[[[WT_000004]]][[[WT_000005]]]！".to_owned());
        table.entries.get_mut("u000002").unwrap().translation =
            Some("访问 [[[WT_000001]]]".to_owned());
        table.entries.get_mut("u000003").unwrap().translation = Some("打开页面".to_owned());
        table.entries.get_mut("u000004").unwrap().translation = Some("粗体".to_owned());
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<p>你好 {{name}}，<a title='访问 {site}' aria-label='打开页面'><strong title=\"粗体\">世界</strong></a>！</p>"
        );
    }

    #[test]
    fn textarea_translates_attributes_but_preserves_default_value() {
        let html = "<textarea placeholder=Message aria-label='Your message' title='Help'>Hello &amp; <b>not markup</b> {{name}}</textarea>";
        let mut table = extract(html).unwrap();
        assert_eq!(table.entries.len(), 3);
        assert!(
            table
                .entries
                .values()
                .all(|entry| entry.kind == EntryKind::Attribute)
        );
        for entry in table.entries.values_mut() {
            entry.translation = Some(format!("译：{}", entry.source));
        }
        assert_eq!(
            apply(html, &table, false).unwrap(),
            "<textarea placeholder=\"译：Message\" aria-label='译：Your message' title='译：Help'>Hello &amp; <b>not markup</b> {{name}}</textarea>"
        );
        assert!(extract_version(html, 1).unwrap().entries.is_empty());
    }

    #[test]
    fn translate_yes_and_empty_reset_only_soft_inheritance() {
        let html = "<div translate=no>Keep <span translate=yes title='Hint'>Translate</span> keep <span translate=''>Again</span><span translate=invalid>Keep</span><textarea translate=yes placeholder='Message'>Keep</textarea><code><span translate=yes>Keep</span></code><pre><span translate=yes>Keep</span></pre><div class=notranslate><span translate=yes>Keep</span></div></div><p>After</p>";
        let mut table = extract(html).unwrap();
        let sources = table
            .entries
            .values()
            .map(|entry| entry.source.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                "[[[WT_000001]]]Translate[[[WT_000002]]]",
                "Hint",
                "[[[WT_000001]]]Again[[[WT_000002]]]",
                "Message",
                "After"
            ]
        );
        fill_identity(&mut table);
        assert_eq!(apply(html, &table, false).unwrap(), html);
        let legacy = extract_version(html, 1).unwrap();
        assert_eq!(legacy.entries.len(), 1);
        assert_eq!(legacy.entries["u000001"].source, "After");
    }

    #[test]
    fn legacy_version_keeps_original_grouping_and_applies() {
        let html = "<p>Click <a title=Open>here</a> or <a title=Open>there</a>.</p>";
        let mut legacy = extract_version(html, 1).unwrap();
        assert_eq!(legacy.version, 1);
        assert_eq!(
            legacy
                .entries
                .values()
                .map(|entry| entry.source.as_str())
                .collect::<Vec<_>>(),
            ["Click", "Open", "here", "or", "Open", "there"]
        );
        fill_identity(&mut legacy);
        assert_eq!(apply(html, &legacy, false).unwrap(), html);
        legacy.entries.get_mut("u000002").unwrap().translation = Some("打开".to_owned());
        legacy.entries.get_mut("u000005").unwrap().translation = Some("开启".to_owned());
        assert_eq!(
            apply(html, &legacy, false).unwrap(),
            "<p>Click <a title=\"打开\">here</a> or <a title=\"开启\">there</a>.</p>"
        );
        legacy.version = 2;
        assert!(apply(html, &legacy, false).is_err());
        assert!(extract_version(html, 3).is_err());
    }

    #[test]
    fn many_paragraphs_compose_only_their_own_attribute_edits() {
        let html = "<p>Click <a title=Open>here</a>.</p>".repeat(2048);
        let mut table = extract(&html).unwrap();
        for entry in table.entries.values_mut() {
            entry.translation = Some(match entry.kind {
                EntryKind::Text => "点击[[[WT_000001]]]这里[[[WT_000002]]]。".to_owned(),
                EntryKind::Attribute => format!("打开 {}", entry.start),
            });
        }
        let result = apply(&html, &table, false).unwrap();
        let expected = table
            .entries
            .values()
            .filter(|entry| entry.kind == EntryKind::Attribute)
            .map(|entry| format!("<p>点击<a title=\"打开 {}\">这里</a>。</p>", entry.start))
            .collect::<String>();
        assert_eq!(result, expected);
    }
}
