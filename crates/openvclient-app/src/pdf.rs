//! PDF previews without bundling a PDF engine: the first page is rendered
//! by a tool the system usually has (`sips` on macOS, Poppler's `pdftoppm`
//! elsewhere). Without one, the preview falls back to the page count and an
//! "Open in viewer" button.

use std::path::PathBuf;

/// A scratch directory for one preview, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch() -> std::io::Result<Scratch> {
    let dir = std::env::temp_dir().join(format!("openvclient-pdf-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(Scratch(dir))
}

/// Page count from the document's page objects (good enough for a label).
pub fn page_count(bytes: &[u8]) -> usize {
    let mut n = 0;
    let pat = b"/Type";
    let mut i = 0;
    while i + pat.len() < bytes.len() {
        if &bytes[i..i + pat.len()] == pat {
            let mut j = i + pat.len();
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\n' || bytes[j] == b'\r') {
                j += 1;
            }
            if bytes[j..].starts_with(b"/Page") && !bytes[j..].starts_with(b"/Pages") {
                n += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    n
}

/// Render page 1 to PNG bytes with whichever system tool is available.
pub async fn first_page_png(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let dir = scratch().map_err(|e| e.to_string())?;
    let input = dir.0.join("in.pdf");
    std::fs::write(&input, &bytes).map_err(|e| e.to_string())?;
    let out = dir.0.join("page.png");
    let mut tried = Vec::new();
    if cfg!(target_os = "macos") {
        tried.push("sips");
        let st = openvclient_core::process::command("sips").arg("-s").arg("format").arg("png").arg(&input).arg("--out").arg(&out).output().await;
        if matches!(&st, Ok(o) if o.status.success()) && out.exists() {
            return std::fs::read(&out).map_err(|e| e.to_string());
        }
    }
    tried.push("pdftoppm");
    let base = dir.0.join("page");
    let st = openvclient_core::process::command("pdftoppm").args(["-png", "-r", "110", "-f", "1", "-l", "1", "-singlefile"]).arg(&input).arg(&base).output().await;
    if matches!(&st, Ok(o) if o.status.success()) && out.exists() {
        return std::fs::read(&out).map_err(|e| e.to_string());
    }
    Err(format!("No PDF renderer found ({}). Install Poppler to see pages here, or open the file in your viewer.", tried.join(", ")))
}

/// Save the PDF to a temporary file and open it with the system viewer.
pub fn open_in_viewer(name: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = std::env::temp_dir().join("openvclient-open");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let path = dir.join(if safe.to_ascii_lowercase().ends_with(".pdf") { safe } else { format!("{safe}.pdf") });
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    crate::update::open_url(&path.display().to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_pages_not_page_trees() {
        let doc = b"%PDF-1.4\n1 0 obj << /Type /Pages /Kids [2 0 R 3 0 R] >> endobj\n2 0 obj << /Type /Page >> endobj\n3 0 obj <</Type/Page>> endobj";
        assert_eq!(super::page_count(doc), 2);
    }
}
