//! `POST /convert?name=<filename>` — convert an uploaded office file or PDF to
//! HTML so it can be imported into the editor. Conversion is done with tools
//! on the host: LibreOffice (`soffice`) for doc/docx/odt/rtf and poppler's
//! `pdftohtml` for PDFs.

use axum::{Json, body::Bytes, extract::Query};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tokio::process::Command;

use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct ConvertParams {
    /// Original filename — its extension selects the converter.
    name: String,
}

/// Hard cap on conversion time; a hung LibreOffice must not pin the server.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn convert(
    Query(params): Query<ConvertParams>,
    body: Bytes,
) -> Result<Json<JsonValue>, AppError> {
    if body.is_empty() {
        return Err(AppError::BadRequest("empty file".into()));
    }
    let ext = params
        .name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    // Validate BEFORE touching the filesystem — `ext` comes from the client
    // and is embedded in a path (a separator or unknown type must 400, not
    // 500 with a leaked scratch dir).
    if !matches!(ext.as_str(), "doc" | "docx" | "odt" | "rtf" | "pdf") {
        return Err(AppError::BadRequest(format!(
            "unsupported file type: .{ext} (use pdf, doc, docx, odt or rtf)"
        )));
    }

    // Isolated scratch dir per conversion (also holds the LibreOffice
    // profile, so parallel conversions can't fight over a profile lock).
    let work = std::env::temp_dir().join(format!(
        "qims-convert-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
    ));
    std::fs::create_dir_all(&work).map_err(|e| AppError::Internal(e.to_string()))?;
    let input = work.join(format!("input.{ext}"));
    if let Err(e) = std::fs::write(&input, &body) {
        let _ = std::fs::remove_dir_all(&work);
        return Err(AppError::Internal(e.to_string()));
    }

    log::info!("converting '{}' ({} KB)", params.name, body.len() / 1024);
    let started = std::time::Instant::now();

    let result = match ext.as_str() {
        "doc" | "docx" | "odt" | "rtf" => convert_office(&work, &input).await,
        "pdf" => convert_pdf(&input).await.map(|html| {
            json!({ "html": html, "page": null, "pages": [], "footers": {} })
        }),
        other => Err(AppError::BadRequest(format!(
            "unsupported file type: .{other} (use pdf, doc, docx, odt or rtf)"
        ))),
    };
    let _ = std::fs::remove_dir_all(&work);
    let mut result = result?;

    // Keep the user's original file next to the conversion (temporary
    // measure while converter fidelity is being improved): the document
    // record created from this import references it, so the untouched
    // source stays downloadable. A failed save never fails the conversion.
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "original".to_string(),
            save_original(&params.name, &ext, &body)
                .unwrap_or(JsonValue::Null),
        );
    }

    log::info!(
        "converted '{}' in {}",
        params.name,
        crate::logger::elapsed_str(started.elapsed())
    );
    Ok(Json(result))
}

/// Where original imported files are kept: `<data dir>/originals`, next to
/// the SurrealDB store. The backend runs with `qims-backend` as its working
/// directory (dev.sh / serve.sh), so the default lands in `qims-backend/data`.
/// `QIMS_DATA_DIR` overrides the base for test harnesses.
pub fn originals_dir() -> std::path::PathBuf {
    let data = std::env::var("QIMS_DATA_DIR").unwrap_or_else(|_| "data".to_string());
    std::path::Path::new(&data).join("originals")
}

/// MIME type for a (validated) import extension.
fn mime_for(ext: &str) -> &'static str {
    match ext {
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        "odt" => "application/vnd.oasis.opendocument.text",
        "rtf" => "application/rtf",
        _ => "application/octet-stream",
    }
}

