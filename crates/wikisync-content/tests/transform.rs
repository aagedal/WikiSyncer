use wikisync_content::{
    MARKDOWN_TRANSFORMER_VERSION, OutputKind, PLAIN_TEXT_TRANSFORMER_VERSION, to_markdown,
    to_plain_text, transform,
};

const ARTICLE_SOURCE: &str = include_str!("../../../fixtures/content/article.wiki");
const ARTICLE_TEXT: &str = include_str!("../../../fixtures/content/article.txt");
const ARTICLE_MARKDOWN: &str = include_str!("../../../fixtures/content/article.md");

#[test]
fn representative_article_matches_golden_outputs() {
    assert_eq!(to_plain_text(ARTICLE_SOURCE), ARTICLE_TEXT);
    assert_eq!(to_markdown(ARTICLE_SOURCE), ARTICLE_MARKDOWN);
}

#[test]
fn output_carries_a_deterministic_cache_version() {
    let plain = transform(ARTICLE_SOURCE, OutputKind::PlainText);
    let markdown = transform(ARTICLE_SOURCE, OutputKind::Markdown);

    assert_eq!(plain.transformer_version, PLAIN_TEXT_TRANSFORMER_VERSION);
    assert_eq!(markdown.transformer_version, MARKDOWN_TRANSFORMER_VERSION);
    assert_eq!(plain.body, to_plain_text(ARTICLE_SOURCE));
    assert_eq!(markdown.body, to_markdown(ARTICLE_SOURCE));
}

#[test]
fn malformed_or_unclosed_constructs_remain_bounded_and_readable() {
    let source = "before [[unclosed {{template|value <ref>citation";
    assert_eq!(
        to_plain_text(source),
        "before [[unclosed {{template|value citation\n"
    );
    assert_eq!(
        to_markdown(source),
        "before \\[\\[unclosed {{template|value citation\n"
    );
}
