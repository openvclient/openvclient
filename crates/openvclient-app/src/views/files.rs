//! File-manager view for storage connections (Azure Blob, S3 and compatible
//! providers, FTP/FTPS, SFTP): breadcrumb navigation with back/forward/up,
//! a sortable file list with multi-select, upload (button or drag and drop),
//! download, new folder, rename, share link, delete, and a preview panel.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Key, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use egui_material_icons::icons::*;
use openvclient_core::resource::{ActionOutcome, Args, Content, FileEntry, FileListing, ResourceSession};
use openvclient_core::{Error, Result};
use uuid::Uuid;

use crate::app::AppCtx;
use crate::bg::{Job, poll};
use crate::theme::{self, ico};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Modified,
    Kind,
}

/// A batch of actions run in the background (uploads, downloads, deletes).
struct Batch {
    label: String,
    job: Job<Vec<(String, Result<ActionOutcome>)>>,
    /// Per-item status, updated by the running batch.
    items: Arc<std::sync::Mutex<Vec<BatchItem>>>,
}

#[derive(Clone)]
struct BatchItem {
    name: String,
    /// Local file size for uploads (weights the progress bar); 0 when unknown.
    bytes: u64,
    state: ItemState,
}

#[derive(Clone, PartialEq)]
enum ItemState {
    Waiting,
    Running,
    Done,
    Failed(String),
}

enum Modal {
    NewFolder(String),
    Rename(FileEntry, String),
    Share(String),
}

pub struct FilesTab {
    pub conn: Uuid,
    /// Current folder; `None` = top level (buckets / containers).
    cwd: Option<String>,
    back: Vec<Option<String>>,
    forward: Vec<Option<String>>,
    listing: Option<Result<FileListing>>,
    job: Option<Job<Result<FileListing>>>,
    filter: String,
    applied_filter: String,
    sort: (SortBy, bool),
    selected: BTreeSet<String>,
    anchor: Option<usize>,
    preview: Option<(FileEntry, Option<Result<Content>>)>,
    /// Width of the preview pane; drag its left edge to change it.
    preview_w: f32,
    /// Right-click on empty space in the list opens this menu at the pointer.
    blank_menu: Option<egui::Pos2>,
    /// Decoded preview image, keyed by entry id (or the decode error).
    preview_tex: Option<(String, std::result::Result<egui::TextureHandle, String>)>,
    /// First PDF page being rendered for the preview: (entry id, job).
    pdf_job: Option<(String, Job<std::result::Result<Vec<u8>, String>>)>,
    pdf_png: Option<(String, std::result::Result<Vec<u8>, String>)>,
    preview_job: Option<Job<Result<Content>>>,
    batch: Option<Batch>,
    /// Items inside each listed folder, counted in the background: (count, more).
    counts: std::collections::HashMap<String, (usize, bool)>,
    count_job: Option<Job<Vec<(String, Result<(usize, bool)>)>>>,
    modal: Option<Modal>,
    delete_confirmed: Arc<AtomicBool>,
    pending_delete: Vec<FileEntry>,
    started: bool,
}

/// File type icon from the extension.
fn file_icon(name: &str) -> egui_material_icons::MaterialIcon {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic" => ICON_IMAGE,
        "json" | "yaml" | "yml" | "toml" | "xml" => ICON_DATA_OBJECT,
        "csv" | "tsv" | "parquet" | "xlsx" | "xls" | "avro" | "orc" => ICON_TABLE_CHART,
        "zip" | "gz" | "tgz" | "tar" | "bz2" | "xz" | "7z" | "rar" | "zst" => ICON_FOLDER_ZIP,
        "pdf" => ICON_PICTURE_AS_PDF,
        "mp4" | "mov" | "mkv" | "avi" | "webm" => ICON_MOVIE,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => ICON_AUDIO_FILE,
        "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "cpp" | "h" | "sh" | "sql" | "html" | "css" => ICON_CODE,
        "txt" | "md" | "log" | "ini" | "conf" | "cfg" => ICON_DESCRIPTION,
        _ => ICON_DRAFT,
    }
}

fn human(n: u64) -> String {
    openvclient_core::containers::human_bytes(n)
}

fn when(t: &chrono::DateTime<chrono::Utc>) -> String {
    let local = t.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    if local.date_naive() == today { format!("Today {}", local.format("%H:%M")) } else { local.format("%Y-%m-%d %H:%M").to_string() }
}

fn kind_label(e: &FileEntry) -> String {
    if e.is_dir {
        return match e.kind.as_str() {
            "container" => "Container".into(),
            "bucket" => "Bucket".into(),
            _ => "Folder".into(),
        };
    }
    let ext = e.name.rsplit_once('.').map(|(_, x)| x.to_ascii_uppercase()).unwrap_or_default();
    if ext.is_empty() { "File".into() } else { format!("{ext} file") }
}

impl FilesTab {
    pub fn new(conn: Uuid, start: Option<String>) -> Self {
        let started = start.is_some();
        Self {
            conn,
            cwd: start,
            back: Vec::new(),
            forward: Vec::new(),
            listing: None,
            job: None,
            filter: String::new(),
            applied_filter: String::new(),
            sort: (SortBy::Name, true),
            selected: BTreeSet::new(),
            anchor: None,
            preview: None,
            preview_w: 340.0,
            blank_menu: None,
            preview_tex: None,
            pdf_job: None,
            pdf_png: None,
            preview_job: None,
            batch: None,
            counts: Default::default(),
            count_job: None,
            modal: None,
            delete_confirmed: Arc::new(AtomicBool::new(false)),
            pending_delete: Vec::new(),
            started,
        }
    }

    /// Open `node` (a folder id) from outside, e.g. the sidebar.
    pub fn navigate(&mut self, node: Option<String>) {
        if node != self.cwd {
            self.go(node);
        }
    }

    /// Test hook: select an entry by name as a single click would.
    #[cfg(test)]
    pub fn click_name(&mut self, cx: &AppCtx, name: &str) {
        let entries = self.entries();
        if let Some((i, e)) = entries.iter().enumerate().find(|(_, e)| e.name == name) {
            self.selected = BTreeSet::from([e.id.clone()]);
            self.anchor = Some(i);
            if !e.is_dir {
                self.show_preview(cx, e.clone());
            }
        }
    }

