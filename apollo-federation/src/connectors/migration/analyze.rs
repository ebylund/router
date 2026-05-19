//! `connect-migrate analyze`: walk a project tree, find
//! `@connect(selection: …)` directives, dual-parse each selection
//! under `ConnectSpec::V0_3` and `ConnectSpec::V0_4`, and emit a
//! recommendations.md file (or JSONL records) describing the sites
//! that need attention before upgrading.
//!
//! The output format is specified in `apollographql/connect-migrate`
//! `SKILL.md` under "Recommendations format". This module is the
//! reference implementation of the writer half; `apply` (Phase 5)
//! will implement the reader half.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use apollo_parser::Parser as GqlParser;
use apollo_parser::cst;
use apollo_parser::cst::CstNode;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::connectors::ConnectSpec;
use crate::connectors::JSONSelection;
use crate::connectors::migration::DiffKind;
use crate::connectors::migration::FollowedBy;

/// Per-site record emitted by `analyze`. One entry per breaking
/// `DiffKind` found inside a single `@connect` directive's selection.
/// Cosmetic kinds (`SubSelectionToLitObject`, `LegacyObjectToLitObject`)
/// are filtered out before emission since they have no behavior change.
#[derive(Debug, Serialize, Clone)]
pub struct Site {
    /// 8-hex stable identifier (`DefaultHasher` truncated). Survives
    /// surrounding line shifts in the source file.
    pub id: String,
    /// Project-relative path to the `.graphql` source file.
    pub file: String,
    /// GraphQL schema coordinate (`Type.field`).
    pub coordinate: String,
    /// `DiffKind` variant name in snake_case.
    pub kind: String,
    /// The literal source text of the token at issue. For null/bool
    /// variants this is `"null"`/`"true"`/`"false"`. For string variants
    /// it is the unquoted text.
    pub text: String,
    /// Tail-context classification from the parser.
    pub followed_by: FollowedBy,
    /// Recommended decision per the SKILL.md heuristics.
    pub recommendation: Recommendation,
    /// One-line justification of the recommendation.
    pub reasoning: String,
    /// Approximate line in the source file. v0.0.2 first cut: the
    /// `@connect` directive's start, not the exact token within the
    /// selection — `apply` re-locates the token by text match.
    pub line: Option<usize>,
    /// Approximate column in the source file. Same caveat as `line`.
    pub col: Option<usize>,
    /// Byte offset range of the divergent token inside `selection`.
    /// Used by the rewrite-block computation to do precise in-place
    /// replacements without text-search heuristics. Not emitted in
    /// markdown output; included in JSON output.
    pub source_range: Option<(usize, usize)>,
    /// The full normalized selection text (`$$` → `$` applied),
    /// included for context display and apply-time lookup.
    pub selection: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum Recommendation {
    #[serde(rename = "keep-v0.3")]
    KeepV03,
    #[serde(rename = "embrace-v0.4")]
    EmbraceV04,
    #[serde(rename = "???")]
    Ambiguous,
}

impl Recommendation {
    fn as_str(self) -> &'static str {
        match self {
            Recommendation::KeepV03 => "keep-v0.3",
            Recommendation::EmbraceV04 => "embrace-v0.4",
            Recommendation::Ambiguous => "???",
        }
    }
}

/// Top-level entry point. Walks the given paths and returns every
/// breaking site found. The order is stable across runs for the same
/// inputs (sites are emitted in file-walk order, then in diff-walk
/// order within each `@connect` directive).
pub fn analyze(paths: &[PathBuf], project_root: &Path) -> Vec<Site> {
    let mut out = Vec::new();
    for p in paths {
        walk(p, project_root, &mut out);
    }
    out
}

fn walk(path: &Path, project_root: &Path, out: &mut Vec<Site>) {
    if path.is_file() {
        if path.extension().and_then(|e| e.to_str()) == Some("graphql") {
            scan_file(path, project_root, out);
        }
        return;
    }
    if path.is_dir() {
        let Ok(entries) = fs::read_dir(path) else { return };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for child in paths {
            walk(&child, project_root, out);
        }
    }
}

