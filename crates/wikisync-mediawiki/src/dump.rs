//! Streaming reader for Wikimedia `pages-meta-current` multistream dumps.
//!
//! Wikimedia's multistream files concatenate independent bzip2 members whose
//! decompressed bytes form one XML document. This module reads every member in
//! sequence and retains at most one bounded page record. It never materializes the
//! decompressed dump or an unbounded XML token.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bzip2::read::MultiBzDecoder;
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesCData, BytesDecl, BytesRef, BytesStart, BytesText, Event};
use wikisync_core::{PageId, PageTitle, RevisionId};

use crate::RevisionMetadata;

const DEFAULT_MAX_COMPRESSED_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const DEFAULT_MAX_DECOMPRESSED_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
const DEFAULT_MAX_PAGES: u64 = 100_000_000;
const DEFAULT_MAX_PAGE_XML_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_MAX_TEXT_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_METADATA_FIELD_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_SITEINFO_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MAX_NAMESPACES: usize = 1_024;
const MAX_XML_DEPTH: usize = 32;
const MAX_XML_NAME_BYTES: usize = 128;
const MAX_FILTER_ENTRIES: usize = 1_024;
const EVENT_OVERHEAD_BYTES: usize = 4 * 1024;

/// Resource ceilings for one multistream dump scan.
///
/// The defaults accommodate current Wikimedia language dumps while still imposing
/// fixed ceilings. Callers and tests may choose smaller positive values. Validation
/// happens before any compressed input is read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpLimits {
    /// Maximum compressed bytes accepted from the source reader.
    pub max_compressed_bytes: u64,
    /// Maximum decompressed XML bytes parsed across every bzip2 member.
    pub max_decompressed_bytes: u64,
    /// Maximum `<page>` records examined, including records filtered out.
    pub max_pages: u64,
    /// Maximum decompressed XML bytes occupied by one `<page>` record.
    pub max_page_xml_bytes: u64,
    /// Maximum decoded UTF-8 bytes in one current revision's `<text>`.
    pub max_text_bytes: usize,
    /// Maximum decoded UTF-8 bytes in one metadata field.
    pub max_metadata_field_bytes: usize,
    /// Maximum decompressed bytes before the end of `<siteinfo>`.
    pub max_siteinfo_bytes: u64,
    /// Maximum namespace declarations accepted from `<siteinfo>`.
    pub max_namespaces: usize,
}

impl Default for DumpLimits {
    fn default() -> Self {
        Self {
            max_compressed_bytes: DEFAULT_MAX_COMPRESSED_BYTES,
            max_decompressed_bytes: DEFAULT_MAX_DECOMPRESSED_BYTES,
            max_pages: DEFAULT_MAX_PAGES,
            max_page_xml_bytes: DEFAULT_MAX_PAGE_XML_BYTES,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_metadata_field_bytes: DEFAULT_MAX_METADATA_FIELD_BYTES,
            max_siteinfo_bytes: DEFAULT_MAX_SITEINFO_BYTES,
            max_namespaces: DEFAULT_MAX_NAMESPACES,
        }
    }
}

impl DumpLimits {
    fn validate(self) -> Result<Self, DumpError> {
        for (name, value) in [
            ("compressed byte limit", self.max_compressed_bytes),
            ("decompressed byte limit", self.max_decompressed_bytes),
            ("page count limit", self.max_pages),
            ("page XML byte limit", self.max_page_xml_bytes),
            ("site-info byte limit", self.max_siteinfo_bytes),
        ] {
            if value == 0 {
                return Err(DumpError::InvalidLimit(name));
            }
        }
        for (name, value) in [
            ("revision text byte limit", self.max_text_bytes),
            ("metadata field byte limit", self.max_metadata_field_bytes),
            ("namespace count limit", self.max_namespaces),
        ] {
            if value == 0 {
                return Err(DumpError::InvalidLimit(name));
            }
        }
        if self.max_text_bytes as u64 > self.max_page_xml_bytes {
            return Err(DumpError::InvalidLimit(
                "revision text byte limit must not exceed the page XML byte limit",
            ));
        }
        Ok(self)
    }

    fn max_event_bytes(self) -> usize {
        let page_limit = usize::try_from(self.max_page_xml_bytes).unwrap_or(usize::MAX);
        self.max_text_bytes
            .max(self.max_metadata_field_bytes)
            .saturating_add(EVENT_OVERHEAD_BYTES)
            .min(page_limit)
    }
}

/// Namespace and content-model selection applied while scanning a dump.
///
/// Filtered records still count toward the page ceiling. The default yields only
/// main-namespace wikitext, which is the stable-v1 Wikipedia bootstrap scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpFilter {
    namespaces: BTreeSet<i32>,
    content_models: BTreeSet<String>,
}

impl DumpFilter {
    /// Builds a non-empty filter. Content-model names must be short, non-empty UTF-8
    /// strings without control characters.
    pub fn new(
        namespaces: impl IntoIterator<Item = i32>,
        content_models: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, DumpError> {
        let mut namespaces_set = BTreeSet::new();
        for namespace in namespaces {
            if namespaces_set.len() == MAX_FILTER_ENTRIES && !namespaces_set.contains(&namespace) {
                return Err(DumpError::InvalidFilter(
                    "namespace set exceeds the entry limit",
                ));
            }
            namespaces_set.insert(namespace);
        }
        if namespaces_set.is_empty() {
            return Err(DumpError::InvalidFilter("namespace set cannot be empty"));
        }
        let mut models = BTreeSet::new();
        for model in content_models {
            let model = model.into();
            validate_short_text(&model, "content model", DEFAULT_MAX_METADATA_FIELD_BYTES)?;
            if models.len() == MAX_FILTER_ENTRIES && !models.contains(&model) {
                return Err(DumpError::InvalidFilter(
                    "content-model set exceeds the entry limit",
                ));
            }
            models.insert(model);
        }
        if models.is_empty() {
            return Err(DumpError::InvalidFilter(
                "content-model set cannot be empty",
            ));
        }
        Ok(Self {
            namespaces: namespaces_set,
            content_models: models,
        })
    }

    fn accepts(&self, namespace: i32, content_model: &str) -> bool {
        self.namespaces.contains(&namespace) && self.content_models.contains(content_model)
    }
}

impl Default for DumpFilter {
    fn default() -> Self {
        Self::new([wikisync_core::MAIN_NAMESPACE], ["wikitext"])
            .expect("default dump filter is valid")
    }
}

/// One namespace declaration from dump site metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpNamespace {
    /// MediaWiki namespace number.
    pub key: i32,
    /// Source case rule, such as `first-letter`.
    pub case_rule: String,
    /// Localized namespace label; the main namespace is normally empty.
    pub name: String,
}

/// Bounded language/source metadata parsed before any pages are yielded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpSiteInfo {
    /// MediaWiki export namespace URI, such as
    /// `http://www.mediawiki.org/xml/export-0.11/`.
    pub export_schema: String,
    /// MediaWiki export format version declared on the root element.
    pub export_version: String,
    /// Wikimedia database name, such as `enwiki`.
    pub database_name: String,
    /// Root `xml:lang` language tag.
    pub language_code: String,
    /// Canonical wiki base URL when supplied by the dump.
    pub base_url: Option<String>,
    /// MediaWiki generator string when supplied by the dump.
    pub generator: Option<String>,
    /// Site-wide title case rule.
    pub case_rule: String,
    /// Namespace declarations in source order.
    pub namespaces: Vec<DumpNamespace>,
}

