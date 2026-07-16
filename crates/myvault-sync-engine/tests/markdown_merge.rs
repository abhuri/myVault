use myvault_sync_engine::conflict::{
    merge_markdown_three_way, MarkdownMergeIssue, MarkdownMergeOutcome, MarkdownVersion,
    NewlineStyle, MAX_MARKDOWN_LOGICAL_LINES, MAX_MARKDOWN_VERSION_BYTES,
};

fn merged(base: &str, local: &str, remote: &str) -> String {
    match merge_markdown_three_way(base, local, remote) {
        MarkdownMergeOutcome::Merged(result) => {
            assert_eq!(result.byte_length, result.content.len() as u64);
            assert_eq!(result.sha256.len(), 64);
            result.content
        }
        MarkdownMergeOutcome::PreserveBoth(issue) => panic!("unexpected preserve-both: {issue:?}"),
    }
}

fn issue(base: &str, local: &str, remote: &str) -> MarkdownMergeIssue {
    match merge_markdown_three_way(base, local, remote) {
        MarkdownMergeOutcome::PreserveBoth(issue) => issue,
        MarkdownMergeOutcome::Merged(result) => panic!("unexpected merge: {:?}", result.content),
    }
}

#[test]
fn merges_disjoint_and_adjacent_nonempty_edits() {
    assert_eq!(
        merged("a\nb\nc\nd\n", "A\nb\nc\nd\n", "a\nb\nc\nD\n"),
        "A\nb\nc\nD\n"
    );
    assert_eq!(merged("a\nb\nc\n", "A\nb\nc\n", "a\nB\nc\n"), "A\nB\nc\n");
}

#[test]
fn same_anchor_insertions_coalesce_or_conflict() {
    assert_eq!(merged("a\nb\n", "a\nx\nb\n", "a\nx\nb\n"), "a\nx\nb\n");
    assert_eq!(
        issue("a\nb\n", "a\nx\nb\n", "a\ny\nb\n"),
        MarkdownMergeIssue::OverlappingEdits
    );
}

#[test]
fn identical_nonempty_edits_coalesce_once() {
    assert_eq!(merged("a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n"), "a\nB\nc\n");
    assert_eq!(merged("a\nb\nc\n", "a\nc\n", "a\nc\n"), "a\nc\n");
}

#[test]
fn insertion_touching_replacement_boundary_conflicts() {
    assert_eq!(
        issue("a\nb\nc\n", "a\nx\nb\nc\n", "a\nB\nc\n"),
        MarkdownMergeIssue::OverlappingEdits
    );
    assert_eq!(
        issue("a\nb\nc\n", "a\nb\nx\nc\n", "a\nB\nc\n"),
        MarkdownMergeIssue::OverlappingEdits
    );
}

#[test]
fn deletion_and_edit_with_interior_intersection_conflict() {
    assert_eq!(
        issue("a\nb\nc\nd\n", "a\nd\n", "a\nb\nC\nd\n"),
        MarkdownMergeIssue::OverlappingEdits
    );
}

#[test]
fn repeated_line_myers_tie_break_is_golden() {
    assert_eq!(
        merged(
            "head\nrepeat\nrepeat\ntail\n",
            "head\nLOCAL\nrepeat\nrepeat\ntail\n",
            "head\nrepeat\nrepeat\nREMOTE\ntail\n",
        ),
        "head\nLOCAL\nrepeat\nrepeat\nREMOTE\ntail\n"
    );
}

#[test]
fn newline_style_and_final_newline_follow_three_way_selection() {
    let outcome = merge_markdown_three_way("a\nb\n", "a\r\nb\r\n", "a\nB\n");
    match outcome {
        MarkdownMergeOutcome::Merged(result) => {
            assert_eq!(result.content, "a\r\nB\r\n");
            assert_eq!(result.newline_style, NewlineStyle::CrLf);
            assert!(result.final_newline);
        }
        MarkdownMergeOutcome::PreserveBoth(issue) => panic!("unexpected issue: {issue:?}"),
    }

    assert_eq!(merged("a\n", "a", "A\n"), "A");
}

#[test]
fn one_sided_frontmatter_change_can_merge_with_body_change() {
    assert_eq!(
        merged(
            "---\ntitle: old\n---\nbody\n",
            "---\ntitle: new\n---\nbody\n",
            "---\ntitle: old\n---\nBODY\n",
        ),
        "---\ntitle: new\n---\nBODY\n"
    );
}

#[test]
fn divergent_frontmatter_never_line_merges() {
    assert_eq!(
        issue(
            "---\na: 1\nb: 1\n---\nbody\n",
            "---\na: 2\nb: 1\n---\nbody\n",
            "---\na: 1\nb: 2\n---\nbody\n",
        ),
        MarkdownMergeIssue::DivergentFrontmatter
    );
}

#[test]
fn identical_two_sided_frontmatter_change_preserves_both() {
    assert_eq!(
        issue(
            "---\ntitle: old\n---\nbody\n",
            "---\ntitle: new\n---\nbody\n",
            "---\ntitle: new\n---\nbody\n",
        ),
        MarkdownMergeIssue::DivergentFrontmatter
    );
}

#[test]
fn rejects_unsafe_newlines_bom_and_unclosed_frontmatter() {
    assert_eq!(
        issue("a\n", "a\rb", "a\n"),
        MarkdownMergeIssue::BareCarriageReturn {
            version: MarkdownVersion::Local
        }
    );
    assert_eq!(
        issue("a\n", "a\n", "a\r\nb\n"),
        MarkdownMergeIssue::MixedNewlines {
            version: MarkdownVersion::Remote
        }
    );
    assert_eq!(
        issue("a\n", "\u{feff}a\n", "a\n"),
        MarkdownMergeIssue::ByteOrderMark {
            version: MarkdownVersion::Local
        }
    );
    assert_eq!(
        issue("---\na: 1\n", "a\n", "a\n"),
        MarkdownMergeIssue::UnclosedFrontmatter {
            version: MarkdownVersion::Base
        }
    );
}

#[test]
fn enforces_version_and_logical_line_bounds() {
    let oversized = "x".repeat(MAX_MARKDOWN_VERSION_BYTES + 1);
    assert_eq!(
        issue("", &oversized, ""),
        MarkdownMergeIssue::VersionTooLarge {
            version: MarkdownVersion::Local
        }
    );

    let too_many_lines = "\n".repeat(MAX_MARKDOWN_LOGICAL_LINES + 1);
    assert_eq!(
        issue("", "", &too_many_lines),
        MarkdownMergeIssue::TooManyLogicalLines {
            version: MarkdownVersion::Remote
        }
    );
}

#[test]
fn deterministic_diff_work_bound_fails_closed_before_myers() {
    let base = "a\n".repeat(10_001);
    let changed = "b\n".repeat(10_001);
    assert_eq!(
        issue(&base, &changed, &changed),
        MarkdownMergeIssue::DiffComplexityLimit
    );
}