fn scan_file(path: &Path, project_root: &Path, out: &mut Vec<Site>) {
    let Ok(sdl) = fs::read_to_string(path) else { return };
    let line_index = LineIndex::new(&sdl);
    let rel = path
        .strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string();
    let parser = GqlParser::new(&sdl);
    let cst = parser.parse();
    let doc = cst.document();

    for def in doc.definitions() {
        match def {
            cst::Definition::ObjectTypeDefinition(otd) => {
                let type_name = otd.name().map(|n| n.text().to_string()).unwrap_or_default();
                if let Some(directives) = otd.directives() {
                    scan_directives(&type_name, directives, &rel, &line_index, out);
                }
                if let Some(fields) = otd.fields_definition() {
                    scan_fields(&type_name, fields, &rel, &line_index, out);
                }
            }
            cst::Definition::ObjectTypeExtension(ote) => {
                let type_name = ote.name().map(|n| n.text().to_string()).unwrap_or_default();
                if let Some(directives) = ote.directives() {
                    scan_directives(&type_name, directives, &rel, &line_index, out);
                }
                if let Some(fields) = ote.fields_definition() {
                    scan_fields(&type_name, fields, &rel, &line_index, out);
                }
            }
            cst::Definition::InterfaceTypeDefinition(itd) => {
                let type_name = itd.name().map(|n| n.text().to_string()).unwrap_or_default();
                if let Some(directives) = itd.directives() {
                    scan_directives(&type_name, directives, &rel, &line_index, out);
                }
                if let Some(fields) = itd.fields_definition() {
                    scan_fields(&type_name, fields, &rel, &line_index, out);
                }
            }
            cst::Definition::InterfaceTypeExtension(ite) => {
                let type_name = ite.name().map(|n| n.text().to_string()).unwrap_or_default();
                if let Some(directives) = ite.directives() {
                    scan_directives(&type_name, directives, &rel, &line_index, out);
                }
                if let Some(fields) = ite.fields_definition() {
                    scan_fields(&type_name, fields, &rel, &line_index, out);
                }
            }
            _ => {}
        }
    }
}

fn scan_directives(
    type_name: &str,
    directives: cst::Directives,
    file: &str,
    line_index: &LineIndex,
    out: &mut Vec<Site>,
) {
    for d in directives.directives() {
        if d.name().map(|n| n.text().to_string()).as_deref() == Some("connect") {
            handle_connect(&d, type_name.to_string(), file, line_index, out);
        }
    }
}

fn scan_fields(
    type_name: &str,
    fields: cst::FieldsDefinition,
    file: &str,
    line_index: &LineIndex,
    out: &mut Vec<Site>,
) {
    for field in fields.field_definitions() {
        let field_name = field.name().map(|n| n.text().to_string()).unwrap_or_default();
        let coordinate = format!("{type_name}.{field_name}");
        let Some(directives) = field.directives() else { continue };
        for d in directives.directives() {
            if d.name().map(|n| n.text().to_string()).as_deref() == Some("connect") {
                handle_connect(&d, coordinate.clone(), file, line_index, out);
            }
        }
    }
}

fn handle_connect(
    d: &cst::Directive,
    coordinate: String,
    file: &str,
    line_index: &LineIndex,
    out: &mut Vec<Site>,
) {
    let Some(args) = d.arguments() else { return };
    let Some(selection_text) = extract_selection(args) else { return };
    // Router config expansion converts `$$` → `$` before parsing
    // selections at runtime, so the at-rest form gets normalized here
    // too. See v04_divergence.rs::compute_verdict for the rationale.
    let normalized = selection_text.replace("$$", "$");
    let Ok(v3) = JSONSelection::parse_with_spec(&normalized, ConnectSpec::V0_3) else { return };
    let Ok(v4) = JSONSelection::parse_with_spec(&normalized, ConnectSpec::V0_4) else { return };
    if v3.structural_eq(&v4) {
        return;
    }
    let diffs = v3.diff_kinds(&v4);

    let directive_start = usize::from(d.syntax().text_range().start());
    let (line_no, col_no) = line_index.position(directive_start);

    for kind in diffs {
        // Skip cosmetic kinds; analyze only reports actionable sites.
        if matches!(
            kind,
            DiffKind::SubSelectionToLitObject { .. } | DiffKind::LegacyObjectToLitObject { .. }
        ) {
            continue;
        }
        let kind_name = diff_kind_name(&kind);
        let text = diff_kind_text(&kind).to_string();
        let followed_by = diff_kind_followed_by(&kind);
        let source_range = diff_kind_source_range(&kind);
        let (rec, reason) = classify(&kind);
        let id = hash_id(file, &coordinate, kind_name, &text, &normalized);
        out.push(Site {
            id,
            file: file.to_string(),
            coordinate: coordinate.clone(),
            kind: kind_name.to_string(),
            text,
            followed_by,
            recommendation: rec,
            reasoning: reason,
            line: Some(line_no),
            col: Some(col_no),
            source_range,
            selection: normalized.clone(),
        });
    }
}

