//! Content Browser (Maya: Windows > Content Browser).
//!
//! Browse the current project, the file system, Inca's built-in examples and your favorites.
//! The window has Maya's layout: a path bar with back / forward / up and history, a folder
//! tree with the Projects / File System / Examples / Favorites tabs, a thumbnail (or list)
//! view of the current folder with search and a size slider, and Metadata / Preview panes.
//!
//! Double-click opens a scene when the current scene is empty and imports it otherwise (the
//! same rule as dropping a file on the window), sources a MEL script, applies a material
//! preset to the selection, and turns an image into a file texture. Dragging an item onto a
//! viewport imports it; a material preset or an image dropped on an object is assigned to it.
//!
//! Built-in examples are MEL generators embedded at build time (build.rs scans
//! assets/examples). Each starts with a small header:
//! `// @name`, `// @description`, `// @kind scene|material`, and scenes split their MEL at a
//! `// @settings` line: what comes before builds the content (and is all an import runs),
//! what comes after sets the camera, render and playback settings when the example is opened.

use crate::app::IncaApp;
use crate::theme;
use egui::{pos2, vec2, Color32, Rect, RichText, Sense, Stroke};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Instant, SystemTime};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/examples_gen.rs"));
}

/// Thumbnail edge for user-file thumbnails rendered in the background.
const THUMB_PX: u32 = 160;
/// Scene files larger than this get an icon instead of a rendered thumbnail.
const THUMB_MAX_BYTES: u64 = 40 << 20;

const SCENE_EXTS: &[&str] = &["inca", "ma", "mb", "fbx"];
const MODEL_EXTS: &[&str] = &["obj", "stl", "3mf", "gltf", "glb", "inca-layer"];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg"];
const TEXT_EXTS: &[&str] = &["mel", "txt", "md", "json", "py", "ma"];

// ------------------------------------------------------------------ examples

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExampleKind {
    Scene,
    Material,
}

/// A built-in example (embedded MEL generator).
#[derive(Clone, Debug)]
pub struct Example {
    /// `Category/Sub/File.mel`, relative to the examples root
    pub path: &'static str,
    pub name: String,
    pub description: String,
    pub kind: ExampleKind,
    pub mel: &'static str,
    pub thumb: Option<&'static [u8]>,
}

impl Example {
    /// The folder it sits in (`Lighting And Rendering`), `""` at the root.
    pub fn folder(&self) -> &'static str {
        self.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
    }
    /// MEL that builds the content: all an import runs.
    pub fn scene_part(&self) -> &'static str {
        split_settings(self.mel).0
    }
    /// MEL run after the content when the example is opened as a new scene.
    pub fn settings_part(&self) -> &'static str {
        split_settings(self.mel).1
    }
}

fn split_settings(src: &str) -> (&str, &str) {
    let mut at = 0;
    for line in src.split_inclusive('\n') {
        if line.trim() == "// @settings" {
            return (&src[..at], &src[at + line.len()..]);
        }
        at += line.len();
    }
    (src, "")
}

fn header(src: &str, key: &str) -> Option<String> {
    let tag = format!("// @{key}");
    src.lines().map(str::trim).find_map(|l| l.strip_prefix(&tag).filter(|r| r.is_empty() || r.starts_with(' ')).map(|r| r.trim().to_string()))
}

/// Every built-in example, parsed from the embedded files.
pub fn examples() -> &'static [Example] {
    static ALL: std::sync::OnceLock<Vec<Example>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        generated::EXAMPLE_FILES
            .iter()
            .map(|&(path, mel, thumb)| {
                let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".mel");
                Example {
                    path,
                    name: header(mel, "name").unwrap_or_else(|| stem.to_string()),
                    description: header(mel, "description").unwrap_or_default(),
                    kind: if header(mel, "kind").as_deref() == Some("material") { ExampleKind::Material } else { ExampleKind::Scene },
                    mel,
                    thumb,
                }
            })
            .collect()
    })
}

/// The example folders: every folder that holds an example, and its parents.
fn example_folders() -> Vec<String> {
    let mut set: Vec<String> = vec![];
    for e in examples() {
        let mut acc = String::new();
        for part in e.folder().split('/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if !set.contains(&acc) {
                set.push(acc.clone());
            }
        }
    }
    set.sort();
    set
}

// ------------------------------------------------------------------ state

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum CbTab {
    Projects,
    FileSystem,
    #[default]
    Examples,
    Favorites,
}

impl CbTab {
    const ALL: [CbTab; 4] = [CbTab::Projects, CbTab::FileSystem, CbTab::Examples, CbTab::Favorites];
    fn label(self) -> &'static str {
        match self {
            CbTab::Projects => "Projects",
            CbTab::FileSystem => "File System",
            CbTab::Examples => "Examples",
            CbTab::Favorites => "Favorites",
        }
    }
}

/// A place the content view can show.
#[derive(Clone, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub enum Loc {
    Dir(PathBuf),
    /// a folder of the built-in examples (`""` = their root)
    Examples(String),
    Favorites,
}

impl Loc {
    fn tab(&self) -> CbTab {
        match self {
            Loc::Dir(_) => CbTab::FileSystem,
            Loc::Examples(_) => CbTab::Examples,
            Loc::Favorites => CbTab::Favorites,
        }
    }
    fn parent(&self) -> Option<Loc> {
        match self {
            Loc::Dir(p) => p.parent().map(|p| Loc::Dir(p.to_path_buf())),
            Loc::Examples(f) if !f.is_empty() => Some(Loc::Examples(f.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default())),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum SortBy {
    #[default]
    Name,
    Type,
    Size,
    Date,
}

/// What the Content Browser remembers between sessions.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CbPrefs {
    pub tab: CbTab,
    pub loc: Option<Loc>,
    /// favorite files and folders (paths), and examples as `example:<path>`
    pub favorites: Vec<String>,
    pub thumb_size: f32,
    pub list_view: bool,
    pub sort: SortBy,
    pub show_meta: bool,
    pub show_preview: bool,
}

impl Default for CbPrefs {
    fn default() -> Self {
        CbPrefs { tab: CbTab::Examples, loc: None, favorites: vec![], thumb_size: 112.0, list_view: false, sort: SortBy::Name, show_meta: true, show_preview: true }
    }
}

/// One entry of the content view.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Folder(Loc),
    File(PathBuf),
    Example(usize),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub target: Target,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

impl Item {
    fn ext(&self) -> String {
        match &self.target {
            Target::File(p) => ext_of(p),
            Target::Example(_) => "mel".into(),
            Target::Folder(_) => String::new(),
        }
    }
    fn is_folder(&self) -> bool {
        matches!(self.target, Target::Folder(_))
    }
    /// A stable string for widget ids.
    fn target_key(&self) -> String {
        match &self.target {
            Target::Folder(l) => format!("{l:?}"),
            Target::File(p) => p.display().to_string(),
            Target::Example(i) => format!("example:{i}"),
        }
    }
    fn fav_key(&self) -> Option<String> {
        match &self.target {
            Target::File(p) => Some(p.to_string_lossy().to_string()),
            Target::Folder(Loc::Dir(p)) => Some(p.to_string_lossy().to_string()),
            Target::Example(i) => Some(format!("example:{}", examples()[*i].path)),
            _ => None,
        }
    }
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

type Pixels = (u32, u32, Vec<u8>);

/// A worker answer: the key and its pixels, `None` for no thumbnail, or `Err` when the request
/// went stale (another folder is shown now) and was skipped.
type ThumbResult = (String, Result<Option<Pixels>, ()>);

/// Background thumbnail renderer for user files: one worker thread, newest request first.
/// Requests carry a generation; when the folder changes or the window closes the generation
/// moves on and older requests are skipped, so nothing keeps path tracing in the background.
struct ThumbWorker {
    tx: mpsc::Sender<(String, PathBuf, u64)>,
    rx: mpsc::Receiver<ThumbResult>,
    generation: Arc<AtomicU64>,
}

impl ThumbWorker {
    fn start() -> Self {
        let (tx, req_rx) = mpsc::channel::<(String, PathBuf, u64)>();
        let (res_tx, rx) = mpsc::channel::<ThumbResult>();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::spawn(move || {
            while let Ok(first) = req_rx.recv() {
                let mut queue = vec![first];
                loop {
                    while let Ok(more) = req_rx.try_recv() {
                        queue.push(more);
                    }
                    let Some((key, path, gen)) = queue.pop() else { break };
                    let answer = if gen != current.load(Ordering::Relaxed) {
                        Err(())
                    } else {
                        // a broken file must not take the worker down with it
                        Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| thumbnail_for(&path))).ok().flatten())
                    };
                    if res_tx.send((key, answer)).is_err() {
                        return;
                    }
                }
            }
        });
        ThumbWorker { tx, rx, generation }
    }
}

#[derive(Default)]
pub struct CbState {
    pub prefs: CbPrefs,
    pub back: Vec<Loc>,
    pub fwd: Vec<Loc>,
    pub filter: String,
    /// the selected item (by its target)
    pub selected: Option<Target>,
    /// the tab each history entry was shown in
    back_tabs: Vec<CbTab>,
    fwd_tabs: Vec<CbTab>,
    /// the visible items, kept until the folder, search, sort or listing changes
    view_cache: Option<(u64, Arc<Vec<Item>>)>,
    /// favorite entries checked on disk, kept for a few seconds
    fav_cache: HashMap<String, (Instant, Option<Item>)>,
    /// drive roots, read once per File System visit or Refresh
    drives: Option<Vec<PathBuf>>,
    /// frame counter and the frame each thumbnail texture was last used (for eviction)
    frame: u64,
    used: HashMap<String, u64>,
    /// focus the new-folder or path field on the frame it opens
    focus_field: bool,
    /// the remembered place was checked since the window opened
    validated: bool,
    /// folders expanded in the tree
    pub expanded: HashSet<PathBuf>,
    pub examples_expanded: HashSet<String>,
    /// directory listings, kept for a few seconds
    listing: HashMap<PathBuf, (Instant, Vec<Item>)>,
    /// thumbnails by key (example path, or file path + size + mtime)
    textures: HashMap<String, egui::TextureHandle>,
    /// thumbnail keys with no image (failed or not renderable)
    no_thumb: HashSet<String>,
    pending: HashSet<String>,
    worker: Option<ThumbWorker>,
    /// item being dragged out of the content view
    pub drag: Option<Item>,
    /// an open waiting for the save-changes answer
    pub confirm_open: Option<Item>,
    /// typed path in the path bar while editing it
    path_edit: Option<String>,
    /// new folder name being typed
    new_folder: Option<String>,
    preview_text: Option<(PathBuf, Option<SystemTime>, String)>,
    /// screen rect of every item drawn this frame (for the UI tests)
    pub tile_rects: Vec<(Target, Rect)>,
}

impl CbState {
    pub fn loc(&self) -> Loc {
        self.prefs.loc.clone().unwrap_or(Loc::Examples(String::new()))
    }

    /// Show another place, remembering where we were for Back.
    pub fn go(&mut self, loc: Loc) {
        let cur = self.loc();
        if cur != loc {
            self.back.push(cur);
            self.back_tabs.push(self.prefs.tab);
            self.fwd.clear();
            self.fwd_tabs.clear();
        }
        self.set_loc(loc);
    }

