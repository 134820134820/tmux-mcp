#![cfg_attr(target_os = "linux", allow(dead_code, unused_imports))]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use rstest::fixture;
use rstest::rstest;

use tmux_mcp_rs::tmux::{search_text, subsearch_text, SearchOptions, SubsearchOptions};
use tmux_mcp_rs::types::SearchMode;

const FIXTURE_PATH: &str = "tests/fixtures/old-man-and-the-sea.txt";

/// Load the fixture text, or `None` if absent. It is gitignored to avoid
/// committing copyrighted text, so dependent tests skip on a fresh clone.
fn load_fixture() -> Option<String> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_PATH);
    fs::read_to_string(fixture).ok()
}

#[cfg(not(target_os = "linux"))]
#[fixture]
fn oms_text() -> Option<String> {
    load_fixture()
}

#[cfg(target_os = "linux")]
#[test]
fn search_tests_skipped_on_linux() {
    eprintln!("skipping search fixture tests on linux");
}

#[cfg(not(target_os = "linux"))]
#[rstest]
fn literal_probe_then_subsearch(oms_text: Option<String>) {
    let Some(oms_text) = oms_text else {
        eprintln!("skipping: fixture {FIXTURE_PATH} not present");
        return;
    };
    let result = search_text(
        "fixture",
        &oms_text,
        "DiMaggio",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(40),
            max_matches: Some(50),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("literal search");

    assert!(result.total_matches >= 1);
    let first = &result.matches[0];
    assert!(first.offset_bytes < oms_text.len() as u64);
    assert!(first.snippet.contains("DiMaggio"));

    let sub = subsearch_text(
        "fixture",
        &oms_text,
        first.offset_bytes,
        first.match_len,
        "baseball",
        SearchMode::Literal,
        SubsearchOptions {
            context_bytes: 900,
            max_matches: Some(50),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("subsearch literal");

    assert!(sub.total_matches >= 1);
    assert!(sub.matches.iter().any(|m| m.snippet.contains("baseball")));
}

#[cfg(not(target_os = "linux"))]
#[rstest]
fn regex_probe_then_refine(oms_text: Option<String>) {
    let Some(oms_text) = oms_text else {
        eprintln!("skipping: fixture {FIXTURE_PATH} not present");
        return;
    };
    let result = search_text(
        "fixture",
        &oms_text,
        r#"\"[^\"]+\""#,
        SearchMode::Regex,
        SearchOptions {
            context_bytes: Some(10),
            max_matches: Some(1000),
            max_scan_bytes: Some(oms_text.len() as u64),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("regex search");

    assert!(result.total_matches >= 10);
    let first = &result.matches[0];

    let refine = subsearch_text(
        "fixture",
        &oms_text,
        first.offset_bytes,
        first.match_len,
        r#"(the boy said|the old man said)"#,
        SearchMode::Regex,
        SubsearchOptions {
            context_bytes: 600,
            max_matches: Some(20),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("regex refine");

    assert!(refine.total_matches >= 1);
}

#[cfg(not(target_os = "linux"))]
#[rstest]
#[case("old man")]
#[case("the boy")]
fn literal_multiword_queries(oms_text: Option<String>, #[case] query: &str) {
    let Some(oms_text) = oms_text else {
        eprintln!("skipping: fixture {FIXTURE_PATH} not present");
        return;
    };
    let result = search_text(
        "fixture",
        &oms_text,
        query,
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(20),
            max_matches: Some(50),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("multiword search");

    assert!(result.total_matches >= 1);
    assert!(result.matches.iter().any(|m| m.snippet.contains(query)));
}

#[cfg(all(not(target_os = "linux"), feature = "fuzzy"))]
#[rstest]
fn fuzzy_probe_then_confirm(oms_text: Option<String>) {
    let Some(oms_text) = oms_text else {
        eprintln!("skipping: fixture {FIXTURE_PATH} not present");
        return;
    };
    let result = search_text(
        "fixture",
        &oms_text,
        "dimagio",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(40),
            max_matches: Some(50),
            max_scan_bytes: Some(oms_text.len() as u64),
            include_similarity: false,
            fuzzy_match: true,
            similarity_threshold: Some(0.85),
            resume_from_offset: None,
        },
    )
    .expect("fuzzy search");

    assert!(result.total_matches >= 1);
    let first = &result.matches[0];
    assert!(first.snippet.contains("DiMaggio"));
}

#[cfg(not(target_os = "linux"))]
#[rstest]
fn snippet_generation_is_bounded() {
    let text = "hello world";
    let result = search_text(
        "fixture",
        text,
        "world",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(2),
            max_matches: Some(10),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("snippet search");

    let m = &result.matches[0];
    assert!(m.snippet.contains("world"));
    assert!(m.snippet.len() <= "world".len() + 4);
}

#[cfg(not(target_os = "linux"))]
#[rstest]
fn snippet_respects_scan_window() {
    let text = "prefix alpha beta suffix";
    let result = search_text(
        "fixture",
        text,
        "beta",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(10),
            max_matches: Some(5),
            max_scan_bytes: Some(17),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("scan-window search");

    let m = &result.matches[0];
    assert_eq!(m.context_start, 3);
    assert!(m.context_end <= 17);
    assert_eq!(&m.snippet, "fix alpha beta");
    assert!(m.snippet.contains("beta"));
}

#[cfg(not(target_os = "linux"))]
#[rstest]
fn match_id_is_deterministic() {
    let text = "aba";
    let result1 = search_text(
        "fixture",
        text,
        "a",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(1),
            max_matches: Some(10),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("search first");
    let result2 = search_text(
        "fixture",
        text,
        "a",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(1),
            max_matches: Some(10),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("search second");

    assert_eq!(result1.matches[0].match_id, result2.matches[0].match_id);
    assert_ne!(result1.matches[0].match_id, result1.matches[1].match_id);
}

#[test]
fn literal_pages_find_boundary_spanning_matches_once_with_utf8_cursors() {
    let text = "éabneedle--needle--aéneedle";
    let expected: Vec<u64> = text
        .match_indices("needle")
        .map(|(offset, _)| offset as u64)
        .collect();
    for max_scan_bytes in 2..=9 {
        let mut resume: Option<BTreeMap<String, u64>> = None;
        let mut actual = Vec::new();
        let mut final_cursor = None;

        for _ in 0..32 {
            let page = search_text(
                "fixture",
                text,
                "needle",
                SearchMode::Literal,
                SearchOptions {
                    context_bytes: Some(3),
                    max_matches: Some(1),
                    max_scan_bytes: Some(max_scan_bytes),
                    include_similarity: false,
                    fuzzy_match: false,
                    similarity_threshold: None,
                    resume_from_offset: resume,
                },
            )
            .expect("literal page");

            assert!(page.bytes_scanned_total <= max_scan_bytes);
            let cursor = *page
                .resume_from_offset
                .get("fixture")
                .expect("complete cursor metadata");
            assert!(text.is_char_boundary(cursor as usize));
            actual.extend(page.matches.iter().map(|entry| entry.offset_bytes));

            if page.truncated_buffers.is_empty() {
                final_cursor = Some(page.resume_from_offset);
                break;
            }
            assert_eq!(page.truncated_buffers, vec!["fixture"]);
            resume = Some(page.resume_from_offset);
        }

        assert_eq!(actual, expected, "scan budget {max_scan_bytes}");
        let final_cursor = final_cursor.expect("pagination completed");
        assert_eq!(final_cursor.get("fixture"), Some(&(text.len() as u64)));

        let replay = search_text(
            "fixture",
            text,
            "needle",
            SearchMode::Literal,
            SearchOptions {
                context_bytes: Some(3),
                max_matches: Some(1),
                max_scan_bytes: Some(max_scan_bytes),
                include_similarity: false,
                fuzzy_match: false,
                similarity_threshold: None,
                resume_from_offset: Some(final_cursor),
            },
        )
        .expect("replay completed cursor");
        assert!(replay.matches.is_empty());
        assert!(replay.truncated_buffers.is_empty());
    }
}

#[test]
fn literal_paging_rejects_non_progressing_or_invalid_utf8_cursors() {
    let too_small = search_text(
        "fixture",
        "éneedle",
        "é",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(1),
            max_scan_bytes: Some(1),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect_err("one byte cannot consume a two-byte scalar");
    assert!(too_small
        .to_string()
        .contains("cannot consume the next UTF-8 character"));

    let invalid_resume = search_text(
        "fixture",
        "éneedle",
        "needle",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(1),
            max_scan_bytes: Some(8),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: Some(BTreeMap::from([("fixture".to_string(), 1)])),
        },
    )
    .expect_err("resume must be a character boundary");
    assert!(invalid_resume
        .to_string()
        .contains("not a UTF-8 character boundary"));
}

#[test]
fn zero_search_budgets_are_rejected_instead_of_returning_stuck_cursors() {
    for (max_matches, max_scan_bytes, expected) in [
        (Some(0), Some(1), "maxMatches"),
        (Some(1), Some(0), "maxScanBytes"),
    ] {
        let error = search_text(
            "fixture",
            "needle",
            "needle",
            SearchMode::Literal,
            SearchOptions {
                context_bytes: Some(0),
                max_matches,
                max_scan_bytes,
                include_similarity: false,
                fuzzy_match: false,
                similarity_threshold: None,
                resume_from_offset: None,
            },
        )
        .expect_err("zero budget must fail");
        assert!(error.to_string().contains(expected));
    }
}

#[test]
fn unbounded_regex_requires_and_uses_a_full_buffer_scan_budget() {
    let text = format!("a{}z", "x".repeat(65_536));
    let default_budget = search_text(
        "fixture",
        &text,
        r"a.*z",
        SearchMode::Regex,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(10),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect_err("default budget must not pretend an unbounded regex is pageable");
    let message = default_budget.to_string();
    assert!(message.contains("cannot preserve partial matcher state"));
    assert!(message.contains(&format!("at least {} bytes", text.len())));

    let result = search_text(
        "fixture",
        &text,
        r"a.*z",
        SearchMode::Regex,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(10),
            max_scan_bytes: Some(text.len() as u64),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect("full-buffer regex scan");
    assert_eq!(result.bytes_scanned_total, text.len() as u64);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].offset_bytes, 0);
    assert_eq!(result.matches[0].match_len as usize, text.len());
    assert!(result.truncated_buffers.is_empty());
}

#[cfg(feature = "rapidfuzz")]
#[test]
fn fuzzy_search_default_budget_rejects_buffers_that_cannot_be_paged_statelessly() {
    let text = format!("needle\n{}", "short line\n".repeat(7_000));
    let error = search_text(
        "fixture",
        &text,
        "nedle",
        SearchMode::Literal,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(10),
            max_scan_bytes: None,
            include_similarity: false,
            fuzzy_match: true,
            similarity_threshold: Some(0.7),
            resume_from_offset: None,
        },
    )
    .expect_err("fuzzy line state requires an explicit full-buffer budget");
    assert!(error
        .to_string()
        .contains("cannot preserve partial matcher state"));
    assert!(error
        .to_string()
        .contains(&format!("at least {} bytes", text.len())));
}

#[test]
fn regex_max_matches_pages_preserve_global_matching_and_do_not_replay() {
    let text = "a1z--a222z--a33333z";
    let options = |resume| SearchOptions {
        context_bytes: Some(0),
        max_matches: Some(1),
        max_scan_bytes: Some(text.len() as u64),
        include_similarity: false,
        fuzzy_match: false,
        similarity_threshold: None,
        resume_from_offset: resume,
    };

    let first = search_text("fixture", text, r"a.*?z", SearchMode::Regex, options(None))
        .expect("first regex result page");
    assert_eq!(first.matches[0].offset_bytes, 0);
    assert_eq!(first.truncated_buffers, vec!["fixture"]);

    let second = search_text(
        "fixture",
        text,
        r"a.*?z",
        SearchMode::Regex,
        options(Some(first.resume_from_offset)),
    )
    .expect("second regex result page");
    assert_eq!(second.matches[0].offset_bytes, 5);

    let third = search_text(
        "fixture",
        text,
        r"a.*?z",
        SearchMode::Regex,
        options(Some(second.resume_from_offset)),
    )
    .expect("third regex result page");
    assert_eq!(third.matches[0].offset_bytes, 12);
    assert!(third.truncated_buffers.is_empty());
    assert_eq!(
        third.resume_from_offset.get("fixture"),
        Some(&(text.len() as u64))
    );

    let replay = search_text(
        "fixture",
        text,
        r"a.*?z",
        SearchMode::Regex,
        options(Some(third.resume_from_offset)),
    )
    .expect("completed regex cursor replay");
    assert!(replay.matches.is_empty());
}

#[test]
fn top_level_regex_rejects_zero_length_hits_that_offsets_cannot_page() {
    let error = search_text(
        "fixture",
        "word",
        r"\b",
        SearchMode::Regex,
        SearchOptions {
            context_bytes: Some(0),
            max_matches: Some(10),
            max_scan_bytes: Some(4),
            include_similarity: false,
            fuzzy_match: false,
            similarity_threshold: None,
            resume_from_offset: None,
        },
    )
    .expect_err("zero-length hits cannot use an offset-only cursor");
    assert!(error.to_string().contains("zero-length match"));
    assert!(error.to_string().contains("without duplicates"));
}