fn diff_kind_source_range(kind: &DiffKind) -> Option<(usize, usize)> {
    match kind {
        DiffKind::KeyFlippedToLiteralNull { source_range, .. } => *source_range,
        DiffKind::KeyFlippedToLiteralBool { source_range, .. } => *source_range,
        DiffKind::KeyFieldFlippedToLiteralString { source_range, .. } => *source_range,
        DiffKind::KeyQuotedFlippedToLiteralString { source_range, .. } => *source_range,
        DiffKind::SubSelectionToLitObject { source_range } => *source_range,
        DiffKind::LegacyObjectToLitObject { source_range } => *source_range,
        DiffKind::Other { source_range, .. } => *source_range,
    }
}

fn extract_selection(args: cst::Arguments) -> Option<String> {
    for arg in args.arguments() {
        let name = arg.name()?.text().to_string();
        if name == "selection" {
            if let cst::Value::StringValue(s) = arg.value()? {
                return Some(decode_graphql_string(&s.source_string()));
            }
        }
    }
    None
}

fn diff_kind_name(kind: &DiffKind) -> &'static str {
    match kind {
        DiffKind::KeyFlippedToLiteralNull { .. } => "key_flipped_to_literal_null",
        DiffKind::KeyFlippedToLiteralBool { .. } => "key_flipped_to_literal_bool",
        DiffKind::KeyFieldFlippedToLiteralString { .. } => "key_field_flipped_to_literal_string",
        DiffKind::KeyQuotedFlippedToLiteralString { .. } => "key_quoted_flipped_to_literal_string",
        DiffKind::SubSelectionToLitObject { .. } => "sub_selection_to_lit_object",
        DiffKind::LegacyObjectToLitObject { .. } => "legacy_object_to_lit_object",
        DiffKind::Other { .. } => "other",
    }
}

fn diff_kind_text(kind: &DiffKind) -> &str {
    match kind {
        DiffKind::KeyFlippedToLiteralNull { .. } => "null",
        DiffKind::KeyFlippedToLiteralBool { value: true, .. } => "true",
        DiffKind::KeyFlippedToLiteralBool { value: false, .. } => "false",
        DiffKind::KeyFieldFlippedToLiteralString { text, .. } => text,
        DiffKind::KeyQuotedFlippedToLiteralString { text, .. } => text,
        _ => "",
    }
}

fn diff_kind_followed_by(kind: &DiffKind) -> FollowedBy {
    match kind {
        DiffKind::KeyFlippedToLiteralNull { followed_by, .. } => *followed_by,
        DiffKind::KeyFlippedToLiteralBool { followed_by, .. } => *followed_by,
        DiffKind::KeyFieldFlippedToLiteralString { followed_by, .. } => *followed_by,
        DiffKind::KeyQuotedFlippedToLiteralString { followed_by, .. } => *followed_by,
        _ => FollowedBy::Nothing,
    }
}

