//! `read` of a PDF: extracted text, bounded and reported.

use super::super::*;

/// A minimal PDF with one text line per page, written byte by byte so the
/// oracle does not depend on the parser under test.
fn pdf_with_pages(texts: &[&str]) -> Vec<u8> {
    let n = texts.len();
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Count {n} /Kids [{}] >>",
            (0..n)
                .map(|i| format!("{} 0 R", 4 + 2 * i))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    for (i, text) in texts.iter().enumerate() {
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

// Catches: a PDF is returned as raw bytes (lossy-decoded binary) instead of its text.
#[test]
fn read_of_a_pdf_returns_its_extracted_text() {
    let ws = populated();
    fs::write(
        ws.root.join("spec.pdf"),
        pdf_with_pages(&["First page marker", "Second page marker"]),
    )
    .unwrap();
    let text = ws
        .call("read", json!({"files": [{"path": "spec.pdf"}]}))
        .unwrap();
    assert!(text.contains("First page marker"), "{text}");
    assert!(text.contains("Second page marker"), "{text}");
    assert!(!text.contains("%PDF"), "{text}");
}

// Catches: extra pages are silently dropped, so the model thinks it saw the whole document.
#[test]
fn read_of_a_pdf_over_the_page_limit_reports_the_pages_it_left_out() {
    let ws = populated();
    let texts: Vec<String> = (1..=101).map(|i| format!("Page{i}x")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    fs::write(ws.root.join("long.pdf"), pdf_with_pages(&refs)).unwrap();
    let mut all = String::new();
    let mut offset = 0u64;
    loop {
        let page = ws
            .call(
                "read",
                json!({"files": [{"path": "long.pdf", "offset": offset}]}),
            )
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&page).unwrap();
        let item = &result["files"][0];
        all.push_str(item["content"].as_str().unwrap());
        match item["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert!(all.contains("Page100x"), "{all}");
    assert!(!all.contains("Page101x"), "{all}");
    assert!(all.contains("101 pages") && all.contains("100"), "{all}");
}

// Catches: a PDF over the size limit is parsed anyway, or refused without a size and limit.
#[test]
fn read_of_an_oversized_pdf_is_refused_naming_size_and_limit() {
    let ws = populated();
    let mut bytes = pdf_with_pages(&["x"]);
    bytes.resize(20 * 1024 * 1024 + 1, b' ');
    fs::write(ws.root.join("huge.pdf"), bytes).unwrap();
    let message = ws
        .call("read", json!({"files": [{"path": "huge.pdf"}]}))
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&message).unwrap();
    let message = result["files"][0]["error"].as_str().unwrap();
    assert!(
        message.contains("20971521") && message.contains("20971520"),
        "{message}"
    );
}

// Catches: a malformed PDF crashes the process or returns binary garbage as text.
#[test]
fn read_of_a_malformed_pdf_is_a_typed_error() {
    let ws = populated();
    fs::write(ws.root.join("bad.pdf"), b"%PDF-1.4\nthis is not a pdf").unwrap();
    let result = ws
        .call("read", json!({"files": [{"path": "bad.pdf"}]}))
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert!(
        result["files"][0]["error"]
            .as_str()
            .unwrap()
            .contains("PDF")
    );
}

// Catches: PDF detection triggers on the extension, so a text file named .pdf is mangled.
#[test]
fn read_of_a_text_file_named_pdf_stays_text() {
    let ws = populated();
    fs::write(ws.root.join("notes.pdf"), "plain notes\n").unwrap();
    let result = ws
        .call("read", json!({"files": [{"path": "notes.pdf"}]}))
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["files"][0]["content"], "plain notes\n");
}