    pub fn title(&self) -> String {
        self.cwd.as_deref().map(|c| c.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(c).to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Files".into())
    }

    fn session(cx: &AppCtx, conn: Uuid) -> Option<Arc<dyn ResourceSession>> {
        cx.conns.session(conn).and_then(|h| h.resource()).cloned()
    }

    fn go(&mut self, node: Option<String>) {
        self.back.push(self.cwd.take());
        self.forward.clear();
        self.cwd = node;
        self.reset_view();
    }

    fn reset_view(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.preview = None;
        self.preview_job = None;
        self.filter.clear();
        self.applied_filter.clear();
        self.listing = None;
        self.job = None;
    }

    fn load(&mut self, cx: &AppCtx) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let (node, filter) = (self.cwd.clone(), self.applied_filter.clone());
        self.job = Some(cx.bg.spawn("Loading folder", async move { s.list_dir(node.as_deref(), &filter).await }));
    }

    fn entries(&self) -> Vec<FileEntry> {
        let mut v = match &self.listing {
            Some(Ok(l)) => l.entries.clone(),
            _ => Vec::new(),
        };
        let (by, asc) = self.sort;
        v.sort_by(|a, b| {
            // Folders first, then the chosen column.
            let o = b.is_dir.cmp(&a.is_dir).then_with(|| match by {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified.cmp(&b.modified),
                SortBy::Kind => kind_label(a).cmp(&kind_label(b)),
            });
            if asc || a.is_dir != b.is_dir { o } else { o.reverse() }
        });
        v
    }

    fn selection(&self) -> Vec<FileEntry> {
        self.entries().into_iter().filter(|e| self.selected.contains(&e.id)).collect()
    }

    fn open_entry(&mut self, cx: &AppCtx, e: &FileEntry) {
        if e.is_dir {
            self.go(Some(e.id.clone()));
        } else {
            self.show_preview(cx, e.clone());
        }
    }