    fn set_loc(&mut self, loc: Loc) {
        // the Projects and Favorites trees hold folders too: browsing them keeps the tab
        let keep = matches!(loc, Loc::Dir(_)) && matches!(self.prefs.tab, CbTab::Projects | CbTab::Favorites);
        if !keep {
            self.prefs.tab = loc.tab();
        }
        if self.prefs.loc.as_ref() != Some(&loc) {
            self.cancel_thumbs();
        }
        self.prefs.loc = Some(loc);
        self.selected = None;
        self.path_edit = None;
        self.new_folder = None;
    }

    pub fn go_back(&mut self) {
        if let Some(l) = self.back.pop() {
            let t = self.back_tabs.pop().unwrap_or(l.tab());
            self.fwd.push(self.loc());
            self.fwd_tabs.push(self.prefs.tab);
            self.set_loc(l);
            self.prefs.tab = t;
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(l) = self.fwd.pop() {
            let t = self.fwd_tabs.pop().unwrap_or(l.tab());
            self.back.push(self.loc());
            self.back_tabs.push(self.prefs.tab);
            self.set_loc(l);
            self.prefs.tab = t;
        }
    }

    /// Skip the thumbnails still queued for the folder we are leaving.
    fn cancel_thumbs(&mut self) {
        if let Some(w) = &self.worker {
            w.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The window closed: stop background work and free the thumbnail textures.
    pub fn idle(&mut self) {
        if self.worker.is_some() || !self.textures.is_empty() {
            self.cancel_thumbs();
            self.textures.clear();
            self.used.clear();
            self.view_cache = None;
        }
        self.validated = false;
    }

    pub fn is_favorite(&self, key: &str) -> bool {
        self.prefs.favorites.iter().any(|f| f == key)
    }

    pub fn toggle_favorite(&mut self, key: String) {
        if let Some(i) = self.prefs.favorites.iter().position(|f| *f == key) {
            self.prefs.favorites.remove(i);
        } else {
            self.prefs.favorites.push(key);
        }
        self.fav_cache.clear();
        self.view_cache = None;
    }

    fn favorite(&mut self, key: &str) -> Option<Item> {
        if let Some((at, it)) = self.fav_cache.get(key) {
            if at.elapsed().as_secs_f32() < 3.0 {
                return it.clone();
            }
        }
        let it = favorite_item(key);
        self.fav_cache.insert(key.to_string(), (Instant::now(), it.clone()));
        it
    }

    fn list_dir(&mut self, dir: &Path) -> Vec<Item> {
        if let Some((at, items)) = self.listing.get(dir) {
            if at.elapsed().as_secs_f32() < 3.0 {
                return items.clone();
            }
        }
        let items = read_dir_items(dir);
        self.listing.insert(dir.to_path_buf(), (Instant::now(), items.clone()));
        items
    }

    fn subdirs(&mut self, dir: &Path) -> Vec<PathBuf> {
        self.list_dir(dir).into_iter().filter_map(|i| if let Target::Folder(Loc::Dir(p)) = i.target { Some(p) } else { None }).collect()
    }

    /// Forget cached listings (after a change on disk).
    pub fn refresh(&mut self) {
        self.listing.clear();
        self.fav_cache.clear();
        self.drives = None;
        self.view_cache = None;
        self.no_thumb.clear();
    }

    /// The items of a place, unfiltered and unsorted.
    pub fn items_at(&mut self, loc: &Loc) -> Vec<Item> {
        match loc {
            Loc::Dir(d) => self.list_dir(d),
            Loc::Examples(folder) => example_items(folder),
            Loc::Favorites => {
                let favs = self.prefs.favorites.clone();
                favs.iter().filter_map(|f| self.favorite(f)).collect()
            }
        }
    }
}

fn example_items(folder: &str) -> Vec<Item> {
    let mut out = vec![];
    for f in example_folders() {
        let parent = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if parent == folder {
            out.push(Item { name: f.rsplit('/').next().unwrap_or(&f).to_string(), target: Target::Folder(Loc::Examples(f.clone())), size: None, modified: None, created: None });
        }
    }
    for (i, e) in examples().iter().enumerate() {
        if e.folder() == folder {
            out.push(Item { name: e.name.clone(), target: Target::Example(i), size: Some(e.mel.len() as u64), modified: None, created: None });
        }
    }
    out
}

fn favorite_item(key: &str) -> Option<Item> {
    if let Some(p) = key.strip_prefix("example:") {
        let i = examples().iter().position(|e| e.path == p)?;
        let e = &examples()[i];
        return Some(Item { name: e.name.clone(), target: Target::Example(i), size: Some(e.mel.len() as u64), modified: None, created: None });
    }
    let p = PathBuf::from(key);
    let md = std::fs::metadata(&p).ok()?;
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| key.to_string());
    let target = if md.is_dir() { Target::Folder(Loc::Dir(p)) } else { Target::File(p) };
    Some(Item { name, target, size: md.is_file().then(|| md.len()), modified: md.modified().ok(), created: md.created().ok() })
}

fn read_dir_items(dir: &Path) -> Vec<Item> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // hidden files and Maya's thumbnail folders stay out of the way
        if name.starts_with('.') || name.eq_ignore_ascii_case("mayaSwatches") || name == "desktop.ini" || name.eq_ignore_ascii_case("thumbs.db") {
            continue;
        }
        let p = e.path();
        // follow links and junctions to what they point at (a dangling one stays a file)
        let Ok(md) = std::fs::metadata(&p).or_else(|_| e.metadata()) else { continue };
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::fs::MetadataExt;
            // hidden + system: legacy profile junctions such as "My Documents" (access denied)
            if e.metadata().is_ok_and(|m| m.file_attributes() & 0x6 == 0x6) {
                continue;
            }
        }
        let target = if md.is_dir() { Target::Folder(Loc::Dir(p)) } else { Target::File(p) };
        out.push(Item { name, target, size: md.is_file().then(|| md.len()), modified: md.modified().ok(), created: md.created().ok() });
    }
    out
}

/// Match an item name against the search text: `*` wildcards, or `.ext` for a file type.
pub fn matches_filter(name: &str, ext: &str, filter: &str) -> bool {
    let f = filter.trim().to_lowercase();
    if f.is_empty() {
        return true;
    }
    if let Some(e) = f.strip_prefix('.') {
        if !e.contains('*') {
            return ext == e;
        }
    }
    let n = name.to_lowercase();
    if !f.contains('*') {
        return n.contains(&f);
    }
    // glob with '*' only
    let parts: Vec<&str> = f.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match n[pos..].find(part) {
            Some(at) => {
                if i == 0 && at != 0 {
                    return false;
                }
                pos += at + part.len();
            }
            None => return false,
        }
    }
    parts.last().is_some_and(|l| l.is_empty()) || n.ends_with(parts.last().unwrap())
}

fn sort_items(items: &mut [Item], by: SortBy) {
    items.sort_by(|a, b| {
        b.is_folder().cmp(&a.is_folder()).then_with(|| match by {
            SortBy::Name => std::cmp::Ordering::Equal,
            SortBy::Type => a.ext().cmp(&b.ext()),
            SortBy::Size => b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)),
            SortBy::Date => b.modified.cmp(&a.modified),
        })
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The kind of thing a file is, for its icon and what a double-click does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Scene,
    Model,
    Image,
    Script,
    Other,
}

pub fn file_kind(p: &Path) -> FileKind {
    let e = ext_of(p);
    if SCENE_EXTS.contains(&e.as_str()) {
        FileKind::Scene
    } else if MODEL_EXTS.contains(&e.as_str()) {
        FileKind::Model
    } else if IMAGE_EXTS.contains(&e.as_str()) {
        FileKind::Image
    } else if e == "mel" {
        FileKind::Script
    } else {
        FileKind::Other
    }
}

fn icon_for(item: &Item) -> &'static str {
    match &item.target {
        Target::Folder(_) => "cbFolder",
        Target::Example(i) => match examples()[*i].kind {
            ExampleKind::Material => "cbMaterial",
            ExampleKind::Scene => "cbScene",
        },
        Target::File(p) => match file_kind(p) {
            FileKind::Scene => "cbScene",
            FileKind::Model => "cbModel",
            FileKind::Image => "cbImage",
            FileKind::Script => "cbScript",
            FileKind::Other => "cbFile",
        },
    }
}

// ------------------------------------------------------------------ thumbnails

fn thumb_key(item: &Item) -> Option<String> {
    match &item.target {
        Target::Example(i) => Some(format!("example:{}", examples()[*i].path)),
        Target::File(p) => {
            let kind = file_kind(p);
            if !matches!(kind, FileKind::Scene | FileKind::Model | FileKind::Image) || ext_of(p) == "mb" {
                return None;
            }
            let mtime = item.modified.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            Some(format!("{}|{}|{}", p.display(), item.size.unwrap_or(0), mtime))
        }
        Target::Folder(_) => None,
    }
}

fn thumb_cache_path(key: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    Some(eframe::storage_dir("Inca")?.join("thumbs").join(format!("{:016x}.png", h.finish())))
}

fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some((img.width(), img.height(), img.into_raw()))
}

/// Thumbnail pixels of a user file: images are scaled down, scenes and models are path traced
/// at low quality from a three-quarter view that frames everything.
pub fn thumbnail_for(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    match file_kind(path) {
        FileKind::Image => {
            let img = image::open(path).ok()?.thumbnail(THUMB_PX * 2, THUMB_PX * 2).to_rgba8();
            Some((img.width(), img.height(), img.into_raw()))
        }
        FileKind::Scene | FileKind::Model => {
            if std::fs::metadata(path).ok()?.len() > THUMB_MAX_BYTES {
                return None;
            }
            let mut s = inca_core::scene::Scene::new();
            let r = if SCENE_EXTS.contains(&ext_of(path).as_str()) { inca_core::io::open_scene(&mut s, path).map(|_| ()) } else { inca_core::io::import(&mut s, path).map(|_| ()) };
            r.ok()?;
            render_scene_thumb(&mut s, THUMB_PX, 8)
        }
        _ => None,
    }
}

/// Path trace a small square picture of a scene: from its render camera when it names one
/// (defaultRenderGlobals.camera), otherwise from the persp camera moved to frame all visible
/// meshes from a three-quarter view.
pub fn render_scene_thumb(s: &mut inca_core::scene::Scene, px: u32, samples: u32) -> Option<(u32, u32, Vec<u8>)> {
    let render_cam = s.render.get("camera").and_then(|v| v.as_str()).and_then(|c| s.find(c)).filter(|c| s.name_of(c) != "persp");
    if let Some(cam) = render_cam {
        let name = s.name_of(&cam);
        return Some(path_trace(s, &name, px, samples));
    }
    let mut bb = inca_core::math::Aabb::EMPTY;
    let ids: Vec<String> = s.nodes.iter().filter(|(_, n)| n.mesh().is_some() && !n.startup()).map(|(id, _)| id.clone()).collect();
    for id in &ids {
        if s.is_visible(id) {
            bb.union(&s.world_bbox(id));
        }
    }
    if bb.is_empty() {
        return None;
    }
    let cam = s.find("persp")?;
    let c = (bb.min + bb.max) * 0.5;
    let r = ((bb.max - bb.min).length() * 0.5).max(1e-3);
    let (elev, yaw) = (25f64.to_radians(), 35f64.to_radians());
    // a plain perspective camera: the scene is a throwaway copy
    if let Some(n) = s.nodes.get_mut(&cam) {
        if let inca_core::scene::Shape::Camera(c) = &mut n.shape {
            c.ortho = false;
            c.focal_length = 35.0;
            c.horizontal_film_aperture = 1.417;
        }
    }
    let fov = inca_core::scene::CamShape::default().fov_y(1.0);
    let d = r / (fov * 0.5).sin() * 1.05;
    let p = c + glam::DVec3::new(yaw.sin() * elev.cos(), elev.sin(), yaw.cos() * elev.cos()) * d;
    for (a, v) in [("tx", p.x), ("ty", p.y), ("tz", p.z), ("rx", -elev.to_degrees()), ("ry", yaw.to_degrees()), ("rz", 0.0)] {
        s.set_attr_f(&cam, a, v);
    }
    let name = s.name_of(&cam);
    Some(path_trace(s, &name, px, samples))
}