/// One current page from a filtered `pages-meta-current` dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpPage {
    /// Stable remote page identity.
    pub page_id: PageId,
    /// MediaWiki namespace number.
    pub namespace: i32,
    /// Current page title.
    pub title: PageTitle,
    /// Redirect target declared by the page, when present.
    pub redirect_title: Option<PageTitle>,
    /// Exactly one current revision from the dump record.
    pub revision: DumpRevision,
}

/// Current revision metadata and canonical main-slot source from a dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpRevision {
    /// Metadata aligned with the Action API representation.
    pub metadata: RevisionMetadata,
    /// MediaWiki content format, normally `text/x-wiki` for wikitext.
    pub content_format: String,
    /// Decoded canonical UTF-8 source. Suppressed text is represented by `None`.
    pub source: Option<Vec<u8>>,
}

/// A bounded streaming reader over a bzip2 multistream Wikimedia XML dump.
///
/// Construction consumes only the bounded prologue and `<siteinfo>`. Iteration then
/// yields at most one owned [`DumpPage`] at a time. After the first error, the reader
/// is fused and returns `None`.
pub struct DumpReader<R: Read> {
    reader: XmlReader<R>,
    event_buffer: Vec<u8>,
    path: Vec<Vec<u8>>,
    limits: DumpLimits,
    filter: DumpFilter,
    site_info: DumpSiteInfo,
    page: Option<PageBuilder>,
    page_start: u64,
    last_event_start: u64,
    pages_examined: u64,
    pages_yielded: u64,
    document_ended: bool,
    failed: bool,
    compressed_limit_exceeded: Arc<AtomicBool>,
    decompressed_limit_exceeded: Arc<AtomicBool>,
    event_limit_exceeded: Arc<AtomicBool>,
}

impl<R: Read> fmt::Debug for DumpReader<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DumpReader")
            .field("path", &self.path)
            .field("limits", &self.limits)
            .field("filter", &self.filter)
            .field("site_info", &self.site_info)
            .field("pages_examined", &self.pages_examined)
            .field("pages_yielded", &self.pages_yielded)
            .field("document_ended", &self.document_ended)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

type XmlReader<R> =
    Reader<EventBoundedBufRead<DecompressedBoundedRead<MultiBzDecoder<CompressedBoundedRead<R>>>>>;

impl<R: Read> DumpReader<R> {
    /// Creates a main-namespace wikitext reader with explicit resource limits.
    pub fn new(input: R, limits: DumpLimits) -> Result<Self, DumpError> {
        Self::with_filter(input, limits, DumpFilter::default())
    }

    /// Creates a reader with an explicit namespace/content-model filter.
    pub fn with_filter(
        input: R,
        limits: DumpLimits,
        filter: DumpFilter,
    ) -> Result<Self, DumpError> {
        let limits = limits.validate()?;
        let compressed_limit_exceeded = Arc::new(AtomicBool::new(false));
        let compressed = CompressedBoundedRead::new(
            input,
            limits.max_compressed_bytes,
            Arc::clone(&compressed_limit_exceeded),
        );
        let decompressed = MultiBzDecoder::new(compressed);
        let decompressed_limit_exceeded = Arc::new(AtomicBool::new(false));
        let decompressed = DecompressedBoundedRead::new(
            decompressed,
            limits.max_decompressed_bytes,
            Arc::clone(&decompressed_limit_exceeded),
        );
        let event_limit_exceeded = Arc::new(AtomicBool::new(false));
        let bounded_events = EventBoundedBufRead::new(
            decompressed,
            limits.max_event_bytes(),
            Arc::clone(&event_limit_exceeded),
        );
        let mut reader = Reader::from_reader(bounded_events);
        reader.config_mut().enable_all_checks(true);
        reader.config_mut().trim_text(false);

        let mut dump = Self {
            reader,
            event_buffer: Vec::with_capacity(8 * 1024),
            path: Vec::with_capacity(8),
            limits,
            filter,
            site_info: DumpSiteInfo {
                export_schema: String::new(),
                export_version: String::new(),
                database_name: String::new(),
                language_code: String::new(),
                base_url: None,
                generator: None,
                case_rule: String::new(),
                namespaces: Vec::new(),
            },
            page: None,
            page_start: 0,
            last_event_start: 0,
            pages_examined: 0,
            pages_yielded: 0,
            document_ended: false,
            failed: false,
            compressed_limit_exceeded,
            decompressed_limit_exceeded,
            event_limit_exceeded,
        };
        dump.read_site_info()?;
        Ok(dump)
    }

    /// Returns immutable source/language metadata parsed from `<siteinfo>`.
    #[must_use]
    pub fn site_info(&self) -> &DumpSiteInfo {
        &self.site_info
    }

    /// Number of page records examined so far, including filtered records.
    #[must_use]
    pub fn pages_examined(&self) -> u64 {
        self.pages_examined
    }

    /// Number of records yielded so far.
    #[must_use]
    pub fn pages_yielded(&self) -> u64 {
        self.pages_yielded
    }

