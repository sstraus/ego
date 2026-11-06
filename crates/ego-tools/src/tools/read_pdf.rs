//! PDF text for `read`: the extracted text, bounded and reported.

use ego_core::permit::ToolError;

/// Largest PDF file `read` parses.
pub(super) const MAX_PDF_BYTES: usize = 20 * 1024 * 1024;
/// Most pages `read` extracts from one PDF.
pub(super) const MAX_PDF_PAGES: usize = 100;

/// Content, not extension, decides: a text file named `.pdf` stays text.
pub(super) fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// The text of up to [`MAX_PDF_PAGES`] pages, with a trailing note when
/// pages were left out or had no extractable text.
pub(super) fn extract_text(bytes: &[u8]) -> Result<String, ToolError> {
    if bytes.len() > MAX_PDF_BYTES {
        return Err(ToolError::InvalidParams(format!(
            "PDF is {} bytes; read parses a PDF of at most {MAX_PDF_BYTES} bytes",
            bytes.len()
        )));
    }
    let document = lopdf::Document::load_mem(bytes)
        .map_err(|error| ToolError::InvalidParams(format!("cannot read PDF: {error}")))?;
    let numbers: Vec<u32> = document.get_pages().keys().copied().collect();
    let total = numbers.len();
    let mut text = String::new();
    let mut unreadable = 0usize;
    for number in numbers.iter().take(MAX_PDF_PAGES) {
        match document.extract_text(&[*number]) {
            Ok(page) => {
                text.push_str(page.trim_end());
                text.push('\n');
            }
            Err(_) => unreadable += 1,
        }
    }
    if total > MAX_PDF_PAGES {
        text.push_str(&format!(
            "\n[PDF has {total} pages; read extracts the first {MAX_PDF_PAGES}]\n"
        ));
    }
    if unreadable > 0 {
        text.push_str(&format!(
            "\n[{unreadable} PDF page(s) had no extractable text]\n"
        ));
    }
    if total == 0 {
        return Err(ToolError::InvalidParams("PDF has no pages".to_owned()));
    }
    Ok(text)
}