/// Classify a `DiffKind` into a recommendation bucket using the
/// heuristics from `SKILL.md` Mode A Step A2.
fn classify(kind: &DiffKind) -> (Recommendation, String) {
    match kind {
        DiffKind::KeyQuotedFlippedToLiteralString { text, followed_by, .. } => {
            if !matches!(followed_by, FollowedBy::Nothing) {
                return (
                    Recommendation::KeepV03,
                    format!(
                        "`\"{text}\"` is followed by a path access — literals don't have fields, so the v0.3 field-reference reading is the intended one."
                    ),
                );
            }
            if !is_valid_graphql_identifier(text) {
                return (
                    Recommendation::KeepV03,
                    format!(
                        "`\"{text}\"` contains characters not valid in a GraphQL identifier, so it is almost certainly a quoted field name from a REST response."
                    ),
                );
            }
            if is_likely_constant_literal(text) {
                return (
                    Recommendation::EmbraceV04,
                    format!(
                        "`\"{text}\"` looks like a short constant; the v0.4 reading as a literal string matches the apparent intent."
                    ),
                );
            }
            (
                Recommendation::Ambiguous,
                format!(
                    "`\"{text}\"` could be either a quoted field name or an intended literal. Review against your backend."
                ),
            )
        }
        DiffKind::KeyFlippedToLiteralNull { followed_by, .. } => {
            if !matches!(followed_by, FollowedBy::Nothing) {
                return (
                    Recommendation::KeepV03,
                    "`null` is followed by a path access — literals have no fields, so the v0.3 field-reference reading is the intended one.".to_string(),
                );
            }
            (
                Recommendation::EmbraceV04,
                "Bare `null` in value position is almost always intended as a literal null value; v0.3 returned the same thing accidentally via response normalization.".to_string(),
            )
        }
        DiffKind::KeyFlippedToLiteralBool { value, followed_by, .. } => {
            let lit = if *value { "true" } else { "false" };
            if !matches!(followed_by, FollowedBy::Nothing) {
                return (
                    Recommendation::KeepV03,
                    format!(
                        "`{lit}` is followed by a path access — literals have no fields, so the v0.3 field-reference reading is the intended one."
                    ),
                );
            }
            (
                Recommendation::EmbraceV04,
                format!("Bare `{lit}` in value position is almost always intended as a literal boolean."),
            )
        }
        DiffKind::KeyFieldFlippedToLiteralString { text, followed_by, .. } => {
            if !matches!(followed_by, FollowedBy::Nothing) {
                return (
                    Recommendation::KeepV03,
                    format!("`{text}` is followed by a path access — literals have no fields."),
                );
            }
            (
                Recommendation::Ambiguous,
                format!(
                    "`{text}` is ambiguous (bare identifier interpreted as a string under v0.4). Review against your backend."
                ),
            )
        }
        DiffKind::SubSelectionToLitObject { .. }
        | DiffKind::LegacyObjectToLitObject { .. }
        | DiffKind::Other { .. } => (
            Recommendation::Ambiguous,
            "Unclassified diff — please review manually.".to_string(),
        ),
    }
}

fn is_valid_graphql_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_likely_constant_literal(s: &str) -> bool {
    if s.is_empty() || s.len() > 8 {
        return false;
    }
    let all_upper_or_punct = s.chars().all(|c| {
        c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-'
    });
    let numeric_ish = s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == ',');
    let single_symbol = s.len() == 1 && !s.chars().next().unwrap().is_alphabetic();
    all_upper_or_punct || numeric_ish || single_symbol
}

fn hash_id(file: &str, coordinate: &str, kind: &str, text: &str, selection: &str) -> String {
    let mut hasher = DefaultHasher::new();
    file.hash(&mut hasher);
    coordinate.hash(&mut hasher);
    kind.hash(&mut hasher);
    text.hash(&mut hasher);
    selection.hash(&mut hasher);
    let h = hasher.finish();
    // First 4 bytes (8 hex chars). Collision is uninteresting for
    // human-scale recommendation files; apply double-checks against
    // file/coordinate/text on lookup anyway.
    format!("{:08x}", (h as u32))
}

/// A `Section` represents one `@connect(selection: …)` directive that
/// contains at least one divergent token. Multiple sites within the
/// same selection (e.g., several tokens in a multi-field selection)
/// roll up into a single Section so the developer makes one decision
/// covering the entire selection's rewrite.
#[derive(Debug)]
struct Section<'a> {
    file: &'a str,
    coordinate: &'a str,
    selection: &'a str,
    sites: Vec<&'a Site>,
    /// The selection text with `$.` fortifications applied to every
    /// site that the heuristic flagged as `keep-v0.3`. Equal to
    /// `selection` if no site got a keep-v0.3 recommendation.
    proposed_rewrite: String,
}

fn group_into_sections<'a>(sites: &'a [Site]) -> Vec<Section<'a>> {
    // Group by (file, coordinate, selection) — these all share one
    // `@connect(...)` directive. Order: file-walk order from analyze.
    let mut sections: Vec<Section<'a>> = Vec::new();
    for site in sites {
        let last = sections.last_mut();
        let same = last
            .as_ref()
            .map(|s| s.file == site.file && s.coordinate == site.coordinate && s.selection == site.selection)
            .unwrap_or(false);
        if let Some(s) = last.filter(|_| same) {
            s.sites.push(site);
        } else {
            sections.push(Section {
                file: &site.file,
                coordinate: &site.coordinate,
                selection: &site.selection,
                sites: vec![site],
                proposed_rewrite: String::new(),
            });
        }
    }
    for section in &mut sections {
        section.proposed_rewrite = compute_proposed_rewrite(section);
    }
    sections
}