    fn read_site_info(&mut self) -> Result<(), DumpError> {
        let mut builder = SiteInfoBuilder::default();
        let mut namespace: Option<NamespaceBuilder> = None;
        let mut saw_root = false;
        let mut saw_site_info = false;
        loop {
            let event = self.read_event()?;
            match event {
                Event::Decl(declaration) => validate_declaration(&declaration)?,
                Event::DocType(_) => return Err(DumpError::UnsupportedXml("DOCTYPE")),
                Event::Start(start) => {
                    self.validate_start(&start)?;
                    self.reject_nested_scalar_markup()?;
                    self.push_name(start.name().as_ref())?;
                    if let Some(field) = self.siteinfo_scalar_field() {
                        builder.start_field(field)?;
                    }
                    if self.path_is(&[b"mediawiki"]) {
                        if saw_root {
                            return Err(DumpError::InvalidStructure(
                                "dump contains more than one mediawiki root",
                            ));
                        }
                        saw_root = true;
                        builder.export_schema = Some(required_attribute(
                            &start,
                            b"xmlns",
                            "mediawiki export schema",
                            self.limits.max_metadata_field_bytes,
                        )?);
                        builder.export_version = Some(required_attribute(
                            &start,
                            b"version",
                            "mediawiki export version",
                            self.limits.max_metadata_field_bytes,
                        )?);
                        builder.language_code = Some(required_attribute(
                            &start,
                            b"xml:lang",
                            "mediawiki xml:lang",
                            self.limits.max_metadata_field_bytes,
                        )?);
                    } else if self.path_is(&[b"mediawiki", b"siteinfo"]) {
                        saw_site_info = true;
                    } else if self.path_is(&[
                        b"mediawiki",
                        b"siteinfo",
                        b"namespaces",
                        b"namespace",
                    ]) {
                        if namespace.is_some() {
                            return Err(DumpError::InvalidStructure(
                                "nested namespace declaration",
                            ));
                        }
                        namespace = Some(NamespaceBuilder {
                            key: required_attribute(
                                &start,
                                b"key",
                                "namespace key",
                                self.limits.max_metadata_field_bytes,
                            )?,
                            case_rule: required_attribute(
                                &start,
                                b"case",
                                "namespace case",
                                self.limits.max_metadata_field_bytes,
                            )?,
                            name: String::new(),
                        });
                    } else if self.path_is(&[b"mediawiki", b"page"]) {
                        return Err(DumpError::InvalidStructure(
                            "page appeared before siteinfo ended",
                        ));
                    }
                }
                Event::Empty(empty) => {
                    self.validate_start(&empty)?;
                    self.reject_nested_scalar_markup()?;
                    let name = empty.name();
                    self.push_name(name.as_ref())?;
                    if let Some(field) = self.siteinfo_scalar_field() {
                        builder.start_field(field)?;
                    }
                    if self.path_is(&[b"mediawiki", b"siteinfo", b"namespaces", b"namespace"]) {
                        if builder.namespaces.len() == self.limits.max_namespaces {
                            return Err(DumpError::NamespaceLimitExceeded {
                                limit: self.limits.max_namespaces,
                            });
                        }
                        builder.namespaces.push(
                            NamespaceBuilder {
                                key: required_attribute(
                                    &empty,
                                    b"key",
                                    "namespace key",
                                    self.limits.max_metadata_field_bytes,
                                )?,
                                case_rule: required_attribute(
                                    &empty,
                                    b"case",
                                    "namespace case",
                                    self.limits.max_metadata_field_bytes,
                                )?,
                                name: String::new(),
                            }
                            .finish()?,
                        );
                    } else if self.path_is(&[b"mediawiki", b"page"]) {
                        return Err(DumpError::InvalidStructure(
                            "page appeared before siteinfo ended",
                        ));
                    }
                    self.pop_name(name.as_ref())?;
                }
                Event::Text(text) => {
                    let value = decoded_text(&text)?;
                    if self.path_is(&[b"mediawiki", b"siteinfo", b"dbname"]) {
                        append_bounded(
                            &mut builder.database_name,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "database name",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"base"]) {
                        append_optional_bounded(
                            &mut builder.base_url,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "base URL",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"generator"]) {
                        append_optional_bounded(
                            &mut builder.generator,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "generator",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"case"]) {
                        append_bounded(
                            &mut builder.case_rule,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "site case rule",
                        )?;
                    } else if self.path_is(&[
                        b"mediawiki",
                        b"siteinfo",
                        b"namespaces",
                        b"namespace",
                    ]) {
                        append_bounded(
                            &mut namespace
                                .as_mut()
                                .ok_or(DumpError::InvalidStructure(
                                    "namespace text appeared outside a declaration",
                                ))?
                                .name,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "namespace name",
                        )?;
                    }
                }
                Event::CData(cdata) => {
                    drop(decoded_cdata(&cdata)?);
                    return Err(DumpError::InvalidStructure(
                        "CDATA is not accepted in siteinfo",
                    ));
                }
                Event::Comment(comment) => validate_comment(&comment)?,
                Event::PI(_) => return Err(DumpError::UnsupportedXml("processing instruction")),
                Event::End(end) => {
                    let closing_namespace =
                        self.path_is(&[b"mediawiki", b"siteinfo", b"namespaces", b"namespace"]);
                    let closing_site_info = self.path_is(&[b"mediawiki", b"siteinfo"]);
                    self.pop_name(end.name().as_ref())?;
                    if closing_namespace {
                        if builder.namespaces.len() == self.limits.max_namespaces {
                            return Err(DumpError::NamespaceLimitExceeded {
                                limit: self.limits.max_namespaces,
                            });
                        }
                        builder.namespaces.push(
                            namespace
                                .take()
                                .ok_or(DumpError::InvalidStructure(
                                    "namespace ended without a declaration",
                                ))?
                                .finish()?,
                        );
                    }
                    if closing_site_info {
                        if !saw_root || !saw_site_info {
                            return Err(DumpError::InvalidStructure(
                                "dump omitted mediawiki/siteinfo",
                            ));
                        }
                        self.site_info = builder.finish()?;
                        return Ok(());
                    }
                }
                Event::Eof => {
                    return Err(DumpError::InvalidStructure(
                        "dump ended before siteinfo was complete",
                    ));
                }
                Event::GeneralRef(reference) => {
                    let value = decoded_reference(&reference)?;
                    if self.path_is(&[b"mediawiki", b"siteinfo", b"dbname"]) {
                        append_bounded(
                            &mut builder.database_name,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "database name",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"base"]) {
                        append_optional_bounded(
                            &mut builder.base_url,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "base URL",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"generator"]) {
                        append_optional_bounded(
                            &mut builder.generator,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "generator",
                        )?;
                    } else if self.path_is(&[b"mediawiki", b"siteinfo", b"case"]) {
                        append_bounded(
                            &mut builder.case_rule,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "site case rule",
                        )?;
                    } else if self.path_is(&[
                        b"mediawiki",
                        b"siteinfo",
                        b"namespaces",
                        b"namespace",
                    ]) {
                        append_bounded(
                            &mut namespace
                                .as_mut()
                                .ok_or(DumpError::InvalidStructure(
                                    "namespace reference appeared outside a declaration",
                                ))?
                                .name,
                            &value,
                            self.limits.max_metadata_field_bytes,
                            "namespace name",
                        )?;
                    } else {
                        return Err(DumpError::UnsupportedXml(
                            "entity reference outside a supported siteinfo field",
                        ));
                    }
                }
            }
            if self.reader.buffer_position() > self.limits.max_siteinfo_bytes {
                return Err(DumpError::SiteInfoTooLarge {
                    limit: self.limits.max_siteinfo_bytes,
                });
            }
        }
    }

