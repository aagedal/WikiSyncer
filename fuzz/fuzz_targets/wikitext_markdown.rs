#![no_main]

use libfuzzer_sys::fuzz_target;
use wikisync_content::{DiffMode, diff, to_markdown, to_plain_text, to_search_content};

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let source = String::from_utf8_lossy(data);
    let plain = to_plain_text(&source);
    let markdown = to_markdown(&source);
    let search = to_search_content(&source);

    assert_eq!(plain, to_plain_text(&source));
    assert_eq!(markdown, to_markdown(&source));
    assert_eq!(search, to_search_content(&source));
    assert_eq!(search.body, plain);

    for output in [&plain, &markdown, &search.headings] {
        assert!(output.is_empty() || output.ends_with('\n'));
        assert!(!output.ends_with("\n\n"));
    }

    // Rewriting already-derived Markdown and diffing either representation must
    // remain bounded and panic-free even when the source is malformed wikitext.
    let rewritten = to_markdown(&markdown);
    assert!(rewritten.is_empty() || rewritten.ends_with('\n'));
    let _ = diff(&plain, &markdown, DiffMode::Reading);
    let _ = diff(&source, &markdown, DiffMode::ExactSource);
});