/// Build the rewrite preview by applying `$.` fortifications to every
/// `keep-v0.3`-recommended token in the section. Token positions come
/// from each site's `source_range` (byte offsets into `selection`).
fn compute_proposed_rewrite(section: &Section<'_>) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for site in &section.sites {
        if !matches!(site.recommendation, Recommendation::KeepV03) {
            continue;
        }
        let Some((start, end)) = site.source_range else { continue };
        let new_text = match site.kind.as_str() {
            "key_flipped_to_literal_null" => "$.null".to_string(),
            "key_flipped_to_literal_bool" => format!("$.{}", site.text),
            "key_field_flipped_to_literal_string" => format!("$.{}", site.text),
            "key_quoted_flipped_to_literal_string" => format!("$.\"{}\"", site.text),
            _ => continue,
        };
        edits.push((start, end, new_text));
    }
    if edits.is_empty() {
        return section.selection.to_string();
    }
    edits.sort_by_key(|(s, _, _)| *s);
    let mut out = section.selection.to_string();
    for (start, end, new_text) in edits.into_iter().rev() {
        if end <= out.len() && out.is_char_boundary(start) && out.is_char_boundary(end) {
            out.replace_range(start..end, &new_text);
        }
    }
    out
}

/// Emit `recommendations.md` per the SKILL.md v1 format.
pub fn write_markdown<W: Write>(
    out: &mut W,
    sites: &[Site],
    project_root: &Path,
    generator_version: &str,
) -> std::io::Result<()> {
    let generated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());

    writeln!(out, "<!-- connect-migrate recommendations v1 -->")?;
    writeln!(out, "<!-- generator: connect-migrate {generator_version} -->")?;
    writeln!(out, "<!-- generated-at: {generated_at} -->")?;
    writeln!(
        out,
        "<!-- project-root: {} -->",
        project_root.display()
    )?;
    writeln!(out)?;
    writeln!(out, "# `connect/v0.3` → `connect/v0.4` migration recommendations")?;
    writeln!(out)?;
    if sites.is_empty() {
        writeln!(
            out,
            "No sites in this project need migration — every `@connect(selection: …)` parses identically under v0.3 and v0.4."
        )?;
        return Ok(());
    }

    let sections = group_into_sections(sites);

    writeln!(
        out,
        "{} section(s) need a decision ({} divergent token(s) across {} `@connect` selection(s)). For each section, edit the **Proposed rewrite** block as needed and check the box that reflects your decision, then run:",
        sections.len(),
        sites.len(),
        sections.len(),
    )?;
    writeln!(out)?;
    writeln!(out, "    connect-migrate apply recommendations.md")?;
    writeln!(out)?;
    writeln!(out, "Each section has two decision options. **Exactly one must be checked.** The defaults reflect what the analyzer recommends; edit the Proposed rewrite block, flip the checkbox, or both.")?;
    writeln!(out)?;
    writeln!(out, "- **apply the rewrite above** — apply uses the contents of the Proposed rewrite block as the new selection.")?;
    writeln!(out, "- **leave the source unchanged** — apply makes no change (accept the v0.4 literal reading).")?;
    writeln!(out)?;

    let total = sections.len();
    for (idx, section) in sections.iter().enumerate() {
        let n = idx + 1;
        let apply_default = section.proposed_rewrite != section.selection;
        writeln!(out, "---")?;
        writeln!(out)?;
        writeln!(
            out,
            "## section {n} of {total} — `{}` (`{}`)",
            section.file, section.coordinate,
        )?;
        writeln!(out)?;
        for site in &section.sites {
            writeln!(out, "<!-- connect-migrate site v1")?;
            writeln!(out, "  id: {}", site.id)?;
            writeln!(out, "  file: {}", site.file)?;
            if let Some(line) = site.line {
                writeln!(out, "  line: {line}")?;
            }
            if let Some(col) = site.col {
                writeln!(out, "  col: {col}")?;
            }
            writeln!(out, "  coordinate: {}", site.coordinate)?;
            writeln!(out, "  kind: {}", site.kind)?;
            if !site.text.is_empty() {
                writeln!(out, "  text: {}", quote_for_comment(&site.text))?;
            }
            writeln!(out, "  followed_by: {}", followed_by_name(site.followed_by))?;
            writeln!(out, "  recommendation: {}", site.recommendation.as_str())?;
            writeln!(out, "-->")?;
        }
        writeln!(out)?;
        writeln!(out, "**Original selection:**")?;
        writeln!(out)?;
        writeln!(out, "```graphql")?;
        write_block_lines(out, section.selection)?;
        writeln!(out, "```")?;
        writeln!(out)?;
        for site in &section.sites {
            writeln!(out, "- {}", site.reasoning)?;
        }
        writeln!(out)?;
        writeln!(out, "**Proposed rewrite** (edit if needed):")?;
        writeln!(out)?;
        writeln!(out, "```graphql")?;
        write_block_lines(out, &section.proposed_rewrite)?;
        writeln!(out, "```")?;
        writeln!(out)?;
        writeln!(out, "**Decide:**")?;
        writeln!(
            out,
            "- [{}] apply the rewrite above",
            if apply_default { "x" } else { " " }
        )?;
        writeln!(
            out,
            "- [{}] leave the source unchanged",
            if apply_default { " " } else { "x" }
        )?;
        writeln!(out)?;
    }
    Ok(())
}