    fn next_page(&mut self) -> Result<Option<DumpPage>, DumpError> {
        loop {
            let event = self.read_event()?;
            self.check_page_size()?;
            match event {
                Event::Decl(_) => {
                    return Err(DumpError::InvalidStructure(
                        "XML declaration appeared after siteinfo",
                    ));
                }
                Event::DocType(_) => return Err(DumpError::UnsupportedXml("DOCTYPE")),
                Event::Start(start) => {
                    self.validate_start(&start)?;
                    self.reject_nested_scalar_markup()?;
                    self.reject_deleted_contributor_child()?;
                    self.push_name(start.name().as_ref())?;
                    if self.path.len() > 4
                        && self.path_has_prefix(&[b"mediawiki", b"page", b"revision", b"text"])
                    {
                        return Err(DumpError::InvalidStructure(
                            "revision text contained a nested XML element",
                        ));
                    }
                    if self.path_is(&[b"mediawiki", b"page"]) {
                        if self.page.is_some() {
                            return Err(DumpError::InvalidStructure("nested page element"));
                        }
                        self.page_start = self.last_event_start;
                        self.page = Some(PageBuilder::default());
                    } else if self.path_is(&[b"mediawiki", b"page", b"revision"]) {
                        self.current_page_mut()?.start_revision()?;
                    } else {
                        self.mark_page_scalar_field()?;
                    }
                    if self.path_is(&[b"mediawiki", b"page", b"revision", b"minor"]) {
                        self.current_revision_mut()?.minor = true;
                    } else if self.path_is(&[b"mediawiki", b"page", b"redirect"]) {
                        let target = required_attribute(
                            &start,
                            b"title",
                            "redirect title",
                            self.limits.max_metadata_field_bytes,
                        )?;
                        self.current_page_mut()?.redirect_title = Some(target);
                    } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"text"]) {
                        self.start_text(&start)?;
                    } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor"]) {
                        let deleted = has_attribute(&start, b"deleted")?;
                        self.current_revision_mut()?.start_contributor(deleted)?;
                    } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"comment"])
                        && has_attribute(&start, b"deleted")?
                    {
                        self.current_revision_mut()?.comment_deleted = true;
                    }
                }
                Event::Empty(empty) => {
                    self.validate_start(&empty)?;
                    self.handle_empty(&empty)?;
                }
                Event::Text(text) => {
                    let value = decoded_text(&text)?;
                    self.handle_page_text(&value)?;
                }
                Event::CData(cdata) => {
                    let value = decoded_cdata(&cdata)?;
                    self.handle_page_text(&value)?;
                }
                Event::Comment(comment) => validate_comment(&comment)?,
                Event::PI(_) => return Err(DumpError::UnsupportedXml("processing instruction")),
                Event::End(end) => {
                    let closing_revision = self.path_is(&[b"mediawiki", b"page", b"revision"]);
                    let closing_page = self.path_is(&[b"mediawiki", b"page"]);
                    let closing_root = self.path_is(&[b"mediawiki"]);
                    self.pop_name(end.name().as_ref())?;
                    if closing_revision {
                        self.current_page_mut()?.finish_revision()?;
                    }
                    if closing_page {
                        self.pages_examined = self.pages_examined.checked_add(1).ok_or(
                            DumpError::PageLimitExceeded {
                                limit: self.limits.max_pages,
                            },
                        )?;
                        if self.pages_examined > self.limits.max_pages {
                            return Err(DumpError::PageLimitExceeded {
                                limit: self.limits.max_pages,
                            });
                        }
                        let page = self
                            .page
                            .take()
                            .ok_or(DumpError::InvalidStructure(
                                "page ended without a page record",
                            ))?
                            .finish(self.limits)?;
                        if !self
                            .site_info
                            .namespaces
                            .iter()
                            .any(|namespace| namespace.key == page.namespace)
                        {
                            return Err(DumpError::InvalidField(
                                "page namespace absent from siteinfo",
                            ));
                        }
                        let model = page
                            .revision
                            .metadata
                            .content_model
                            .as_deref()
                            .expect("dump revision construction requires a content model");
                        if self.filter.accepts(page.namespace, model) {
                            self.pages_yielded = self.pages_yielded.saturating_add(1);
                            return Ok(Some(page));
                        }
                    }
                    if closing_root {
                        self.document_ended = true;
                    }
                }
                Event::Eof => {
                    if self.page.is_some() || !self.document_ended || !self.path.is_empty() {
                        return Err(DumpError::InvalidStructure(
                            "dump ended before the mediawiki document was complete",
                        ));
                    }
                    return Ok(None);
                }
                Event::GeneralRef(reference) => {
                    let value = decoded_reference(&reference)?;
                    self.handle_page_text(&value)?;
                }
            }
        }
    }

    fn handle_empty(&mut self, empty: &BytesStart<'_>) -> Result<(), DumpError> {
        let name = empty.name();
        self.reject_nested_scalar_markup()?;
        self.reject_deleted_contributor_child()?;
        self.push_name(name.as_ref())?;
        if self.path.len() > 4
            && self.path_has_prefix(&[b"mediawiki", b"page", b"revision", b"text"])
        {
            return Err(DumpError::InvalidStructure(
                "revision text contained a nested XML element",
            ));
        }
        self.mark_page_scalar_field()?;
        if self.path_is(&[b"mediawiki", b"page", b"redirect"]) {
            let target = required_attribute(
                empty,
                b"title",
                "redirect title",
                self.limits.max_metadata_field_bytes,
            )?;
            self.current_page_mut()?.redirect_title = Some(target);
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"minor"]) {
            self.current_revision_mut()?.minor = true;
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"text"]) {
            self.start_text(empty)?;
            let deleted = has_attribute(empty, b"deleted")?;
            if !deleted {
                self.current_revision_mut()?
                    .source
                    .get_or_insert_with(Vec::new);
            }
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor"]) {
            let deleted = has_attribute(empty, b"deleted")?;
            self.current_revision_mut()?.start_contributor(deleted)?;
            if !deleted {
                return Err(DumpError::InvalidStructure(
                    "empty contributor must be marked deleted",
                ));
            }
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"comment"])
            && has_attribute(empty, b"deleted")?
        {
            self.current_revision_mut()?.comment_deleted = true;
        }
        self.pop_name(name.as_ref())
    }

    fn reject_nested_scalar_markup(&self) -> Result<(), DumpError> {
        let scalar = self.path_is(&[b"mediawiki", b"siteinfo", b"dbname"])
            || self.path_is(&[b"mediawiki", b"siteinfo", b"base"])
            || self.path_is(&[b"mediawiki", b"siteinfo", b"generator"])
            || self.path_is(&[b"mediawiki", b"siteinfo", b"case"])
            || self.path_is(&[b"mediawiki", b"siteinfo", b"namespaces", b"namespace"])
            || self.path_is(&[b"mediawiki", b"page", b"title"])
            || self.path_is(&[b"mediawiki", b"page", b"ns"])
            || self.path_is(&[b"mediawiki", b"page", b"id"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"id"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"parentid"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"timestamp"])
            || self.path_is(&[
                b"mediawiki",
                b"page",
                b"revision",
                b"contributor",
                b"username",
            ])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor", b"ip"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor", b"id"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"comment"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"model"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"format"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"sha1"])
            || self.path_is(&[b"mediawiki", b"page", b"revision", b"text"]);
        if scalar {
            return Err(DumpError::InvalidStructure(
                "scalar dump metadata contained nested markup",
            ));
        }
        Ok(())
    }

    fn siteinfo_scalar_field(&self) -> Option<&'static str> {
        if self.path_is(&[b"mediawiki", b"siteinfo", b"dbname"]) {
            Some("database name")
        } else if self.path_is(&[b"mediawiki", b"siteinfo", b"base"]) {
            Some("base URL")
        } else if self.path_is(&[b"mediawiki", b"siteinfo", b"generator"]) {
            Some("generator")
        } else if self.path_is(&[b"mediawiki", b"siteinfo", b"case"]) {
            Some("site case rule")
        } else {
            None
        }
    }

    fn reject_deleted_contributor_child(&mut self) -> Result<(), DumpError> {
        if self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor"])
            && self.current_revision_mut()?.contributor_deleted
        {
            return Err(DumpError::InvalidStructure(
                "deleted contributor contained identity fields",
            ));
        }
        Ok(())
    }

    fn mark_page_scalar_field(&mut self) -> Result<(), DumpError> {
        if self.path_is(&[b"mediawiki", b"page", b"title"]) {
            self.current_page_mut()?.start_field("page title")
        } else if self.path_is(&[b"mediawiki", b"page", b"ns"]) {
            self.current_page_mut()?.start_field("page namespace")
        } else if self.path_is(&[b"mediawiki", b"page", b"id"]) {
            self.current_page_mut()?.start_field("page ID")
        } else if self.path_is(&[b"mediawiki", b"page", b"redirect"]) {
            self.current_page_mut()?.start_field("page redirect")
        } else {
            let field = if self.path_is(&[b"mediawiki", b"page", b"revision", b"id"]) {
                Some("revision ID")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"parentid"]) {
                Some("parent revision ID")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"timestamp"]) {
                Some("revision timestamp")
            } else if self.path_is(&[
                b"mediawiki",
                b"page",
                b"revision",
                b"contributor",
                b"username",
            ]) || self.path_is(&[
                b"mediawiki",
                b"page",
                b"revision",
                b"contributor",
                b"ip",
            ]) {
                Some("revision contributor identity")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor", b"id"]) {
                Some("contributor ID")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"minor"]) {
                Some("minor marker")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"comment"]) {
                Some("revision comment")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"model"]) {
                Some("content model")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"format"]) {
                Some("content format")
            } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"sha1"]) {
                Some("revision SHA-1")
            } else {
                None
            };
            if let Some(field) = field {
                self.current_revision_mut()?.start_field(field)?;
            }
            Ok(())
        }
    }

    fn start_text(&mut self, start: &BytesStart<'_>) -> Result<(), DumpError> {
        let deleted = has_attribute(start, b"deleted")?;
        let declared_bytes = optional_attribute(
            start,
            b"bytes",
            "text byte count",
            self.limits.max_metadata_field_bytes,
        )?
        .map(|value| parse_u64(&value, "text byte count"))
        .transpose()?;
        if declared_bytes.is_some_and(|bytes| bytes > self.limits.max_text_bytes as u64) {
            return Err(DumpError::TextTooLarge {
                limit: self.limits.max_text_bytes,
            });
        }
        let revision = self.current_revision_mut()?;
        if revision.saw_text {
            return Err(DumpError::InvalidStructure(
                "current revision contained more than one text element",
            ));
        }
        revision.saw_text = true;
        revision.text_deleted = deleted;
        revision.declared_text_bytes = declared_bytes;
        if !deleted {
            revision.source = Some(Vec::with_capacity(
                declared_bytes
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .unwrap_or(0),
            ));
        }
        Ok(())
    }

    fn handle_page_text(&mut self, value: &str) -> Result<(), DumpError> {
        let limit = self.limits.max_metadata_field_bytes;
        if self.path_is(&[b"mediawiki", b"page", b"title"]) {
            append_bounded(
                &mut self.current_page_mut()?.title,
                value,
                limit,
                "page title",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"ns"]) {
            append_bounded(
                &mut self.current_page_mut()?.namespace,
                value,
                limit,
                "namespace",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"id"]) {
            append_bounded(
                &mut self.current_page_mut()?.page_id,
                value,
                limit,
                "page ID",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"id"]) {
            append_bounded(
                &mut self.current_revision_mut()?.revision_id,
                value,
                limit,
                "revision ID",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"parentid"]) {
            append_optional_bounded(
                &mut self.current_revision_mut()?.parent_id,
                value,
                limit,
                "parent revision ID",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"timestamp"]) {
            append_bounded(
                &mut self.current_revision_mut()?.timestamp,
                value,
                limit,
                "revision timestamp",
            )
        } else if self.path_is(&[
            b"mediawiki",
            b"page",
            b"revision",
            b"contributor",
            b"username",
        ]) || self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor", b"ip"])
        {
            if self.current_revision_mut()?.contributor_deleted {
                return Err(DumpError::InvalidStructure(
                    "deleted contributor contained identity text",
                ));
            }
            append_optional_bounded(
                &mut self.current_revision_mut()?.user,
                value,
                limit,
                "revision contributor",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"contributor", b"id"]) {
            if self.current_revision_mut()?.contributor_deleted {
                return Err(DumpError::InvalidStructure(
                    "deleted contributor contained identity text",
                ));
            }
            append_optional_bounded(
                &mut self.current_revision_mut()?.user_id,
                value,
                limit,
                "contributor ID",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"comment"]) {
            if !self.current_revision_mut()?.comment_deleted {
                append_optional_bounded(
                    &mut self.current_revision_mut()?.comment,
                    value,
                    limit,
                    "revision comment",
                )?;
            }
            Ok(())
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"model"]) {
            append_bounded(
                &mut self.current_revision_mut()?.content_model,
                value,
                limit,
                "content model",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"format"]) {
            append_bounded(
                &mut self.current_revision_mut()?.content_format,
                value,
                limit,
                "content format",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"sha1"]) {
            append_optional_bounded(
                &mut self.current_revision_mut()?.sha1,
                value,
                limit,
                "revision SHA-1",
            )
        } else if self.path_is(&[b"mediawiki", b"page", b"revision", b"text"]) {
            let text_limit = self.limits.max_text_bytes;
            let revision = self.current_revision_mut()?;
            if revision.text_deleted {
                if !value.is_empty() {
                    return Err(DumpError::InvalidStructure(
                        "deleted revision text contained source bytes",
                    ));
                }
                return Ok(());
            }
            let source = revision.source.as_mut().ok_or(DumpError::InvalidStructure(
                "revision text appeared before its text element",
            ))?;
            let next = source
                .len()
                .checked_add(value.len())
                .filter(|length| *length <= text_limit)
                .ok_or(DumpError::TextTooLarge { limit: text_limit })?;
            source.extend_from_slice(value.as_bytes());
            debug_assert_eq!(source.len(), next);
            Ok(())
        } else {
            Ok(())
        }
    }

    fn read_event(&mut self) -> Result<Event<'static>, DumpError> {
        self.event_buffer.clear();
        self.last_event_start = self.reader.buffer_position();
        let result = self.reader.read_event_into(&mut self.event_buffer);
        let position = self.reader.buffer_position();
        self.reader.get_mut().reset_event();
        if position > self.limits.max_decompressed_bytes {
            return Err(DumpError::DecompressedLimitExceeded {
                limit: self.limits.max_decompressed_bytes,
            });
        }
        match result {
            Ok(event) => Ok(event.into_owned()),
            Err(_error) if self.compressed_limit_exceeded.load(Ordering::Acquire) => {
                Err(DumpError::CompressedLimitExceeded {
                    limit: self.limits.max_compressed_bytes,
                })
            }
            Err(_error) if self.decompressed_limit_exceeded.load(Ordering::Acquire) => {
                Err(DumpError::DecompressedLimitExceeded {
                    limit: self.limits.max_decompressed_bytes,
                })
            }
            Err(_) if self.event_limit_exceeded.load(Ordering::Acquire) => {
                Err(DumpError::XmlEventTooLarge {
                    limit: self.limits.max_event_bytes(),
                })
            }
            Err(error) => Err(DumpError::Xml(error)),
        }
    }

    fn validate_start(&self, start: &BytesStart<'_>) -> Result<(), DumpError> {
        if start.name().as_ref().len() > MAX_XML_NAME_BYTES {
            return Err(DumpError::XmlNameTooLong {
                limit: MAX_XML_NAME_BYTES,
            });
        }
        for attribute in start.attributes() {
            let attribute = attribute.map_err(quick_xml::Error::from)?;
            if attribute.key.as_ref().len() > MAX_XML_NAME_BYTES {
                return Err(DumpError::XmlNameTooLong {
                    limit: MAX_XML_NAME_BYTES,
                });
            }
            let value = attribute
                .normalized_value(XmlVersion::Explicit1_0)
                .map_err(DumpError::Xml)?;
            if value.len() > self.limits.max_metadata_field_bytes {
                return Err(DumpError::FieldTooLarge {
                    field: "XML attribute",
                    limit: self.limits.max_metadata_field_bytes,
                });
            }
        }
        Ok(())
    }

    fn push_name(&mut self, name: &[u8]) -> Result<(), DumpError> {
        if self.path.len() == MAX_XML_DEPTH {
            return Err(DumpError::XmlDepthExceeded {
                limit: MAX_XML_DEPTH,
            });
        }
        if name.len() > MAX_XML_NAME_BYTES {
            return Err(DumpError::XmlNameTooLong {
                limit: MAX_XML_NAME_BYTES,
            });
        }
        self.path.push(name.to_vec());
        Ok(())
    }

    fn pop_name(&mut self, name: &[u8]) -> Result<(), DumpError> {
        let opened = self
            .path
            .pop()
            .ok_or(DumpError::InvalidStructure("unexpected closing element"))?;
        if opened != name {
            return Err(DumpError::InvalidStructure(
                "closing element did not match the parser path",
            ));
        }
        Ok(())
    }

    fn path_is(&self, expected: &[&[u8]]) -> bool {
        self.path.len() == expected.len()
            && self
                .path
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.as_slice() == *expected)
    }

    fn path_has_prefix(&self, expected: &[&[u8]]) -> bool {
        self.path.len() >= expected.len()
            && self
                .path
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.as_slice() == *expected)
    }

    fn current_page_mut(&mut self) -> Result<&mut PageBuilder, DumpError> {
        self.page.as_mut().ok_or(DumpError::InvalidStructure(
            "page field appeared outside a page",
        ))
    }

    fn current_revision_mut(&mut self) -> Result<&mut RevisionBuilder, DumpError> {
        self.current_page_mut()?
            .revision_in_progress
            .as_mut()
            .ok_or(DumpError::InvalidStructure(
                "revision field appeared outside a revision",
            ))
    }

    fn check_page_size(&self) -> Result<(), DumpError> {
        if self.page.is_some()
            && self
                .reader
                .buffer_position()
                .saturating_sub(self.page_start)
                > self.limits.max_page_xml_bytes
        {
            return Err(DumpError::PageTooLarge {
                limit: self.limits.max_page_xml_bytes,
            });
        }
        Ok(())
    }
}

