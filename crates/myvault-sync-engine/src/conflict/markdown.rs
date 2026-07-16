use sha2::{Digest, Sha256};
use similar::{Algorithm, DiffTag, TextDiff};

pub const MAX_MARKDOWN_VERSION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MARKDOWN_COMBINED_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_MARKDOWN_LOGICAL_LINES: usize = 100_000;
pub const MAX_MARKDOWN_DIFF_WORK: usize = 100_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkdownVersion {
    Base,
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NewlineStyle {
    None,
    Lf,
    CrLf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkdownMergeIssue {
    EvidenceFingerprintMismatch,
    VersionTooLarge { version: MarkdownVersion },
    CombinedInputTooLarge,
    TooManyLogicalLines { version: MarkdownVersion },
    ByteOrderMark { version: MarkdownVersion },
    BareCarriageReturn { version: MarkdownVersion },
    MixedNewlines { version: MarkdownVersion },
    UnclosedFrontmatter { version: MarkdownVersion },
    DivergentFrontmatter,
    OverlappingEdits,
    DivergentNewlineStyle,
    DivergentFinalNewline,
    InvalidEditScript,
    DiffComplexityLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedMarkdown {
    pub content: String,
    pub sha256: String,
    pub byte_length: u64,
    pub newline_style: NewlineStyle,
    pub final_newline: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkdownMergeOutcome {
    Merged(MergedMarkdown),
    PreserveBoth(MarkdownMergeIssue),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedMarkdown<'a> {
    lines: Vec<&'a str>,
    newline_style: NewlineStyle,
    final_newline: bool,
    frontmatter_end: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Edit<'a> {
    start: usize,
    end: usize,
    replacement: Vec<&'a str>,
}

/// Pure, deterministic three-way merge for bounded regular Markdown content.
///
/// A non-merged outcome contains no partial content and never inserts conflict
/// markers. The caller decides whether the safe fallback is preserve-both or
/// needs-reconcile based on its materialization capabilities.
#[must_use]
pub fn merge_markdown_three_way(base: &str, local: &str, remote: &str) -> MarkdownMergeOutcome {
    let Some(combined) = base
        .len()
        .checked_add(local.len())
        .and_then(|size| size.checked_add(remote.len()))
    else {
        return preserve(MarkdownMergeIssue::CombinedInputTooLarge);
    };
    if combined > MAX_MARKDOWN_COMBINED_BYTES {
        return preserve(MarkdownMergeIssue::CombinedInputTooLarge);
    }

    let base = match parse_markdown(base, MarkdownVersion::Base) {
        Ok(parsed) => parsed,
        Err(issue) => return preserve(issue),
    };
    let local = match parse_markdown(local, MarkdownVersion::Local) {
        Ok(parsed) => parsed,
        Err(issue) => return preserve(issue),
    };
    let remote = match parse_markdown(remote, MarkdownVersion::Remote) {
        Ok(parsed) => parsed,
        Err(issue) => return preserve(issue),
    };

    let diff_work = base
        .lines
        .len()
        .saturating_mul(local.lines.len().max(remote.lines.len()));
    if diff_work > MAX_MARKDOWN_DIFF_WORK {
        return preserve(MarkdownMergeIssue::DiffComplexityLimit);
    }

    if divergent_frontmatter(&base, &local, &remote) {
        return preserve(MarkdownMergeIssue::DivergentFrontmatter);
    }

    let Some(newline_style) = select_three_way(
        base.newline_style,
        local.newline_style,
        remote.newline_style,
    ) else {
        return preserve(MarkdownMergeIssue::DivergentNewlineStyle);
    };
    let Some(final_newline) = select_three_way(
        base.final_newline,
        local.final_newline,
        remote.final_newline,
    ) else {
        return preserve(MarkdownMergeIssue::DivergentFinalNewline);
    };

    let Ok(local_edits) = edits_from_myers(&base.lines, &local.lines) else {
        return preserve(MarkdownMergeIssue::InvalidEditScript);
    };
    let Ok(remote_edits) = edits_from_myers(&base.lines, &remote.lines) else {
        return preserve(MarkdownMergeIssue::InvalidEditScript);
    };
    let edits = match combine_edits(local_edits, remote_edits) {
        Ok(edits) => edits,
        Err(issue) => return preserve(issue),
    };
    let Some(merged_lines) = apply_edits(&base.lines, &edits) else {
        return preserve(MarkdownMergeIssue::InvalidEditScript);
    };
    let Some(content) = render(&merged_lines, newline_style, final_newline) else {
        return preserve(MarkdownMergeIssue::InvalidEditScript);
    };
    let Ok(byte_length) = u64::try_from(content.len()) else {
        return preserve(MarkdownMergeIssue::InvalidEditScript);
    };
    let digest = Sha256::digest(content.as_bytes());

    MarkdownMergeOutcome::Merged(MergedMarkdown {
        byte_length,
        sha256: format!("{digest:x}"),
        content,
        newline_style,
        final_newline,
    })
}

fn preserve(issue: MarkdownMergeIssue) -> MarkdownMergeOutcome {
    MarkdownMergeOutcome::PreserveBoth(issue)
}

fn parse_markdown(
    input: &str,
    version: MarkdownVersion,
) -> Result<ParsedMarkdown<'_>, MarkdownMergeIssue> {
    if input.len() > MAX_MARKDOWN_VERSION_BYTES {
        return Err(MarkdownMergeIssue::VersionTooLarge { version });
    }
    if input.starts_with('\u{feff}') {
        return Err(MarkdownMergeIssue::ByteOrderMark { version });
    }

    let bytes = input.as_bytes();
    let mut saw_lf = false;
    let mut saw_crlf = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                if bytes.get(index + 1) != Some(&b'\n') {
                    return Err(MarkdownMergeIssue::BareCarriageReturn { version });
                }
                saw_crlf = true;
                index += 2;
            }
            b'\n' => {
                saw_lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    if saw_lf && saw_crlf {
        return Err(MarkdownMergeIssue::MixedNewlines { version });
    }

    let newline_style = if saw_crlf {
        NewlineStyle::CrLf
    } else if saw_lf {
        NewlineStyle::Lf
    } else {
        NewlineStyle::None
    };
    let final_newline = input.ends_with('\n');
    let mut lines: Vec<&str> = input
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if final_newline {
        let removed = lines.pop();
        if removed != Some("") {
            return Err(MarkdownMergeIssue::BareCarriageReturn { version });
        }
    } else if input.is_empty() {
        lines.clear();
    }
    if lines.len() > MAX_MARKDOWN_LOGICAL_LINES {
        return Err(MarkdownMergeIssue::TooManyLogicalLines { version });
    }

    let frontmatter_end = if lines.first() == Some(&"---") {
        match lines.iter().skip(1).position(|line| *line == "---") {
            Some(relative) => Some(relative + 2),
            None => return Err(MarkdownMergeIssue::UnclosedFrontmatter { version }),
        }
    } else {
        None
    };

    Ok(ParsedMarkdown {
        lines,
        newline_style,
        final_newline,
        frontmatter_end,
    })
}

fn frontmatter<'a>(parsed: &'a ParsedMarkdown<'a>) -> Option<&'a [&'a str]> {
    parsed.frontmatter_end.map(|end| &parsed.lines[..end])
}

fn divergent_frontmatter(
    base: &ParsedMarkdown<'_>,
    local: &ParsedMarkdown<'_>,
    remote: &ParsedMarkdown<'_>,
) -> bool {
    let base_region = frontmatter(base);
    let local_region = frontmatter(local);
    let remote_region = frontmatter(remote);
    local_region != base_region && remote_region != base_region
}

fn select_three_way<T: Copy + Eq>(base: T, local: T, remote: T) -> Option<T> {
    if local == remote {
        Some(local)
    } else if local == base {
        Some(remote)
    } else if remote == base {
        Some(local)
    } else {
        None
    }
}

fn edits_from_myers<'a>(
    base: &[&'a str],
    changed: &[&'a str],
) -> Result<Vec<Edit<'a>>, MarkdownMergeIssue> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_slices(base, changed);
    let mut edits = Vec::new();
    let mut pending: Option<Edit<'a>> = None;

    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            if let Some(edit) = pending.take() {
                edits.push(edit);
            }
            continue;
        }
        let old = op.old_range();
        let new = op.new_range();
        let edit = pending.get_or_insert_with(|| Edit {
            start: old.start,
            end: old.start,
            replacement: Vec::new(),
        });
        edit.end = old.end;
        edit.replacement.extend_from_slice(&changed[new]);
    }
    if let Some(edit) = pending {
        edits.push(edit);
    }
    if apply_edits(base, &edits).as_deref() != Some(changed) {
        return Err(MarkdownMergeIssue::InvalidEditScript);
    }
    Ok(edits)
}

fn combine_edits<'a>(
    local: Vec<Edit<'a>>,
    remote: Vec<Edit<'a>>,
) -> Result<Vec<Edit<'a>>, MarkdownMergeIssue> {
    let mut combined = local;
    for candidate in remote {
        if let Some(existing) = combined.iter().find(|edit| same_edit(edit, &candidate)) {
            let _ = existing;
            continue;
        }
        if combined.iter().any(|edit| edits_overlap(edit, &candidate)) {
            return Err(MarkdownMergeIssue::OverlappingEdits);
        }
        combined.push(candidate);
    }
    combined.sort_by_key(|edit| (edit.start, edit.end));
    Ok(combined)
}

