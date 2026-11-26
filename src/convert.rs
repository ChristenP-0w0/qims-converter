//! `POST /convert?name=<filename>` — convert an uploaded office file or PDF to
//! HTML so it can be imported into the editor. Conversion is done with tools
//! on the host: LibreOffice (`soffice`) for doc/docx/odt/rtf and for the
//! spreadsheet formats xlsx/xls/ods, and poppler's `pdftohtml` for PDFs.

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
    if !matches!(
        ext.as_str(),
        "doc" | "docx" | "odt" | "rtf" | "pdf" | "xlsx" | "xlsm" | "xls" | "ods"
    ) {
        return Err(AppError::BadRequest(format!(
            "unsupported file type: .{ext} \
             (use pdf, doc, docx, odt, rtf, xlsx, xlsm, xls or ods)"
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
        "xlsx" | "xlsm" | "xls" | "ods" => convert_sheet(&work, &input).await,
        "pdf" => convert_pdf(&input).await.map(|html| {
            json!({ "html": html, "page": null, "pages": [], "footers": {} })
        }),
        other => Err(AppError::BadRequest(format!(
            "unsupported file type: .{other} \
             (use pdf, doc, docx, odt, rtf, xlsx, xlsm, xls or ods)"
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
    //
    // First choice goes through flat ODT so tabs survive (see
    // `xhtml_keeping_tabs`); the direct export is the fallback.
    let mut tabs = JsonValue::Null;
    let mut rows = JsonValue::Null;
    let xhtml = match xhtml_keeping_tabs(work, input, &profile).await {
        Some((xhtml, stops, heights)) => {
            tabs = stops;
            rows = heights;
            Some(xhtml)
        }
        None => match run(soffice(&profile)
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
        },
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

    Ok(json!({
        "html": html,
        "page": page,
        "pages": pages,
        "footers": footers,
        "tabs": tabs,
        "rows": rows,
    }))
}

/// Stands in for every tab in the XHTML. A private-use code point, so it
/// cannot collide with real text.
const TAB_MARK: char = '\u{E009}';

/// XHTML with each tab kept as [`TAB_MARK`], plus the tab stops that place
/// them.
///
/// LibreOffice's XHTML export (an XSLT) writes a tab as eight alternating
/// no-break and plain spaces, or as one no-break space, and drops the tab
/// stops. A table of contents loses its dot leaders and right-aligned page
/// numbers, and tabbed columns stop lining up. So: export flat ODT, swap each
/// `<text:tab/>` for the mark, read the stops from the same file, then export
/// that file to XHTML. The receiver lays the marks out against the stops.
async fn xhtml_keeping_tabs(
    work: &std::path::Path,
    input: &std::path::Path,
    profile: &std::path::Path,
) -> Option<(String, JsonValue, JsonValue)> {
    let flat = work.join("flat");
    if let Err(err) = run(soffice(profile)
        .arg("--convert-to")
        .arg("fodt")
        .arg("--outdir")
        .arg(&flat)
        .arg(input))
    .await
    {
        log::warn!("flat ODT export failed, tabs will be approximated: {err}");
        return None;
    }
    let stem = input.file_stem()?;
    let fodt_path = flat.join(stem).with_extension("fodt");
    let fodt = std::fs::read_to_string(&fodt_path).ok()?;
    let stops = tab_stops(&fodt);
    let heights = row_heights(&fodt);
    std::fs::write(&fodt_path, patch_flat(&fodt)).ok()?;
    if let Err(err) = run(soffice(profile)
        .arg("--convert-to")
        .arg("xhtml:XHTML Writer File:UTF8")
        .arg("--outdir")
        .arg(&flat)
        .arg(&fodt_path))
    .await
    {
        log::warn!("XHTML export of the flat ODT failed: {err}");
        return None;
    }
    let xhtml = std::fs::read_to_string(fodt_path.with_extension("xhtml")).ok()?;
    Some((xhtml, stops, heights))
}

/// Table row heights the XHTML export drops, keyed like its row classes
/// (`Table1.4` → `Table1_4`, written `row-Table1_4` on the `<tr>`), in px.
///
/// A form's writing box is often an empty row held open by its height —
/// SF235's NOTES box is a min-row-height of 8cm — and without it the box
/// shrank to one line. Both a minimum and an exact height count, as minimums.
fn row_heights(fodt: &str) -> JsonValue {
    let mut rows = serde_json::Map::new();
    for style in split_tags(fodt, "style:style") {
        let open = &style[..style.find('>').unwrap_or(style.len())];
        if attr(open, "style:family").as_deref() != Some("table-row") {
            continue;
        }
        let Some(name) = attr(open, "style:name") else { continue };
        let props = split_tags(&style, "style:table-row-properties");
        let Some(props) = props.first() else { continue };
        let height = attr(props, "style:min-row-height")
            .or_else(|| attr(props, "style:row-height"))
            .and_then(|v| length_px(&v));
        if let Some(height) = height {
            rows.insert(name.replace('.', "_"), json!(height));
        }
    }
    JsonValue::Object(rows)
}

/// Work around what LibreOffice's XHTML export gets wrong, in the flat ODT it
/// exports from: tabs (see `mark_tabs`), list indents and end alignment.
fn patch_flat(fodt: &str) -> String {
    // The export maps `end` to `left` for left-to-right text (its
    // style_mapping_css.xsl) — right-aligned codes, dates and signature
    // lines came in left-aligned.
    let aligned = mark_tabs(fodt).replace("fo:text-align=\"end\"", "fo:text-align=\"right\"");
    legacy_list_levels(&aligned)
}

/// Rewrite list levels from Word's "label alignment" form to the older
/// "label width and position" form, the only one the XHTML export indents.
///
/// Every list from a Word document uses the newer form, and the export wrote
/// each item at `margin-left: 0cm`: nested bullets all sat on the margin. The
/// two say the same thing — the label starts at margin-left + text-indent and
/// the text at margin-left — so `space-before` = margin-left + text-indent and
/// `min-label-width` = −text-indent.
fn legacy_list_levels(fodt: &str) -> String {
    const OPEN: &str = "<style:list-level-properties text:list-level-position-and-space-mode=\"label-alignment\"";
    const CLOSE: &str = "</style:list-level-properties>";
    let cm = |px: f64| format!("{:.3}cm", px * 2.54 / 96.0);
    let mut out = String::with_capacity(fodt.len());
    let mut rest = fodt;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let block = &rest[i..];
        let Some(end) = block.find(CLOSE) else {
            out.push_str(block);
            return out;
        };
        let props = &block[..end + CLOSE.len()];
        let alignment = split_tags(props, "style:list-level-label-alignment");
        match alignment.first() {
            Some(a) => {
                let margin = attr(a, "fo:margin-left").and_then(|v| length_px(&v)).unwrap_or(0.0);
                let indent = attr(a, "fo:text-indent").and_then(|v| length_px(&v)).unwrap_or(0.0);
                out.push_str(&format!(
                    "<style:list-level-properties text:space-before=\"{}\" text:min-label-width=\"{}\"/>",
                    cm(margin + indent),
                    cm(-indent)
                ));
            }
            None => out.push_str(props),
        }
        rest = &block[end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Replace every `<text:tab/>` (with or without attributes) with [`TAB_MARK`].
fn mark_tabs(fodt: &str) -> String {
    let mut out = String::with_capacity(fodt.len());
    let mut rest = fodt;
    while let Some(i) = rest.find("<text:tab") {
        let after = &rest[i + "<text:tab".len()..];
        // `<text:table…`, `<text:tab-stop…` share the prefix.
        let is_tab = after.starts_with("/>") || after.starts_with(' ');
        let end = after.find("/>").filter(|_| is_tab);
        match end {
            Some(end) if !after[..end].contains('>') => {
                out.push_str(&rest[..i]);
                out.push(TAB_MARK);
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The tab stops each paragraph style ends up with, keyed by style name —
/// which is the XHTML class minus its `paragraph-` prefix.
///
/// `{ "distance": px, "relative_to_indent": bool, "styles": { name: [ { "pos":
/// px, "align": "left|right|center|decimal", "leader": "dotted|solid|dashed"
/// } ] } }`. A style without its own `<style:tab-stops>` inherits its parent's;
/// one with them replaces the parent's outright, as ODF says. `distance` is the
/// default stop spacing used past the last explicit stop.
fn tab_stops(fodt: &str) -> JsonValue {
    // name → (parent, own stops or None when the style declares none)
    let mut own: std::collections::HashMap<String, (Option<String>, Option<Vec<JsonValue>>)> =
        std::collections::HashMap::new();
    for style in split_tags(fodt, "style:style") {
        let open = &style[..style.find('>').unwrap_or(style.len())];
        if attr(open, "style:family").as_deref() != Some("paragraph") {
            continue;
        }
        let Some(name) = attr(open, "style:name") else {
            continue;
        };
        let stops = style.contains("<style:tab-stops").then(|| {
            split_tags(&style, "style:tab-stop")
                .iter()
                .filter_map(|stop| {
                    let pos = length_px(&attr(stop, "style:position")?)?;
                    let align = attr(stop, "style:type")
                        .or_else(|| attr(stop, "style:char").map(|_| "decimal".into()))
                        .unwrap_or_else(|| "left".into());
                    let leader = match (
                        attr(stop, "style:leader-text").as_deref(),
                        attr(stop, "style:leader-style").as_deref(),
                    ) {
                        (_, None | Some("none")) => JsonValue::Null,
                        (Some("."), _) | (_, Some("dotted")) => json!("dotted"),
                        (Some("-"), _) | (_, Some("dash" | "long-dash")) => json!("dashed"),
                        _ => json!("solid"),
                    };
                    Some(json!({ "pos": pos, "align": align, "leader": leader }))
                })
                .collect()
        });
        own.insert(name, (attr(open, "style:parent-style-name"), stops));
    }

    let mut styles = serde_json::Map::new();
    for name in own.keys() {
        let mut at = Some(name.clone());
        // Bounded walk: a parent cycle in a broken file must not hang us.
        for _ in 0..32 {
            let Some((parent, stops)) = at.as_ref().and_then(|n| own.get(n)) else {
                break;
            };
            if let Some(stops) = stops {
                if !stops.is_empty() {
                    styles.insert(name.clone(), JsonValue::Array(stops.clone()));
                }
                break;
            }
            at = parent.clone();
        }
    }

    let defaults = split_tags(fodt, "style:default-style")
        .into_iter()
        .find(|s| attr(s, "style:family").as_deref() == Some("paragraph"));
    let distance = defaults
        .as_deref()
        .and_then(|s| attr(s, "style:tab-stop-distance"))
        .and_then(|v| length_px(&v))
        .unwrap_or(48.0); // 1.27cm, LibreOffice's own default
    // Word documents measure stops from the page margin; ODF from the indent.
    let relative_to_indent = !fodt.contains(
        "config:name=\"TabsRelativeToIndent\" config:type=\"boolean\">false<",
    );
    json!({ "distance": distance, "relative_to_indent": relative_to_indent, "styles": styles })
}

/// The value of `name="…"` in a tag, if present.
fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// An ODF length ("1.27cm", "0.5in", "12pt", "5mm") in CSS px at 96 dpi.
fn length_px(value: &str) -> Option<f64> {
    let split = value.find(|c: char| c.is_ascii_alphabetic())?;
    let number: f64 = value[..split].parse().ok()?;
    let per_unit = match &value[split..] {
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "in" => 96.0,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        "px" => 1.0,
        _ => return None,
    };
    Some((number * per_unit * 100.0).round() / 100.0)
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    #[test]
    fn rewrites_list_levels_and_end_alignment() {
        let fodt = r#"<style:paragraph-properties fo:text-align="end"/><text:list-level-style-bullet text:level="2"><style:list-level-properties text:list-level-position-and-space-mode="label-alignment"><style:list-level-label-alignment text:label-followed-by="listtab" fo:text-indent="-0.635cm" fo:margin-left="2.54cm"/></style:list-level-properties></text:list-level-style-bullet><style:list-level-properties text:min-label-width="1cm"/>"#;
        let out = patch_flat(fodt);
        assert!(out.contains(r#"fo:text-align="right""#));
        assert!(out.contains(r#"<style:list-level-properties text:space-before="1.905cm" text:min-label-width="0.635cm"/>"#), "{out}");
        assert!(out.contains(r#"<style:list-level-properties text:min-label-width="1cm"/>"#), "older form left alone");
    }

    #[test]
    fn marks_tabs_but_not_tables_or_stops() {
        let fodt = r#"<text:p>1.<text:tab/>A<text:tab text:tab-ref="2"/>2</text:p><text:table-of-content/><style:tab-stop style:position="1cm"/>"#;
        let out = mark_tabs(fodt);
        assert_eq!(out.matches(TAB_MARK).count(), 2);
        assert!(out.contains("<text:table-of-content/>"));
        assert!(out.contains("<style:tab-stop "));
    }

    #[test]
    fn resolves_inherited_stops_and_units() {
        let fodt = r#"
<style:default-style style:family="paragraph"><style:paragraph-properties style:tab-stop-distance="2.54cm"/></style:default-style>
<style:style style:name="Contents_20_2" style:family="paragraph" style:parent-style-name="Standard"><style:paragraph-properties><style:tab-stops><style:tab-stop style:position="0.635cm"/><style:tab-stop style:position="14.984cm" style:type="right" style:leader-style="dotted" style:leader-text="."/></style:tab-stops></style:paragraph-properties></style:style>
<style:style style:name="P5" style:family="paragraph" style:parent-style-name="Contents_20_2"><style:paragraph-properties fo:margin-left="1cm"/></style:style>
<style:style style:name="P6" style:family="paragraph" style:parent-style-name="Contents_20_2"><style:paragraph-properties><style:tab-stops/></style:paragraph-properties></style:style>
<style:style style:name="T1" style:family="text"/>
<config:config-item config:name="TabsRelativeToIndent" config:type="boolean">false</config:config-item>"#;
        let tabs = tab_stops(fodt);
        assert_eq!(tabs["distance"], json!(96.0));
        assert_eq!(tabs["relative_to_indent"], json!(false));
        let toc = &tabs["styles"]["Contents_20_2"];
        assert_eq!(toc[0], json!({ "pos": 24.0, "align": "left", "leader": null }));
        assert_eq!(toc[1]["align"], json!("right"));
        assert_eq!(toc[1]["leader"], json!("dotted"));
        assert_eq!(tabs["styles"]["P5"], *toc, "inherits the parent's stops");
        assert!(tabs["styles"].get("P6").is_none(), "empty tab-stops clears them");
        assert_eq!(length_px("0.5in"), Some(48.0));

        let rows = row_heights(
            r#"<style:style style:name="Table1.4" style:family="table-row"><style:table-row-properties style:min-row-height="7.999cm"/></style:style><style:style style:name="Table1.1" style:family="table-row"><style:table-row-properties fo:keep-together="always"/></style:style>"#,
        );
        assert_eq!(rows, json!({ "Table1_4": 302.32 }));
    }
}

/// Spreadsheets via LibreOffice.
///
/// Kept apart from `convert_office` because a spreadsheet differs in three
/// ways that matter: images (the word-processor export embeds them, the
/// spreadsheet one writes them as sibling PNGs next to the .xhtml, so a
/// returned document would carry dead `src`s), the print range (the export
/// dumps the whole used range, while a sheet prints only its print area), and
/// where the page geometry lives — paper size and margins are per-sheet print
/// settings rather than section properties, and a worksheet is far more often
/// landscape than a text document is.
async fn convert_sheet(
    work: &std::path::Path,
    input: &std::path::Path,
) -> Result<JsonValue, AppError> {
    let profile = work.join("profile");
    // Workbook data first: it is cheap, and a workbook too big to import is
    // turned away before LibreOffice spends a minute exporting it.
    let (page, print_area, sheets) = match unpack_workbook(work, input).await {
        Ok(unpacked) => {
            let sheets = workbook_sheets(&unpacked);
            (sheet_page_setup(&unpacked, &sheets), print_area(&unpacked), sheets)
        }
        Err(err) => {
            log::warn!("sheet page setup skipped: {err}");
            (JsonValue::Null, JsonValue::Null, Vec::new())
        }
    };
    if let Some(refusal) = too_large(&sheets) {
        return Err(AppError::BadRequest(refusal));
    }

    // Plain `xhtml`, deliberately NOT `xhtml:XHTML Calc File:UTF8`. Naming the
    // Calc filter explicitly drops every drawing object on the sheet — a
    // workbook whose letterhead and stamps are images came through with none
    // of them, and the export shrank by nearly half. Letting LibreOffice pick
    // the filter for the input keeps them, written as sibling PNGs.
    run(soffice(&profile)
        .arg("--convert-to")
        .arg("xhtml")
        .arg("--outdir")
        .arg(work)
        .arg(input))
    .await?;
    let mut html = std::fs::read_to_string(input.with_extension("xhtml"))
        .map_err(|_| AppError::Internal("LibreOffice produced no output".into()))?;

    inline_sibling_images(work, &mut html);

    let sheets: Vec<JsonValue> = sheets.into_iter().map(|s| s.json).collect();
    let pages = page_snippets(work, input, &profile).await;

    Ok(json!({
        "html": html,
        "page": page,
        "pages": pages,
        "footers": {},
        "print_area": print_area,
        "sheets": sheets,
    }))
}

/// Unzip the workbook itself, for the print settings the HTML export drops.
///
/// `ensure_unpacked` cannot serve here: it converts its input to **docx** and
/// unzips that, which for a spreadsheet yields a word-processor document with
/// no `xl/` tree at all. An .xlsx is already the zip we want; .xls and .ods
/// get converted to one first.
async fn unpack_workbook(
    work: &std::path::Path,
    input: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let unpacked = work.join("workbook");
    if unpacked.is_dir() {
        return Ok(unpacked);
    }
    let xlsx = if input.extension().and_then(|e| e.to_str()) == Some("xlsx") {
        input.to_path_buf()
    } else {
        let profile = work.join("profile");
        run(soffice(&profile)
            .arg("--convert-to")
            .arg("xlsx")
            .arg("--outdir")
            .arg(work)
            .arg(input))
        .await
        .map_err(|e| format!("xlsx conversion failed: {e}"))?;
        input.with_extension("xlsx")
    };
    run(Command::new("unzip")
        .arg("-o")
        .arg("-q")
        .arg(&xlsx)
        .arg("-d")
        .arg(&unpacked))
    .await
    .map_err(|e| format!("unzip failed: {e}"))?;
    Ok(unpacked)
}

/// Rewrite `src="sheet_html_1a2b.png"` to a data URI, reading the file the
/// spreadsheet filter dropped next to its output. Anything that cannot be read
/// is left as it is: a missing picture beats a failed conversion.
fn inline_sibling_images(work: &std::path::Path, html: &mut String) {
    use base64::Engine as _;

    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(at) = rest.find("src=\"") {
        let (before, from) = rest.split_at(at + 5);
        out.push_str(before);
        let Some(end) = from.find('"') else {
            rest = from;
            break;
        };
        let (raw, after) = from.split_at(end);
        rest = after;
        // Already inline, or not ours to resolve.
        if raw.starts_with("data:") || raw.contains("://") || raw.contains('/') {
            out.push_str(raw);
            continue;
        }
        let name = percent_decode(raw);
        let path = work.join(&name);
        let mime = match name
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            _ => "image/jpeg",
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                out.push_str(&format!("data:{mime};base64,{encoded}"));
            }
            Err(_) => out.push_str(raw),
        }
    }
    out.push_str(rest);
    *html = out;
}

/// `%20` and friends — the spreadsheet filter percent-encodes the image
/// filenames it derives from the workbook's own name, which usually has spaces
/// in it.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The sheet's print range (`_xlnm.Print_Area`) as zero-based cell bounds.
///
/// The HTML export dumps the whole used range, but a worksheet only prints
/// what its print area covers — anything the author parked outside it (working
/// notes, scratch columns) has never reached paper and is not part of the
/// document. Reported rather than applied here: the frontend holds the parsed
/// table and can crop it without re-parsing HTML.
fn print_area(unpacked: &std::path::Path) -> JsonValue {
    let Ok(book) = std::fs::read_to_string(unpacked.join("xl").join("workbook.xml")) else {
        return JsonValue::Null;
    };
    let Some(i) = book.find("_xlnm.Print_Area") else {
        return JsonValue::Null;
    };
    let Some(start) = book[i..].find('>').map(|j| i + j + 1) else {
        return JsonValue::Null;
    };
    let Some(end) = book[start..].find('<').map(|j| start + j) else {
        return JsonValue::Null;
    };
    // 'A Sheet'!$A$1:$M$28 — the sheet name is irrelevant, only one sheet is
    // exported. A multi-range print area (comma separated) is left alone:
    // cropping to the first range only would silently lose the rest.
    let range = &book[start..end];
    if range.contains(',') {
        return JsonValue::Null;
    }
    let Some((from, to)) = range.rsplit('!').next().and_then(|r| r.split_once(':')) else {
        return JsonValue::Null;
    };
    match (cell_ref(from), cell_ref(to)) {
        (Some((left, top)), Some((right, bottom))) => json!({
            "top": top, "left": left, "bottom": bottom, "right": right,
        }),
        _ => JsonValue::Null,
    }
}

/// `$M$28` → (12, 27): zero-based column then row.
fn cell_ref(raw: &str) -> Option<(i64, i64)> {
    let cleaned: String = raw.chars().filter(|c| *c != '$').collect();
    let split = cleaned.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = cleaned.split_at(split);
    if letters.is_empty() {
        return None;
    }
    let mut col: i64 = 0;
    for c in letters.chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + (c.to_ascii_uppercase() as i64 - 'A' as i64 + 1);
    }
    Some((col - 1, digits.parse::<i64>().ok()? - 1))
}

/// The most filled cells (across visible sheets) a workbook may have to be
/// imported. Forms and worksheets run to a few hundred; the largest chart
/// workbook seen has ~6,700. Past this the import is a data log, not a form:
/// a 100,000-cell one exported 55 MB of HTML and hung the browser.
const MAX_SHEET_CELLS: usize = 20_000;

/// Why a workbook is refused, worded for the person importing it; None when
/// it is small enough.
fn too_large(sheets: &[Sheet]) -> Option<String> {
    let filled: usize = sheets
        .iter()
        .filter(|s| !s.hidden)
        .map(|s| s.json["cells"].as_object().map_or(0, |c| c.len()))
        .sum();
    (filled > MAX_SHEET_CELLS).then(|| {
        format!(
            "This spreadsheet is too large to import: it has {filled} filled cells, and QIMS \
             imports up to {MAX_SHEET_CELLS}. Import a smaller workbook, or ask IT if this one \
             needs to come in."
        )
    })
}

/// One worksheet as the workbook lists it, in tab order.
struct Sheet {
    hidden: bool,
    path: std::path::PathBuf,
    /// `{ name, hidden, print_area, cells: { "B4": [points, wrap] } }`
    json: JsonValue,
}

/// Every worksheet with the cell formatting the HTML export loses.
///
/// The export rounds each font size to one of seven HTML sizes (10pt and 11pt
/// come out the same, and the default lands on `x-small`) and does not say
/// whether a cell wraps — which is what decides, in Excel, whether text runs
/// on over the empty cells beside it or breaks onto a second line. Both are
/// read here from the workbook itself: `styles.xml` for the font size and
/// wrap of each cell format, the sheet for which format each filled cell
/// uses. Print areas are per sheet (`localSheetId` is the tab index).
fn workbook_sheets(unpacked: &std::path::Path) -> Vec<Sheet> {
    let xl = unpacked.join("xl");
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let book = read(xl.join("workbook.xml"));
    let rich = shared_rich_text(&read(xl.join("sharedStrings.xml")));
    let rels = read(xl.join("_rels").join("workbook.xml.rels"));
    let styles = read(xl.join("styles.xml"));

    // Font sizes by font id, then (points, wrap) by cell format index.
    let section = |xml: &str, tag: &str| -> String {
        split_tags(xml, tag).into_iter().next().unwrap_or_default()
    };
    let font_sizes: Vec<Option<f64>> = split_tags(&section(&styles, "fonts"), "font")
        .iter()
        .map(|font| {
            split_tags(font, "sz")
                .first()
                .and_then(|sz| attr(sz, "val"))
                .and_then(|v| v.parse().ok())
        })
        .collect();
    let formats: Vec<(Option<f64>, bool)> = split_tags(&section(&styles, "cellXfs"), "xf")
        .iter()
        .map(|xf| {
            let size = attr(xf, "fontId")
                .and_then(|id| id.parse::<usize>().ok())
                .and_then(|id| font_sizes.get(id).copied().flatten());
            let wrap = split_tags(xf, "alignment")
                .first()
                .and_then(|a| attr(a, "wrapText"))
                .is_some_and(|v| v == "1" || v == "true");
            (size, wrap)
        })
        .collect();

    let defined: Vec<String> = split_tags(&book, "definedName");
    split_tags(&book, "sheet")
        .iter()
        .enumerate()
        .filter_map(|(index, tag)| {
            let name = xml_unescape(&attr(tag, "name")?);
            let rid = attr(tag, "r:id")?;
            let rel = split_tags(&rels, "Relationship")
                .into_iter()
                .find(|r| attr(r, "Id").as_deref() == Some(rid.as_str()))?;
            let target = attr(&rel, "Target")?;
            let path = match target.strip_prefix('/') {
                Some(absolute) => unpacked.join(absolute),
                None => xl.join(target),
            };
            let hidden = attr(tag, "state").is_some_and(|s| s != "visible");

            let area = defined
                .iter()
                .find(|d| {
                    attr(d, "name").as_deref() == Some("_xlnm.Print_Area")
                        && attr(d, "localSheetId").as_deref() == Some(index.to_string().as_str())
                })
                .and_then(|d| print_range(&d[d.find('>')? + 1..d.rfind('<')?]))
                .unwrap_or(JsonValue::Null);

            let xml = read(path.clone());
            let mut cells = serde_json::Map::new();
            let mut rich_cells = serde_json::Map::new();
            for cell in split_tags(&xml, "c") {
                // Only cells with something in them: an empty formatted cell
                // has nothing to size or wrap.
                if !(cell.contains("<v>") || cell.contains("<is>") || cell.contains("<f")) {
                    continue;
                }
                let Some(r) = attr(&cell, "r") else { continue };
                let format = attr(&cell, "s").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let (size, wrap) = formats.get(format).copied().unwrap_or((None, false));
                // A shared string written in runs of differing format.
                if attr(&cell, "t").as_deref() == Some("s") {
                    let runs = split_tags(&cell, "v")
                        .first()
                        .and_then(|v| v[3..v.len() - 4].trim().parse::<usize>().ok())
                        .and_then(|i| rich.get(i).cloned().flatten());
                    if let Some(runs) = runs {
                        rich_cells.insert(r.clone(), runs);
                    }
                }
                cells.insert(r, json!([size, wrap]));
            }
            Some(Sheet {
                hidden,
                path,
                json: json!({
                    "name": name,
                    "hidden": hidden,
                    "print_area": area,
                    "cells": cells,
                    "rich": rich_cells,
                }),
            })
        })
        .collect()
}

/// Each shared string that is written in formatted runs, as
/// `[[text, format | null], …]`; None for a plain string.
///
/// The HTML export gives a cell one font, so "**BW =** Big well" lost its bold
/// label and footnote marks their superscript. A run's `format` holds only
/// what it sets — `b`, `i`, `u`, `s`, `sup`, `sub`, `sz` (points), `color`
/// (#rrggbb) — and is null for a run with no properties of its own, which
/// takes the cell's format.
fn shared_rich_text(sst: &str) -> Vec<Option<JsonValue>> {
    // `<b/>`, or `<b val="…"/>` where 0/false/none turns it off.
    let on = |props: &str, tag: &str| -> bool {
        props.contains(&format!("<{tag}/>"))
            || split_tags(props, tag).first().is_some_and(|t| {
                !matches!(attr(t, "val").as_deref(), Some("0" | "false" | "none"))
            })
    };
    split_tags(sst, "si")
        .iter()
        .map(|si| {
            let runs = split_tags(si, "r");
            if runs.is_empty() {
                return None;
            }
            let parts: Vec<JsonValue> = runs
                .iter()
                .map(|run| {
                    let text: String = split_tags(run, "t")
                        .iter()
                        .filter_map(|t| Some(xml_unescape(&t[t.find('>')? + 1..t.rfind('<')?])))
                        .collect();
                    let format = split_tags(run, "rPr").first().map(|props| {
                        let mut f = serde_json::Map::new();
                        for (key, tag) in [("b", "b"), ("i", "i"), ("u", "u"), ("s", "strike")] {
                            if on(props, tag) {
                                f.insert(key.into(), json!(true));
                            }
                        }
                        match split_tags(props, "vertAlign").first().and_then(|v| attr(v, "val")) {
                            Some(v) if v == "superscript" => drop(f.insert("sup".into(), json!(true))),
                            Some(v) if v == "subscript" => drop(f.insert("sub".into(), json!(true))),
                            _ => {}
                        }
                        if let Some(sz) = split_tags(props, "sz")
                            .first()
                            .and_then(|t| attr(t, "val"))
                            .and_then(|v| v.parse::<f64>().ok())
                        {
                            f.insert("sz".into(), json!(sz));
                        }
                        // ARGB; theme and indexed colours are left to the cell.
                        if let Some(rgb) = split_tags(props, "color").first().and_then(|t| attr(t, "rgb")) {
                            if rgb.len() == 8 {
                                f.insert("color".into(), json!(format!("#{}", &rgb[2..]).to_lowercase()));
                            }
                        }
                        JsonValue::Object(f)
                    });
                    json!([text, format])
                })
                .collect();
            Some(JsonValue::Array(parts))
        })
        .collect()
}

/// `'A Sheet'!$A$1:$M$28` → `{top,left,bottom,right}`; None for a multi-range
/// area, which cropping to one range would silently cut short.
fn print_range(range: &str) -> Option<JsonValue> {
    if range.contains(',') {
        return None;
    }
    let (from, to) = range.rsplit('!').next()?.split_once(':')?;
    let ((left, top), (right, bottom)) = (cell_ref(from)?, cell_ref(to)?);
    Some(json!({ "top": top, "left": left, "bottom": bottom, "right": right }))
}

/// The five XML entities, for attribute values shown to people.
fn xml_unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Page geometry from the first visible worksheet's print settings.
///
/// `<pageSetup paperSize orientation>` names the paper — 9 is A4, 1 is Letter,
/// and anything else falls back to A4 rather than guessing — while
/// `<pageMargins>` gives the margins in inches. Landscape swaps width and
/// height, which is the common case for a worksheet and the reason this is
/// worth reading at all: rendered portrait, a wide sheet loses its right-hand
/// columns.
fn sheet_page_setup(unpacked: &std::path::Path, sheets: &[Sheet]) -> JsonValue {
    // The first sheet that prints, not `sheet1.xml`: the file numbering
    // follows creation order, not the tab order the export follows.
    let path = sheets
        .iter()
        .find(|s| !s.hidden)
        .map(|s| s.path.clone())
        .unwrap_or_else(|| unpacked.join("xl").join("worksheets").join("sheet1.xml"));
    let Ok(sheet) = std::fs::read_to_string(path) else {
        return JsonValue::Null;
    };
    let attr = |tag: &str, name: &str| -> Option<String> {
        let i = sheet.find(tag)?;
        let end = sheet[i..].find('>')? + i;
        let frag = &sheet[i..end];
        let key = format!("{name}=\"");
        let j = frag.find(&key)? + key.len();
        let k = frag[j..].find('"')? + j;
        Some(frag[j..k].to_string())
    };
    let inches = |name: &str, fallback: f64| -> i64 {
        let value = attr("<pageMargins", name)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(fallback);
        (value * 96.0).round() as i64
    };
    // A4 (210×297mm) and Letter (8.5×11in) at 96 dpi.
    let (mut width, mut height) = match attr("<pageSetup", "paperSize").as_deref() {
        Some("1") | Some("2") => (816, 1056),
        _ => (794, 1123),
    };
    if attr("<pageSetup", "orientation").as_deref() == Some("landscape") {
        std::mem::swap(&mut width, &mut height);
    }
    json!({
        "width": width,
        "height": height,
        "top": inches("top", 0.75),
        "right": inches("right", 0.7),
        "bottom": inches("bottom", 0.75),
        "left": inches("left", 0.7),
    })
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

/// Whether the document's first section shows a distinct first page
/// (`<w:titlePg/>`, or `<w:titlePg w:val="1|true"/>`).
fn first_page_distinct(document: &str) -> bool {
    let Some(section) = split_tags(document, "w:sectPr").into_iter().next() else {
        return false;
    };
    section.contains("<w:titlePg/>")
        || split_tags(&section, "w:titlePg")
            .first()
            .is_some_and(|t| !matches!(attr(t, "w:val").as_deref(), Some("0" | "false" | "off")))
}

/// File names (in `word/media`) of the pictures in the header page one
/// shows: the first-page header when the first section turns it on, the
/// default one otherwise. None when that cannot be worked out, so the caller
/// keeps every picture rather than guess.
fn page_one_header_media(unpacked: &std::path::Path) -> Option<Vec<String>> {
    let word = unpacked.join("word");
    let document = std::fs::read_to_string(word.join("document.xml")).ok()?;
    let rels = std::fs::read_to_string(word.join("_rels").join("document.xml.rels")).ok()?;
    let section = split_tags(&document, "w:sectPr").into_iter().next()?;
    let wanted = if first_page_distinct(&document) { "first" } else { "default" };
    let rid = split_tags(&section, "w:headerReference")
        .into_iter()
        .find(|h| attr(h, "w:type").as_deref() == Some(wanted))
        .and_then(|h| attr(&h, "r:id"));
    let Some(rid) = rid else {
        return Some(Vec::new()); // page one has no header: no header pictures
    };
    let target = split_tags(&rels, "Relationship")
        .into_iter()
        .find(|r| attr(r, "Id").as_deref() == Some(rid.as_str()))
        .and_then(|r| attr(&r, "Target"))?;
    let header = target.trim_start_matches('/').trim_start_matches("word/");
    let header_rels = std::fs::read_to_string(
        word.join("_rels").join(format!("{}.rels", header.rsplit('/').next()?)),
    )
    .unwrap_or_default();
    Some(
        split_tags(&header_rels, "Relationship")
            .iter()
            .filter_map(|r| attr(r, "Target"))
            .filter_map(|t| t.rsplit('/').next().map(str::to_string))
            .collect(),
    )
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
    // A first-page footer Word never shows: the section does not turn the
    // distinct first page on.
    if !first_page_distinct(&refs) {
        first = None;
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
    // Only the pictures of the header page one actually shows. A document
    // with a first-page header it never switched on (no <w:titlePg/>) carried
    // that header's big logo into the import.
    if let Some(shown) = page_one_header_media(unpacked) {
        entries.retain(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| shown.iter().any(|s| s == n))
        });
    }

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


#[cfg(test)]
mod sheet_tests {
    use super::*;

    #[test]
    fn reads_cell_sizes_wrap_and_per_sheet_print_areas() {
        let dir = std::env::temp_dir().join(format!("qims-sheet-test-{}", std::process::id()));
        let xl = dir.join("xl");
        std::fs::create_dir_all(xl.join("_rels")).unwrap();
        std::fs::create_dir_all(xl.join("worksheets")).unwrap();
        std::fs::write(
            xl.join("workbook.xml"),
            r#"<workbook><sheets><sheet name="Hidden" sheetId="9" state="hidden" r:id="rId2"/><sheet name="GC &amp; MS" sheetId="1" r:id="rId1"/></sheets><definedNames><definedName name="_xlnm.Print_Area" localSheetId="1">'GC &amp; MS'!$A$1:$M$28</definedName></definedNames></workbook>"#,
        )
        .unwrap();
        std::fs::write(
            xl.join("_rels").join("workbook.xml.rels"),
            r#"<Relationships><Relationship Id="rId1" Type="t" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="t" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#,
        )
        .unwrap();
        std::fs::write(
            xl.join("styles.xml"),
            r#"<styleSheet><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="10"/></font></fonts><cellStyleXfs><xf fontId="1"/></cellStyleXfs><cellXfs count="2"><xf fontId="0"/><xf fontId="1" applyAlignment="1"><alignment horizontal="center" wrapText="1"/></xf></cellXfs></styleSheet>"#,
        )
        .unwrap();
        std::fs::write(
            xl.join("worksheets").join("sheet1.xml"),
            r#"<worksheet><cols><col min="1" max="1" width="10"/></cols><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" s="1" t="s"><v>1</v></c><c r="C1" s="1"/></row></sheetData></worksheet>"#,
        )
        .unwrap();
        std::fs::write(xl.join("worksheets").join("sheet2.xml"), "<worksheet/>").unwrap();

        let sheets = workbook_sheets(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(sheets.len(), 2);
        assert!(sheets[0].hidden);
        let gc = &sheets[1].json;
        assert_eq!(gc["name"], json!("GC & MS"));
        assert_eq!(gc["print_area"], json!({ "top": 0, "left": 0, "bottom": 27, "right": 12 }));
        assert_eq!(gc["cells"]["A1"], json!([11.0, false]));
        assert_eq!(gc["cells"]["B1"], json!([10.0, true]));
        assert!(gc["cells"].get("C1").is_none(), "empty cells are left out");
        assert!(too_large(&sheets).is_none());
        let filled: serde_json::Map<String, JsonValue> =
            (0..=MAX_SHEET_CELLS).map(|i| (format!("A{i}"), json!([null, false]))).collect();
        let big = Sheet { hidden: false, path: dir.clone(), json: json!({ "cells": filled }) };
        let refusal = too_large(std::slice::from_ref(&big)).expect("over the limit");
        assert!(refusal.contains("20001 filled cells"), "{refusal}");
        let hidden = Sheet { hidden: true, ..big };
        assert!(too_large(&[hidden]).is_none(), "hidden sheets do not count");

        let sst = r#"<sst><si><t>plain</t></si><si><r><rPr><b/><sz val="10"/><color rgb="FFFF0000"/></rPr><t>BW =</t></r><r><t xml:space="preserve"> Big well</t></r><r><rPr><vertAlign val="superscript"/><b val="0"/></rPr><t>a</t></r></si></sst>"#;
        let rich = shared_rich_text(sst);
        assert!(rich[0].is_none());
        assert_eq!(
            rich[1],
            Some(json!([
                ["BW =", { "b": true, "sz": 10.0, "color": "#ff0000" }],
                [" Big well", null],
                ["a", { "sup": true }]
            ]))
        );
    }
}