impl<R: Read> Iterator for DumpReader<R> {
    type Item = Result<DumpPage, DumpError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || (self.document_ended && self.path.is_empty()) {
            return None;
        }
        match self.next_page() {
            Ok(Some(page)) => Some(Ok(page)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

#[derive(Default)]
struct SiteInfoBuilder {
    export_schema: Option<String>,
    export_version: Option<String>,
    language_code: Option<String>,
    database_name: String,
    base_url: Option<String>,
    generator: Option<String>,
    case_rule: String,
    namespaces: Vec<DumpNamespace>,
    seen_fields: BTreeSet<&'static str>,
}

impl SiteInfoBuilder {
    fn start_field(&mut self, field: &'static str) -> Result<(), DumpError> {
        if !self.seen_fields.insert(field) {
            return Err(DumpError::InvalidStructure(
                "siteinfo contained a duplicate scalar field",
            ));
        }
        Ok(())
    }

    fn finish(self) -> Result<DumpSiteInfo, DumpError> {
        let export_schema = required_nonempty(self.export_schema, "mediawiki export schema")?;
        if !export_schema.starts_with("http://www.mediawiki.org/xml/export-")
            || !export_schema.ends_with('/')
        {
            return Err(DumpError::InvalidField("mediawiki export schema"));
        }
        let export_version = required_nonempty(self.export_version, "mediawiki export version")?;
        validate_short_text(
            &export_version,
            "mediawiki export version",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        let language_code = required_nonempty(self.language_code, "mediawiki xml:lang")?;
        validate_language_code(&language_code)?;
        validate_short_text(
            &self.database_name,
            "database name",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        validate_short_text(
            &self.case_rule,
            "site case rule",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        if self.namespaces.is_empty() {
            return Err(DumpError::MissingField("site namespaces"));
        }
        Ok(DumpSiteInfo {
            export_schema,
            export_version,
            database_name: self.database_name,
            language_code,
            base_url: self.base_url,
            generator: self.generator,
            case_rule: self.case_rule,
            namespaces: self.namespaces,
        })
    }
}

struct NamespaceBuilder {
    key: String,
    case_rule: String,
    name: String,
}

impl NamespaceBuilder {
    fn finish(self) -> Result<DumpNamespace, DumpError> {
        validate_short_text(
            &self.case_rule,
            "namespace case rule",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        Ok(DumpNamespace {
            key: parse_i32(&self.key, "namespace key")?,
            case_rule: self.case_rule,
            name: self.name,
        })
    }
}

#[derive(Default)]
struct PageBuilder {
    title: String,
    namespace: String,
    page_id: String,
    redirect_title: Option<String>,
    revision_in_progress: Option<RevisionBuilder>,
    revision: Option<DumpRevision>,
    seen_fields: BTreeSet<&'static str>,
}

impl PageBuilder {
    fn start_field(&mut self, field: &'static str) -> Result<(), DumpError> {
        if !self.seen_fields.insert(field) {
            return Err(DumpError::InvalidStructure(
                "page contained a duplicate scalar field",
            ));
        }
        Ok(())
    }

    fn start_revision(&mut self) -> Result<(), DumpError> {
        if self.revision_in_progress.is_some() || self.revision.is_some() {
            return Err(DumpError::InvalidStructure(
                "current dump page contained more than one revision",
            ));
        }
        self.revision_in_progress = Some(RevisionBuilder::default());
        Ok(())
    }

    fn finish_revision(&mut self) -> Result<(), DumpError> {
        self.revision = Some(
            self.revision_in_progress
                .take()
                .ok_or(DumpError::InvalidStructure(
                    "revision ended without a revision record",
                ))?
                .finish()?,
        );
        Ok(())
    }

    fn finish(self, limits: DumpLimits) -> Result<DumpPage, DumpError> {
        if self.revision_in_progress.is_some() {
            return Err(DumpError::InvalidStructure(
                "page ended before its revision ended",
            ));
        }
        let title = PageTitle::new(required_string(self.title, "page title")?)
            .map_err(|_| DumpError::InvalidField("page title"))?;
        if title.as_str().len() > limits.max_metadata_field_bytes {
            return Err(DumpError::FieldTooLarge {
                field: "page title",
                limit: limits.max_metadata_field_bytes,
            });
        }
        let page_id = PageId::new(parse_u64(&self.page_id, "page ID")?)
            .map_err(|_| DumpError::InvalidField("page ID"))?;
        let redirect_title = self
            .redirect_title
            .map(PageTitle::new)
            .transpose()
            .map_err(|_| DumpError::InvalidField("redirect title"))?;
        Ok(DumpPage {
            page_id,
            namespace: parse_i32(&self.namespace, "namespace")?,
            title,
            redirect_title,
            revision: self
                .revision
                .ok_or(DumpError::MissingField("current revision"))?,
        })
    }
}

#[derive(Default)]
struct RevisionBuilder {
    revision_id: String,
    parent_id: Option<String>,
    timestamp: String,
    user: Option<String>,
    user_id: Option<String>,
    comment: Option<String>,
    comment_deleted: bool,
    minor: bool,
    content_model: String,
    content_format: String,
    sha1: Option<String>,
    declared_text_bytes: Option<u64>,
    source: Option<Vec<u8>>,
    text_deleted: bool,
    saw_text: bool,
    contributor_deleted: bool,
    seen_fields: BTreeSet<&'static str>,
}

impl RevisionBuilder {
    fn start_field(&mut self, field: &'static str) -> Result<(), DumpError> {
        if !self.seen_fields.insert(field) {
            return Err(DumpError::InvalidStructure(
                "revision contained a duplicate scalar field",
            ));
        }
        Ok(())
    }

    fn start_contributor(&mut self, deleted: bool) -> Result<(), DumpError> {
        self.start_field("revision contributor")?;
        self.contributor_deleted = deleted;
        Ok(())
    }

    fn finish(self) -> Result<DumpRevision, DumpError> {
        let revision_id = RevisionId::new(parse_u64(&self.revision_id, "revision ID")?)
            .map_err(|_| DumpError::InvalidField("revision ID"))?;
        let parent_id = self
            .parent_id
            .as_deref()
            .map(|value| parse_u64(value, "parent revision ID"))
            .transpose()?
            .filter(|value| *value != 0)
            .map(RevisionId::new)
            .transpose()
            .map_err(|_| DumpError::InvalidField("parent revision ID"))?;
        validate_short_text(
            &self.timestamp,
            "revision timestamp",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        validate_short_text(
            &self.content_model,
            "content model",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        validate_short_text(
            &self.content_format,
            "content format",
            DEFAULT_MAX_METADATA_FIELD_BYTES,
        )?;
        let source_size = self.source.as_ref().map(Vec::len);
        if !self.text_deleted && self.source.is_none() {
            return Err(DumpError::MissingField("revision text"));
        }
        if let (Some(declared), Some(actual)) = (self.declared_text_bytes, source_size) {
            if declared != actual as u64 {
                return Err(DumpError::TextSizeMismatch { declared, actual });
            }
        }
        let user_id = if self.contributor_deleted {
            None
        } else {
            self.user_id
                .as_deref()
                .map(|value| parse_u64(value, "contributor ID"))
                .transpose()?
        };
        Ok(DumpRevision {
            metadata: RevisionMetadata {
                revision_id,
                parent_id,
                timestamp: self.timestamp,
                user: if self.contributor_deleted {
                    None
                } else {
                    self.user
                },
                user_id,
                comment: if self.comment_deleted {
                    None
                } else {
                    self.comment
                },
                minor: self.minor,
                size: self
                    .declared_text_bytes
                    .or_else(|| source_size.map(|size| size as u64)),
                sha1: self.sha1,
                content_model: Some(self.content_model),
            },
            content_format: self.content_format,
            source: self.source,
        })
    }
}

struct CompressedBoundedRead<R> {
    inner: R,
    remaining: u64,
    exceeded: Arc<AtomicBool>,
}

impl<R> CompressedBoundedRead<R> {
    fn new(inner: R, limit: u64, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            remaining: limit,
            exceeded,
        }
    }
}

impl<R: Read> Read for CompressedBoundedRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut extra = [0_u8; 1];
            return match self.inner.read(&mut extra)? {
                0 => Ok(0),
                _ => {
                    self.exceeded.store(true, Ordering::Release);
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "compressed dump byte limit exceeded",
                    ))
                }
            };
        }
        let allowed = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

struct DecompressedBoundedRead<R> {
    inner: R,
    remaining: u64,
    exceeded: Arc<AtomicBool>,
}

impl<R> DecompressedBoundedRead<R> {
    fn new(inner: R, limit: u64, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            remaining: limit,
            exceeded,
        }
    }
}

impl<R: Read> Read for DecompressedBoundedRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut extra = [0_u8; 1];
            return match self.inner.read(&mut extra)? {
                0 => Ok(0),
                _ => {
                    self.exceeded.store(true, Ordering::Release);
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "decompressed dump byte limit exceeded",
                    ))
                }
            };
        }
        let allowed = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

struct EventBoundedBufRead<R: Read> {
    inner: BufReader<R>,
    event_remaining: usize,
    event_limit: usize,
    exceeded: Arc<AtomicBool>,
}

impl<R: Read> EventBoundedBufRead<R> {
    fn new(inner: R, event_limit: usize, exceeded: Arc<AtomicBool>) -> Self {
        Self {
            inner: BufReader::with_capacity(64 * 1024, inner),
            event_remaining: event_limit,
            event_limit,
            exceeded,
        }
    }