/// Persist the uploaded file under a collision-proof name and describe it for
/// the client, which passes the reference on when the document is created.
fn save_original(name: &str, ext: &str, body: &Bytes) -> Option<JsonValue> {
    // Flatten the client-supplied name into a safe single path segment.
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stored = format!(
        "{}-{safe}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let dir = originals_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::error!("could not create {}: {e}", dir.display());
        return None;
    }
    if let Err(e) = std::fs::write(dir.join(&stored), body) {
        log::error!("could not save original '{name}': {e}");
        return None;
    }
    log::info!("saved original '{name}' as originals/{stored}");
    Some(json!({
        "file": stored,
        "name": name,
        "mime": mime_for(ext),
        "size": body.len(),
    }))
}

/// Word-processor formats via LibreOffice, with images embedded as data URIs.
async fn convert_office(
    work: &std::path::Path,
    input: &std::path::Path,
) -> Result<JsonValue, AppError> {
    let profile = work.join("profile");
    // XHTML resolves list/heading numbering into literal text (matching how
    // the document actually renders); the legacy HTML filter loses it. Fall
    // back to the legacy filter if the XHTML conversion fails in ANY way —
    // command error or missing output file.
    let xhtml = match run(soffice(&profile)
        .arg("--convert-to")
        .arg("xhtml:XHTML Writer File:UTF8")
        .arg("--outdir")
        .arg(work)
        .arg(input))
    .await
    {
        Ok(_) => std::fs::read_to_string(input.with_extension("xhtml")).ok(),
        Err(err) => {
            log::warn!("XHTML filter failed, falling back to legacy HTML: {err}");
            None
        }
    };
    let mut html = match xhtml {
        Some(xhtml) => xhtml,
        None => {
            run(soffice(&profile)
                .arg("--convert-to")
                .arg("html:HTML (StarWriter):EmbedImages")
                .arg("--outdir")
                .arg(work)
                .arg(input))
            .await?;
            std::fs::read_to_string(input.with_extension("html"))
                .map_err(|_| AppError::Internal("LibreOffice produced no output".into()))?
        }
    };
    // Return the FULL page (head styles included) — the frontend resolves the
    // stylesheet cascade in a sandboxed iframe for a one-to-one import.

    // LibreOffice's HTML-family filters drop page-footer content entirely and
    // drop header/vector images. Salvage both from the document's OOXML form.
    let mut footers = json!({});
    let mut page = JsonValue::Null;
    match ensure_unpacked(work, input).await {
        Ok(unpacked) => {
            // Header/vector images — only when the export has none of its own.
            if !html.to_ascii_lowercase().contains("<img") {
                match salvage_images(work, &unpacked).await {
                    Ok(tags) if !tags.is_empty() => {
                        let lower = html.to_ascii_lowercase();
                        if let Some(at) = lower
                            .find("<body")
                            .and_then(|i| lower[i..].find('>').map(|j| i + j + 1))
                        {
                            html.insert_str(at, &tags);
                            log::info!("salvaged header image(s) into the import");
                        }
                    }
                    Ok(_) => {}
                    Err(err) => log::warn!("image salvage skipped: {err}"),
                }
            }
            // Footer text (copyright lines etc.) classified by placement type,
            // so the frontend can put the first-page footer ON page one.
            footers = salvage_footers_classified(&unpacked);
            page = page_setup(&unpacked);
        }
        Err(err) => log::warn!("OOXML salvage skipped: {err}"),
    }

    // Page-start snippets from LibreOffice's own PDF rendering — the ground
    // truth for WHERE each original page begins, used by the frontend to place
    // hard page breaks.
    let pages = page_snippets(work, input, &profile).await;

    Ok(json!({ "html": html, "page": page, "pages": pages, "footers": footers }))
}

