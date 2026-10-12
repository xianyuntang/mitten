//! Text from PDF, Word (docx), and PowerPoint (pptx) files, for `fetch_url`, `read_file`, and
//! Discord attachments. The format is told from the file's first bytes, not its name or type.

use std::io::{Cursor, Read};

use anyhow::{Context, Result, anyhow, bail};
use zip::ZipArchive;

/// Largest document read, downloaded or local.
pub const MAX_BYTES: usize = 20_000_000;
/// Most bytes one XML part may unpack to, so a zip bomb can't exhaust memory.
const MAX_XML_BYTES: u64 = 50_000_000;

/// Whether `bytes` look like a PDF or an Office zip that `extract` may read.
pub fn is_document(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-") || bytes.starts_with(b"PK\x03\x04")
}

/// Whether a file named `name` is probably a document, before it is downloaded.
pub fn has_document_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [".pdf", ".docx", ".pptx"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// The document's text. Parsing runs on a blocking thread, and a parser panic on a malformed
/// file becomes an error.
pub async fn extract(bytes: Vec<u8>) -> Result<String> {
    tokio::task::spawn_blocking(move || extract_now(&bytes))
        .await
        .map_err(|_| anyhow!("the document could not be parsed"))?
}

fn extract_now(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(b"%PDF-") {
        return pdf(bytes);
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).context("not a PDF, docx, or pptx file")?;
    if archive.by_name("word/document.xml").is_ok() {
        let xml = part(&mut archive, "word/document.xml")?;
        return Ok(xml_text(&xml, "w:t", "w:p"));
    }
    let mut slides: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(|name| {
            let name = name.ok()?;
            let n = name
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((n, name.into_owned()))
        })
        .collect();
    if slides.is_empty() {
        bail!("only PDF, docx, and pptx documents can be read");
    }
    slides.sort();
    let mut out = String::new();
    for (n, name) in slides {
        let xml = part(&mut archive, &name)?;
        out.push_str(&format!(
            "--- slide {n} ---\n{}\n",
            xml_text(&xml, "a:t", "a:p")
        ));
    }
    Ok(out)
}

fn pdf(bytes: &[u8]) -> Result<String> {
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes)
        .map_err(|err| anyhow!("failed to read the PDF: {err}"))?;
    let mut out = String::new();
    for (index, text) in pages.iter().enumerate() {
        out.push_str(&format!("--- page {} ---\n{}\n", index + 1, text.trim()));
    }
    Ok(out)
}

/// One XML part of an Office zip, read up to `MAX_XML_BYTES`.
fn part(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<String> {
    let mut xml = String::new();
    archive
        .by_name(name)
        .with_context(|| format!("the document has no {name}"))?
        .take(MAX_XML_BYTES)
        .read_to_string(&mut xml)
        .with_context(|| format!("failed to unpack {name}"))?;
    Ok(xml)
}

/// The character data inside `text` elements, a line per `paragraph` element.
// ponytail: a tag scanner, not an XML parser; enough for Office's machine-written markup.
fn xml_text(xml: &str, text: &str, paragraph: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        if inside {
            out.push_str(&unescape(&rest[..start]));
        }
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start + 1..start + len];
        rest = &rest[start + len + 1..];
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        if name == text {
            inside = !closing && !tag.ends_with('/');
        } else if name == paragraph && closing {
            out.push('\n');
        } else if name == "w:tab" && !closing {
            out.push('\t');
        } else if matches!(name, "w:br" | "a:br") {
            out.push('\n');
        }
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn office(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .expect("start file");
            zip.write_all(body.as_bytes()).expect("write file");
        }
        zip.finish().expect("finish zip").into_inner()
    }

    #[tokio::test]
    async fn reads_docx_paragraphs_and_pptx_slides_in_order() {
        let docx = office(&[(
            "word/document.xml",
            r#"<w:document><w:body><w:p><w:r><w:t>價格</w:t></w:r><w:r><w:tab/><w:t xml:space="preserve"> A &amp; B</w:t></w:r></w:p><w:p><w:r><w:t>end</w:t></w:r></w:p></w:body></w:document>"#,
        )]);
        assert!(is_document(&docx));
        assert_eq!(extract(docx).await.expect("docx"), "價格\t A & B\nend\n");

        let pptx = office(&[
            (
                "ppt/slides/slide10.xml",
                "<p:sld><a:p><a:r><a:t>ten</a:t></a:r></a:p></p:sld>",
            ),
            (
                "ppt/slides/slide2.xml",
                "<p:sld><a:p><a:r><a:t>two</a:t></a:r></a:p></p:sld>",
            ),
        ]);
        assert_eq!(
            extract(pptx).await.expect("pptx"),
            "--- slide 2 ---\ntwo\n\n--- slide 10 ---\nten\n\n"
        );
    }

    #[tokio::test]
    async fn refuses_other_zips_and_garbage() {
        assert!(
            extract(office(&[("xl/workbook.xml", "<x/>")]))
                .await
                .is_err()
        );
        assert!(extract(b"%PDF-1.4 broken".to_vec()).await.is_err());
        assert!(!is_document(b"<html>"));
        assert!(has_document_name("Report.PDF"));
        assert!(!has_document_name("notes.txt"));
    }
}