    fn reset_event(&mut self) {
        self.event_remaining = self.event_limit;
    }
}

impl<R: Read> Read for EventBoundedBufRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let length = available.len().min(buffer.len());
        buffer[..length].copy_from_slice(&available[..length]);
        self.consume(length);
        Ok(length)
    }
}

impl<R: Read> BufRead for EventBoundedBufRead<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.event_remaining == 0 {
            self.exceeded.store(true, Ordering::Release);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "XML event byte limit exceeded",
            ));
        }
        let available = self.inner.fill_buf()?;
        Ok(&available[..available.len().min(self.event_remaining)])
    }

    fn consume(&mut self, amount: usize) {
        let consumed = amount.min(self.event_remaining);
        self.event_remaining -= consumed;
        self.inner.consume(consumed);
    }
}

fn validate_declaration(declaration: &BytesDecl<'_>) -> Result<(), DumpError> {
    if let Some(encoding) = declaration.encoding() {
        let encoding = encoding.map_err(quick_xml::Error::from)?;
        if !encoding.eq_ignore_ascii_case(b"utf-8") && !encoding.eq_ignore_ascii_case(b"utf8") {
            return Err(DumpError::UnsupportedEncoding);
        }
    }
    Ok(())
}

fn validate_comment(comment: &BytesText<'_>) -> Result<(), DumpError> {
    comment.decode().map_err(quick_xml::Error::from)?;
    Ok(())
}