    fn show_preview(&mut self, cx: &AppCtx, e: FileEntry) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let id = e.id.clone();
        self.preview = Some((e, None));
        self.preview_job = Some(cx.bg.spawn("Loading preview", async move { s.open(&id, Default::default()).await }));
    }

    /// Run `(node, action, args)` items one after another in the background.
    fn run_batch(&mut self, cx: &AppCtx, label: String, items: Vec<(String, &'static str, Args)>) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let total = items.len();
        let status: Vec<BatchItem> = items
            .iter()
            .map(|(node, _, args)| {
                let full = args.get("name").or(args.get("file")).cloned().unwrap_or_else(|| node.clone());
                let name = full.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or(&full).to_string();
                let bytes = args.get("file").and_then(|f| std::fs::metadata(f).ok()).map(|m| m.len()).unwrap_or(0);
                BatchItem { name, bytes, state: ItemState::Waiting }
            })
            .collect();
        let shared = Arc::new(std::sync::Mutex::new(status));
        let progress = shared.clone();
        let job = cx.bg.spawn(label.clone(), async move {
            let mut out = Vec::with_capacity(total);
            for (i, (node, action, args)) in items.into_iter().enumerate() {
                let name = args.get("name").or(args.get("file")).cloned().unwrap_or_else(|| node.clone());
                if let Ok(mut p) = progress.lock() {
                    p[i].state = ItemState::Running;
                }
                let r = s.run_action(&node, action, args).await;
                if let Ok(mut p) = progress.lock() {
                    p[i].state = match &r {
                        Ok(_) => ItemState::Done,
                        Err(e) => ItemState::Failed(e.message.clone()),
                    };
                }
                out.push((name, r));
            }
            out
        });
        self.batch = Some(Batch { label, job, items: shared });
    }

    fn upload_paths(&mut self, cx: &mut AppCtx, paths: Vec<PathBuf>) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let (Some(dir), Some(action)) = (self.cwd.clone(), s.file_actions().upload) else {
            cx.toasts.info("Open a folder first", "Files are uploaded into the folder that is open.");
            return;
        };
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
        if files.is_empty() {
            cx.toasts.info("Nothing to upload", "Folders can't be uploaded yet; drop or pick files.");
            return;
        }
        let n = files.len();
        let items = files
            .into_iter()
            .map(|p| {
                let mut a = Args::new();
                a.insert("file".into(), p.display().to_string());
                (dir.clone(), action, a)
            })
            .collect();
        self.run_batch(cx, format!("Uploading {n} file{}", if n == 1 { "" } else { "s" }), items);
    }

    fn download(&mut self, cx: &AppCtx, files: Vec<FileEntry>) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let Some(action) = s.file_actions().download else { return };
        let files: Vec<FileEntry> = files.into_iter().filter(|f| !f.is_dir).collect();
        let targets: Vec<(FileEntry, PathBuf)> = match files.len() {
            0 => return,
            1 => match rfd::FileDialog::new().set_file_name(&files[0].name).save_file() {
                Some(p) => vec![(files[0].clone(), p)],
                None => return,
            },
            _ => match rfd::FileDialog::new().set_title("Download to folder").pick_folder() {
                Some(dir) => files
                    .into_iter()
                    .map(|f| {
                        let p = dir.join(&f.name);
                        (f, p)
                    })
                    .collect(),
                None => return,
            },
        };
        let n = targets.len();
        let items = targets
            .into_iter()
            .map(|(f, p)| {
                let mut a = Args::new();
                a.insert("file".into(), p.display().to_string());
                a.insert("name".into(), f.name.clone());
                (f.id, action, a)
            })
            .collect();
        self.run_batch(cx, format!("Downloading {n} file{}", if n == 1 { "" } else { "s" }), items);
    }

    fn ask_delete(&mut self, cx: &mut AppCtx, items: Vec<FileEntry>) {
        if items.is_empty() {
            return;
        }
        let names: Vec<String> = items.iter().take(8).map(|e| format!("{}{}", e.name, if e.is_dir { "/" } else { "" })).collect();
        let more = items.len().saturating_sub(8);
        let dirs = items.iter().filter(|e| e.is_dir).count();
        let mut body = format!("This permanently deletes:\n{}", names.join("\n"));
        if more > 0 {
            body.push_str(&format!("\n… and {more} more"));
        }
        if dirs > 0 {
            body.push_str("\n\nFolders are deleted with everything inside them.");
        }
        let flag = self.delete_confirmed.clone();
        self.pending_delete = items;
        let n = self.pending_delete.len();
        cx.confirm(Some(self.conn), format!("Delete {n} item{}?", if n == 1 { "" } else { "s" }), body, "Delete", None, move |_| flag.store(true, Ordering::SeqCst));
    }

    fn do_delete(&mut self, cx: &AppCtx) {
        let Some(s) = Self::session(cx, self.conn) else { return };
        let fa = s.file_actions();
        let items: Vec<(String, &'static str, Args)> = std::mem::take(&mut self.pending_delete)
            .into_iter()
            .filter_map(|e| {
                let action = if e.is_dir { fa.delete_dir } else { fa.delete_file }?;
                let mut a = Args::new();
                a.insert("name".into(), e.name.clone());
                Some((e.id, action, a))
            })
            .collect();
        let n = items.len();
        if n > 0 {
            self.run_batch(cx, format!("Deleting {n} item{}", if n == 1 { "" } else { "s" }), items);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        let Some(s) = Self::session(cx, self.conn) else {
            crate::views::unsupported(ui, "Not connected. Connect from the sidebar to browse files.");
            return;
        };
        if !self.started {
            self.started = true;
            self.cwd = s.root_dir();
        }
        let fa = s.file_actions();
        let read_only = cx.store.profile(self.conn).is_some_and(|p| p.read_only);

        // ---- background results -------------------------------------------------
        if self.listing.is_none() && self.job.is_none() {
            self.load(cx);
        }
        if let Some(r) = poll(&mut self.job) {
            // Count what is inside each folder, one quick listing at a time.
            if let Ok(l) = &r {
                let dirs: Vec<String> = l.entries.iter().filter(|e| e.is_dir).take(60).map(|e| e.id.clone()).collect();
                if !dirs.is_empty() {
                    let s2 = s.clone();
                    self.count_job = Some(cx.bg.spawn("Counting folder items", async move {
                        let mut out = Vec::with_capacity(dirs.len());
                        for d in dirs {
                            let r = s2.list_dir(Some(&d), "").await.map(|l| (l.entries.len(), l.truncated));
                            out.push((d, r));
                        }
                        out
                    }));
                }
            }
            self.listing = Some(r);
        }
        if let Some(rs) = poll(&mut self.count_job) {
            for (id, r) in rs {
                if let Ok(c) = r {
                    self.counts.insert(id, c);
                }
            }
        }
        if let Some(r) = poll(&mut self.preview_job)
            && let Some((_, c)) = &mut self.preview
        {
            *c = Some(r);
        }
        if let Some(b) = &mut self.batch
            && let Some(results) = b.job.take()
        {
            let label = b.label.clone();
            self.batch = None;
            let failed: Vec<&(String, Result<ActionOutcome>)> = results.iter().filter(|(_, r)| r.is_err()).collect();
            let ok = results.len() - failed.len();
            if let Some((_, Ok(o))) = results.first()
                && let Some(Content::Text(url)) = &o.content
                && results.len() == 1
                && url.starts_with("http")
            {
                self.modal = Some(Modal::Share(url.clone()));
            } else if failed.is_empty() {
                let msg = results.first().and_then(|(_, r)| r.as_ref().ok()).map(|o| o.message.clone()).unwrap_or_default();
                cx.toasts.ok(if results.len() == 1 { msg } else { format!("{label}: done ({ok})") });
            } else {
                let (name, err) = failed[0];
                let e = err.as_ref().err().cloned().unwrap_or_else(|| Error::other("failed"));
                cx.toasts.error(format!("{label}: {} of {} failed (first: {name})", failed.len(), results.len()), &e);
            }
            self.listing = None; // refresh
        }
        if self.delete_confirmed.swap(false, Ordering::SeqCst) {
            self.do_delete(cx);
        }

        // ---- drag and drop --------------------------------------------------------
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() && ui.ui_contains_pointer() {
            if read_only {
                cx.toasts.info("Read-only connection", "Uploads are turned off for this connection.");
            } else {
                self.upload_paths(cx, dropped);
            }
        }

        // ---- navigation bar ---------------------------------------------------------
        let conn_name = cx.profile_name(self.conn);
        let crumbs = self.cwd.as_deref().map(|c| s.breadcrumbs(c)).unwrap_or_default();
        let root = s.root_dir();
        let mut nav: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            let b = |ui: &mut egui::Ui, icon, tip: &str, on: bool| {
                ui.add_enabled(on, egui::Button::new(String::from(icon)).min_size(egui::vec2(30.0, 28.0))).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
            };
            if b(ui, ICON_ARROW_BACK, "Back", !self.back.is_empty()).clicked() {
                let prev = self.back.pop().unwrap_or(None);
                self.forward.push(self.cwd.take());
                self.cwd = prev;
                self.reset_view();
            }
            if b(ui, ICON_ARROW_FORWARD, "Forward", !self.forward.is_empty()).clicked() {
                let next = self.forward.pop().unwrap_or(None);
                self.back.push(self.cwd.take());
                self.cwd = next;
                self.reset_view();
            }
            let up = if crumbs.len() >= 2 {
                Some(Some(crumbs[crumbs.len() - 2].0.clone()))
            } else if self.cwd.is_some() && root.is_none() {
                Some(None)
            } else {
                None
            };
            if b(ui, ICON_ARROW_UPWARD, "Up one level", up.is_some()).clicked() {
                nav = up.clone();
            }
            ui.add_space(theme::space::XS);
            // Breadcrumb: connection › container › folder › …
            egui::Frame::new().fill(t.muted).corner_radius(6.0).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
                ui.set_max_width((ui.available_width() - 280.0).max(200.0));
                egui::ScrollArea::horizontal().id_salt("crumbs").stick_to_right(true).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let link = |ui: &mut egui::Ui, text: String, current: bool| {
                            let rt = if current { RichText::new(text).strong().color(t.fg) } else { RichText::new(text).color(t.muted_fg) };
                            ui.add(egui::Label::new(rt).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand)
                        };
                        let at_root = self.cwd.is_none() || (root.is_some() && self.cwd == root && crumbs.len() <= 1);
                        let first = if theme::is_cartoon() {
                            // Cartoon: the connection's sticker, then its name.
                            let driver = cx.store.profile(self.conn).map(|p| p.driver.clone()).unwrap_or_default();
                            crate::views::driver_glyph(ui, &driver, 14.0, t.muted_fg);
                            conn_name.clone()
                        } else {
                            ico(ICON_CLOUD, &conn_name)
                        };
                        if link(ui, first, at_root && crumbs.is_empty()).clicked() && root.is_none() {
                            nav = Some(None);
                        }
                        for (i, (id, label)) in crumbs.iter().enumerate() {
                            ui.label(RichText::new(String::from(ICON_CHEVRON_RIGHT)).color(t.muted_fg));
                            let current = i == crumbs.len() - 1;
                            if link(ui, label.clone(), current).clicked() && !current {
                                nav = Some(Some(id.clone()));
                            }
                        }
                    });
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if b(ui, ICON_REFRESH, "Refresh (F5)", true).clicked() || ui.input(|i| i.key_pressed(Key::F5)) {
                    self.listing = None;
                    self.job = None;
                }
                let r = ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(ico(ICON_SEARCH, "Filter this folder")).desired_width(220.0));
                if (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || (r.changed() && self.filter.is_empty()) {
                    self.applied_filter = self.filter.clone();
                    self.listing = None;
                    self.job = None;
                }
            });
        });
        if let Some(n) = nav {
            self.go(n);
            return;
        }

        // ---- action bar ---------------------------------------------------------------
        let sel = self.selection();
        let files_sel: Vec<FileEntry> = sel.iter().filter(|e| !e.is_dir).cloned().collect();
        let in_folder = self.cwd.is_some();
        let busy = self.batch.is_some();
        ui.add_space(theme::space::XS);
        ui.horizontal(|ui| {
            let can_write = !read_only && in_folder && !busy;
            if fa.upload.is_some()
                && ui
                    .add_enabled(can_write, egui::Button::new(RichText::new(ico(ICON_UPLOAD, "Upload")).color(t.on_accent)).fill(t.accent))
                    .on_hover_text("Upload files into this folder (or drop files here)")
                    .on_disabled_hover_text(if read_only { "Read-only connection" } else { "Open a folder to upload into" })
                    .clicked()
                && let Some(paths) = rfd::FileDialog::new().set_title("Upload files").pick_files()
            {
                self.upload_paths(cx, paths);
            }
            if fa.mkdir.is_some() && ui.add_enabled(can_write, egui::Button::new(ico(ICON_CREATE_NEW_FOLDER, "New folder"))).clicked() {
                self.modal = Some(Modal::NewFolder(String::new()));
            }
            ui.separator();
            let dl_label = if files_sel.len() > 1 { format!("Download ({})", files_sel.len()) } else { "Download".into() };
            if fa.download.is_some() && ui.add_enabled(!files_sel.is_empty() && !busy, egui::Button::new(ico(ICON_DOWNLOAD, &dl_label))).on_disabled_hover_text("Select one or more files").clicked() {
                self.download(cx, files_sel.clone());
            }
            if let Some(share) = fa.share
                && ui
                    .add_enabled(files_sel.len() == 1 && sel.len() == 1 && !busy, egui::Button::new(ico(ICON_LINK, "Share link")))
                    .on_disabled_hover_text("Select one file")
                    .on_hover_text("Create a temporary download link")
                    .clicked()
            {
                let mut a = Args::new();
                a.insert("hours".into(), "24".into());
                self.run_batch(cx, "Creating link".into(), vec![(files_sel[0].id.clone(), share, a)]);
            }
            if fa.rename.is_some() && ui.add_enabled(sel.len() == 1 && can_write, egui::Button::new(ico(ICON_DRIVE_FILE_RENAME_OUTLINE, "Rename"))).clicked() {
                self.modal = Some(Modal::Rename(sel[0].clone(), sel[0].name.clone()));
            }
            let can_delete =
                !sel.is_empty() && !read_only && !busy && sel.iter().all(|e| if e.is_dir { fa.delete_dir.is_some() && e.kind != "container" && e.kind != "bucket" } else { fa.delete_file.is_some() });
            if ui
                .add_enabled(can_delete, egui::Button::new(RichText::new(ico(ICON_DELETE, "Delete")).color(if can_delete { t.destructive } else { t.muted_fg })))
                .on_disabled_hover_text("Select files or folders")
                .clicked()
            {
                self.ask_delete(cx, sel.clone());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total = match &self.listing {
                    Some(Ok(l)) => l.entries.len(),
                    _ => 0,
                };
                let bytes: u64 = sel.iter().filter_map(|e| e.size).sum();
                let text = if sel.is_empty() {
                    format!("{total} item{}", if total == 1 { "" } else { "s" })
                } else {
                    format!("{} of {total} selected{}", sel.len(), if bytes > 0 { format!(" · {}", human(bytes)) } else { String::new() })
                };
                ui.label(RichText::new(text).color(t.muted_fg));
            });
        });
        if let Some(b) = &self.batch {
            let items = b.items.lock().map(|v| v.clone()).unwrap_or_default();
            let done = items.iter().filter(|i| matches!(i.state, ItemState::Done | ItemState::Failed(_))).count();
            let failed = items.iter().filter(|i| matches!(i.state, ItemState::Failed(_))).count();
            // Weight by size when sizes are known (uploads), else by item count.
            let total_b: u64 = items.iter().map(|i| i.bytes).sum();
            let frac = if total_b > 0 {
                items.iter().filter(|i| matches!(i.state, ItemState::Done | ItemState::Failed(_))).map(|i| i.bytes).sum::<u64>() as f32 / total_b as f32
            } else {
                done as f32 / items.len().max(1) as f32
            };
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(format!("{} · {done} of {}", b.label, items.len())).color(t.info));
                if total_b > 0 {
                    ui.label(RichText::new(human(total_b)).color(t.muted_fg));
                }
                if failed > 0 {
                    ui.label(RichText::new(format!("{failed} failed")).color(t.destructive));
                }
            });
            ui.add(egui::ProgressBar::new(frac).desired_height(6.0).fill(t.accent));
            if items.len() > 1 {
                egui::CollapsingHeader::new(RichText::new("Files").small().color(t.muted_fg)).id_salt(("batch_items", self.conn)).default_open(items.len() <= 8).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for it in &items {
                            ui.horizontal(|ui| {
                                let (icon, color) = match &it.state {
                                    ItemState::Waiting => (ICON_SCHEDULE, t.muted_fg),
                                    ItemState::Running => (ICON_SYNC, t.info),
                                    ItemState::Done => (ICON_CHECK_CIRCLE, t.accent),
                                    ItemState::Failed(_) => (ICON_ERROR, t.destructive),
                                };
                                ui.label(RichText::new(String::from(icon)).color(color));
                                let r = ui.add(egui::Label::new(RichText::new(&it.name).small()).truncate());
                                if let ItemState::Failed(m) = &it.state {
                                    r.on_hover_text(m);
                                }
                                if it.bytes > 0 {
                                    ui.label(RichText::new(human(it.bytes)).small().color(t.muted_fg));
                                }
                            });
                        }
                    });
                });
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        ui.add_space(theme::space::XS);

        // ---- body: list + preview -------------------------------------------------------
        let preview_w = if self.preview.is_some() { self.preview_w.clamp(220.0, (ui.available_width() * 0.6).max(220.0)) } else { 0.0 };
        let area = ui.available_rect_before_wrap();
        let gap = if preview_w > 0.0 { 12.0 } else { 0.0 };
        let list_rect = egui::Rect::from_min_max(area.min, egui::pos2(area.max.x - preview_w - gap, area.max.y));
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        self.list_ui(&mut list_ui, cx, &s, hovering, read_only);
        if self.preview.is_some() {
            let prev_rect = egui::Rect::from_min_max(egui::pos2(area.max.x - preview_w, area.min.y), area.max);
            // Draggable divider between the list and the preview.
            let x = prev_rect.left() - gap / 2.0;
            let handle = egui::Rect::from_min_max(egui::pos2(x - 4.0, area.top()), egui::pos2(x + 4.0, area.bottom()));
            let hr = ui.interact(handle, egui::Id::unique(("preview_split", self.conn)), Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            if hr.dragged() {
                self.preview_w = (preview_w - hr.drag_delta().x).max(220.0);
            }
            let col = if hr.hovered() || hr.dragged() { t.ring } else { t.border };
            ui.painter().vline(x, area.y_range(), egui::Stroke::new(if hr.hovered() { 2.0 } else { 1.0 }, col));
            let mut pui = ui.new_child(egui::UiBuilder::new().max_rect(prev_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            self.preview_ui(&mut pui, cx);
        }
        ui.allocate_rect(area, Sense::hover());

        self.modal_ui(ui.ctx(), cx, &s);
    }

    fn list_ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx, s: &Arc<dyn ResourceSession>, hovering: bool, read_only: bool) {
        let t = theme::tokens(ui);
        let rect = ui.available_rect_before_wrap();
        match &self.listing {
            None => {
                ui.add_space(theme::space::XL);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Loading…").color(t.muted_fg));
                });
                return;
            }
            Some(Err(e)) => {
                crate::views::error_box(ui, e);
                if ui.button(ico(ICON_REFRESH, "Try again")).clicked() {
                    self.listing = None;
                }
                return;
            }
            Some(Ok(l)) if l.entries.is_empty() => {
                ui.add_space(theme::space::XL * 2.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(String::from(ICON_FOLDER_OPEN)).size(44.0).color(t.muted_fg));
                    ui.label(RichText::new(if self.applied_filter.is_empty() { "This folder is empty" } else { "No items match the filter" }).size(16.0).color(t.fg));
                    if self.cwd.is_some() && !read_only && s.file_actions().upload.is_some() {
                        ui.label(RichText::new("Drop files here or use Upload").color(t.muted_fg));
                    }
                });
                self.drop_overlay(ui, rect, hovering);
                return;
            }
            _ => {}
        }
        let entries = self.entries();
        let truncated = matches!(&self.listing, Some(Ok(l)) if l.truncated);
        if truncated {
            ui.label(RichText::new(ico(ICON_INFO, "Only the first items are shown. Narrow with the filter.")).small().color(t.warning));
        }
        // Keyboard: Cmd/Ctrl+A, Enter, Delete, Backspace (up), arrows.
        let mut open: Option<FileEntry> = None;
        let typing = ui.ctx().egui_wants_keyboard_input();
        if !typing && ui.ui_contains_pointer() || !typing && !self.selected.is_empty() {
            let (sel_all, enter, del, up_dir, down, up) = ui.input(|i| {
                (
                    i.modifiers.command && i.key_pressed(Key::A),
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::Delete),
                    i.key_pressed(Key::Backspace),
                    i.key_pressed(Key::ArrowDown),
                    i.key_pressed(Key::ArrowUp),
                )
            });
            if sel_all {
                self.selected = entries.iter().map(|e| e.id.clone()).collect();
            }
            if enter && self.selected.len() == 1 {
                open = entries.iter().find(|e| self.selected.contains(&e.id)).cloned();
            }
            if del && !read_only {
                let sel = self.selection();
                self.ask_delete(cx, sel);
            }
            if up_dir {
                let crumbs = self.cwd.as_deref().map(|c| s.breadcrumbs(c)).unwrap_or_default();
                if crumbs.len() >= 2 {
                    self.go(Some(crumbs[crumbs.len() - 2].0.clone()));
                    return;
                } else if self.cwd.is_some() && s.root_dir().is_none() {
                    self.go(None);
                    return;
                }
            }
            if (down || up) && !entries.is_empty() {
                let cur = self.anchor.unwrap_or(0) as i64 + if down { 1 } else { -1 };
                let i = cur.clamp(0, entries.len() as i64 - 1) as usize;
                self.anchor = Some(i);
                self.selected = BTreeSet::from([entries[i].id.clone()]);
                if !entries[i].is_dir {
                    self.show_preview(cx, entries[i].clone());
                }
            }
        }

        let header = |ui: &mut egui::Ui, label: &str, by: SortBy, sort: &mut (SortBy, bool)| {
            let arrow = if sort.0 == by { if sort.1 { String::from(ICON_ARROW_DROP_UP) } else { String::from(ICON_ARROW_DROP_DOWN) } } else { String::new() };
            let r = ui.add(egui::Label::new(RichText::new(format!("{label} {arrow}")).strong().color(t.muted_fg)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.clicked() {
                *sort = if sort.0 == by { (by, !sort.1) } else { (by, true) };
            }
        };
        let mut clicked: Option<(usize, egui::Modifiers)> = None;
        let mut double: Option<usize> = None;
        let mut context: Option<(usize, FileEntry)> = None;
        let mut menu_action: Option<(&'static str, FileEntry)> = None;
        let row_h = 30.0;
        let sel_ids = self.selected.clone();
        let mut sort = self.sort;
        TableBuilder::new(ui)
            .id_salt(("files", self.conn, self.cwd.clone()))
            .striped(false)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .max_scroll_height(f32::INFINITY)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::exact(96.0))
            .column(Column::exact(150.0))
            .column(Column::initial(150.0).at_least(80.0).clip(true))
            .header(28.0, |mut h| {
                h.col(|ui| header(ui, "Name", SortBy::Name, &mut sort));
                // Size values are right-aligned, so the header is too.
                h.col(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| header(ui, "Size", SortBy::Size, &mut sort));
                });
                h.col(|ui| {
                    ui.add_space(theme::space::MD);
                    header(ui, "Modified", SortBy::Modified, &mut sort);
                });
                h.col(|ui| header(ui, "Type", SortBy::Kind, &mut sort));
            })
            .body(|body| {
                body.rows(row_h, entries.len(), |mut row| {
                    let i = row.index();
                    let e = &entries[i];
                    row.set_selected(sel_ids.contains(&e.id));
                    row.col(|ui| {
                        let icon = if e.is_dir { if e.kind == "container" || e.kind == "bucket" { ICON_INVENTORY_2 } else { ICON_FOLDER } } else { file_icon(&e.name) };
                        let color = if e.is_dir { t.warning } else { t.info };
                        crate::views::item_glyph(ui, icon, 18.0, color);
                        ui.add(egui::Label::new(RichText::new(&e.name).color(t.fg)).truncate().selectable(false)).on_hover_text(&e.name);
                    });
                    row.col(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Folders have no size; leave the cell empty rather than a dash.
                            let size = match e.size {
                                Some(n) => human(n),
                                None if e.is_dir => match self.counts.get(&e.id) {
                                    Some((0, _)) => "empty".into(),
                                    Some((n, true)) => format!("{n}+ items"),
                                    Some((1, false)) => "1 item".into(),
                                    Some((n, false)) => format!("{n} items"),
                                    None => String::new(),
                                },
                                None => "—".into(),
                            };
                            ui.label(RichText::new(size).color(t.muted_fg));
                        });
                    });
                    row.col(|ui| {
                        ui.add_space(theme::space::MD);
                        let m = match &e.modified {
                            Some(m) => when(m),
                            None if e.is_dir => String::new(),
                            None => "—".into(),
                        };
                        ui.label(RichText::new(m).color(t.muted_fg));
                    });
                    row.col(|ui| {
                        let k = match &e.detail {
                            Some(d) if !e.is_dir => format!("{} · {d}", kind_label(e)),
                            _ => kind_label(e),
                        };
                        ui.add(egui::Label::new(RichText::new(k).color(t.muted_fg)).truncate());
                    });
                    let resp = row.response().on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.double_clicked() {
                        double = Some(i);
                    } else if resp.clicked() {
                        clicked = Some((i, resp.ctx.input(|inp| inp.modifiers)));
                    }
                    if resp.secondary_clicked() {
                        context = Some((i, e.clone()));
                    }
                    resp.context_menu(|ui| {
                        if ui.button(ico(if e.is_dir { ICON_FOLDER_OPEN } else { ICON_VISIBILITY }, if e.is_dir { "Open" } else { "Preview" })).clicked() {
                            menu_action = Some(("open", e.clone()));
                            ui.close();
                        }
                        if !e.is_dir && ui.button(ico(ICON_DOWNLOAD, "Download…")).clicked() {
                            menu_action = Some(("download", e.clone()));
                            ui.close();
                        }
                        if !e.is_dir && s.file_actions().share.is_some() && ui.button(ico(ICON_LINK, "Share link")).clicked() {
                            menu_action = Some(("share", e.clone()));
                            ui.close();
                        }
                        if ui.button(ico(ICON_CONTENT_COPY, "Copy name")).clicked() {
                            ui.ctx().copy_text(e.name.clone());
                            ui.close();
                        }
                        if !read_only && s.file_actions().rename.is_some() && ui.button(ico(ICON_DRIVE_FILE_RENAME_OUTLINE, "Rename…")).clicked() {
                            menu_action = Some(("rename", e.clone()));
                            ui.close();
                        }
                        if !read_only && e.kind != "container" && e.kind != "bucket" {
                            ui.separator();
                            if ui.button(RichText::new(ico(ICON_DELETE, "Delete…")).color(t.destructive)).clicked() {
                                menu_action = Some(("delete", e.clone()));
                                ui.close();
                            }
                        }
                    });
                });
            });
        self.sort = sort;
        self.drop_overlay(ui, rect, hovering);

        // Right-click on empty space: folder-level actions.
        let pointer = ui.input(|i| i.pointer.interact_pos());
        if context.is_none()
            && ui.input(|i| i.pointer.secondary_clicked())
            && let Some(pos) = pointer.filter(|p| rect.contains(*p))
        {
            self.blank_menu = Some(pos);
        }
        if let Some(pos) = self.blank_menu {
            let fa = s.file_actions();
            let can_write = !read_only && self.cwd.is_some();
            let mut close = false;
            let area = egui::Area::new(egui::Id::unique(("files_blank_menu", self.conn))).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(180.0);
                    if fa.mkdir.is_some() && ui.add_enabled(can_write, egui::Button::new(ico(ICON_CREATE_NEW_FOLDER, "New folder…"))).clicked() {
                        self.modal = Some(Modal::NewFolder(String::new()));
                        close = true;
                    }
                    if fa.upload.is_some() && ui.add_enabled(can_write, egui::Button::new(ico(ICON_UPLOAD, "Upload files…"))).clicked() {
                        close = true;
                        if let Some(paths) = rfd::FileDialog::new().set_title("Upload files").pick_files() {
                            self.upload_paths(cx, paths);
                        }
                    }
                    if ui.button(ico(ICON_REFRESH, "Refresh")).clicked() {
                        self.listing = None;
                        close = true;
                    }
                    if ui.button(ico(ICON_SELECT_ALL, "Select all")).clicked() {
                        self.selected = entries.iter().map(|e| e.id.clone()).collect();
                        close = true;
                    }
                });
            });
            let clicked_outside = ui.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer() && !ui.input(|i| i.pointer.secondary_clicked());
            if close || clicked_outside || ui.input(|i| i.key_pressed(Key::Escape)) {
                self.blank_menu = None;
            }
        }

        // ---- apply interactions -------------------------------------------------------
        if let Some((i, e)) = context
            && !self.selected.contains(&e.id)
        {
            self.selected = BTreeSet::from([e.id.clone()]);
            self.anchor = Some(i);
        }
        if let Some((i, m)) = clicked {
            let id = entries[i].id.clone();
            if m.shift
                && let Some(a) = self.anchor
            {
                let (lo, hi) = (a.min(i), a.max(i));
                self.selected = entries[lo..=hi].iter().map(|e| e.id.clone()).collect();
            } else if m.command {
                if !self.selected.remove(&id) {
                    self.selected.insert(id);
                }
                self.anchor = Some(i);
            } else {
                self.selected = BTreeSet::from([id]);
                self.anchor = Some(i);
            }
            if self.selected.len() == 1 && !entries[i].is_dir && !m.shift && !m.command {
                self.show_preview(cx, entries[i].clone());
            } else if self.selected.len() != 1 {
                self.preview = None;
            }
        }
        if let Some(i) = double {
            open = Some(entries[i].clone());
        }
        if let Some((what, e)) = menu_action {
            match what {
                "open" => open = Some(e),
                "download" => self.download(cx, vec![e]),
                "share" => {
                    if let Some(share) = s.file_actions().share {
                        let mut a = Args::new();
                        a.insert("hours".into(), "24".into());
                        self.run_batch(cx, "Creating link".into(), vec![(e.id.clone(), share, a)]);
                    }
                }
                "rename" => self.modal = Some(Modal::Rename(e.clone(), e.name.clone())),
                "delete" => self.ask_delete(cx, vec![e]),
                _ => {}
            }
        }
        if let Some(e) = open {
            self.open_entry(cx, &e);
        }
    }

    fn drop_overlay(&self, ui: &egui::Ui, rect: egui::Rect, hovering: bool) {
        if !hovering || self.cwd.is_none() {
            return;
        }
        let t = theme::tokens(ui);
        let p = ui.painter();
        p.rect(rect.shrink(4.0), 10.0, t.accent.gamma_multiply(0.12), egui::Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, format!("{}  Drop files to upload here", String::from(ICON_CLOUD_UPLOAD)), egui::FontId::proportional(18.0), t.fg);
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui, cx: &mut AppCtx) {
        let t = theme::tokens(ui);
        let Some((e, content)) = self.preview.clone() else { return };
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                crate::views::item_glyph(ui, file_icon(&e.name), 22.0, t.info);
                ui.add(egui::Label::new(RichText::new(&e.name).strong()).wrap());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(String::from(ICON_CLOSE)).on_hover_text("Close preview").clicked() {
                        self.preview = None;
                    }
                });
            });
            ui.label(RichText::new(format!("{} · {}", e.size.map(human).unwrap_or_default(), e.modified.as_ref().map(when).unwrap_or_default())).color(t.muted_fg));
            ui.add_space(theme::space::SM);
            if theme::primary_button(ui, ico(ICON_DOWNLOAD, "Download")).clicked() {
                self.download(cx, vec![e.clone()]);
            }
            ui.add_space(theme::space::SM);
            ui.separator();
            match &content {
                None => {
                    ui.spinner();
                }
                Some(Err(err)) => crate::views::error_box(ui, err),
                Some(Ok(c)) => {
                    egui::ScrollArea::both().id_salt("file_preview").auto_shrink([false, false]).show(ui, |ui| match c {
                        Content::Text(s) | Content::Json(s) => {
                            let shown: String = s.chars().take(200_000).collect();
                            ui.add(egui::Label::new(RichText::new(shown).monospace().size(12.0)).wrap_mode(egui::TextWrapMode::Extend));
                        }
                        Content::Props(p) => {
                            egui::Grid::new("file_props").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                                for (k, v) in p {
                                    ui.label(RichText::new(k).color(t.muted_fg));
                                    ui.add(egui::Label::new(v).wrap());
                                    ui.end_row();
                                }
                            });
                        }
                        Content::Empty(m) => {
                            ui.label(RichText::new(m).color(t.muted_fg));
                        }
                        Content::Pdf(bytes) => {
                            let pages = crate::pdf::page_count(bytes);
                            ui.label(RichText::new(format!("PDF · {} page{} · {}", pages, if pages == 1 { "" } else { "s" }, human(bytes.len() as u64))).small().color(t.muted_fg));
                            if ui.button(ico(ICON_OPEN_IN_NEW, "Open in viewer")).clicked()
                                && let Err(err) = crate::pdf::open_in_viewer(&e.name, bytes)
                            {
                                cx.toasts.error_text("Could not open the PDF", err);
                            }
                            ui.add_space(theme::space::SM);
                            // Render page 1 once per file, in the background.
                            if self.pdf_png.as_ref().is_none_or(|(id, _)| *id != e.id) && self.pdf_job.as_ref().is_none_or(|(id, _)| *id != e.id) {
                                let b = bytes.clone();
                                self.pdf_job = Some((e.id.clone(), cx.bg.spawn("Rendering PDF page", crate::pdf::first_page_png(b))));
                            }
                            if let Some((id, job)) = &mut self.pdf_job
                                && let Some(r) = job.take()
                            {
                                self.pdf_png = Some((id.clone(), r));
                                self.pdf_job = None;
                            }
                            match &self.pdf_png {
                                Some((id, Ok(png))) if *id == e.id => {
                                    if self.preview_tex.as_ref().is_none_or(|(tid, _)| *tid != e.id) {
                                        let tex = image::load_from_memory(png).map_err(|err| err.to_string()).map(|img| {
                                            let img = if img.width() > 2048 || img.height() > 2048 { img.thumbnail(2048, 2048) } else { img };
                                            let rgba = img.to_rgba8();
                                            let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                                            ui.ctx().load_texture(format!("preview:{}", e.id), ci, egui::TextureOptions::LINEAR)
                                        });
                                        self.preview_tex = Some((e.id.clone(), tex));
                                    }
                                    if let Some((_, Ok(tex))) = &self.preview_tex {
                                        let size = tex.size_vec2();
                                        let w = ui.available_width().min(size.x);
                                        // Pages are paper: white behind transparent renders.
                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, w * size.y / size.x.max(1.0)), Sense::hover());
                                        ui.painter().rect_filled(rect, 2.0, egui::Color32::WHITE);
                                        ui.painter().rect_stroke(rect, 2.0, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
                                        egui::Image::new(tex).paint_at(ui, rect);
                                        ui.label(RichText::new("Page 1").small().color(t.muted_fg));
                                    }
                                }
                                Some((id, Err(m))) if *id == e.id => {
                                    ui.label(RichText::new(m).color(t.muted_fg));
                                }
                                _ => {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label(RichText::new("Rendering page 1…").color(t.muted_fg));
                                    });
                                }
                            }
                        }
                        Content::Image(bytes) => {
                            if self.preview_tex.as_ref().is_none_or(|(id, _)| *id != e.id) {
                                let tex = image::load_from_memory(bytes).map_err(|err| format!("Could not read this image: {err}")).map(|img| {
                                    // Keep textures modest; the pane is a few hundred points wide.
                                    let img = if img.width() > 2048 || img.height() > 2048 { img.thumbnail(2048, 2048) } else { img };
                                    let rgba = img.to_rgba8();
                                    let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                                    ui.ctx().load_texture(format!("preview:{}", e.id), ci, egui::TextureOptions::LINEAR)
                                });
                                self.preview_tex = Some((e.id.clone(), tex));
                            }
                            match &self.preview_tex {
                                Some((_, Ok(tex))) => {
                                    let size = tex.size_vec2();
                                    let w = ui.available_width().min(size.x);
                                    ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(w, w * size.y / size.x.max(1.0))));
                                    ui.label(RichText::new(format!("{} × {} px", size.x as u32, size.y as u32)).small().color(t.muted_fg));
                                }
                                Some((_, Err(m))) => {
                                    ui.label(RichText::new(m).color(t.muted_fg));
                                }
                                None => {}
                            }
                        }
                        _ => {
                            ui.label(RichText::new("No preview for this file type. Use Download.").color(t.muted_fg));
                        }
                    });
                }
            }
        });
    }

    fn modal_ui(&mut self, ctx: &egui::Context, cx: &mut AppCtx, s: &Arc<dyn ResourceSession>) {
        let Some(m) = self.modal.take() else { return };
        let mut keep = true;
        let mut submit: Option<(String, &'static str, Args, String)> = None;
        let m = match m {
            Modal::NewFolder(mut name) => {
                egui::Modal::new(egui::Id::unique(("files_mkdir", self.conn))).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.heading("New folder");
                    let r = ui.add(egui::TextEdit::singleline(&mut name).hint_text("Folder name").desired_width(f32::INFINITY));
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(theme::space::SM);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                        let ok = !name.trim().is_empty() && !name.contains('/');
                        if (ui.add_enabled(ok, egui::Button::new("Create")).clicked() || (enter && ok))
                            && let (Some(dir), Some(a)) = (self.cwd.clone(), s.file_actions().mkdir)
                        {
                            let mut args = Args::new();
                            args.insert("name".into(), name.trim().to_string());
                            submit = Some((dir, a, args, "Creating folder".into()));
                        }
                    });
                });
                Modal::NewFolder(name)
            }
            Modal::Rename(e, mut name) => {
                egui::Modal::new(egui::Id::unique(("files_rename", self.conn))).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.heading(format!("Rename {}", e.name));
                    let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(theme::space::SM);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                        let ok = !name.trim().is_empty() && name.trim() != e.name && !name.contains('/');
                        if (ui.add_enabled(ok, egui::Button::new("Rename")).clicked() || (enter && ok))
                            && let Some(a) = s.file_actions().rename
                        {
                            let mut args = Args::new();
                            args.insert("name".into(), name.trim().to_string());
                            submit = Some((e.id.clone(), a, args, "Renaming".into()));
                        }
                    });
                });
                Modal::Rename(e, name)
            }
            Modal::Share(url) => {
                egui::Modal::new(egui::Id::unique(("files_share", self.conn))).show(ctx, |ui| {
                    ui.set_width(520.0_f32.min(ctx.content_rect().width() - 48.0));
                    ui.heading("Share link");
                    ui.label(RichText::new("Anyone with this link can download the file until it expires (24 hours).").color(theme::muted(ui)));
                    ui.add_space(theme::space::SM);
                    let mut u = url.clone();
                    ui.add(egui::TextEdit::multiline(&mut u).desired_rows(3).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace));
                    ui.horizontal(|ui| {
                        if theme::primary_button(ui, ico(ICON_CONTENT_COPY, "Copy link")).clicked() {
                            ui.ctx().copy_text(url.clone());
                            cx.toasts.ok("Link copied");
                        }
                        if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            keep = false;
                        }
                    });
                });
                Modal::Share(url)
            }
        };
        if let Some((node, action, args, label)) = submit {
            self.run_batch(cx, label, vec![(node, action, args)]);
            keep = false;
        }
        if keep {
            self.modal = Some(m);
        }
    }
}