fn path_trace(s: &inca_core::scene::Scene, camera: &str, px: u32, samples: u32) -> (u32, u32, Vec<u8>) {
    let mut st = inca_core::render::RenderSettings::from_scene(s);
    st.width = px;
    st.height = px;
    st.samples = samples;
    st.bounces = 2;
    st.pixel_aspect = 1.0;
    st.transparent = false;
    if !s.render.contains_key("background") {
        st.background = [0.16, 0.16, 0.16];
    }
    let rs = inca_core::render::RenderScene::build(s, camera, st);
    let mut acc = vec![0f32; (px * px * 3) as usize];
    for pass in 0..samples {
        rs.render_pass(&mut acc, pass);
    }
    (px, px, rs.resolve(&acc, samples))
}

/// Edge of a material preset's live swatch.
const SWATCH_PX: usize = 160;

/// A material preset drawn as a lit ball (the Hypershade swatch): its MEL runs in a throwaway
/// scene and the material it makes is shaded on the CPU, so no render is needed.
pub fn material_swatch(e: &Example, px: usize) -> Option<Pixels> {
    if e.kind != ExampleKind::Material {
        return None;
    }
    let mut s = inca_core::scene::Scene::new();
    let before: HashSet<String> = s.material_ids().into_iter().collect();
    let mut mel = inca_core::mel::Interp::new();
    mel.run(&mut s, &mut inca_core::commands::NoHost, e.scene_part()).ok()?;
    let id = s.material_ids().into_iter().rev().find(|m| !before.contains(m))?;
    let m = s.material(&id)?;
    let tex = m.maps.get(m.color_attr()).and_then(|t| t.as_str()).and_then(|t| s.texture(t));
    let place = tex.as_ref().and_then(|t| s.place_of(t));
    let rgba = inca_core::materials::swatch(&m, tex.as_ref(), place.as_ref().map(|p| &p.attrs), px);
    Some((px as u32, px as u32, rgba))
}

impl CbState {
    /// The thumbnail of an item, if there is one yet (user files render in the background).
    fn thumbnail(&mut self, ctx: &egui::Context, item: &Item) -> Option<egui::TextureHandle> {
        let key = thumb_key(item)?;
        if let Some(t) = self.textures.get(&key) {
            let t = t.clone();
            self.used.insert(key, self.frame);
            return Some(t);
        }
        if self.no_thumb.contains(&key) || self.pending.contains(&key) {
            return None;
        }
        let px = match &item.target {
            // a material preset without a rendered thumbnail gets a live swatch
            Target::Example(i) => examples()[*i].thumb.and_then(decode_png).or_else(|| material_swatch(&examples()[*i], SWATCH_PX)),
            Target::File(p) => {
                // the disk cache, else ask the worker
                match thumb_cache_path(&key).and_then(|c| std::fs::read(c).ok()).and_then(|b| decode_png(&b)) {
                    Some(px) => Some(px),
                    None => {
                        let w = self.worker.get_or_insert_with(ThumbWorker::start);
                        let gen = w.generation.load(Ordering::Relaxed);
                        if w.tx.send((key.clone(), p.clone(), gen)).is_ok() {
                            self.pending.insert(key);
                        } else {
                            // the worker is gone: start a fresh one next time
                            self.worker = None;
                        }
                        return None;
                    }
                }
            }
            Target::Folder(_) => None,
        };
        self.store_thumb(ctx, key, px)
    }

    fn store_thumb(&mut self, ctx: &egui::Context, key: String, px: Option<(u32, u32, Vec<u8>)>) -> Option<egui::TextureHandle> {
        let Some((w, h, rgba)) = px else {
            self.no_thumb.insert(key);
            return None;
        };
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let tex = ctx.load_texture(format!("cb:{key}"), img, egui::TextureOptions::LINEAR);
        self.used.insert(key.clone(), self.frame);
        self.textures.insert(key, tex.clone());
        Some(tex)
    }