fn decoded_text(text: &BytesText<'_>) -> Result<String, DumpError> {
    let decoded = text.xml10_content().map_err(quick_xml::Error::from)?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(quick_xml::Error::from)
        .map_err(DumpError::Xml)
}

fn decoded_cdata(cdata: &BytesCData<'_>) -> Result<String, DumpError> {
    cdata
        .xml10_content()
        .map(|value| value.into_owned())
        .map_err(quick_xml::Error::from)
        .map_err(DumpError::Xml)
}

fn decoded_reference(reference: &BytesRef<'_>) -> Result<String, DumpError> {
    if let Some(value) = reference.resolve_char_ref().map_err(DumpError::Xml)? {
        if !valid_xml_character(value) {
            return Err(DumpError::InvalidField("XML character reference"));
        }
        return Ok(value.to_string());
    }
    let name = reference.decode().map_err(quick_xml::Error::from)?;
    quick_xml::escape::resolve_predefined_entity(&name)
        .map(str::to_owned)
        .ok_or(DumpError::UnsupportedXml("non-predefined entity reference"))
}

fn valid_xml_character(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{a}' | '\u{d}')
        || ('\u{20}'..='\u{d7ff}').contains(&value)
        || ('\u{e000}'..='\u{fffd}').contains(&value)
        || ('\u{10000}'..='\u{10ffff}').contains(&value)
}

fn optional_attribute(
    start: &BytesStart<'_>,
    key: &[u8],
    field: &'static str,
    limit: usize,
) -> Result<Option<String>, DumpError> {
    let mut found = None;
    for attribute in start.attributes() {
        let attribute = attribute.map_err(quick_xml::Error::from)?;
        if attribute.key.as_ref() == key {
            if found.is_some() {
                return Err(DumpError::InvalidField(field));
            }
            let value = attribute
                .normalized_value(XmlVersion::Explicit1_0)
                .map_err(DumpError::Xml)?
                .into_owned();
            if value.len() > limit {
                return Err(DumpError::FieldTooLarge { field, limit });
            }
            found = Some(value);
        }
    }
    Ok(found)
}

