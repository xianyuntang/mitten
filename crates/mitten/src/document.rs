//! Text from PDF, Word (docx), and PowerPoint (pptx) files, for `fetch_url`, `read_file`, and
//! Discord attachments, and the `view_pdf` tool, which shows a PDF's pages to the model as images.
//! The format is told from the file's first bytes, not its name or type.

use std::io::{Cursor, Read};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use rig_core::completion::ToolDefinition;
use rig_core::message::{DocumentSourceKind, Image, ImageMediaType};
use serde_json::{Value, json};
use zip::ZipArchive;

use crate::config::Config;
use crate::fetch::Fetcher;
use crate::files;

/// Largest document read, downloaded or local.
pub const MAX_BYTES: usize = 20_000_000;
/// Most bytes one XML part may unpack to, so a zip bomb can't exhaust memory.
const MAX_XML_BYTES: u64 = 50_000_000;
/// Most PDF pages rendered as images at once.
const MAX_PAGE_IMAGES: usize = 10;
/// Pages of a PDF shown as images up front; `view_pdf` shows the rest on request.
const PREVIEW_PAGES: usize = 3;
/// Longest side of a rendered page in pixels; Anthropic scales larger images down anyway.
const PAGE_PIXELS: f32 = 1568.0;
/// Most PNG bytes rendered from one PDF, so the request stays under the API's size limit.
const MAX_PAGE_BYTES: usize = 15_000_000;

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

pub fn view_tool() -> ToolDefinition {
    ToolDefinition {
        name: "view_pdf".to_owned(),
        description: format!(
            "Look at pages of a PDF as images, up to {MAX_PAGE_IMAGES} per call. Text read from a PDF \
             can be garbled or out of order and leaves out figures, charts, and scanned pages, so \
             view the pages whose layout, tables, or pictures matter. Give exactly one of `path` (a \
             file on the user's machine) or `url` (a public PDF, or an attachment's URL)."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The PDF file to view."},
                "url": {"type": "string", "description": "The PDF to download and view."},
                "first_page": {"type": "integer", "description": "First page, counting from 1. Default 1."},
                "last_page": {
                    "type": "integer",
                    "description": format!("Last page. Default and limit: first_page + {}.", MAX_PAGE_IMAGES - 1),
                },
            },
        }),
    }
}

/// Runs one `view_pdf` call: text for the tool result, and the pages to show the model.
pub async fn view(config: &Config, fetcher: &Fetcher, args: &Value) -> (String, Vec<Image>) {
    match view_pages(config, fetcher, args).await {
        Ok(viewed) => viewed,
        Err(err) => (format!("error: {err:#}"), Vec::new()),
    }
}

async fn view_pages(
    config: &Config,
    fetcher: &Fetcher,
    args: &Value,
) -> Result<(String, Vec<Image>)> {
    let bytes = match (args["path"].as_str(), args["url"].as_str()) {
        (Some(_), None) => files::read_bytes(config, &args["path"]).await?.1,
        (None, Some(url)) => fetcher.download(url.trim()).await?.2,
        _ => bail!("give exactly one of `path` or `url`"),
    };
    if !bytes.starts_with(b"%PDF-") {
        bail!("not a PDF");
    }
    let page = |key: &str| args[key].as_u64().and_then(|n| usize::try_from(n).ok());
    let first = page("first_page").unwrap_or(1).max(1);
    let last = page("last_page")
        .unwrap_or(usize::MAX)
        .min(first.saturating_add(MAX_PAGE_IMAGES - 1));
    if last < first {
        bail!("last_page is before first_page");
    }
    let (total, pngs) = pdf_pages(bytes, first, last).await?;
    if pngs.is_empty() {
        bail!("the PDF has {total} pages; page {first} is past the end");
    }
    let shown = first + pngs.len() - 1;
    let mut text = format!(
        "Pages {first}-{shown} of {total} follow as images, after this message's tool results."
    );
    if shown < last.min(total) {
        text.push_str(&format!(
            " The rest were left out to keep the request small; view from page {} next.",
            shown + 1
        ));
    }
    let images = pngs
        .iter()
        .map(|png| image(png, Some(ImageMediaType::PNG)))
        .collect();
    Ok((text, images))
}