    /// Take finished background thumbnails (and save them to the disk cache).
    fn poll_thumbs(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        // keep GPU memory bounded: drop textures not drawn lately (the disk cache brings them
        // back quickly)
        if self.textures.len() > 400 {
            let (frame, used) = (self.frame, &self.used);
            self.textures.retain(|k, _| used.get(k).is_some_and(|f| frame - f < 3));
            let keep: HashSet<String> = self.textures.keys().cloned().collect();
            self.used.retain(|k, _| keep.contains(k));
        }
        let mut done = vec![];
        let mut dead = false;
        if let Some(w) = &self.worker {
            loop {
                match w.rx.try_recv() {
                    Ok(r) => done.push(r),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        dead = true;
                        break;
                    }
                }
            }
        }
        if dead {
            self.worker = None;
            self.no_thumb.extend(self.pending.drain());
        }
        for (key, answer) in done {
            self.pending.remove(&key);
            // skipped as stale: it may be asked for again later
            let Ok(px) = answer else { continue };
            if let (Some((w, h, rgba)), Some(path)) = (&px, thumb_cache_path(&key)) {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = image::save_buffer(&path, rgba, *w, *h, image::ColorType::Rgba8);
            }
            self.store_thumb(ctx, key, px);
        }
        if !self.pending.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

// ------------------------------------------------------------------ actions

/// A path as the inside of a MEL string literal: forward slashes, quotes escaped.
fn mel_path(p: &Path) -> String {
    mel_escape(&p.display().to_string().replace('\\', "/"))
}

fn mel_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Run example MEL as one undo step without echoing every line.
fn run_quiet(app: &mut IncaApp, label: &str, src: &str) -> Result<(), String> {
    app.s.checkpoint(label);
    let before = app.s.rev;
    let r = app.exec_mel(src, false).map(|_| ());
    if app.s.rev == before {
        app.s.undo_stack.pop();
    }
    if let Err(e) = &r {
        app.last_result = format!("// Error: {e} //");
    }
    r
}

/// Open a built-in example as a new, untitled scene.
pub fn open_example(app: &mut IncaApp, i: usize) {
    let e = &examples()[i];
    app.s.reset();
    app.smooth_cache.clear();
    app.stats_cache.clear();
    app.windows_state.hs.reset_work_tabs();
    app.reset_camera_names();
    app.clear_view_history();
    let src = format!("{}\n{}\nselect -cl;\n", e.scene_part(), e.settings_part());
    let r = app.exec_mel(&src, false);
    app.s.touch();
    app.s.undo_stack.clear();
    app.s.redo_stack.clear();
    app.s.file = None;
    app.s.modified = false;
    match r {
        Ok(_) => app.echo(format!("// Content Browser: opened example \"{}\"", e.path)),
        Err(err) => app.last_result = format!("// Error: {err} //"),
    }
}

/// Import a built-in example into the current scene (one undo step), or apply a material
/// preset to the selection.
pub fn import_example(app: &mut IncaApp, i: usize) {
    let e = &examples()[i];
    let label = if e.kind == ExampleKind::Material { "apply material preset" } else { "import example" };
    if run_quiet(app, label, e.scene_part()).is_ok() {
        app.echo(format!("// Content Browser: {} \"{}\"", if e.kind == ExampleKind::Material { "applied material preset" } else { "imported example" }, e.path));
    }
}

/// An image as a file texture: on the selected objects through a new lambert, otherwise just
/// the texture node.
pub fn image_to_texture(app: &mut IncaApp, p: &Path, objects: &[String]) {
    let stem = p.file_stem().map(|s| s.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>()).unwrap_or_else(|| "image".into());
    // MEL names cannot start with a digit (4k_wood.jpg would lex as 4 and k_wood)
    let stem = if stem.is_empty() || stem.starts_with(|c: char| c.is_ascii_digit()) { format!("img_{stem}") } else { stem };
    let mut mel = format!("string $tex = `shadingNode -asTexture file -n {stem}File`;\nsetAttr ($tex + \".fileTextureName\") -type \"string\" \"{}\";\n", mel_path(p));
    if !objects.is_empty() {
        let names: Vec<String> = objects.iter().map(|o| app.s.name_of(o)).collect();
        mel += &format!("string $m = `shadingNode -asShader lambert -n {stem}Mat`;\nconnectAttr -f ($tex + \".outColor\") ($m + \".color\");\nselect -r {};\nhyperShade -assign $m;\n", names.join(" "));
    }
    let _ = app.run_mel("file texture", &mel);
}

/// What a double-click does (Maya imports; Inca opens a scene into an empty scene, like a
/// file dropped on the window).
pub fn activate(app: &mut IncaApp, item: &Item) {
    match &item.target {
        Target::Folder(l) => app.windows_state.cb.go(l.clone()),
        Target::Example(i) => match examples()[*i].kind {
            ExampleKind::Material => import_example(app, *i),
            ExampleKind::Scene if scene_is_empty(app) => open(app, item),
            ExampleKind::Scene => import_example(app, *i),
        },
        Target::File(p) => match file_kind(p) {
            FileKind::Scene if scene_is_empty(app) && ext_of(p) != "mb" => open(app, item),
            FileKind::Scene | FileKind::Model => import_file(app, p),
            FileKind::Script => source_mel(app, p),
            FileKind::Image => {
                let sel = app.s.sel.clone();
                image_to_texture(app, p, &sel);
            }
            FileKind::Other => app.last_result = format!("// Warning: Inca can't open .{} files //", ext_of(p)),
        },
    }
}

/// Open a scene (asking to save changes first); examples open as a new untitled scene.
pub fn open(app: &mut IncaApp, item: &Item) {
    if app.s.modified && app.test.is_none() {
        app.windows_state.cb.confirm_open = Some(item.clone());
        return;
    }
    open_now(app, item);
}

fn open_now(app: &mut IncaApp, item: &Item) {
    match &item.target {
        Target::Example(i) if examples()[*i].kind == ExampleKind::Scene => open_example(app, *i),
        Target::File(p) => app.open_path(p),
        _ => activate(app, item),
    }
}

pub fn import_file(app: &mut IncaApp, p: &Path) {
    let _ = app.run_mel("import", &format!("file -i \"{}\";", mel_path(p)));
}

fn source_mel(app: &mut IncaApp, p: &Path) {
    match std::fs::read_to_string(p) {
        Ok(src) => {
            if run_quiet(app, "source", &src).is_ok() {
                app.echo(format!("source \"{}\";", mel_path(p)));
            }
        }
        Err(e) => app.last_result = format!("// Error: {e} //"),
    }
}

fn scene_is_empty(app: &IncaApp) -> bool {
    app.s.nodes.values().all(|n| n.startup())
}

fn show_in_explorer(p: &Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // explorer wants /select,"path" with only the path quoted
        let _ = if p.is_dir() { std::process::Command::new("explorer").arg(p).spawn() } else { std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", p.display())).spawn() };
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(p).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(if p.is_dir() { p } else { p.parent().unwrap_or(p) }).spawn();
    }
}

/// Whether a release at `q` lands on a viewport (not on a window in front of it).
fn can_drop_at(app: &IncaApp, ctx: &egui::Context, q: egui::Pos2) -> bool {
    !ctx.layer_id_at(q).is_some_and(|l| l.order != egui::Order::Background) && app.vps.iter().any(|vp| vp.rect.contains(q))
}

/// Drop a dragged item at a screen point: on a viewport, a material preset or image goes onto
/// the object under the cursor, and anything else is imported.
pub fn drop_item(app: &mut IncaApp, item: &Item, q: egui::Pos2) -> bool {
    let Some(idx) = app.vps.iter().position(|vp| vp.rect.contains(q)) else { return false };
    let rect = app.vps[idx].rect;
    let cam = app.vps[idx].camera.clone();
    let cv = crate::viewport::cam_view(&app.s, &cam, [rect.width(), rect.height()]);
    let s = &app.s;
    let obj = crate::viewport::pick_object_if(s, &app.vps[idx], &cv, q, &mut app.mesh_cache, &|id| !s.is_template(id)).filter(|o| app.s.node(o).and_then(|n| n.mesh()).is_some());
    match &item.target {
        Target::Example(i) if examples()[*i].kind == ExampleKind::Material => {
            let Some(o) = obj else { return false };
            app.select_names(&[o], inca_core::scene::SelMode::Replace);
            import_example(app, *i);
        }
        Target::File(p) if file_kind(p) == FileKind::Image => image_to_texture(app, p, &obj.into_iter().collect::<Vec<_>>()),
        Target::File(p) if file_kind(p) == FileKind::Script => source_mel(app, p),
        Target::File(p) if matches!(file_kind(p), FileKind::Scene | FileKind::Model) => import_file(app, p),
        Target::Example(i) => import_example(app, *i),
        _ => return false,
    }
    true
}

// ------------------------------------------------------------------ UI

/// The window body.
pub fn window(app: &mut IncaApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    app.windows_state.cb.poll_thumbs(&ctx);
    if !app.windows_state.cb.validated {
        // a remembered folder may be gone, or belong to another project
        app.windows_state.cb.validated = true;
        let tab = app.windows_state.cb.prefs.tab;
        let root = crate::project::default_dir(app, "");
        let stale = match &app.windows_state.cb.prefs.loc {
            None => true,
            Some(Loc::Dir(d)) => !d.is_dir() || (tab == CbTab::Projects && !d.starts_with(&root)),
            Some(_) => false,
        };
        if stale {
            let loc = initial_loc(app, tab);
            let cb = &mut app.windows_state.cb;
            cb.prefs.loc = Some(loc);
            cb.back.clear();
            cb.fwd.clear();
            cb.back_tabs.clear();
            cb.fwd_tabs.clear();
        }
    }
    path_bar(app, ui);
    toolbar(app, ui);
    ui.separator();
    let cb = &app.windows_state.cb;
    let (show_meta, show_preview) = (cb.prefs.show_meta, cb.prefs.show_preview);
    egui::Panel::left("cb_tree").default_size(280.0).min_size(180.0).max_size(420.0).resizable(true).show(ui, |ui| tree_panel(app, ui));
    if show_meta || show_preview {
        egui::Panel::right("cb_info").exact_size(230.0).resizable(false).show(ui, |ui| info_panel(app, ui, show_meta, show_preview));
    }
    egui::CentralPanel::default().show(ui, |ui| content(app, ui));
    confirm_dialog(app, &ctx);
    drag_feedback(app, &ctx);
}

/// Show a place given as text (MEL `contentBrowser -location`): `Examples`, `Examples/<folder>`,
/// `Favorites`, or a directory path.
pub fn go_to_location(app: &mut IncaApp, place: &str) {
    let place = place.trim().trim_end_matches('/');
    let loc = if place.eq_ignore_ascii_case("Favorites") {
        Loc::Favorites
    } else if let Some(rest) = place.strip_prefix("Examples").filter(|r| r.is_empty() || r.starts_with('/')) {
        Loc::Examples(rest.trim_start_matches('/').to_string())
    } else if Path::new(place).is_dir() {
        Loc::Dir(PathBuf::from(place))
    } else {
        app.last_result = format!("// Warning: contentBrowser: no such location \"{place}\" //");
        return;
    };
    app.windows_state.cb.go(loc);
}

fn initial_loc(app: &IncaApp, tab: CbTab) -> Loc {
    match tab {
        CbTab::Projects => Loc::Dir(crate::project::default_dir(app, "")),
        CbTab::FileSystem => Loc::Dir(home_dir()),
        CbTab::Examples => Loc::Examples(String::new()),
        CbTab::Favorites => Loc::Favorites,
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn path_bar(app: &mut IncaApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let cb = &mut app.windows_state.cb;
        if ui.add_enabled(!cb.back.is_empty(), egui::Button::new("◀").small()).on_hover_text("Back").clicked() {
            cb.go_back();
        }
        if ui.add_enabled(!cb.fwd.is_empty(), egui::Button::new("▶").small()).on_hover_text("Forward").clicked() {
            cb.go_forward();
        }
        let loc = cb.loc();
        if let Some(edit) = &mut cb.path_edit {
            let r = ui.add(egui::TextEdit::singleline(edit).desired_width((ui.available_width() - 30.0).max(80.0)));
            if std::mem::take(&mut cb.focus_field) {
                r.request_focus();
            } else if r.lost_focus() || !r.has_focus() {
                // Enter goes there; clicking elsewhere or Esc just closes the field
                let typed = PathBuf::from(edit.trim());
                cb.path_edit = None;
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && typed.is_dir() {
                    cb.go(Loc::Dir(typed));
                }
            }
            return;
        }
        // breadcrumbs
        let crumbs: Vec<(String, Loc)> = match &loc {
            Loc::Dir(p) => {
                let mut v = vec![];
                let mut acc = PathBuf::new();
                for c in p.components() {
                    acc.push(c.as_os_str());
                    let label = match c {
                        std::path::Component::Prefix(pr) => pr.as_os_str().to_string_lossy().to_string(),
                        std::path::Component::RootDir => continue,
                        other => other.as_os_str().to_string_lossy().to_string(),
                    };
                    let mut target = acc.clone();
                    if matches!(c, std::path::Component::Prefix(_)) {
                        target.push("\\");
                    }
                    v.push((label, Loc::Dir(target)));
                }
                v
            }
            Loc::Examples(f) => {
                let mut v = vec![("Examples".to_string(), Loc::Examples(String::new()))];
                let mut acc = String::new();
                for part in f.split('/').filter(|p| !p.is_empty()) {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    v.push((part.to_string(), Loc::Examples(acc.clone())));
                }
                v
            }
            Loc::Favorites => vec![("Favorites".to_string(), Loc::Favorites)],
        };
        let n = crumbs.len();
        let mut go = None;
        // a long path scrolls instead of widening the window; the last folder stays in view
        let crumb_w = (ui.available_width() - 60.0).max(40.0);
        egui::ScrollArea::horizontal()
            .id_salt("cb_crumbs")
            .max_width(crumb_w)
            .auto_shrink([true, true])
            .stick_to_right(true)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, (label, l)) in crumbs.into_iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new(">").color(theme::TEXT_DIM));
                        }
                        let text = if i + 1 == n { RichText::new(label).strong() } else { RichText::new(label) };
                        if ui.add(egui::Button::new(text).frame(false)).clicked() {
                            go = Some(l);
                        }
                    }
                });
            });
        if let Some(l) = go {
            cb.go(l);
        }
        // the arrow lists recent places; click the empty part of the bar to type a path
        let history: Vec<Loc> = cb.back.iter().rev().take(12).cloned().collect();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.menu_button("Recent", |ui| {
            if history.is_empty() {
                ui.label(RichText::new("No history").color(theme::TEXT_DIM));
            }
            for l in history {
                let label = match &l {
                    Loc::Dir(p) => p.display().to_string(),
                    Loc::Examples(f) => format!("Examples/{f}"),
                    Loc::Favorites => "Favorites".into(),
                };
                if ui.button(label).clicked() {
                    cb.go(l);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("Recent locations");
        let rest = ui.available_rect_before_wrap();
        if rest.width() > 8.0 && ui.allocate_rect(rest, Sense::click()).on_hover_text("Click to type a path").clicked() {
            if let Loc::Dir(p) = &loc {
                cb.path_edit = Some(p.display().to_string());
                cb.focus_field = true;
            }
        }
        });
    });
}

fn toolbar(app: &mut IncaApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let loc = app.windows_state.cb.loc();
        let cb = &mut app.windows_state.cb;
        let list = cb.prefs.list_view;
        if crate::icons::button(ui, if list { "cbThumbs" } else { "cbList" }, if list { "Thumbnail view" } else { "List view" }, false, 22.0).clicked() {
            cb.prefs.list_view = !list;
        }
        let parent = loc.parent();
        if crate::icons::button_ex(ui, "cbUp", "Up one level", false, 22.0, 18.0, parent.is_some()).clicked() {
            if let Some(p) = parent {
                cb.go(p);
            }
        }
        let is_dir = matches!(loc, Loc::Dir(_));
        if crate::icons::button_ex(ui, "cbNewFolder", "New folder", false, 22.0, 18.0, is_dir).clicked() {
            cb.new_folder = Some("New Folder".into());
            cb.focus_field = true;
        }
        let fav_key = cb.selected.as_ref().and_then(|t| match t {
            Target::File(p) | Target::Folder(Loc::Dir(p)) => Some(p.to_string_lossy().to_string()),
            Target::Example(i) => Some(format!("example:{}", examples()[*i].path)),
            _ => None,
        });
        let fav_on = fav_key.as_ref().is_some_and(|k| cb.is_favorite(k));
        if crate::icons::button_ex(ui, "cbStar", if fav_on { "Remove from Favorites" } else { "Add to Favorites" }, fav_on, 22.0, 18.0, fav_key.is_some()).clicked() {
            if let Some(k) = fav_key {
                cb.toggle_favorite(k);
            }
        }
        egui::ComboBox::from_id_salt("cb_sort")
            .width(70.0)
            .selected_text(format!("{:?}", cb.prefs.sort))
            .show_ui(ui, |ui| {
                for s in [SortBy::Name, SortBy::Type, SortBy::Size, SortBy::Date] {
                    ui.selectable_value(&mut cb.prefs.sort, s, format!("{s:?}"));
                }
            })
            .response
            .on_hover_text("Sort by");
        ui.menu_button("⚙", |ui| {
            ui.checkbox(&mut cb.prefs.show_meta, "Metadata");
            ui.checkbox(&mut cb.prefs.show_preview, "Preview");
            ui.separator();
            if ui.button("Refresh").clicked() {
                cb.refresh();
                ui.close();
            }
        })
        .response
        .on_hover_text("Panels");
        ui.add(egui::Slider::new(&mut cb.prefs.thumb_size, 64.0..=256.0).show_value(false)).on_hover_text("Thumbnail size");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !cb.filter.is_empty() && ui.small_button("×").on_hover_text("Clear search").clicked() {
                cb.filter.clear();
            }
            ui.add(egui::TextEdit::singleline(&mut cb.filter).hint_text("Search...").desired_width(150.0)).on_hover_text("Filter this folder: * is a wildcard, .ma shows one file type");
        });
    });
}

