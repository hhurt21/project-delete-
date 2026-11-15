#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod fsops;
mod kinds;
mod procs;
mod projects;
mod scan;

use kinds::Kind;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;

// ---------- 設定（探す場所） ----------

#[derive(Serialize, Deserialize, Clone, Default)]
struct Settings {
    roots: Vec<String>,
    /// プロジェクトの置き場（容量の一覧を出す場所）
    #[serde(default = "default_projects_dir")]
    projects_dir: String,
}

/// D:\claude があればそこ、無ければユーザーのフォルダ
fn default_projects_dir() -> String {
    let claude = Path::new("D:\\claude");
    if claude.is_dir() {
        return claude.display().to_string();
    }
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).display().to_string()).unwrap_or_default()
}

fn settings_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("rust-cleaner")
        .join("settings.json")
}

#[cfg(windows)]
fn fixed_drives() -> Vec<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_FIXED: u32 = 3;
    (b'A'..=b'Z')
        .map(|c| format!("{}:\\", c as char))
        .filter(|d| {
            let w: Vec<u16> = d.encode_utf16().chain([0]).collect();
            unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) == DRIVE_FIXED }
        })
        .collect()
}

#[cfg(not(windows))]
fn fixed_drives() -> Vec<String> {
    Vec::new()
}

/// はじめての既定: ユーザーのフォルダと、C 以外の内蔵ドライブ全体
fn default_settings() -> Settings {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        roots.push(PathBuf::from(home).display().to_string());
    }
    for d in fixed_drives() {
        if !d.eq_ignore_ascii_case("C:\\") {
            roots.push(d);
        }
    }
    Settings { roots, projects_dir: default_projects_dir() }
}

fn load_settings() -> Settings {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(default_settings)
}

