//! プロジェクトの置き場（例: D:\claude）の中を、フォルダごとに測る
use crate::fsops::measure;
use crate::kinds::{self, Kind, Level};
use crate::scan::{incremental_dirs, inspect};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 「見るだけ」のフォルダは、これより小さければ出さない
const VIEW_MIN: u64 = 10 * 1024 * 1024;

struct Hit {
    kind: Kind,
    path: PathBuf,
    bytes: u64,
}

#[derive(Default)]
struct Tally {
    rust_target: u64,
    node_modules: u64,
    git: u64,
    other: u64,
    /// どの種類にも入らなかったファイルの合計（「見るだけ」の大きさに使う）
    plain: u64,
    newest: i64,
    langs: BTreeSet<&'static str>,
    hits: Vec<Hit>,
}

impl Tally {
    fn join(mut self, o: Tally) -> Tally {
        self.rust_target += o.rust_target;
        self.node_modules += o.node_modules;
        self.git += o.git;
        self.other += o.other;
        self.plain += o.plain;
        self.newest = self.newest.max(o.newest);
        self.langs.extend(o.langs);
        self.hits.extend(o.hits);
        self
    }
}

fn mtime_ms(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 種類の分かったフォルダを数える（中には入らない）
fn tally_kind(p: &Path, kind: Kind) -> Tally {
    let mut t = Tally::default();
    let lang = kinds::info(kind).lang;
    if !lang.is_empty() {
        t.langs.insert(lang);
    }
    match kind {
        Kind::RustTarget => {
            let info = inspect(p);
            t.rust_target = info.bytes;
            t.newest = info.newest;
            for inc in incremental_dirs(p) {
                let bytes = measure(&inc).bytes;
                if bytes > 0 {
                    t.hits.push(Hit { kind: Kind::RustIncremental, path: inc, bytes });
                }
            }
            t.hits.push(Hit { kind, path: p.to_path_buf(), bytes: info.bytes });
        }
        _ => {
            let m = measure(p);
            if kind == Kind::NodeModules {
                t.node_modules = m.bytes;
            } else {
                t.other = m.bytes;
            }
            t.newest = m.newest;
            t.hits.push(Hit { kind, path: p.to_path_buf(), bytes: m.bytes });
        }
    }
    t
}

/// フォルダ1つ。in_view は「見るだけ」のフォルダの中（入れ子の「見るだけ」は出さない）
fn tally_dir(p: &Path, name: &str, in_view: bool) -> Tally {
    if name.eq_ignore_ascii_case(".git") {
        return Tally { git: measure(p).bytes, ..Default::default() };
    }
    if let Some(kind) = kinds::classify(p) {
        return tally_kind(p, kind);
    }
    let view_here = !in_view && kinds::is_view_name(name);
    let mut t = tally(p, in_view || view_here);
    if view_here && t.plain >= VIEW_MIN {
        t.hits.push(Hit { kind: Kind::View, path: p.to_path_buf(), bytes: t.plain });
    }
    t
}

fn tally(dir: &Path, in_view: bool) -> Tally {
    let Ok(rd) = fs::read_dir(dir) else { return Tally::default() };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            let Ok(ft) = e.file_type() else { return Tally::default() };
            if ft.is_symlink() {
                return Tally::default();
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                return tally_dir(&e.path(), &name, in_view);
            }
            let Ok(md) = e.metadata() else { return Tally::default() };
            let mut t = Tally { other: md.len(), plain: md.len(), newest: mtime_ms(&md), ..Default::default() };
            if let Some(lang) = kinds::lang_of_file(&name) {
                t.langs.insert(lang);
            }
            t
        })
        .reduce(Tally::default, Tally::join)
}

/// 片づけ候補（同じ種類はまとめる）
#[derive(Serialize)]
pub struct Item {
    pub kind: Kind,
    pub level: Level,
    pub lang: &'static str,
    pub title: &'static str,
    pub note: &'static str,
    /// 消した後に戻すコマンド
    pub restore: Option<String>,
    pub paths: Vec<String>,
    pub bytes: u64,
    /// false のときはボタンを出さない（見るだけ・戻し方が見つからない）
    pub deletable: bool,
}

fn items_of(hits: Vec<Hit>) -> Vec<Item> {
    // (種類, 消せるか, 戻し方) ごとにまとめる。「見るだけ」は1つずつ
    let mut groups: BTreeMap<(Kind, bool, Option<String>, String), (Vec<String>, u64)> = BTreeMap::new();
    for h in hits {
        let restore = kinds::restore(h.kind, &h.path);
        let deletable = kinds::deletable(h.kind, &h.path);
        let solo = if h.kind == Kind::View { h.path.display().to_string() } else { String::new() };
        let g = groups.entry((h.kind, deletable, restore, solo)).or_default();
        g.0.push(h.path.display().to_string());
        g.1 += h.bytes;
    }
    let mut items: Vec<Item> = groups
        .into_iter()
        .map(|((kind, deletable, restore, _), (mut paths, bytes))| {
            paths.sort();
            let info = kinds::info(kind);
            let note = if !deletable && kind != Kind::View {
                "戻し方（requirements.txt など）が見つからないので、このツールでは消しません"
            } else {
                info.note
            };
            Item { kind, level: info.level, lang: info.lang, title: info.title, note, restore, paths, bytes, deletable }
        })
        .collect();
    items.sort_by(|a, b| a.level.cmp(&b.level).then(b.bytes.cmp(&a.bytes)));
    items
}

#[derive(Serialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// 見つけた言語（Rust・Python・JS・C#・Java・Go）
    pub langs: Vec<&'static str>,
    pub rust: bool,
    pub rust_target: u64,
    pub node_modules: u64,
    pub git: u64,
    pub other: u64,
    pub newest: i64,
    pub items: Vec<Item>,
}

#[derive(Serialize)]
pub struct ProjectsReply {
    pub dir: String,
    pub projects: Vec<Project>,
    /// 置き場の直下にあるファイル（フォルダに入っていないもの）
    pub loose_bytes: u64,
    pub seconds: f64,
}

pub fn list(dir: &Path) -> Result<ProjectsReply, String> {
    let started = std::time::Instant::now();
    let rd = fs::read_dir(dir).map_err(|e| format!("{} を開けません: {e}", dir.display()))?;
    let entries: Vec<_> = rd.flatten().collect();
    let loose_bytes = entries
        .iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    let mut projects: Vec<Project> = entries
        .par_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()))
        .map(|e| {
            let p = e.path();
            let t = tally_dir(&p, &e.file_name().to_string_lossy(), false);
            Project {
                name: e.file_name().to_string_lossy().into_owned(),
                path: p.display().to_string(),
                bytes: t.rust_target + t.node_modules + t.git + t.other,
                rust: t.langs.contains("Rust"),
                langs: t.langs.into_iter().collect(),
                rust_target: t.rust_target,
                node_modules: t.node_modules,
                git: t.git,
                other: t.other,
                newest: t.newest,
                items: items_of(t.hits),
            }
        })
        .collect();
    projects.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    Ok(ProjectsReply { dir: dir.display().to_string(), projects, loose_bytes, seconds: started.elapsed().as_secs_f64() })
}