/// Page geometry from the OOXML section properties (twips → px at 96 dpi).
fn page_setup(unpacked: &std::path::Path) -> JsonValue {
    let Ok(doc) = std::fs::read_to_string(unpacked.join("word").join("document.xml"))
    else {
        return JsonValue::Null;
    };
    let attr = |tag: &str, name: &str| -> Option<f64> {
        let i = doc.find(tag)?;
        let end = doc[i..].find('>')? + i;
        let frag = &doc[i..end];
        let key = format!("{name}=\"");
        let j = frag.find(&key)? + key.len();
        let k = frag[j..].find('"')? + j;
        frag[j..k].parse::<f64>().ok()
    };
    let px = |twips: f64| (twips / 15.0).round() as i64;
    match (
        attr("<w:pgSz", "w:w"),
        attr("<w:pgSz", "w:h"),
        attr("<w:pgMar", "w:top"),
        attr("<w:pgMar", "w:right"),
        attr("<w:pgMar", "w:bottom"),
        attr("<w:pgMar", "w:left"),
    ) {
        (Some(w), Some(h), Some(t), Some(r), Some(b), Some(l)) => json!({
            "width": px(w), "height": px(h),
            "top": px(t), "right": px(r), "bottom": px(b), "left": px(l),
        }),
        _ => JsonValue::Null,
    }
}

/// Render the document to PDF and return, for each page from 2 on, a snippet
/// of the page's first body text (the repeating page-header prefix shared by
/// the interior pages is stripped). Empty when the document has < 3 pages.
async fn page_snippets(
    work: &std::path::Path,
    input: &std::path::Path,
    profile: &std::path::Path,
) -> Vec<String> {
    if run(soffice(profile)
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(work)
        .arg(input))
    .await
    .is_err()
    {
        return Vec::new();
    }
    let pdf = input.with_extension("pdf");

    let mut texts: Vec<String> = Vec::new();
    let mut empties = 0;
    for page in 2..=100u32 {
        let out = run(Command::new("pdftotext")
            .arg("-f")
            .arg(page.to_string())
            .arg("-l")
            .arg(page.to_string())
            .arg(&pdf)
            .arg("-"))
        .await
        .unwrap_or_default();
        let norm = out.split_whitespace().collect::<Vec<_>>().join(" ");
        if norm.is_empty() {
            // One textless page can be an image-only interior page; two in a
            // row means the document ended.
            empties += 1;
            if empties >= 2 {
                break;
            }
            texts.push(String::new());
            continue;
        }
        empties = 0;
        texts.push(norm);
    }
    while texts.last().is_some_and(|t| t.is_empty()) {
        texts.pop();
    }
    if texts.len() < 2 {
        // With a single interior page the repeating header can't be inferred,
        // and an unstripped snippet would match the header block instead.
        return Vec::new();
    }

    // Longest common prefix across non-empty interior pages = the header.
    let non_empty: Vec<&String> = texts.iter().filter(|t| !t.is_empty()).collect();
    if non_empty.len() < 2 {
        return Vec::new();
    }
    let mut lcp = non_empty[0].clone();
    for text in &non_empty[1..] {
        let common = lcp
            .char_indices()
            .zip(text.chars())
            .take_while(|((_, a), b)| a == b)
            .count();
        lcp = lcp.chars().take(common).collect();
    }
    if lcp.chars().count() < 10 {
        lcp.clear();
    }

    texts
        .into_iter()
        .map(|t| {
            let body = t.strip_prefix(&lcp).unwrap_or(&t).trim_start();
            body.chars().take(80).collect::<String>()
        })
        .collect()
}

/// Convert the input to OOXML (if it isn't already) and unzip it, returning
/// the unpacked directory. Idempotent per conversion workspace.
async fn ensure_unpacked(
    work: &std::path::Path,
    input: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let unpacked = work.join("unpacked");
    if unpacked.is_dir() {
        return Ok(unpacked);
    }
    let profile = work.join("profile");
    let docx = if input.extension().and_then(|e| e.to_str()) == Some("docx") {
        input.to_path_buf()
    } else {
        run(soffice(&profile)
            .arg("--convert-to")
            .arg("docx")
            .arg("--outdir")
            .arg(work)
            .arg(input))
        .await
        .map_err(|e| format!("docx conversion failed: {e}"))?;
        input.with_extension("docx")
    };
    run(Command::new("unzip")
        .arg("-o")
        .arg("-q")
        .arg(&docx)
        .arg("-d")
        .arg(&unpacked))
    .await
    .map_err(|e| format!("unzip failed: {e}"))?;
    Ok(unpacked)
}