fn required_attribute(
    start: &BytesStart<'_>,
    key: &[u8],
    field: &'static str,
    limit: usize,
) -> Result<String, DumpError> {
    required_nonempty(optional_attribute(start, key, field, limit)?, field)
}

fn has_attribute(start: &BytesStart<'_>, key: &[u8]) -> Result<bool, DumpError> {
    Ok(optional_attribute(
        start,
        key,
        "XML attribute",
        DEFAULT_MAX_METADATA_FIELD_BYTES,
    )?
    .is_some())
}

fn append_bounded(
    target: &mut String,
    value: &str,
    limit: usize,
    field: &'static str,
) -> Result<(), DumpError> {
    target
        .len()
        .checked_add(value.len())
        .filter(|length| *length <= limit)
        .ok_or(DumpError::FieldTooLarge { field, limit })?;
    target.push_str(value);
    Ok(())
}

fn append_optional_bounded(
    target: &mut Option<String>,
    value: &str,
    limit: usize,
    field: &'static str,
) -> Result<(), DumpError> {
    append_bounded(target.get_or_insert_with(String::new), value, limit, field)
}

fn required_nonempty(value: Option<String>, field: &'static str) -> Result<String, DumpError> {
    required_string(value.unwrap_or_default(), field)
}

fn required_string(value: String, field: &'static str) -> Result<String, DumpError> {
    if value.trim().is_empty() {
        Err(DumpError::MissingField(field))
    } else {
        Ok(value)
    }
}

fn validate_short_text(value: &str, field: &'static str, limit: usize) -> Result<(), DumpError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(DumpError::InvalidField(field));
    }
    if value.len() > limit {
        return Err(DumpError::FieldTooLarge { field, limit });
    }
    Ok(())
}

fn validate_language_code(value: &str) -> Result<(), DumpError> {
    if value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(DumpError::InvalidField("mediawiki xml:lang"));
    }
    Ok(())
}

fn parse_u64(value: &str, field: &'static str) -> Result<u64, DumpError> {
    value
        .trim()
        .parse()
        .map_err(|_| DumpError::InvalidField(field))
}

fn parse_i32(value: &str, field: &'static str) -> Result<i32, DumpError> {
    value
        .trim()
        .parse()
        .map_err(|_| DumpError::InvalidField(field))
}

/// A deterministic parse, structure, encoding, or resource-limit failure.
#[derive(Debug)]
pub enum DumpError {
    /// A configured resource ceiling was zero or internally inconsistent.
    InvalidLimit(&'static str),
    /// A namespace/content-model filter was empty or invalid.
    InvalidFilter(&'static str),
    /// Compressed input exceeded its configured ceiling.
    CompressedLimitExceeded { limit: u64 },
    /// Decompressed XML exceeded its configured ceiling.
    DecompressedLimitExceeded { limit: u64 },
    /// The site-information prologue exceeded its configured ceiling.
    SiteInfoTooLarge { limit: u64 },
    /// The number of page records exceeded its configured ceiling.
    PageLimitExceeded { limit: u64 },
    /// One page's decompressed XML exceeded its configured ceiling.
    PageTooLarge { limit: u64 },
    /// One revision's decoded source exceeded its configured ceiling.
    TextTooLarge { limit: usize },
    /// One XML token exceeded the bounded parser buffer.
    XmlEventTooLarge { limit: usize },
    /// XML nesting exceeded the fixed parser depth.
    XmlDepthExceeded { limit: usize },
    /// An element or attribute name exceeded the fixed name bound.
    XmlNameTooLong { limit: usize },
    /// Site metadata declared too many namespaces.
    NamespaceLimitExceeded { limit: usize },
    /// A decoded metadata field exceeded its configured ceiling.
    FieldTooLarge { field: &'static str, limit: usize },
    /// A required field was absent or empty.
    MissingField(&'static str),
    /// A field could not be validated or parsed.
    InvalidField(&'static str),
    /// Declared revision bytes disagreed with decoded source bytes.
    TextSizeMismatch { declared: u64, actual: usize },
    /// XML structure did not match a current-pages dump.
    InvalidStructure(&'static str),
    /// XML features deliberately excluded from the bounded dump subset.
    UnsupportedXml(&'static str),
    /// Only UTF-8 XML dumps are accepted.
    UnsupportedEncoding,
    /// XML parsing or underlying bzip2 I/O failed.
    Xml(quick_xml::Error),
}

impl fmt::Display for DumpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit(name) => write!(formatter, "invalid dump {name}"),
            Self::InvalidFilter(message) => write!(formatter, "invalid dump filter: {message}"),
            Self::CompressedLimitExceeded { limit } => {
                write!(formatter, "compressed dump exceeded the {limit}-byte limit")
            }
            Self::DecompressedLimitExceeded { limit } => {
                write!(
                    formatter,
                    "decompressed dump exceeded the {limit}-byte limit"
                )
            }
            Self::SiteInfoTooLarge { limit } => {
                write!(formatter, "dump siteinfo exceeded the {limit}-byte limit")
            }
            Self::PageLimitExceeded { limit } => {
                write!(formatter, "dump exceeded the {limit}-page limit")
            }
            Self::PageTooLarge { limit } => {
                write!(formatter, "dump page exceeded the {limit}-byte XML limit")
            }
            Self::TextTooLarge { limit } => {
                write!(
                    formatter,
                    "dump revision text exceeded the {limit}-byte limit"
                )
            }
            Self::XmlEventTooLarge { limit } => {
                write!(formatter, "dump XML token exceeded the {limit}-byte limit")
            }
            Self::XmlDepthExceeded { limit } => {
                write!(formatter, "dump XML exceeded the nesting limit of {limit}")
            }
            Self::XmlNameTooLong { limit } => {
                write!(formatter, "dump XML name exceeded the {limit}-byte limit")
            }
            Self::NamespaceLimitExceeded { limit } => {
                write!(
                    formatter,
                    "dump siteinfo exceeded the {limit}-namespace limit"
                )
            }
            Self::FieldTooLarge { field, limit } => {
                write!(formatter, "dump {field} exceeded the {limit}-byte limit")
            }
            Self::MissingField(field) => write!(formatter, "dump omitted required {field}"),
            Self::InvalidField(field) => write!(formatter, "dump contained invalid {field}"),
            Self::TextSizeMismatch { declared, actual } => write!(
                formatter,
                "dump revision declared {declared} text bytes but decoded {actual}"
            ),
            Self::InvalidStructure(message) => {
                write!(formatter, "invalid current-pages dump structure: {message}")
            }
            Self::UnsupportedXml(feature) => {
                write!(formatter, "dump uses unsupported XML feature {feature}")
            }
            Self::UnsupportedEncoding => formatter.write_str("dump XML encoding must be UTF-8"),
            Self::Xml(error) => write!(formatter, "invalid or unreadable dump XML: {error}"),
        }
    }
}

impl Error for DumpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Xml(error) => Some(error),
            _ => None,
        }
    }
}

impl From<quick_xml::Error> for DumpError {
    fn from(error: quick_xml::Error) -> Self {
        Self::Xml(error)
    }
}