fn tree_panel(app: &mut IncaApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let cur = app.windows_state.cb.prefs.tab;
        for t in CbTab::ALL {
            if ui.selectable_label(cur == t, RichText::new(t.label()).size(11.5)).clicked() && cur != t {
                let loc = initial_loc(app, t);
                let cb = &mut app.windows_state.cb;
                // history records the tab we are leaving
                cb.go(loc);
                cb.prefs.tab = t;
            }
        }
    });
    ui.separator();
    egui::ScrollArea::both().id_salt("cb_tree_scroll").auto_shrink([false, false]).show(ui, |ui| match app.windows_state.cb.prefs.tab {
        CbTab::Projects => {
            let root = crate::project::default_dir(app, "");
            let label = app.s.project.as_ref().and_then(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| "default".into());
            if !root.is_dir() {
                ui.label(RichText::new("No project folder yet.\nFile > Project Window creates one.").color(theme::TEXT_DIM));
            }
            dir_tree(app, ui, &root, &label, 0);
        }
        CbTab::FileSystem => {
            let home = home_dir();
            dir_tree(app, ui, &home, &home.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Home".into()), 0);
            let ds = app.windows_state.cb.drives.get_or_insert_with(drives).clone();
            for d in ds {
                let label = d.display().to_string();
                dir_tree(app, ui, &d, &label, 0);
            }
        }
        CbTab::Examples => example_tree(app, ui, "", "Examples", 0),
        CbTab::Favorites => {
            let sel = app.windows_state.cb.loc() == Loc::Favorites;
            if tree_row(ui, 0, None, "Favorites", "cbStar", sel).1 {
                app.windows_state.cb.go(Loc::Favorites);
            }
            let favs: Vec<String> = app.windows_state.cb.prefs.favorites.clone();
            for f in favs.iter().filter(|f| !f.starts_with("example:")) {
                // checked on disk at most every few seconds
                if !app.windows_state.cb.favorite(f).is_some_and(|i| i.is_folder()) {
                    continue;
                }
                let p = PathBuf::from(f);
                let label = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| f.clone());
                dir_tree(app, ui, &p, &label, 1);
            }
        }
    });
}

/// The drive roots, from the system's drive mask: no disk access, so a sleeping or
/// disconnected network drive cannot stall the UI.
fn drives() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLogicalDrives() -> u32;
        }
        // SAFETY: GetLogicalDrives takes no arguments and only returns a bit mask.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char))).collect()
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![PathBuf::from("/")]
    }
}

/// One tree row: an expander when `expanded` is Some, an icon and a label.
/// Returns (expander clicked, row clicked).
fn tree_row(ui: &mut egui::Ui, depth: usize, expanded: Option<bool>, label: &str, icon: &str, selected: bool) -> (bool, bool) {
    let h = 20.0;
    let font = egui::FontId::proportional(12.0);
    let text_w = ui.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE).size().x);
    let need = 4.0 + depth as f32 * 14.0 + 34.0 + text_w + 6.0;
    let w = ui.available_width().max(need).max(120.0);
    let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    if selected {
        ui.painter().rect_filled(r, 0.0, theme::HI);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, 0.0, Color32::from_gray(0x4d));
    }
    let x = r.min.x + 4.0 + depth as f32 * 14.0;
    let ex = Rect::from_center_size(pos2(x + 6.0, r.center().y), vec2(12.0, 12.0));
    let mut toggled = false;
    if let Some(open) = expanded {
        let c = r.center().y;
        let pts = if open { vec![pos2(ex.min.x + 2.0, c - 2.5), pos2(ex.max.x - 2.0, c - 2.5), pos2(ex.center().x, c + 3.0)] } else { vec![pos2(ex.min.x + 3.0, c - 4.0), pos2(ex.min.x + 3.0, c + 4.0), pos2(ex.max.x - 2.0, c)] };
        ui.painter().add(egui::Shape::convex_polygon(pts, theme::TEXT, Stroke::NONE));
        if resp.clicked() && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| ex.expand(3.0).contains(p)) {
            toggled = true;
        }
    }
    crate::icons::paint(ui, Rect::from_min_size(pos2(x + 14.0, r.min.y + 2.0), vec2(16.0, 16.0)), icon, Color32::WHITE);
    ui.painter().text(pos2(x + 34.0, r.center().y), egui::Align2::LEFT_CENTER, label, font, if selected { Color32::WHITE } else { theme::TEXT });
    (toggled, resp.clicked() && !toggled)
}

fn dir_tree(app: &mut IncaApp, ui: &mut egui::Ui, dir: &Path, label: &str, depth: usize) {
    let open = app.windows_state.cb.expanded.contains(dir);
    let sel = app.windows_state.cb.loc() == Loc::Dir(dir.to_path_buf());
    let (toggle, click) = tree_row(ui, depth, Some(open), label, "cbFolder", sel);
    let cb = &mut app.windows_state.cb;
    if toggle {
        if open {
            cb.expanded.remove(dir);
        } else {
            cb.expanded.insert(dir.to_path_buf());
        }
    }
    if click {
        cb.go(Loc::Dir(dir.to_path_buf()));
        cb.expanded.insert(dir.to_path_buf());
    }
    if open || click {
        if depth > 24 {
            return;
        }
        let subs = app.windows_state.cb.subdirs(dir);
        for s in subs {
            let name = s.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            dir_tree(app, ui, &s, &name, depth + 1);
        }
    }
}

fn example_tree(app: &mut IncaApp, ui: &mut egui::Ui, folder: &str, label: &str, depth: usize) {
    let open = folder.is_empty() || app.windows_state.cb.examples_expanded.contains(folder);
    let sel = app.windows_state.cb.loc() == Loc::Examples(folder.to_string());
    let children: Vec<String> = example_folders().into_iter().filter(|f| f.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == folder && f != folder).collect();
    let (toggle, click) = tree_row(ui, depth, (!children.is_empty()).then_some(open), label, "cbFolder", sel);
    let cb = &mut app.windows_state.cb;
    if toggle && !folder.is_empty() {
        if open {
            cb.examples_expanded.remove(folder);
        } else {
            cb.examples_expanded.insert(folder.to_string());
        }
    }
    if click {
        cb.go(Loc::Examples(folder.to_string()));
        cb.examples_expanded.insert(folder.to_string());
    }
    if open || click {
        for c in children {
            let name = c.rsplit('/').next().unwrap_or(&c).to_string();
            example_tree(app, ui, &c, &name, depth + 1);
        }
    }
}

/// The items shown in the content view: filtered and sorted, rebuilt only when the place, the
/// search, the sort or the folder listing changes.
pub fn visible_items(app: &mut IncaApp) -> Arc<Vec<Item>> {
    use std::hash::{Hash, Hasher};
    let loc = app.windows_state.cb.loc();
    let cb = &mut app.windows_state.cb;
    // the listing time stamp (or favorites list) is part of the key, so a re-read rebuilds it
    let stamp = match &loc {
        Loc::Dir(d) => {
            cb.list_dir(d);
            cb.listing.get(d).map(|(at, _)| *at)
        }
        _ => None,
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&loc, &cb.filter, cb.prefs.sort as u8, stamp, &cb.prefs.favorites).hash(&mut h);
    let key = h.finish();
    if let Some((k, items)) = &cb.view_cache {
        if *k == key && !matches!(loc, Loc::Favorites) {
            return items.clone();
        }
    }
    let mut items = cb.items_at(&loc);
    let filter = cb.filter.clone();
    items.retain(|i| matches_filter(&i.name, &i.ext(), &filter) || (i.is_folder() && filter.trim().starts_with('.')));
    sort_items(&mut items, cb.prefs.sort);
    let items = Arc::new(items);
    cb.view_cache = Some((key, items.clone()));
    items
}

fn content(app: &mut IncaApp, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let items = visible_items(app);
    // new folder name field
    if let (Some(name), Loc::Dir(dir)) = (app.windows_state.cb.new_folder.clone(), app.windows_state.cb.loc()) {
        ui.horizontal(|ui| {
            ui.label("New folder:");
            let mut n = name;
            let r = ui.text_edit_singleline(&mut n);
            let cb = &mut app.windows_state.cb;
            let opened = std::mem::take(&mut cb.focus_field);
            if opened {
                r.request_focus();
            }
            cb.new_folder = Some(n.clone());
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) || (!opened && !enter && (r.lost_focus() || !r.has_focus())) {
                cb.new_folder = None;
            } else if enter {
                cb.new_folder = None;
                let n = n.trim();
                if !n.is_empty() {
                    match std::fs::create_dir(dir.join(n)) {
                        Ok(()) => cb.refresh(),
                        Err(e) => app.last_result = format!("// Error: {e} //"),
                    }
                }
            }
        });
    }
    app.windows_state.cb.tile_rects.clear();
    let bg = ui.interact(ui.available_rect_before_wrap(), egui::Id::new("cb_content_bg"), Sense::click());
    let mut clicked_any = false;
    // each place keeps its own scroll position
    let scroll_id = ("cb_content", app.windows_state.cb.loc());
    egui::ScrollArea::vertical().id_salt(scroll_id).auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        if items.is_empty() {
            let msg = match app.windows_state.cb.loc() {
                Loc::Favorites => "No favorites yet. Select a file or example and click the star.",
                _ if !app.windows_state.cb.filter.is_empty() => "Nothing matches the search.",
                _ => "This folder is empty.",
            };
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.label(RichText::new(msg).color(theme::TEXT_DIM)));
            return;
        }
        if app.windows_state.cb.prefs.list_view {
            list_view(app, ui, &ctx, &items, viewport, &mut clicked_any);
        } else {
            grid_view(app, ui, &ctx, &items, viewport, &mut clicked_any);
        }
    });
    if bg.clicked() && !clicked_any {
        app.windows_state.cb.selected = None;
    }
}