fn same_edit(left: &Edit<'_>, right: &Edit<'_>) -> bool {
    left.start == right.start && left.end == right.end && left.replacement == right.replacement
}

fn edits_overlap(left: &Edit<'_>, right: &Edit<'_>) -> bool {
    let left_empty = left.start == left.end;
    let right_empty = right.start == right.end;
    match (left_empty, right_empty) {
        (true, true) => left.start == right.start,
        (true, false) => (right.start..=right.end).contains(&left.start),
        (false, true) => (left.start..=left.end).contains(&right.start),
        (false, false) => left.start.max(right.start) < left.end.min(right.end),
    }
}

fn apply_edits<'a>(base: &[&'a str], edits: &[Edit<'a>]) -> Option<Vec<&'a str>> {
    let mut output = Vec::new();
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor || edit.end < edit.start || edit.end > base.len() {
            return None;
        }
        output.extend_from_slice(&base[cursor..edit.start]);
        output.extend_from_slice(&edit.replacement);
        cursor = edit.end;
    }
    output.extend_from_slice(&base[cursor..]);
    Some(output)
}

fn render(lines: &[&str], style: NewlineStyle, final_newline: bool) -> Option<String> {
    if style == NewlineStyle::None && (lines.len() > 1 || final_newline) {
        return None;
    }
    let separator = match style {
        NewlineStyle::None => "",
        NewlineStyle::Lf => "\n",
        NewlineStyle::CrLf => "\r\n",
    };
    let mut output = lines.join(separator);
    if final_newline {
        output.push_str(separator);
    }
    Some(output)
}
