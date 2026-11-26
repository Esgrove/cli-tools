//! Finvoice XML decoding and extraction of statement dates and transaction text.

use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use encoding_rs::Encoding;
use jiff::{ZonedDateTime, civil::Date};
use regex::bytes::Regex;
use roxmltree::{Document, Node, ParsingOptions};

use super::RE_ITEM_DATE;

static RE_XML_ENCODING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<\?xml[ \t\r\n]+[^?]*[ \t\r\n]encoding[ \t\r\n]*=[ \t\r\n]*["']([A-Za-z0-9._-]+)["']"#)
        .expect("valid XML encoding declaration pattern")
});

/// Year and month used to assign years to purchases, which carry only a day and month.
/// A purchase in a later month than the statement month belongs to the previous year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StatementMonth {
    pub(super) year: i32,
    pub(super) month: i32,
}

/// Read transaction text and the statement month from a Finvoice document.
/// The invoicing period end takes precedence over the invoice date and start date,
/// with December of the current year as a fallback.
pub(super) fn read_xml_file(file: &Path) -> Result<(Vec<String>, StatementMonth)> {
    let bytes = std::fs::read(file).with_context(|| format!("Failed to read XML file: {}", file.display()))?;
    let xml = decode_xml(&bytes).with_context(|| format!("Failed to decode XML file: {}", file.display()))?;
    // Accept Finvoice DTD declarations without resolving any external entities.
    let document = Document::parse_with_options(
        &xml,
        ParsingOptions {
            allow_dtd: true,
            ..ParsingOptions::default()
        },
    )
    .with_context(|| format!("Failed to parse XML file: {}", file.display()))?;

    let mut rows = Vec::new();
    let mut start_month = None;
    let mut invoice_month = None;
    let mut period_end_month = None;
    for element in document.descendants().filter(Node::is_element) {
        match element.tag_name().name() {
            "StartDate" => start_month = statement_month(element).or(start_month),
            "InvoiceDate" => invoice_month = statement_month(element).or(invoice_month),
            "InvoicingPeriodEndDate" => period_end_month = statement_month(element).or(period_end_month),
            "SpecificationFreeText" => {
                let text = element_text(element);
                let row = text.trim();
                if RE_ITEM_DATE.is_match(row) {
                    rows.push(row.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok((
        rows,
        period_end_month
            .or(invoice_month)
            .or(start_month)
            .unwrap_or_else(default_statement_month),
    ))
}

/// Decode the XML using its byte order mark or declaration when present.
/// Unmarked input defaults to UTF-8, with a Latin-9 fallback for legacy statements.
fn decode_xml(bytes: &[u8]) -> Result<Cow<'_, str>> {
    let (encoding, content) = if let Some((encoding, mark_len)) = Encoding::for_bom(bytes) {
        (encoding, bytes.get(mark_len..).context("Invalid XML byte order mark")?)
    } else if bytes.starts_with(b"<\0?\0") {
        (encoding_rs::UTF_16LE, bytes)
    } else if bytes.starts_with(b"\0<\0?") {
        (encoding_rs::UTF_16BE, bytes)
    } else if let Some(label) = RE_XML_ENCODING.captures(bytes).and_then(|captures| captures.get(1)) {
        let encoding = Encoding::for_label_no_replacement(label.as_bytes()).with_context(|| {
            format!(
                "Unsupported XML encoding: {}",
                String::from_utf8_lossy(label.as_bytes())
            )
        })?;
        (encoding, bytes)
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(Cow::Borrowed(text));
    } else {
        (encoding_rs::ISO_8859_15, bytes)
    };
    encoding
        .decode_without_bom_handling_and_without_replacement(content)
        .with_context(|| format!("Invalid {} text", encoding.name()))
}

/// Collect decoded text and CDATA without collapsing the padding that separates purchase columns.
fn element_text(element: Node<'_, '_>) -> String {
    element
        .children()
        .filter(Node::is_text)
        .filter_map(|child| child.text())
        .collect()
}

/// Parse a Finvoice date in year, month, and day order.
fn statement_month(element: Node<'_, '_>) -> Option<StatementMonth> {
    if element.attribute("Format") != Some("CCYYMMDD") {
        return None;
    }
    let date = Date::strptime("%Y%m%d", element_text(element).trim()).ok()?;
    Some(StatementMonth {
        year: i32::from(date.year()),
        month: i32::from(date.month()),
    })
}

/// Default to December so all purchases are assigned to the current year.
fn default_statement_month() -> StatementMonth {
    StatementMonth {
        year: i32::from(ZonedDateTime::now().year()),
        month: 12,
    }
}