/// One paragraph extracted from an OOXML footer.
struct FooterParagraph {
    text: String,
    bold: bool,
    align: Option<&'static str>,
    /// Font size in points (from `w:sz`, half-points).
    size_pt: f32,
}

/// Footer HTML for one `footerN.xml`; None when it has no usable text.
fn footer_file_html(path: &std::path::Path) -> Option<String> {
    let xml = std::fs::read_to_string(path).ok()?;
    let paragraphs: Vec<FooterParagraph> = split_tags(&xml, "w:p")
        .into_iter()
        .filter_map(|p| parse_footer_paragraph(&p))
        .collect();
    if paragraphs.is_empty() {
        return None;
    }
    let mut html = String::new();
    for p in paragraphs {
        let align = p
            .align
            .map(|a| format!(" align=\"{a}\""))
            .unwrap_or_default();
        let (b_open, b_close) = if p.bold { ("<b>", "</b>") } else { ("", "") };
        html.push_str(&format!(
            "<p{align} style=\"font-size: {}pt\">{b_open}{}{b_close}</p>",
            p.size_pt, p.text
        ));
    }
    Some(html)
}

/// Classify the document's footers as first-page vs default via the section
/// properties (`<w:footerReference w:type="first|default|even" r:id>`), so the
/// copyright block that Word shows on page one can go back on page one.
/// LibreOffice's HTML-family exports drop page footers entirely.
fn salvage_footers_classified(unpacked: &std::path::Path) -> JsonValue {
    let word = unpacked.join("word");
    let refs = std::fs::read_to_string(word.join("document.xml")).unwrap_or_default();
    let rels = std::fs::read_to_string(word.join("_rels").join("document.xml.rels"))
        .unwrap_or_default();

    // r:id → footer target file, from the relationships part.
    let target_of = |rid: &str| -> Option<String> {
        let key = format!("Id=\"{rid}\"");
        let i = rels.find(&key)?;
        let start = rels[..i].rfind('<')?;
        let end = rels[start..].find('>')? + start;
        let tag = &rels[start..end];
        if !tag.contains("relationships/footer") {
            return None;
        }
        let t = tag.find("Target=\"")? + 8;
        let e = tag[t..].find('"')? + t;
        Some(tag[t..e].trim_start_matches('/').to_string())
    };

    let mut first: Option<String> = None;
    let mut default: Option<String> = None;
    let mut even: Option<String> = None;
    let mut at = 0;
    while let Some(i) = refs[at..].find("<w:footerReference") {
        let start = at + i;
        let Some(end) = refs[start..].find('>') else { break };
        let tag = &refs[start..start + end];
        let kind = tag
            .find("w:type=\"")
            .and_then(|k| tag[k + 8..].find('"').map(|e| &tag[k + 8..k + 8 + e]));
        let rid = tag
            .find("r:id=\"")
            .and_then(|k| tag[k + 6..].find('"').map(|e| &tag[k + 6..k + 6 + e]));
        if let (Some(kind), Some(rid)) = (kind, rid) {
            if let Some(target) = target_of(rid) {
                let html = footer_file_html(&word.join(&target));
                match kind {
                    "first" if first.is_none() => first = html,
                    "default" if default.is_none() => default = html,
                    "even" if even.is_none() => even = html,
                    _ => {}
                }
            }
        }
        at = start + end;
    }

    // Even-page footer only counts when no true default exists.
    if default.is_none() {
        default = even;
    }

    // No references found (unusual container): fall back to every distinct
    // footer file as "default" content so nothing is lost.
    if first.is_none() && default.is_none() {
        let mut files: Vec<_> = match std::fs::read_dir(&word) {
            Ok(dir) => dir
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("footer") && n.ends_with(".xml"))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort_by_key(|p| {
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let num: u32 = stem
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            (num, stem)
        });
        let mut seen = std::collections::HashSet::new();
        let mut all = String::new();
        for file in files {
            if let Some(html) = footer_file_html(&file) {
                if seen.insert(html.clone()) {
                    all.push_str(&html);
                }
            }
        }
        if !all.is_empty() {
            default = Some(all);
        }
    }

    json!({ "first": first, "default": default })
}