fn grid_view(app: &mut IncaApp, ui: &mut egui::Ui, ctx: &egui::Context, items: &[Item], viewport: Rect, clicked_any: &mut bool) {
    let size = app.windows_state.cb.prefs.thumb_size.clamp(48.0, 320.0);
    let tile_w = size + 8.0;
    let label_h = 18.0;
    let row_h = size + 4.0 + label_h + 4.0;
    // the real spacing between tiles: a column count that does not fit would widen the window
    let sp = ui.spacing().item_spacing.x;
    let cols = (((ui.available_width() + sp) / (tile_w + sp)).floor() as usize).max(1);
    let rows = items.len().div_ceil(cols);
    let top = ui.cursor().min;
    // only the rows in view are laid out; the rest is empty space of the right height
    let first = ((viewport.min.y / row_h).floor().max(0.0) as usize).min(rows);
    let last = ((viewport.max.y / row_h).ceil().max(0.0) as usize + 1).min(rows);
    ui.set_min_height(rows as f32 * row_h);
    for row in first..last {
        for col in 0..cols {
            let Some(item) = items.get(row * cols + col) else { break };
            let tile = Rect::from_min_size(top + vec2(col as f32 * (tile_w + sp), row as f32 * row_h), vec2(tile_w, size + 4.0 + label_h));
            let resp = ui.interact(tile, egui::Id::new(("cb_tile", &item.target_key())), Sense::click_and_drag());
            let sel = app.windows_state.cb.selected.as_ref() == Some(&item.target);
            let frame = Rect::from_min_size(tile.min + vec2(4.0, 2.0), vec2(size, size));
            if ui.is_rect_visible(tile) {
                let tex = app.windows_state.cb.thumbnail(ctx, item);
                ui.painter().rect_filled(frame, 2.0, if item.is_folder() { Color32::TRANSPARENT } else { Color32::BLACK });
                if let Some(t) = tex {
                    let [w, h] = t.size().map(|v| v as f32);
                    let s = size / w.max(h);
                    ui.painter().image(t.id(), Rect::from_center_size(frame.center(), vec2(w * s, h * s)), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                } else {
                    let k = if item.is_folder() { 0.78 } else { 0.5 };
                    crate::icons::paint(ui, Rect::from_center_size(frame.center(), vec2(size * k, size * k)), icon_for(item), Color32::WHITE);
                }
                if sel {
                    ui.painter().rect_stroke(frame.expand(1.0), 2.0, Stroke::new(2.0, Color32::from_rgb(0x5e, 0x9c, 0xd3)), egui::StrokeKind::Outside);
                } else if resp.hovered() {
                    ui.painter().rect_stroke(frame.expand(1.0), 2.0, Stroke::new(1.0, Color32::from_gray(0x8a)), egui::StrokeKind::Outside);
                }
                let font = egui::FontId::proportional(11.0);
                let name = elide(ui, &item.name, &font, tile_w - 4.0);
                ui.painter_at(tile).text(pos2(tile.center().x, frame.max.y + 3.0 + label_h * 0.5), egui::Align2::CENTER_CENTER, name, font, if sel { Color32::WHITE } else { theme::TEXT });
            }
            item_interaction(app, ctx, item, &resp, clicked_any);
        }
    }
}

/// Shorten a name with an ellipsis so it fits a width.
fn elide(ui: &egui::Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    let width = |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x);
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

fn list_view(app: &mut IncaApp, ui: &mut egui::Ui, ctx: &egui::Context, items: &[Item], viewport: Rect, clicked_any: &mut bool) {
    let w = ui.available_width();
    let row_h = 22.0;
    let top = ui.cursor().min;
    let first = ((viewport.min.y / row_h).floor().max(0.0) as usize).min(items.len());
    let last = ((viewport.max.y / row_h).ceil().max(0.0) as usize + 1).min(items.len());
    ui.set_min_height(items.len() as f32 * row_h);
    for (i, item) in items.iter().enumerate().take(last).skip(first) {
        let r = Rect::from_min_size(top + vec2(0.0, i as f32 * row_h), vec2(w, row_h));
        let resp = ui.interact(r, egui::Id::new(("cb_row", &item.target_key())), Sense::click_and_drag());
        let sel = app.windows_state.cb.selected.as_ref() == Some(&item.target);
        if sel {
            ui.painter().rect_filled(r, 0.0, theme::HI);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, 0.0, Color32::from_gray(0x4d));
        }
        let ir = Rect::from_min_size(r.min + vec2(4.0, 2.0), vec2(18.0, 18.0));
        match app.windows_state.cb.thumbnail(ctx, item) {
            Some(t) => {
                ui.painter().image(t.id(), ir, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => crate::icons::paint(ui, ir, icon_for(item), Color32::WHITE),
        }
        let painter = ui.painter_at(r);
        painter.text(pos2(r.min.x + 28.0, r.center().y), egui::Align2::LEFT_CENTER, &item.name, egui::FontId::proportional(12.0), if sel { Color32::WHITE } else { theme::TEXT });
        let info = format!("{}    {}", type_label(item), item.size.map(fmt_size).unwrap_or_default());
        painter.text(pos2(r.max.x - 8.0, r.center().y), egui::Align2::RIGHT_CENTER, info, egui::FontId::proportional(11.0), theme::TEXT_DIM);
        item_interaction(app, ctx, item, &resp, clicked_any);
    }
}

fn item_interaction(app: &mut IncaApp, ctx: &egui::Context, item: &Item, resp: &egui::Response, clicked_any: &mut bool) {
    app.windows_state.cb.tile_rects.push((item.target.clone(), resp.rect));
    if resp.clicked() || resp.secondary_clicked() || resp.drag_started() {
        *clicked_any = true;
        app.windows_state.cb.selected = Some(item.target.clone());
    }
    if resp.double_clicked() {
        *clicked_any = true;
        activate(app, item);
    }
    if resp.drag_started_by(egui::PointerButton::Primary) && !item.is_folder() {
        app.windows_state.cb.drag = Some(item.clone());
    }
    if resp.drag_stopped_by(egui::PointerButton::Primary) {
        if let (Some(d), Some(q)) = (app.windows_state.cb.drag.take(), ctx.pointer_hover_pos()) {
            // a release over the browser window itself (or another window) is not a drop
            if can_drop_at(app, ctx, q) {
                drop_item(app, &d, q);
            }
        }
    }
    let resp = resp.clone().on_hover_ui(|ui| {
        ui.label(item_tooltip(item));
    });
    resp.context_menu(|ui| context_menu(app, ui, item));
}

fn item_tooltip(item: &Item) -> String {
    match &item.target {
        Target::Example(i) => {
            let e = &examples()[*i];
            match e.kind {
                ExampleKind::Material => format!("{}\n{}\nDouble-click: apply to the selection. Drag onto an object to assign.", e.name, e.description),
                ExampleKind::Scene => format!("{}\n{}\nDouble-click: open (import if the scene is not empty). Drag into a viewport to import.", e.name, e.description),
            }
        }
        Target::File(p) => match file_kind(p) {
            FileKind::Scene | FileKind::Model => format!("{}\nDouble-click: open into an empty scene, otherwise import. Drag into a viewport to import.", item.name),
            FileKind::Image => format!("{}\nDouble-click: make a file texture (on the selection). Drag onto an object to texture it.", item.name),
            FileKind::Script => format!("{}\nDouble-click: source the script.", item.name),
            FileKind::Other => item.name.clone(),
        },
        Target::Folder(_) => item.name.clone(),
    }
}

fn context_menu(app: &mut IncaApp, ui: &mut egui::Ui, item: &Item) {
    let mut close = false;
    match &item.target {
        Target::Folder(l) => {
            if ui.button("Open Folder").clicked() {
                app.windows_state.cb.go(l.clone());
                close = true;
            }
        }
        Target::Example(i) => {
            let e = &examples()[*i];
            if e.kind == ExampleKind::Scene {
                if ui.button("Open").clicked() {
                    open(app, item);
                    close = true;
                }
                if ui.button("Import").clicked() {
                    import_example(app, *i);
                    close = true;
                }
            } else if ui.add_enabled(!app.s.sel.is_empty(), egui::Button::new("Apply to Selection")).clicked() {
                import_example(app, *i);
                close = true;
            }
            if ui.button("Copy MEL to Clipboard").clicked() {
                ui.ctx().copy_text(e.mel.to_string());
                close = true;
            }
        }
        Target::File(p) => {
            match file_kind(p) {
                FileKind::Scene => {
                    if ui.add_enabled(ext_of(p) != "mb", egui::Button::new("Open")).clicked() {
                        open(app, item);
                        close = true;
                    }
                    if ui.add_enabled(ext_of(p) != "mb", egui::Button::new("Import")).clicked() {
                        import_file(app, p);
                        close = true;
                    }
                }
                FileKind::Model => {
                    if ui.button("Import").clicked() {
                        import_file(app, p);
                        close = true;
                    }
                }
                FileKind::Script => {
                    if ui.button("Source MEL File").clicked() {
                        source_mel(app, p);
                        close = true;
                    }
                }
                FileKind::Image => {
                    if ui.button("Create File Texture").clicked() {
                        image_to_texture(app, p, &[]);
                        close = true;
                    }
                    if ui.add_enabled(!app.s.sel.is_empty(), egui::Button::new("Assign to Selection")).clicked() {
                        let sel = app.s.sel.clone();
                        image_to_texture(app, p, &sel);
                        close = true;
                    }
                }
                FileKind::Other => {}
            }
        }
    }
    ui.separator();
    if let Some(key) = item.fav_key() {
        let fav = app.windows_state.cb.is_favorite(&key);
        if ui.button(if fav { "Remove from Favorites" } else { "Add to Favorites" }).clicked() {
            app.windows_state.cb.toggle_favorite(key);
            close = true;
        }
    }
    let disk = match &item.target {
        Target::File(p) | Target::Folder(Loc::Dir(p)) => Some(p.clone()),
        _ => None,
    };
    if let Some(p) = disk {
        if ui.button(if cfg!(target_os = "windows") { "Show in Explorer" } else { "Show in File Browser" }).clicked() {
            show_in_explorer(&p);
            close = true;
        }
        if ui.button("Copy Path").clicked() {
            ui.ctx().copy_text(p.display().to_string());
            close = true;
        }
    }
    if close {
        ui.close();
    }
}

fn type_label(item: &Item) -> String {
    match &item.target {
        Target::Folder(_) => "Folder".into(),
        Target::Example(i) => match examples()[*i].kind {
            ExampleKind::Scene => "Example scene".into(),
            ExampleKind::Material => "Material preset".into(),
        },
        Target::File(p) => match file_kind(p) {
            FileKind::Scene => format!("{} scene", ext_of(p).to_uppercase()),
            FileKind::Model => format!("{} model", ext_of(p).to_uppercase()),
            FileKind::Image => format!("{} image", ext_of(p).to_uppercase()),
            FileKind::Script => "MEL script".into(),
            FileKind::Other => if ext_of(p).is_empty() { "File".into() } else { format!("{} file", ext_of(p).to_uppercase()) },
        },
    }
}

pub fn fmt_size(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn fmt_time(t: Option<SystemTime>) -> String {
    let Some(secs) = t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64) else { return String::new() };
    // civil date from days since the epoch (UTC)
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (z, era);
    z = days + 719_468;
    era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

fn info_panel(app: &mut IncaApp, ui: &mut egui::Ui, show_meta: bool, show_preview: bool) {
    let ctx = ui.ctx().clone();
    let loc = app.windows_state.cb.loc();
    let _ = &loc;
    let item = app.windows_state.cb.selected.clone().and_then(|t| {
        let cached = app.windows_state.cb.view_cache.as_ref().and_then(|(_, v)| v.iter().find(|i| i.target == t).cloned());
        cached.or_else(|| favorite_targets(app, &t).into_iter().find(|i| i.target == t))
    });
    if show_meta {
        theme::section_header(ui, "Metadata");
        egui::Grid::new("cb_meta").num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(RichText::new(k).color(theme::TEXT_DIM));
                ui.add(egui::Label::new(v).wrap());
                ui.end_row();
            };
            match &item {
                Some(it) => {
                    row(ui, "File name:", match &it.target {
                        Target::Example(i) => examples()[*i].path.rsplit('/').next().unwrap_or("").to_string(),
                        _ => it.name.clone(),
                    });
                    row(ui, "File type:", type_label(it));
                    if let Target::Example(i) = &it.target {
                        let e = &examples()[*i];
                        row(ui, "Name:", e.name.clone());
                        row(ui, "Category:", e.folder().replace('/', " > "));
                        row(ui, "Description:", e.description.clone());
                    }
                    row(ui, "File size:", it.size.map(fmt_size).unwrap_or_default());
                    row(ui, "Created date:", fmt_time(it.created));
                    row(ui, "Last modified date:", fmt_time(it.modified));
                }
                None => {
                    for k in ["File name:", "File type:", "File size:", "Created date:", "Last modified date:"] {
                        row(ui, k, String::new());
                    }
                }
            }
        });
        ui.add_space(8.0);
    }
    if show_preview {
        theme::section_header(ui, "Preview");
        let Some(it) = item else {
            ui.label(RichText::new("No preview available").color(theme::TEXT_DIM));
            return;
        };
        let w = ui.available_width().min(220.0);
        if let Some(t) = app.windows_state.cb.thumbnail(&ctx, &it) {
            let [tw, th] = t.size().map(|v| v as f32);
            let s = w / tw.max(th);
            let (r, _) = ui.allocate_exact_size(vec2(w, th * s), Sense::hover());
            ui.painter().rect_filled(r, 0.0, Color32::BLACK);
            ui.painter().image(t.id(), Rect::from_center_size(r.center(), vec2(tw * s, th * s)), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        let text = match &it.target {
            Target::Example(i) => Some(examples()[*i].mel.to_string()),
            Target::File(p) if TEXT_EXTS.contains(&ext_of(p).as_str()) && it.size.unwrap_or(0) < 4 << 20 => {
                let cb = &mut app.windows_state.cb;
                if cb.preview_text.as_ref().map(|(q, m, _)| (q, *m)) != Some((p, it.modified)) {
                    let t = std::fs::read_to_string(p).map(|s| s.lines().take(200).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                    cb.preview_text = Some((p.clone(), it.modified, t));
                }
                cb.preview_text.as_ref().map(|(_, _, t)| t.clone())
            }
            _ => None,
        };
        if let Some(t) = text {
            egui::ScrollArea::both().id_salt("cb_preview_text").auto_shrink([false, false]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(t).monospace().size(10.0)).extend());
            });
        } else if !matches!(it.target, Target::Example(_)) && app.windows_state.cb.thumb_pending(&it) {
            ui.label(RichText::new("Rendering preview...").color(theme::TEXT_DIM));
        }
    }
}