fn write_block_lines<W: Write>(out: &mut W, body: &str) -> std::io::Result<()> {
    for line in body.lines() {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn followed_by_name(f: FollowedBy) -> &'static str {
    match f {
        FollowedBy::Nothing => "nothing",
        FollowedBy::KeyAccess => "key_access",
        FollowedBy::Method => "method",
        FollowedBy::SubSelection => "sub_selection",
        FollowedBy::Question => "question",
        FollowedBy::Expr => "expr",
    }
}

/// Escape a string for safe inclusion inside an HTML comment field
/// (no `--` sequences, no newlines).
fn quote_for_comment(s: &str) -> String {
    let cleaned = s.replace('\n', "\\n").replace("--", "-\\-");
    format!("\"{cleaned}\"")
}

/// Emit one JSONL record per site to `out`, one record per line.
pub fn write_jsonl<W: Write>(out: &mut W, sites: &[Site]) -> std::io::Result<()> {
    for site in sites {
        let line = serde_json::to_string(site)
            .unwrap_or_else(|_| String::from("{\"error\":\"serialize\"}"));
        writeln!(out, "{line}")?;
    }
    Ok(())
}

// ---- GraphQL string decode (mirrored from corpus extractor) ----

fn decode_graphql_string(source: &str) -> String {
    let s = source;
    if let Some(inner) = strip_pair(s, "\"\"\"") {
        return block_string_value(inner);
    }
    if let Some(inner) = strip_pair(s, "'''") {
        return block_string_value(inner);
    }
    if let Some(inner) = strip_pair(s, "\"") {
        return single_line_value(inner);
    }
    if let Some(inner) = strip_pair(s, "'") {
        return single_line_value(inner);
    }
    s.to_string()
}

fn strip_pair<'a>(s: &'a str, delim: &str) -> Option<&'a str> {
    if s.starts_with(delim) && s.ends_with(delim) && s.len() >= 2 * delim.len() {
        Some(&s[delim.len()..s.len() - delim.len()])
    } else {
        None
    }
}

fn block_string_value(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut common_indent: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let indent = line.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
        if indent < line.len() {
            common_indent = Some(match common_indent {
                Some(c) => c.min(indent),
                None => indent,
            });
        }
    }
    let common = common_indent.unwrap_or(0);
    let mut stripped: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            stripped.push((*line).to_string());
        } else if line.len() >= common {
            stripped.push(line[common..].to_string());
        } else {
            stripped.push(String::new());
        }
    }
    while stripped.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        stripped.remove(0);
    }
    while stripped.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        stripped.pop();
    }
    let mut out = stripped.join("\n");
    out = out.replace("\\\"\"\"", "\"\"\"");
    out
}

fn single_line_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                let mut hex = String::new();
                if chars.peek() == Some(&'{') {
                    chars.next();
                    while let Some(&p) = chars.peek() {
                        if p == '}' {
                            chars.next();
                            break;
                        }
                        hex.push(p);
                        chars.next();
                    }
                } else {
                    for _ in 0..4 {
                        if let Some(p) = chars.next() {
                            hex.push(p);
                        }
                    }
                }
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ---- Line/column index for byte offsets into a source string ----

struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    /// Convert a byte offset into a (1-indexed line, 1-indexed col).
    fn position(&self, offset: usize) -> (usize, usize) {
        let line_idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line_idx).copied().unwrap_or(0);
        (line_idx + 1, offset.saturating_sub(line_start) + 1)
    }
}
