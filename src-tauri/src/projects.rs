//! プロジェクトの置き場（例: D:\claude）の中を、フォルダごとに測る
use crate::fsops::measure;
use crate::scan::{inspect, is_cargo_target};
use rayon::prelude::*;
use serde::Serialize;
use std::fs;
use std::path::Path;

/// 1つのプロジェクトの中身の内訳
#[derive(Default, Clone)]
struct Tally {
    rust_target: u64,
    rust_incremental: u64,
    /// 見つけた cargo の作業フォルダの場所
    targets: Vec<String>,
    node_modules: u64,
    git: u64,
    other: u64,
    newest: i64,
    has_cargo_toml: bool,
}

impl Tally {
    fn join(mut self, o: Tally) -> Tally {
        self.rust_target += o.rust_target;
        self.rust_incremental += o.rust_incremental;
        self.targets.extend(o.targets);
        self.node_modules += o.node_modules;
        self.git += o.git;
        self.other += o.other;
        self.newest = self.newest.max(o.newest);
        self.has_cargo_toml |= o.has_cargo_toml;
        self
    }
}

/// フォルダ1つを、名前で分けて数える（node_modules・.git・cargo の作業フォルダ・そのほか）
fn tally_dir(p: &Path, name: &str) -> Tally {
    if name.eq_ignore_ascii_case("node_modules") {
        let m = measure(p);
        Tally { node_modules: m.bytes, newest: m.newest, ..Default::default() }
    } else if name.eq_ignore_ascii_case(".git") {
        let m = measure(p);
        Tally { git: m.bytes, ..Default::default() }
    } else if is_cargo_target(p) {
        let info = inspect(p);
        Tally {
            rust_target: info.bytes,
            rust_incremental: info.incremental,
            targets: vec![info.path],
            newest: info.newest,
            has_cargo_toml: true,
            ..Default::default()
        }
    } else {
        tally(p)
    }
}

fn tally(dir: &Path) -> Tally {
    let Ok(rd) = fs::read_dir(dir) else { return Tally::default() };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            let Ok(ft) = e.file_type() else { return Tally::default() };
            if ft.is_symlink() {
                return Tally::default();
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if ft.is_dir() {
                tally_dir(&e.path(), &name)
            } else {
                let Ok(md) = e.metadata() else { return Tally::default() };
                let newest = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                Tally {
                    other: md.len(),
                    newest,
                    has_cargo_toml: name.eq_ignore_ascii_case("Cargo.toml"),
                    ..Default::default()
                }
            }
        })
        .reduce(Tally::default, Tally::join)
}

#[derive(Serialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    /// Cargo.toml か cargo の作業フォルダがある
    pub rust: bool,
    pub rust_target: u64,
    pub rust_incremental: u64,
    pub targets: Vec<String>,
    pub node_modules: u64,
    pub git: u64,
    pub other: u64,
    pub newest: i64,
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
            let t = tally_dir(&p, &e.file_name().to_string_lossy());
            Project {
                name: e.file_name().to_string_lossy().into_owned(),
                path: p.display().to_string(),
                bytes: t.rust_target + t.node_modules + t.git + t.other,
                rust: t.has_cargo_toml,
                rust_target: t.rust_target,
                rust_incremental: t.rust_incremental,
                targets: t.targets,
                node_modules: t.node_modules,
                git: t.git,
                other: t.other,
                newest: t.newest,
            }
        })
        .collect();
    projects.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    Ok(ProjectsReply { dir: dir.display().to_string(), projects, loose_bytes, seconds: started.elapsed().as_secs_f64() })
}
