//! Rust の作業フォルダ（cargo の target）を探す
use crate::fsops::{measure, Measure};
use rayon::prelude::*;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Serialize, Clone)]
pub struct Part {
    pub name: String,
    pub bytes: u64,
    pub newest: i64,
    /// この中の incremental（途中の記録）の合計
    pub incremental: u64,
}

#[derive(Serialize, Clone)]
pub struct TargetInfo {
    /// target フォルダの場所
    pub path: String,
    /// プロジェクトのフォルダ（target の親）
    pub project: String,
    /// Cargo.toml の名前。無ければフォルダ名
    pub name: String,
    pub bytes: u64,
    pub newest: i64,
    pub incremental: u64,
    pub parts: Vec<Part>,
}

/// 中に入らないフォルダ（小文字で比べる）
const SKIP: &[&str] = &[
    "node_modules",
    ".git",
    "$recycle.bin",
    "system volume information",
    "windows",
    "winsxs",
    ".cargo",
    ".rustup",
    "programdata",
];

/// cargo が作った target フォルダか
pub fn is_cargo_target(dir: &Path) -> bool {
    if let Ok(s) = fs::read_to_string(dir.join("CACHEDIR.TAG")) {
        if s.contains("cargo") {
            return true;
        }
    }
    // 古い cargo は CACHEDIR.TAG を作らない
    let named_target = dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target"));
    let has_toml = dir.parent().is_some_and(|p| p.join("Cargo.toml").is_file());
    named_target && has_toml && (dir.join(".rustc_info.json").is_file() || dir.join("debug").join("deps").is_dir())
}

fn package_name(project: &Path) -> String {
    let fallback = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(toml) = fs::read_to_string(project.join("Cargo.toml")) else { return fallback };
    let mut in_package = false;
    for line in toml.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_package = l == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = l.strip_prefix("name") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    let v = v.trim().trim_matches('"');
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
    }
    fallback
}

/// target の中の incremental フォルダ全部（target/debug/incremental と target/<機種>/debug/incremental）
pub fn incremental_dirs(target: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(target) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()))
        .flat_map(|e| incremental_dirs_in_part(&e.path()))
        .collect()
}

pub fn inspect(target: &Path) -> TargetInfo {
    let mut parts = Vec::new();
    let mut loose = Measure::default();
    if let Ok(rd) = fs::read_dir(target) {
        let entries: Vec<_> = rd.flatten().collect();
        parts = entries
            .par_iter()
            .filter_map(|e| {
                let ft = e.file_type().ok()?;
                if ft.is_symlink() || !ft.is_dir() {
                    return None;
                }
                let p = e.path();
                let m = measure(&p);
                let incremental = incremental_dirs_in_part(&p).iter().map(|d| measure(d).bytes).sum();
                Some(Part {
                    name: e.file_name().to_string_lossy().into_owned(),
                    bytes: m.bytes,
                    newest: m.newest,
                    incremental,
                })
            })
            .collect();
        for e in entries {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                if let Ok(md) = e.metadata() {
                    loose.bytes += md.len();
                }
            }
        }
    }
    parts.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    let project = target.parent().unwrap_or(target);
    TargetInfo {
        path: target.display().to_string(),
        project: project.display().to_string(),
        name: package_name(project),
        bytes: parts.iter().map(|p| p.bytes).sum::<u64>() + loose.bytes,
        newest: parts.iter().map(|p| p.newest).max().unwrap_or(0),
        incremental: parts.iter().map(|p| p.incremental).sum(),
        parts,
    }
}

fn incremental_dirs_in_part(part: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let inc = part.join("incremental");
    if inc.is_dir() {
        out.push(inc);
    }
    if let Ok(rd) = fs::read_dir(part) {
        for e in rd.flatten() {
            let inc2 = e.path().join("incremental");
            if e.file_type().is_ok_and(|t| t.is_dir()) && inc2.is_dir() {
                out.push(inc2);
            }
        }
    }
    out
}

pub struct Walk<'a> {
    pub cancel: &'a AtomicBool,
    pub seen_dirs: &'a AtomicU64,
}

/// dir の下から target を探す。見つけるたびに found を呼ぶ
pub fn walk(dir: &Path, w: &Walk, found: &(dyn Fn(PathBuf) + Sync)) {
    if w.cancel.load(Ordering::Relaxed) {
        return;
    }
    w.seen_dirs.fetch_add(1, Ordering::Relaxed);
    let Ok(rd) = fs::read_dir(dir) else { return };
    let subdirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()))
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            !SKIP.contains(&n.as_str())
        })
        .map(|e| e.path())
        .collect();
    subdirs.par_iter().for_each(|p| {
        if is_cargo_target(p) {
            found(p.clone());
        } else {
            walk(p, w, found);
        }
    });
}