fn favorite_targets(app: &IncaApp, t: &Target) -> Vec<Item> {
    let key = match t {
        Target::File(p) | Target::Folder(Loc::Dir(p)) => p.to_string_lossy().to_string(),
        Target::Example(i) => format!("example:{}", examples()[*i].path),
        _ => return vec![],
    };
    if app.windows_state.cb.is_favorite(&key) {
        favorite_item(&key).into_iter().collect()
    } else {
        vec![]
    }
}

impl CbState {
    fn thumb_pending(&self, item: &Item) -> bool {
        thumb_key(item).is_some_and(|k| self.pending.contains(&k))
    }
}

fn confirm_dialog(app: &mut IncaApp, ctx: &egui::Context) {
    let Some(item) = app.windows_state.cb.confirm_open.clone() else { return };
    let mut answer = None;
    egui::Modal::new(egui::Id::new("cb_confirm_open")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.label(RichText::new("Save changes to the current scene?").strong());
        ui.label(format!("You are opening \"{}\".", item.name));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                answer = Some(0);
            }
            if ui.button("Don't Save").clicked() {
                answer = Some(1);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(2);
            }
        });
    });
    match answer {
        Some(0) => {
            app.windows_state.cb.confirm_open = None;
            app.action("saveScene");
            if !app.s.modified {
                open_now(app, &item);
            }
        }
        Some(1) => {
            app.windows_state.cb.confirm_open = None;
            open_now(app, &item);
        }
        Some(_) => app.windows_state.cb.confirm_open = None,
        None => {}
    }
}