fn save_settings(s: &Settings) -> Result<(), String> {
    let p = settings_path();
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(&p, serde_json::to_string_pretty(s).unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings() -> Settings {
    load_settings()
}

#[tauri::command]
async fn add_root(app: AppHandle) -> Result<Settings, String> {
    let picked = app.dialog().file().set_title("探すフォルダを選んでください").blocking_pick_folder();
    let mut s = load_settings();
    if let Some(p) = picked.and_then(|f| f.into_path().ok()) {
        let p = p.display().to_string();
        if !s.roots.iter().any(|r| r.eq_ignore_ascii_case(&p)) {
            s.roots.push(p);
            save_settings(&s)?;
        }
    }
    Ok(s)
}

#[tauri::command]
fn remove_root(path: String) -> Result<Settings, String> {
    let mut s = load_settings();
    s.roots.retain(|r| !r.eq_ignore_ascii_case(&path));
    save_settings(&s)?;
    Ok(s)
}

#[tauri::command]
fn reset_roots() -> Result<Settings, String> {
    let mut s = load_settings();
    s.roots = default_settings().roots;
    save_settings(&s)?;
    Ok(s)
}

#[tauri::command]
async fn pick_projects_dir(app: AppHandle) -> Result<Settings, String> {
    let picked = app.dialog().file().set_title("プロジェクトの置き場を選んでください").blocking_pick_folder();
    let mut s = load_settings();
    if let Some(p) = picked.and_then(|f| f.into_path().ok()) {
        s.projects_dir = p.display().to_string();
        save_settings(&s)?;
    }
    Ok(s)
}

#[tauri::command]
async fn list_projects(shared: State<'_, Shared>) -> Result<projects::ProjectsReply, String> {
    let dir = PathBuf::from(load_settings().projects_dir);
    let reply =
        tauri::async_runtime::spawn_blocking(move || projects::list(&dir)).await.map_err(|e| e.to_string())??;
    // ここで見つけて「消せる」と出したものだけ、カードのボタンから消せるようにする
    let mut allowed = shared.allowed.lock().unwrap();
    let mut found = shared.found.lock().unwrap();
    for item in reply.projects.iter().flat_map(|p| &p.items).filter(|i| i.deletable) {
        for p in &item.paths {
            allowed.insert(key(Path::new(p)), item.kind);
            if item.kind == Kind::RustTarget {
                found.insert(key(Path::new(p)));
            }
        }
    }
    Ok(reply)
}

#[derive(Serialize)]
struct ItemsReply {
    result: CleanResult,
    /// 消さなかった場所と理由
    skipped: Vec<String>,
}

/// 消してはいけない理由（無ければ None）。種類ごとに確かめる
fn item_busy_reason(path: &Path, kind: Kind) -> Option<String> {
    match kind {
        Kind::RustTarget => busy_reason(path),
        Kind::RustIncremental => kinds::cargo_target_above(path).and_then(busy_reason),
        _ => running_under(path),
    }
}

/// 一覧で「消せる」と出したものを消す
#[tauri::command]
async fn clean_items(app: AppHandle, shared: State<'_, Shared>, paths: Vec<String>) -> Result<ItemsReply, String> {
    let allowed: Vec<(PathBuf, Option<Kind>)> = {
        let a = shared.allowed.lock().unwrap();
        paths.iter().map(|p| (PathBuf::from(p), a.get(&key(Path::new(p))).copied())).collect()
    };
    tauri::async_runtime::spawn_blocking(move || {
        let mut ok = Vec::new();
        let mut skipped = Vec::new();
        for (p, kind) in allowed {
            let Some(kind) = kind else {
                skipped.push(format!("{}：一覧で「消せる」と出したものではないので消しません", p.display()));
                continue;
            };
            if !p.exists() {
                continue;
            }
            // 測った後に中身が変わっていないか、もう一度見分ける
            if kinds::classify(&p) != Some(kind) || !kinds::deletable(kind, &p) {
                skipped.push(format!("{}：中身が変わったので消しません。「測り直す」を押してください", p.display()));
                continue;
            }
            if let Some(why) = item_busy_reason(&p, kind) {
                skipped.push(format!("{}：{why}", p.display()));
                continue;
            }
            ok.push(p);
        }
        let drive = ok.first().cloned().unwrap_or_default();
        let result = run_clean(&app, ok, &drive);
        Ok(ItemsReply { result, skipped })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- 探す ----------

#[derive(Default)]
struct App {
    /// 探して見つけた target（ここに無い場所は消さない）
    found: Mutex<HashSet<String>>,
    /// プロジェクトの一覧で「消せる」と出した場所と種類
    allowed: Mutex<HashMap<String, Kind>>,
    scanning: AtomicBool,
    cancel: AtomicBool,
}

type Shared = Arc<App>;

fn key(p: &Path) -> String {
    p.display().to_string().to_lowercase()
}

/// 入れ子になった場所は外の方だけ残す
fn outer_roots(roots: &[String]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = roots.iter().map(PathBuf::from).filter(|p| p.is_dir()).collect();
    // 短い（外側の）方から入れる
    paths.sort_by_key(|p| key(p).len());
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if !out.iter().any(|o| key(o).trim_end_matches('\\') == key(&p).trim_end_matches('\\') || under(&p, o)) {
            out.push(p);
        }
    }
    out
}

#[derive(Serialize, Clone)]
struct ScanProgress {
    dirs: u64,
    found: usize,
}

#[derive(Serialize, Clone)]
struct ScanDone {
    dirs: u64,
    found: usize,
    cancelled: bool,
    seconds: f64,
}

#[tauri::command]
fn start_scan(app: AppHandle, shared: State<Shared>) -> Result<(), String> {
    if shared.scanning.swap(true, Ordering::SeqCst) {
        return Err("いま探しています。終わるまで待つか「探すのをやめる」を押してください".into());
    }
    shared.cancel.store(false, Ordering::SeqCst);
    let shared = shared.inner().clone();
    let roots = outer_roots(&load_settings().roots);
    std::thread::spawn(move || {
        let started = Instant::now();
        let seen = AtomicU64::new(0);
        let count = AtomicU64::new(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|sc| {
            sc.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let _ = app.emit(
                        "scan-progress",
                        ScanProgress { dirs: seen.load(Ordering::Relaxed), found: count.load(Ordering::Relaxed) as usize },
                    );
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            let w = scan::Walk { cancel: &shared.cancel, seen_dirs: &seen };
            // 探す場所が重なっても二重に出さない（found は一覧の欄からも足されるので別に持つ）
            let seen_targets = Mutex::new(HashSet::new());
            let found = |target: PathBuf| {
                if !seen_targets.lock().unwrap().insert(key(&target)) {
                    return;
                }
                shared.found.lock().unwrap().insert(key(&target));
                count.fetch_add(1, Ordering::Relaxed);
                let info = scan::inspect(&target);
                let _ = app.emit("scan-found", info);
            };
            for r in &roots {
                if scan::is_cargo_target(r) {
                    found(r.clone());
                } else {
                    scan::walk(r, &w, &found);
                }
            }
            done.store(true, Ordering::Relaxed);
        });
        let _ = app.emit(
            "scan-done",
            ScanDone {
                dirs: seen.load(Ordering::Relaxed),
                found: count.load(Ordering::Relaxed) as usize,
                cancelled: shared.cancel.load(Ordering::Relaxed),
                seconds: started.elapsed().as_secs_f64(),
            },
        );
        shared.scanning.store(false, Ordering::SeqCst);
    });
    Ok(())
}

#[tauri::command]
fn cancel_scan(shared: State<Shared>) {
    shared.cancel.store(true, Ordering::SeqCst);
}

// ---------- 消す ----------

#[derive(Serialize, Clone)]
struct CleanResult {
    /// 消したファイルの大きさの合計
    freed: u64,
    /// ドライブの空きが実際に増えた量（測れたときだけ）
    disk_freed: Option<i64>,
    /// 消せなかったファイルの数（使用中など）
    failed: u64,
    first_error: Option<String>,
}

fn under(path: &Path, dir: &Path) -> bool {
    let p = key(path);
    let d = key(dir);
    let d = d.trim_end_matches('\\');
    p.starts_with(&format!("{d}\\"))
}

/// 消してはいけない理由（無ければ None）
fn busy_reason(target: &Path) -> Option<String> {
    if let Some(why) = running_under(target) {
        return Some(why);
    }
    // ビルド中は cargo が .cargo-lock を握っている
    for dir in lock_dirs(target) {
        let lock = dir.join(".cargo-lock");
        if let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(&lock) {
            if let Err(fs::TryLockError::WouldBlock) = f.try_lock() {
                return Some("いまビルド中です（cargo が使っています）。ビルドが終わってからもう一度押してください".into());
            }
        }
    }
    None
}

/// dir の中の exe が動いていれば、その理由
fn running_under(dir: &Path) -> Option<String> {
    let mut running: Vec<String> = procs::list()
        .into_iter()
        .filter(|p| p.path.as_deref().is_some_and(|x| under(x, dir)))
        .map(|p| p.name)
        .collect();
    running.sort();
    running.dedup();
    (!running.is_empty()).then(|| {
        format!("この中のプログラムが動いています（{}）。閉じてからもう一度押してください", running.join("、"))
    })
}

/// target/debug、target/release、target/<機種>/debug など
fn lock_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = vec![target.to_path_buf()];
    if let Ok(rd) = fs::read_dir(target) {
        for e in rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            out.push(e.path());
            if let Ok(rd2) = fs::read_dir(e.path()) {
                out.extend(rd2.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
            }
        }
    }
    out
}

fn run_clean(app: &AppHandle, dirs: Vec<PathBuf>, drive_of: &Path) -> CleanResult {
    let before = fsops::free_space(drive_of);
    let prog = fsops::Progress::default();
    let done = AtomicBool::new(false);
    std::thread::scope(|sc| {
        sc.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                let _ = app.emit("clean-progress", prog.freed.load(Ordering::Relaxed));
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        for d in &dirs {
            fsops::remove_tree(d, &prog);
        }
        done.store(true, Ordering::Relaxed);
    });
    let after = fsops::free_space(drive_of);
    let first_error = prog.first_error.lock().unwrap().clone();
    CleanResult {
        freed: prog.freed.load(Ordering::Relaxed),
        disk_freed: before.zip(after).map(|(b, a)| a as i64 - b as i64),
        failed: prog.failed.load(Ordering::Relaxed),
        first_error,
    }
}

#[derive(Serialize)]
struct CleanReply {
    result: CleanResult,
    /// 消した後の様子（target ごと消したときは None）
    info: Option<scan::TargetInfo>,
}

/// mode: "incremental"（途中の記録だけ）/ "part"（debug・release など1つ）/ "all"（target ごと）
#[tauri::command]
async fn clean_target(
    app: AppHandle,
    shared: State<'_, Shared>,
    path: String,
    mode: String,
    part: Option<String>,
) -> Result<CleanReply, String> {
    let target = PathBuf::from(&path);
    if !shared.found.lock().unwrap().contains(&key(&target)) || !scan::is_cargo_target(&target) {
        return Err("この場所は「探す」で見つけた Rust の作業フォルダではないので消しません".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(why) = busy_reason(&target) {
            return Err(why);
        }
        let dirs = match mode.as_str() {
            "incremental" => scan::incremental_dirs(&target),
            "part" => {
                let name = part.ok_or("どのフォルダか分かりません")?;
                let p = target.join(&name);
                // 名前に \ や .. を入れて target の外を指すのを防ぐ
                if p.parent() != Some(target.as_path()) || name.contains(['\\', '/']) || name.starts_with('.') || !p.is_dir() {
                    return Err(format!("「{name}」は消せる場所ではありません"));
                }
                vec![p]
            }
            "all" => vec![target.clone()],
            _ => return Err("消し方が分かりません".into()),
        };
        let result = run_clean(&app, dirs, &target);
        let info = if target.is_dir() { Some(scan::inspect(&target)) } else { None };
        Ok(CleanReply { result, info })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err("フォルダが見つかりません".into());
    }
    std::process::Command::new("explorer.exe").arg(&p).spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ---------- Cargo のダウンロード置き場 ----------

fn cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join(".cargo")
    })
}

struct CacheDef {
    key: &'static str,
    rel: &'static str,
    title: &'static str,
    note: &'static str,
    needs_net: bool,
}

const CACHES: &[CacheDef] = &[
    CacheDef {
        key: "registry-src",
        rel: "registry\\src",
        title: "展開した部品のソース",
        note: "ダウンロード済みの部品を広げたもの。消しても、次のビルドで手元の圧縮ファイルから自動で戻ります（ネットは要りません）",
        needs_net: false,
    },
    CacheDef {
        key: "git-checkouts",
        rel: "git\\checkouts",
        title: "Git から取った部品の展開",
        note: "Git から取った部品を広げたもの。消しても、次のビルドで手元から自動で戻ります（ネットは要りません）",
        needs_net: false,
    },
    CacheDef {
        key: "registry-cache",
        rel: "registry\\cache",
        title: "ダウンロードした部品（.crate）",
        note: "消すと、次のビルドでネットからダウンロードし直します。ネットが無いとビルドできなくなります",
        needs_net: true,
    },
    CacheDef {
        key: "git-db",
        rel: "git\\db",
        title: "Git から取った部品の元",
        note: "消すと、次のビルドでネットからダウンロードし直します。ネットが無いとビルドできなくなります",
        needs_net: true,
    },
];

#[derive(Serialize)]
struct CacheInfo {
    key: String,
    path: String,
    title: String,
    note: String,
    needs_net: bool,
    bytes: u64,
    exists: bool,
}

#[tauri::command]
async fn get_cargo_caches() -> Result<Vec<CacheInfo>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let home = cargo_home();
        CACHES
            .iter()
            .map(|c| {
                let p = home.join(c.rel);
                CacheInfo {
                    key: c.key.into(),
                    path: p.display().to_string(),
                    title: c.title.into(),
                    note: c.note.into(),
                    needs_net: c.needs_net,
                    bytes: fsops::measure(&p).bytes,
                    exists: p.is_dir(),
                }
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clean_cargo_cache(app: AppHandle, key: String) -> Result<CleanResult, String> {
    let def = CACHES.iter().find(|c| c.key == key).ok_or("知らない置き場です")?;
    let home = cargo_home();
    let dir = home.join(def.rel);
    tauri::async_runtime::spawn_blocking(move || {
        let running: Vec<String> = procs::list()
            .into_iter()
            .map(|p| p.name)
            .filter(|n| {
                let n = n.to_lowercase();
                n == "cargo.exe" || n == "rustc.exe"
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !running.is_empty() {
            return Err(format!(
                "cargo が動いています（{}）。ビルドが終わってからもう一度押してください",
                running.join("、")
            ));
        }
        if let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(home.join(".package-cache")) {
            if let Err(fs::TryLockError::WouldBlock) = f.try_lock() {
                return Err("cargo がダウンロード置き場を使っています。終わってからもう一度押してください".into());
            }
        }
        Ok(run_clean(&app, vec![dir], &home))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- ドライブの空き ----------

#[derive(Serialize)]
struct DriveFree {
    drive: String,
    free: u64,
}

#[tauri::command]
fn drives_free() -> Vec<DriveFree> {
    fixed_drives()
        .into_iter()
        .filter_map(|d| fsops::free_space(Path::new(&d)).map(|free| DriveFree { drive: d, free }))
        .collect()
}

fn main() {
    let shared: Shared = Arc::new(App::default());
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            add_root,
            remove_root,
            reset_roots,
            pick_projects_dir,
            list_projects,
            start_scan,
            cancel_scan,
            clean_target,
            open_folder,
            get_cargo_caches,
            clean_cargo_cache,
            clean_items,
            drives_free,
        ])
        .run(tauri::generate_context!())
        .expect("起動できませんでした");
}