/// The first pages of the PDF in `bytes`, found at `url`, as images, with a note for the model
/// saying how to see the rest. `None` when it can't be rendered.
pub async fn preview(bytes: Vec<u8>, url: &str) -> Option<(String, Vec<Image>)> {
    let (total, pages) = match pdf_pages(bytes, 1, PREVIEW_PAGES).await {
        Ok(rendered) => rendered,
        Err(err) => {
            tracing::warn!("failed to render {url} as images: {err:#}");
            return None;
        }
    };
    if pages.is_empty() {
        return None;
    }
    let note = format!(
        "[This PDF has {total} pages; pages 1-{} are attached as images. Trust the images over the \
         text for layout, tables, and figures. To see other pages, call view_pdf with url {url}]",
        pages.len()
    );
    let images = pages
        .iter()
        .map(|png| image(png, Some(ImageMediaType::PNG)))
        .collect();
    Some((note, images))
}

/// `bytes` as a base64 image for the model.
pub fn image(bytes: &[u8], media_type: Option<ImageMediaType>) -> Image {
    Image {
        data: DocumentSourceKind::Base64(base64::engine::general_purpose::STANDARD.encode(bytes)),
        media_type,
        ..Image::default()
    }
}

/// The PDF's page count and pages `first` to `last` (counting from 1) as PNGs. Stops early at
/// `MAX_PAGE_IMAGES` pages or `MAX_PAGE_BYTES` bytes.
pub async fn pdf_pages(bytes: Vec<u8>, first: usize, last: usize) -> Result<(usize, Vec<Vec<u8>>)> {
    tokio::task::spawn_blocking(move || render_pages(bytes, first, last))
        .await
        .map_err(|_| anyhow!("the PDF could not be rendered"))?
}

fn render_pages(bytes: Vec<u8>, first: usize, last: usize) -> Result<(usize, Vec<Vec<u8>>)> {
    let pdf = Pdf::new(bytes).map_err(|err| anyhow!("failed to open the PDF: {err:?}"))?;
    let cache = RenderCache::new();
    let interpreter = InterpreterSettings::default();
    let pages = pdf.pages();
    let mut pngs = Vec::new();
    let mut size = 0;
    let wanted = pages
        .iter()
        .skip(first.saturating_sub(1))
        .take(last.saturating_sub(first) + 1)
        .take(MAX_PAGE_IMAGES);
    for page in wanted {
        let (width, height) = page.render_dimensions();
        let scale = PAGE_PIXELS / width.max(height).max(1.0);
        let pixmap = hayro::render(
            page,
            &cache,
            &interpreter,
            &RenderSettings::default(),
            &PixmapSettings {
                x_scale: scale,
                y_scale: scale,
                bg_color: WHITE,
            },
        );
        let png = pixmap.into_png().context("failed to encode a PDF page")?;
        size += png.len();
        if size > MAX_PAGE_BYTES {
            break;
        }
        pngs.push(png);
    }
    Ok((pages.len(), pngs))
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

    /// A one-page PDF saying "hello".
    const PDF: &[u8] = b"%PDF-1.4
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R
/Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >> endobj
4 0 obj << >> stream
BT /F1 24 Tf 20 40 Td (hello) Tj ET
endstream endobj
trailer << /Root 1 0 R >>
%%EOF";

    #[tokio::test]
    async fn renders_pdf_pages_as_png() {
        let (total, pages) = pdf_pages(PDF.to_vec(), 1, 5).await.expect("render");
        assert_eq!((total, pages.len()), (1, 1));
        assert!(pages[0].starts_with(b"\x89PNG"));
        let (_, past) = pdf_pages(PDF.to_vec(), 2, 3).await.expect("render");
        assert!(past.is_empty());
        assert!(pdf_pages(b"%PDF-1.4 broken".to_vec(), 1, 1).await.is_err());
    }

    #[tokio::test]
    async fn view_pdf_shows_requested_pages_from_a_file() {
        let path = std::env::temp_dir().join(format!("mitten-view-{}.pdf", std::process::id()));
        std::fs::write(&path, PDF).expect("write");
        let config = Config::parse("[opencode-go]\napi_key = \"k\"\n").expect("valid");
        let fetcher = Fetcher::new().expect("fetcher");
        let view = |args: Value| {
            let (config, fetcher) = (&config, &fetcher);
            async move { super::view(config, fetcher, &args).await }
        };

        let (text, images) = view(json!({ "path": path })).await;
        assert_eq!(images.len(), 1, "{text}");
        assert!(text.starts_with("Pages 1-1 of 1"), "{text}");
        let (text, images) = view(json!({ "path": path, "first_page": 2 })).await;
        assert!(text.contains("past the end") && images.is_empty(), "{text}");
        let (text, _) = view(json!({ "path": path, "first_page": 3, "last_page": 2 })).await;
        assert!(text.starts_with("error:"), "{text}");
        let (text, _) = view(json!({})).await;
        assert!(text.starts_with("error:"), "{text}");
        std::fs::remove_file(&path).expect("clean up");
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