/// All `<tag …>…</tag>` blocks in an XML string (naive, non-nested scan —
/// OOXML paragraphs don't nest).
fn split_tags(xml: &str, tag: &str) -> Vec<String> {
    let open_a = format!("<{tag}>");
    let open_b = format!("<{tag} ");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut at = 0;
    while at < xml.len() {
        let next = match (xml[at..].find(&open_a), xml[at..].find(&open_b)) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let start = at + next;
        // Self-closing with attributes (`<w:t xml:space="…"/>`): an empty
        // block — grabbing the next `</tag>` would swallow a later element.
        if let Some(gt) = xml[start..].find('>') {
            if xml[..start + gt].ends_with('/') {
                out.push(xml[start..start + gt + 1].to_string());
                at = start + gt + 1;
                continue;
            }
        }
        let Some(end) = xml[start..].find(&close) else {
            break;
        };
        let end = start + end + close.len();
        out.push(xml[start..end].to_string());
        at = end;
    }
    out
}

/// Is any run in this paragraph explicitly bold? `<w:b/>` or `<w:b w:…>` with
/// a value other than 0/false/none (Word emits those to TURN OFF inherited
/// bold). `<w:bCs>`/`<w:bdr>` share the prefix and are skipped.
fn has_bold(p: &str) -> bool {
    let mut at = 0;
    while let Some(i) = p[at..].find("<w:b") {
        let start = at + i;
        let rest = &p[start + 4..];
        match rest.chars().next() {
            Some('/') | Some('>') => return true,
            Some(' ') => {
                if let Some(gt) = rest.find('>') {
                    let tag = &rest[..gt];
                    if !(tag.contains("w:val=\"0\"")
                        || tag.contains("w:val=\"false\"")
                        || tag.contains("w:val=\"none\""))
                    {
                        return true;
                    }
                    at = start + 4 + gt;
                    continue;
                }
                return false;
            }
            _ => {} // <w:bCs…>, <w:bdr…> etc.
        }
        at = start + 4;
    }
    false
}

/// Text + basic formatting of one footer paragraph; None when empty or a
/// stale page-number field ("Page 2 of 6").
fn parse_footer_paragraph(p: &str) -> Option<FooterParagraph> {
    // Concatenate all <w:t> runs. Well-formed OOXML text is entity-escaped
    // (no raw `<`/`>` possible), but CDATA sections or malformed producers
    // could smuggle markup — escape any raw angle brackets so nothing is ever
    // spliced into the HTML as tags.
    let mut text = String::new();
    for t in split_tags(p, "w:t") {
        if t.ends_with("/>") {
            continue; // self-closing empty run
        }
        if let Some(gt) = t.find('>') {
            if let Some(lt) = t.rfind("</") {
                if gt + 1 <= lt {
                    text.push_str(&t[gt + 1..lt].replace('<', "&lt;").replace('>', "&gt;"));
                }
            }
        }
    }
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    // Page-number fields render stale cached values — drop them.
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("page ")
        && lower.contains(" of ")
        && lower
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c.is_whitespace())
    {
        return None;
    }

    let bold = has_bold(p);
    let align = if p.contains("w:jc w:val=\"center\"") {
        Some("center")
    } else if p.contains("w:jc w:val=\"end\"") || p.contains("w:jc w:val=\"right\"") {
        Some("right")
    } else if p.contains("w:jc w:val=\"both\"") {
        Some("justify")
    } else {
        None
    };
    // w:sz is in half-points; footers default small.
    let size_pt = p
        .find("<w:sz w:val=\"")
        .and_then(|i| {
            let rest = &p[i + 13..];
            rest.find('"')
                .and_then(|j| rest[..j].parse::<f32>().ok())
        })
        .map(|half| half / 2.0)
        .unwrap_or(8.0);

    Some(FooterParagraph {
        text: trimmed,
        bold,
        align,
        size_pt,
    })
}

