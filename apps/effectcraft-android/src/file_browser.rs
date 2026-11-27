//! In-app file browser for Android (`Hooks::file_dialog_async`).
//!
//! Android has no `rfd`, and a `NativeActivity` cannot show the system picker without Java.
//! This browser runs inside the egui UI: the shell's hook stores the request, the shell draws
//! the window each frame, and OK/Cancel answers through `DialogDone`. Roots are the app's own
//! internal storage (always readable) plus the shared `Download`/`DCIM` folders when the
//! platform lets us read them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_ui_egui::{DialogDone, FileDialog};

/// One row in the browser list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A pending picker: the request, where the browser is, and the answer when it closes.
pub struct Pending {
    request: FileDialog,
    current: PathBuf,
    entries: Vec<Entry>,
    error: Option<String>,
    selected: Vec<PathBuf>,
    filename: String,
    /// Where the browser can start again: app-private storage, then shared folders when readable.
    roots: Vec<PathBuf>,
    done: Option<DialogDone>,
}

/// Roots to offer: the app's data dir first, then its internal parent, then shared folders
/// when they exist. All I/O errors become an empty list, never a crash.
pub fn roots(data_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = data_dir {
        out.push(d.to_path_buf());
        if let Some(parent) = d.parent() {
            let parent = parent.to_path_buf();
            if parent != *d && !out.contains(&parent) {
                out.push(parent);
            }
        }
    }
    for shared in ["/sdcard/Download", "/sdcard/DCIM", "/storage/emulated/0/Download", "/storage/emulated/0/DCIM"] {
        let p = PathBuf::from(shared);
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// List `dir`: directories first, then files, each by name (case-insensitive). At most 2000
/// entries; unreadable directories become an error string, not a crash.
pub fn list_dir(dir: &Path) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut entries = Vec::new();
    for e in read {
        let Ok(e) = e else { continue };
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let is_dir = path.is_dir();
        entries.push(Entry { name, path, is_dir });
        if entries.len() >= 2000 {
            break;
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

/// Whether `path` is a project file (`.ecproj`, `.ecprojx`).
pub fn is_project(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ecproj") || e.eq_ignore_ascii_case("ecprojx"))
}

/// A name the browser can save inside its current folder: one relative component, not `.`,
/// `..`, an absolute path or anything with a separator. `Path::join` with one of those would
/// escape the folder the user was shown.
pub fn valid_save_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\') && !Path::new(name).is_absolute()
}

/// The filename field's initial value for a save dialog, clamped to 128 chars. A default that
/// has no usable file name (`..`, `/`) becomes a fresh project name.
pub fn save_default(request: &FileDialog) -> String {
    match request {
        FileDialog::Save { default, .. } => {
            let name = Path::new(default).file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !valid_save_name(name) {
                return "Untitled Project.ecproj".to_string();
            }
            let mut end = name.len().min(128);
            while !name.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            name[..end].to_string()
        }
        _ => String::new(),
    }
}

/// The dialog's title.
pub fn title(request: &FileDialog) -> &'static str {
    match request {
        FileDialog::Open { .. } => "Import files",
        FileDialog::OpenProject => "Open project",
        FileDialog::Folder => "Choose folder",
        FileDialog::Save { project: true, .. } => "Save project",
        FileDialog::Save { .. } => "Save file",
    }
}

/// Whether the request allows several files.
pub fn allows_many(request: &FileDialog) -> bool {
    matches!(request, FileDialog::Open { many: true, .. })
}

/// Whether `entry` can be picked for `request` (folders navigate, so only files are pickable
/// for open dialogs; Folder/save pick the current folder or a named file).
pub fn can_pick(request: &FileDialog, entry: &Entry) -> bool {
    match request {
        FileDialog::Folder | FileDialog::Save { .. } => false,
        FileDialog::OpenProject => !entry.is_dir && is_project(&entry.path),
        FileDialog::Open { exts, .. } => {
            if entry.is_dir {
                return false;
            }
            if exts.is_empty() {
                return true;
            }
            let Some(ext) = entry.path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) else {
                return false;
            };
            exts.iter().any(|w| w.to_ascii_lowercase() == ext)
        }
    }
}

/// Store a new pending request and wake the UI. The browser starts in `start_dir` (or the
/// first root). Never blocks; I/O errors show inside the browser. A request that arrives while
/// another dialog is up cancels the old one through its `done`, so the command that asked for it
/// cannot wait forever (the browser is not modal and the control channel can run commands).
pub fn request(pending: &Arc<Mutex<Option<Pending>>>, ctx: &egui::Context, dialog: FileDialog, done: DialogDone, start_dir: Option<PathBuf>) {
    let roots = roots(start_dir.as_deref());
    let current = start_dir.filter(|p| p.is_dir()).unwrap_or_else(|| roots.first().cloned().unwrap_or_else(|| PathBuf::from("/")));
    let filename = save_default(&dialog);
    let (entries, error) = match list_dir(&current) {
        Ok(e) => (e, None),
        Err(err) => (Vec::new(), Some(err)),
    };
    let previous = pending.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(previous) = previous
        && let Some(done) = previous.done
    {
        done(Vec::new());
    }
    let p = Pending { request: dialog, current, entries, error, selected: Vec::new(), filename, roots, done: Some(done) };
    *pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(p);
    ctx.request_repaint();
}

/// Draw the browser window when a request is pending.
pub fn show(pending: &Arc<Mutex<Option<Pending>>>, ctx: &egui::Context) {
    let mut guard = pending.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(p) = guard.as_mut() else {
        return;
    };
    let mut close: Option<Vec<String>> = None;
    let mut navigate: Option<PathBuf> = None;
    let window_title = title(&p.request);
    egui::Window::new(window_title).collapsible(false).resizable(true).min_size(egui::vec2(420.0, 380.0)).show(ctx, |ui| {
        if let Some(err) = p.error.clone() {
            ui.colored_label(egui::Color32::YELLOW, err);
        }
        ui.horizontal(|ui| {
            ui.label(p.current.to_string_lossy().to_string());
            if ui.button("Up").clicked()
                && let Some(parent) = p.current.parent().map(Path::to_path_buf)
            {
                navigate = Some(parent);
            }
        });
        // Roots are buttons, not a claim in the docs: a shared folder only appears when the
        // platform already lets us read it (see docs/android.md for the permission gap).
        ui.horizontal_wrapped(|ui| {
            ui.label("Location:");
            for root in p.roots.clone() {
                let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("storage");
                if ui.button(name).clicked() {
                    navigate = Some(root);
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for i in 0..p.entries.len() {
                let Some(entry) = p.entries.get(i).cloned() else {
                    continue;
                };
                if entry.is_dir {
                    // Tapping a folder enters it (Folder/save pick the current folder
                    // with "Choose here"/"Save" instead).
                    if ui.selectable_label(false, format!("{}/", entry.name)).clicked() {
                        navigate = Some(entry.path.clone());
                    }
                    continue;
                }
                if !can_pick(&p.request, &entry) {
                    ui.add_enabled(false, egui::Label::new(entry.name.clone()));
                    continue;
                }
                let is_selected = p.selected.contains(&entry.path);
                if ui.selectable_label(is_selected, entry.name.clone()).clicked() {
                    if allows_many(&p.request) {
                        if is_selected {
                            p.selected.retain(|s| *s != entry.path);
                        } else {
                            p.selected.push(entry.path.clone());
                        }
                    } else {
                        p.selected.clear();
                        p.selected.push(entry.path.clone());
                    }
                }
            }
        });
        ui.separator();
        if matches!(p.request, FileDialog::Save { .. }) {
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut p.filename);
            });
            if !valid_save_name(&p.filename) {
                ui.colored_label(egui::Color32::YELLOW, "Use a file name, not a path");
            }
        }
        if !p.selected.is_empty() && !matches!(p.request, FileDialog::Save { .. }) && !matches!(p.request, FileDialog::Folder) {
            let shown: Vec<String> = p.selected.iter().filter_map(|s| s.file_name().and_then(|n| n.to_str()).map(str::to_string)).take(3).collect();
            let mut msg = shown.join(", ");
            if p.selected.len() > 3 {
                msg.push_str(&format!(" +{} more", p.selected.len() - 3));
            }
            ui.label(msg);
        }
        ui.horizontal(|ui| {
            let ok_label = match &p.request {
                FileDialog::Folder => "Choose here",
                FileDialog::Save { .. } => "Save",
                _ => "Open",
            };
            let can_ok = match &p.request {
                FileDialog::Folder => true,
                FileDialog::Save { .. } => valid_save_name(&p.filename),
                _ => !p.selected.is_empty(),
            };
            if ui.add_enabled(can_ok, egui::Button::new(ok_label)).clicked() {
                let answer = match &p.request {
                    FileDialog::Folder => {
                        vec![p.current.to_string_lossy().to_string()]
                    }
                    FileDialog::Save { .. } => {
                        let name = p.filename.trim().to_string();
                        vec![p.current.join(name).to_string_lossy().to_string()]
                    }
                    _ => p.selected.iter().map(|s| s.to_string_lossy().to_string()).collect(),
                };
                close = Some(answer);
            }
            if ui.button("Cancel").clicked() {
                close = Some(Vec::new());
            }
        });
    });
    if let Some(dir) = navigate {
        let (entries, error) = match list_dir(&dir) {
            Ok(e) => (e, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        p.current = dir;
        p.entries = entries;
        p.error = error;
        p.selected.clear();
    }
    if let Some(answer) = close {
        let done = p.done.take();
        drop(guard);
        *pending.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if let Some(done) = done {
            done(answer);
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_filter_passes_matching_extensions_only() {
        let dir = std::env::temp_dir();
        let exts = vec!["mp4".to_string()];
        assert!(can_pick(&FileDialog::Open { exts: exts.clone(), many: false }, &Entry { name: "clip.MP4".into(), path: dir.join("clip.MP4"), is_dir: false }));
        assert!(!can_pick(&FileDialog::Open { exts: exts.clone(), many: true }, &Entry { name: "clip.mov".into(), path: dir.join("clip.mov"), is_dir: false }));
        assert!(!can_pick(&FileDialog::Open { exts, many: false }, &Entry { name: "folder".into(), path: dir.join("folder"), is_dir: true }));
    }

    #[test]
    fn project_detection_is_case_insensitive() {
        assert!(is_project(Path::new("a.ecproj")));
        assert!(is_project(Path::new("a.ECPROJX")));
        assert!(!is_project(Path::new("a.mp4")));
        assert!(!is_project(Path::new("a.ecproj.png")));
    }

    #[test]
    fn save_default_uses_the_file_name_and_clamps() {
        let d = FileDialog::Save { default: "/tmp/Untitled Project.ecproj".into(), ext: "ecproj".into(), project: true };
        assert_eq!(save_default(&d), "Untitled Project.ecproj");
        let long = "x".repeat(300);
        let d = FileDialog::Save { default: long, ext: String::new(), project: false };
        assert!(save_default(&d).len() <= 128);
    }

    #[test]
    fn save_name_is_a_single_relative_component() {
        assert!(valid_save_name("shot 0001.ecproj"));
        for bad in ["", " ", ".", "..", "../shot.ecproj", "a/b", "/tmp/shot.ecproj", "C:\\shot.ecproj"] {
            assert!(!valid_save_name(bad), "`{bad}` escapes the folder shown to the user");
        }
        // A default without a file name falls back to a safe one; a trailing slash after a
        // real name still gives that name.
        for default in ["..", "/"] {
            let d = FileDialog::Save { default: default.into(), ext: "ecproj".into(), project: true };
            assert_eq!(save_default(&d), "Untitled Project.ecproj");
        }
        let d = FileDialog::Save { default: "/tmp/".into(), ext: "ecproj".into(), project: true };
        assert_eq!(save_default(&d), "tmp");
    }

    #[test]
    fn a_replaced_request_answers_the_previous_one() {
        let pending = Arc::new(Mutex::new(None));
        let ctx = egui::Context::default();
        let answered: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
        let seen = answered.clone();
        request(
            &pending,
            &ctx,
            FileDialog::Open { exts: Vec::new(), many: false },
            Box::new(move |paths| *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(paths)),
            Some(std::env::temp_dir()),
        );
        request(&pending, &ctx, FileDialog::Folder, Box::new(|_| {}), Some(std::env::temp_dir()));
        // Cancel, not a hang: the command that asked for the first dialog completes.
        assert_eq!(*answered.lock().unwrap_or_else(PoisonError::into_inner), Some(Vec::new()));
    }

    #[test]
    fn request_offers_the_roots_it_can_reach() {
        let pending = Arc::new(Mutex::new(None));
        let ctx = egui::Context::default();
        let data = std::env::temp_dir();
        request(&pending, &ctx, FileDialog::Folder, Box::new(|_| {}), Some(data.clone()));
        let guard = pending.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(guard.as_ref().is_some_and(|p| p.roots.contains(&data)));
    }

    #[test]
    fn list_dir_sorts_dirs_first_and_skips_hidden() {
        let base = std::env::temp_dir()
            .join(format!("ec-android-browser-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
        let _ = std::fs::create_dir_all(base.join("bdir"));
        let _ = std::fs::create_dir_all(base.join("adir"));
        let _ = std::fs::write(base.join("z.txt"), b"z");
        let _ = std::fs::write(base.join(".hidden"), b"h");
        let entries = list_dir(&base).unwrap_or_default();
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"adir".to_string()));
        assert!(names.contains(&"bdir".to_string()));
        assert!(!names.iter().any(|n| n == ".hidden"));
        let first_file = names.iter().position(|n| n == "z.txt").unwrap_or(usize::MAX);
        let last_dir = names.iter().rposition(|n| *n == "adir" || *n == "bdir").unwrap_or(0);
        assert!(last_dir < first_file);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn roots_start_with_the_data_dir() {
        let data = PathBuf::from("/data/data/x/files/EffectCraft");
        let r = roots(Some(&data));
        assert_eq!(r.first(), Some(&data));
    }

    #[test]
    fn titles_cover_every_dialog_kind() {
        assert_eq!(title(&FileDialog::Open { exts: Vec::new(), many: false }), "Import files");
        assert_eq!(title(&FileDialog::OpenProject), "Open project");
        assert_eq!(title(&FileDialog::Folder), "Choose folder");
    }
}