fn drag_feedback(app: &mut IncaApp, ctx: &egui::Context) {
    let Some(item) = &app.windows_state.cb.drag else { return };
    if !ctx.input(|i| i.pointer.primary_down()) {
        app.windows_state.cb.drag = None;
        return;
    }
    let Some(q) = ctx.pointer_hover_pos() else { return };
    let over_vp = can_drop_at(app, ctx, q);
    let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("cb-drag")));
    let text = format!("{}{}", item.name, if over_vp { "" } else { "  (drop on a viewport)" });
    let galley = p.layout_no_wrap(text, egui::FontId::proportional(11.0), Color32::WHITE);
    let rect = Rect::from_min_size(q + vec2(14.0, 14.0), galley.size() + vec2(12.0, 6.0));
    p.rect_filled(rect, 3.0, if over_vp { Color32::from_rgb(0x3d, 0x6b, 0x8c) } else { Color32::from_gray(0x44) });
    p.galley(rect.min + vec2(6.0, 3.0), galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_wildcards_and_extensions() {
        assert!(matches_filter("CornellBox.mel", "mel", "cornell"));
        assert!(matches_filter("CornellBox.mel", "mel", ".mel"));
        assert!(!matches_filter("CornellBox.mel", "mel", ".ma"));
        assert!(matches_filter("CharacterFemale.ma", "ma", "char*.ma"));
        assert!(!matches_filter("BasicHead.ma", "ma", "char*"));
        assert!(matches_filter("BasicHead.ma", "ma", "*head*"));
        assert!(matches_filter("anything", "", ""));
    }

    #[test]
    fn settings_split_and_headers() {
        let src = "// @name Box\n// @description A box.\n// @kind scene\npolyCube;\n// @settings\nsetAttr defaultRenderGlobals.samples 8;\n";
        let (a, b) = split_settings(src);
        assert!(a.contains("polyCube") && !a.contains("samples"));
        assert!(b.contains("samples") && !b.contains("polyCube"));
        assert_eq!(header(src, "name").as_deref(), Some("Box"));
        assert_eq!(header(src, "kind").as_deref(), Some("scene"));
        assert_eq!(split_settings("polyCube;\n"), ("polyCube;\n", ""));
    }

    #[test]
    fn history_and_parent_navigation() {
        let mut cb = CbState::default();
        cb.go(Loc::Examples("Lighting And Rendering".into()));
        cb.go(Loc::Favorites);
        assert_eq!(cb.loc(), Loc::Favorites);
        cb.go_back();
        assert_eq!(cb.loc(), Loc::Examples("Lighting And Rendering".into()));
        cb.go_forward();
        assert_eq!(cb.loc(), Loc::Favorites);
        assert_eq!(Loc::Examples("A/B".into()).parent(), Some(Loc::Examples("A".into())));
        assert_eq!(Loc::Examples("A".into()).parent(), Some(Loc::Examples(String::new())));
        assert_eq!(Loc::Examples(String::new()).parent(), None);
        cb.toggle_favorite("x".into());
        assert!(cb.is_favorite("x"));
        cb.toggle_favorite("x".into());
        assert!(!cb.is_favorite("x"));
    }

    #[test]
    fn folders_sort_first_then_by_name() {
        let mk = |n: &str, dir: bool| Item { name: n.into(), target: if dir { Target::Folder(Loc::Dir(n.into())) } else { Target::File(PathBuf::from(n)) }, size: None, modified: None, created: None };
        let mut v = vec![mk("b.ma", false), mk("Zed", true), mk("a.ma", false), mk("alpha", true)];
        sort_items(&mut v, SortBy::Name);
        let names: Vec<&str> = v.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zed", "a.ma", "b.ma"]);
    }

    #[test]
    fn directory_listing_and_favorites_from_disk() {
        let dir = std::env::temp_dir().join(format!("inca_cb_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::write(dir.join("box.obj"), "v 0 0 0\n").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let mut cb = CbState::default();
        let items = cb.items_at(&Loc::Dir(dir.clone()));
        let names: HashSet<String> = items.iter().map(|i| i.name.clone()).collect();
        assert_eq!(names, ["scenes".to_string(), "box.obj".to_string()].into_iter().collect());
        let obj = items.iter().find(|i| i.name == "box.obj").unwrap();
        assert_eq!(file_kind(Path::new("box.obj")), FileKind::Model);
        cb.toggle_favorite(obj.fav_key().unwrap());
        let favs = cb.items_at(&Loc::Favorites);
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].target, Target::File(dir.join("box.obj")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_app() -> IncaApp {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx);
        IncaApp::new(&cc)
    }

    /// Every example opens as a new scene and imports twice (the scene part must survive name
    /// clashes) without a MEL error; material presets apply to a selected sphere.
    #[test]
    fn every_example_opens_and_imports_twice() {
        assert!(!examples().is_empty(), "no examples were embedded");
        for (i, e) in examples().iter().enumerate() {
            let mut app = test_app();
            match e.kind {
                ExampleKind::Scene => {
                    open_example(&mut app, i);
                    assert!(!app.last_result.starts_with("// Error"), "{}: {}", e.path, app.last_result);
                    assert!(!scene_is_empty(&app), "{}: opened an empty scene", e.path);
                    assert!(!app.s.modified && app.s.file.is_none() && app.s.undo_stack.is_empty(), "{}: should open as a clean untitled scene", e.path);
                    let n = app.s.nodes.len();
                    for _ in 0..2 {
                        import_example(&mut app, i);
                        assert!(!app.last_result.starts_with("// Error"), "{} import: {}", e.path, app.last_result);
                    }
                    assert!(app.s.nodes.len() > n, "{}: the imports added nothing", e.path);
                }
                ExampleKind::Material => {
                    app.exec_mel("polySphere -n ball;", false).unwrap();
                    let ball = app.s.find("ball").unwrap();
                    app.select_names(std::slice::from_ref(&ball), inca_core::scene::SelMode::Replace);
                    let before = app.s.node(&ball).and_then(|n| n.mesh()).map(|m| m.material.clone());
                    import_example(&mut app, i);
                    assert!(!app.last_result.starts_with("// Error"), "{}: {}", e.path, app.last_result);
                    let after = app.s.node(&ball).and_then(|n| n.mesh()).map(|m| m.material.clone());
                    assert_ne!(before, after, "{}: the preset was not assigned to the selection", e.path);
                    import_example(&mut app, i);
                    assert!(!app.last_result.starts_with("// Error"), "{} twice: {}", e.path, app.last_result);
                }
            }
        }
    }

    /// Developer tool, not a check: path trace every example's thumbnail into
    /// assets/examples/<path>.png. Run with
    /// `cargo test -p inca --release gen_example_thumbnails -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn gen_example_thumbnails() {
        let only = std::env::var("INCA_THUMB_ONLY").ok();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join("examples");
        for (i, e) in examples().iter().enumerate() {
            if only.as_ref().is_some_and(|o| !e.path.contains(o.as_str())) {
                continue;
            }
            let t0 = Instant::now();
            let mut app = test_app();
            let (cam, w, h, samples) = match e.kind {
                ExampleKind::Scene => {
                    open_example(&mut app, i);
                    assert!(!app.last_result.starts_with("// Error"), "{}: {}", e.path, app.last_result);
                    let st = inca_core::render::RenderSettings::from_scene(&app.s);
                    let cam = app.s.render.get("camera").and_then(|v| v.as_str()).and_then(|c| app.s.find(c)).map(|c| app.s.name_of(&c)).unwrap_or_else(|| "persp".into());
                    let aspect = st.width.max(1) as f32 / st.height.max(1) as f32;
                    let (w, h) = if aspect >= 1.0 { (256, (256.0 / aspect).round() as u32) } else { ((256.0 * aspect).round() as u32, 256) };
                    (cam, w, h, st.samples.clamp(32, 160))
                }
                ExampleKind::Material => {
                    let stage = r#"
string $fl[] = `polyPlane -w 12 -h 12 -sx 1 -sy 1 -n thumbFloor`;
string $fm = `shadingNode -asShader lambert -n thumbFloorMat`;
setAttr ($fm + ".color") -type double3 0.22 0.22 0.24;
select -r $fl[0]; hyperShade -assign $fm;
string $b[] = `polySphere -r 1 -sx 64 -sy 48 -n thumbBall`;
move 0 1 0 $b[0];
areaLight -n thumbKey -i 28;
xform -ws -t -1.5 5 3.5 -ro -55 -20 0 -s 1.6 1.6 1 thumbKey;
areaLight -n thumbRim -i 18;
xform -ws -t 2.5 3 -3 -ro -30 140 0 -s 1.2 1.2 1 thumbRim;
camera -n thumbCam -fl 60;
xform -ws -t 0 1.55 5.4 -ro -6 0 0 thumbCam;
setAttr defaultRenderGlobals.background 0.42 0.44 0.48;
setAttr defaultRenderGlobals.bounces 8;
select -r $b[0];
"#;
                    app.exec_mel(stage, false).unwrap();
                    import_example(&mut app, i);
                    assert!(!app.last_result.starts_with("// Error"), "{}: {}", e.path, app.last_result);
                    ("thumbCam".to_string(), 256, 256, 96)
                }
            };
            let mut st = inca_core::render::RenderSettings::from_scene(&app.s);
            st.width = w;
            st.height = h;
            st.samples = samples;
            st.transparent = false;
            let rs = inca_core::render::RenderScene::build(&app.s, &cam, st);
            let mut acc = vec![0f32; (w * h * 3) as usize];
            for pass in 0..samples {
                rs.render_pass(&mut acc, pass);
            }
            let px = rs.resolve(&acc, samples);
            let out = root.join(e.path).with_extension("png");
            image::save_buffer(&out, &px, w, h, image::ColorType::Rgba8).unwrap();
            println!("{} -> {} ({w}x{h}, {samples} spp, {:.1}s)", e.path, out.display(), t0.elapsed().as_secs_f32());
        }
    }

    /// Real egui frames: the window opens from its action, a double-click on a folder tile goes
    /// in, a double-click on an example opens it, and dragging an example onto a viewport
    /// imports it there. The window keeps its size from frame to frame.
    #[test]
    fn window_double_click_and_drag_to_viewport() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = IncaApp::new(&cc);
        let mut frame = eframe::Frame::_new_kittest();
        let mut t = 0.0;
        let mut run = |app: &mut IncaApp, events: Vec<egui::Event>| {
            t += 0.05;
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0)));
            raw.time = Some(t);
            raw.events = events;
            app.windows_state.window_seen_now.clear();
            let mut out = ctx.run_ui(raw, |ui| crate::windows::show(app, ui.ctx(), &mut frame));
            out.textures_delta.clear();
            std::mem::swap(&mut app.windows_state.window_seen, &mut app.windows_state.window_seen_now);
        };
        let press = |p: egui::Pos2, down: bool| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: down, modifiers: Default::default() };
        app.action("contentBrowser");
        assert!(app.win.content_browser);
        app.windows_state.cb.prefs = CbPrefs::default();
        app.windows_state.cb.prefs.loc = Some(Loc::Examples(String::new()));
        for _ in 0..4 {
            run(&mut app, vec![]);
        }
        let id = egui::Id::new("ContentBrowserWindow");
        let size0 = ctx.memory(|m| m.area_rect(id)).expect("window shown").size();
        let tile = |app: &IncaApp, want: &Target| app.windows_state.cb.tile_rects.iter().find(|(t, _)| t == want).map(|(_, r)| r.center());
        let folder = Target::Folder(Loc::Examples("Lighting And Rendering".into()));
        let p = tile(&app, &folder).expect("category tile drawn");
        // double-click the folder tile
        run(&mut app, vec![egui::Event::PointerMoved(p)]);
        for down in [true, false, true, false] {
            run(&mut app, vec![press(p, down)]);
        }
        run(&mut app, vec![]);
        assert_eq!(app.windows_state.cb.loc(), Loc::Examples("Lighting And Rendering".into()));
        let cornell = examples().iter().position(|e| e.path.ends_with("CornellBox.mel")).expect("Cornell box example");
        // a pause, so the next double-click is not counted as a triple-click
        for _ in 0..12 {
            run(&mut app, vec![]);
        }
        let p = tile(&app, &Target::Example(cornell)).expect("Cornell box tile drawn");
        // double-click on an empty scene opens the example
        run(&mut app, vec![egui::Event::PointerMoved(p)]);
        for down in [true, false, true, false] {
            run(&mut app, vec![press(p, down)]);
        }
        run(&mut app, vec![]);
        assert!(!scene_is_empty(&app), "the example opened");
        assert!(!app.s.modified, "undo: {:?} log: {:?}", app.s.undo_stack.iter().map(|s| s.label.clone()).collect::<Vec<_>>(), app.script_log.iter().rev().take(5).collect::<Vec<_>>());
        let n = app.s.nodes.len();
        // drag it onto a viewport to the left of the window: an import
        let vp = app.active_vp;
        app.vps[vp].rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 1000.0));
        let drop_at = pos2(100.0, 500.0);
        for _ in 0..12 {
            run(&mut app, vec![]);
        }
        run(&mut app, vec![egui::Event::PointerMoved(p), press(p, true)]);
        for k in 1..=6 {
            run(&mut app, vec![egui::Event::PointerMoved(p + (drop_at - p) * (k as f32 / 6.0))]);
        }
        run(&mut app, vec![press(drop_at, false)]);
        run(&mut app, vec![]);
        assert!(app.s.nodes.len() > n, "dropping the example on a viewport imported it");
        assert!(app.windows_state.cb.drag.is_none());
        let size1 = ctx.memory(|m| m.area_rect(id)).expect("window still shown").size();
        assert!((size1 - size0).length() < 1.0, "the window changed size: {size0:?} -> {size1:?}");
    }

    /// Back and Forward return to the tab a place was shown in, and folders opened from the
    /// Projects or Favorites tree keep that tab.
    #[test]
    fn history_remembers_the_tab() {
        let mut cb = CbState::default();
        let dir = std::env::temp_dir();
        cb.prefs.tab = CbTab::Projects;
        cb.go(Loc::Dir(dir.clone()));
        assert_eq!(cb.prefs.tab, CbTab::Projects, "a project folder stays in the Projects tab");
        // switching to the Examples tab, as the tab row does
        cb.go(Loc::Examples(String::new()));
        cb.prefs.tab = CbTab::Examples;
        cb.go_back();
        assert_eq!((cb.loc(), cb.prefs.tab), (Loc::Dir(dir.clone()), CbTab::Projects));
        cb.go_forward();
        assert_eq!(cb.prefs.tab, CbTab::Examples);
        cb.prefs.tab = CbTab::Favorites;
        cb.go(Loc::Dir(dir));
        assert_eq!(cb.prefs.tab, CbTab::Favorites, "a favorite folder keeps the Favorites tab");
    }

    /// An empty scene with unsaved changes (everything deleted, undo still possible) asks
    /// before an example replaces it.
    #[test]
    fn double_click_asks_before_replacing_unsaved_work() {
        let mut app = test_app();
        app.run_mel("make", "polyCube -n keep;").unwrap();
        app.run_mel("delete", "delete keep;").unwrap();
        assert!(scene_is_empty(&app) && app.s.modified);
        let i = examples().iter().position(|e| e.kind == ExampleKind::Scene).expect("a scene example");
        let item = example_items(examples()[i].folder()).into_iter().find(|it| it.target == Target::Example(i)).unwrap();
        let undo = app.s.undo_stack.len();
        activate(&mut app, &item);
        assert!(app.windows_state.cb.confirm_open.is_some(), "the save prompt is up");
        assert!(scene_is_empty(&app) && app.s.undo_stack.len() == undo, "nothing replaced yet");
    }

    #[test]
    fn image_names_and_paths_make_valid_mel() {
        assert_eq!(mel_escape(r#"C:/a "b"/c"#), r#"C:/a \"b\"/c"#);
        let mut app = test_app();
        let dir = std::env::temp_dir().join(format!("inca_cb_img_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("4k wood.png");
        image::save_buffer(&img, &[200, 120, 40, 255].repeat(4), 2, 2, image::ColorType::Rgba8).unwrap();
        app.run_mel("make", "polyCube -n box;").unwrap();
        let bx = app.s.find("box").unwrap();
        image_to_texture(&mut app, &img, std::slice::from_ref(&bx));
        assert!(!app.last_result.starts_with("// Error"), "{}", app.last_result);
        let mat = app.s.node(&bx).and_then(|n| n.mesh()).map(|m| m.material.clone()).unwrap();
        let name = app.s.material(&mat).map(|m| m.name).unwrap();
        assert!(name.starts_with("img_4k"), "a texture name starting with a digit gets a prefix: {name}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model file gets a path traced thumbnail that frames it.
    #[test]
    fn model_files_get_a_rendered_thumbnail() {
        let dir = std::env::temp_dir().join(format!("inca_cb_obj_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = dir.join("tri.obj");
        std::fs::write(&obj, "v -1 0 0\nv 1 0 0\nv 0 2 0\nf 1 2 3\n").unwrap();
        let (w, h, px) = thumbnail_for(&obj).expect("a thumbnail");
        assert_eq!((w, h), (THUMB_PX, THUMB_PX));
        // the background is flat grey: the triangle shows up as different pixels in the middle
        let at = |x: u32, y: u32| px[((y * w + x) * 4) as usize];
        let corner = at(2, 2);
        assert!((w / 3..2 * w / 3).any(|x| (h / 3..2 * h / 3).any(|y| at(x, y).abs_diff(corner) > 10)), "the model is in the frame");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drive_roots_come_from_the_system() {
        let d = drives();
        assert!(!d.is_empty());
        assert!(d.iter().all(|p| p.is_absolute()));
    }

    /// Every material preset has a live swatch, and a metal is not drawn black.
    #[test]
    fn material_presets_get_live_swatches() {
        for e in examples().iter().filter(|e| e.kind == ExampleKind::Material) {
            let (w, h, px) = material_swatch(e, 64).unwrap_or_else(|| panic!("{}: no swatch", e.path));
            assert_eq!((w, h, px.len()), (64, 64, 64 * 64 * 4));
            let centre = &px[((32 * 64 + 32) * 4) as usize..][..3];
            assert!(centre.iter().map(|&c| c as u32).sum::<u32>() > 60, "{}: the ball is black {centre:?}", e.path);
        }
    }

    /// Developer tool: write every material preset's swatch to $INCA_SWATCH_DIR (for a look).
    #[test]
    #[ignore]
    fn dump_material_swatches() {
        let dir = PathBuf::from(std::env::var("INCA_SWATCH_DIR").expect("set INCA_SWATCH_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        for e in examples().iter().filter(|e| e.kind == ExampleKind::Material) {
            let (w, h, px) = material_swatch(e, 160).unwrap();
            let name = e.path.rsplit('/').next().unwrap().replace(".mel", ".png");
            image::save_buffer(dir.join(name), &px, w, h, image::ColorType::Rgba8).unwrap();
        }
    }

    #[test]
    fn every_example_has_a_valid_header() {
        for e in examples() {
            assert!(header(e.mel, "name").is_some(), "{} has no // @name", e.path);
            assert!(!e.description.is_empty(), "{} has no // @description", e.path);
            assert!(!e.scene_part().contains("defaultRenderGlobals") && !e.scene_part().contains("file -new"), "{}: settings belong after // @settings", e.path);
            assert!(e.thumb.is_none_or(|t| decode_png(t).is_some()), "{}: the thumbnail does not decode", e.path);
        }
    }
}