/// A headless LibreOffice command with an isolated profile.
fn soffice(profile: &std::path::Path) -> Command {
    let mut cmd = Command::new("soffice");
    cmd.arg("--headless")
        .arg(format!("-env:UserInstallation=file://{}", profile.display()));
    cmd
}

/// Extract the document's images (OOXML `word/media`), convert vector formats
/// to PNG, and return them as `<p><img …></p>` tags for injection.
async fn salvage_images(
    work: &std::path::Path,
    unpacked: &std::path::Path,
) -> Result<String, String> {
    use base64::Engine as _;
    let profile = work.join("profile");

    let media = unpacked.join("word").join("media");
    let mut entries: Vec<_> = match std::fs::read_dir(&media) {
        Ok(dir) => dir.flatten().map(|e| e.path()).collect(),
        Err(_) => return Ok(String::new()), // no images in the document
    };
    entries.sort();

    let mut tags = String::new();
    // A single unreadable/unconvertible media file must not lose the rest —
    // skip it (with a warning) and keep salvaging.
    for path in entries {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let (bytes, mime) = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" => {
                let mime = match ext.as_str() {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    _ => "image/jpeg",
                };
                match std::fs::read(&path) {
                    Ok(bytes) => (bytes, mime),
                    Err(err) => {
                        log::warn!("skipping media {}: {err}", path.display());
                        continue;
                    }
                }
            }
            // Vector/exotic formats: render to PNG, then trim the canvas
            // whitespace LibreOffice adds around metafiles.
            "wmf" | "emf" | "svm" | "bmp" | "tif" | "tiff" => {
                if let Err(err) = run(soffice(&profile)
                    .arg("--convert-to")
                    .arg("png")
                    .arg("--outdir")
                    .arg(work)
                    .arg(&path))
                .await
                {
                    log::warn!("skipping media {}: png render failed: {err}", path.display());
                    continue;
                }
                let png = work.join(path.file_stem().unwrap()).with_extension("png");
                let trimmed = work.join("trimmed.png");
                let use_path = if run(Command::new("magick")
                    .arg(&png)
                    .arg("-trim")
                    .arg("+repage")
                    .arg(&trimmed))
                .await
                .is_ok()
                {
                    trimmed
                } else {
                    png // magick unavailable — untrimmed is still better than lost
                };
                match std::fs::read(&use_path) {
                    Ok(bytes) => (bytes, "image/png"),
                    Err(err) => {
                        log::warn!("skipping media {}: {err}", path.display());
                        continue;
                    }
                }
            }
            _ => continue,
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        tags.push_str(&format!(
            "<p><img src=\"data:{mime};base64,{encoded}\"/></p>"
        ));
    }
    Ok(tags)
}

/// PDFs via poppler's pdftohtml (text-oriented single-page HTML).
async fn convert_pdf(input: &std::path::Path) -> Result<String, AppError> {
    run(Command::new("pdftohtml")
        .arg("-i")
        .arg("-noframes")
        .arg("-stdout")
        .arg(input))
    .await
}

/// Run a converter with a timeout; non-zero exit becomes an error.
async fn run(cmd: &mut Command) -> Result<String, AppError> {
    let output = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| AppError::Internal("conversion timed out".into()))?
        .map_err(|e| AppError::Internal(format!("failed to run converter: {e}")))?;
    if !output.status.success() {
        let err: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(300)
            .collect();
        return Err(AppError::Internal(format!("converter failed: {err}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

