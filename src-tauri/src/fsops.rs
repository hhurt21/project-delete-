//! フォルダの大きさを測る・消す（どちらも並列）
use rayon::prelude::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

#[derive(Default, Clone, Copy)]
pub struct Measure {
    pub bytes: u64,
    /// 中で一番新しいファイルの更新時刻（ミリ秒）。ファイルが無ければ 0
    pub newest: i64,
}

impl Measure {
    fn join(self, o: Measure) -> Measure {
        Measure { bytes: self.bytes + o.bytes, newest: self.newest.max(o.newest) }
    }
}

fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// フォルダの中身の合計。リンク（ジャンクション）の先はたどらない
pub fn measure(dir: &Path) -> Measure {
    let Ok(rd) = fs::read_dir(dir) else { return Measure::default() };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| {
            let Ok(ft) = e.file_type() else { return Measure::default() };
            if ft.is_symlink() {
                Measure::default()
            } else if ft.is_dir() {
                measure(&e.path())
            } else {
                match e.metadata() {
                    Ok(m) => Measure { bytes: m.len(), newest: mtime_ms(&m) },
                    Err(_) => Measure::default(),
                }
            }
        })
        .reduce(Measure::default, Measure::join)
}

/// 消している途中の数え
#[derive(Default)]
pub struct Progress {
    pub freed: AtomicU64,
    pub failed: AtomicU64,
    pub first_error: Mutex<Option<String>>,
}

impl Progress {
    fn fail(&self, path: &Path, e: &std::io::Error) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        let mut fe = self.first_error.lock().unwrap();
        if fe.is_none() {
            *fe = Some(format!("{}: {}", path.display(), e));
        }
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(m) = fs::symlink_metadata(path) {
        let mut p = m.permissions();
        if p.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            let _ = fs::set_permissions(path, p);
        }
    }
}

fn remove_file_force(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(path);
            fs::remove_file(path)
        }
        r => r,
    }
}

/// リンクそのものを消す（先の中身には触らない）
fn remove_link(path: &Path) -> std::io::Result<()> {
    remove_file_force(path).or_else(|_| fs::remove_dir(path))
}

/// フォルダを中身ごと消す。消せないファイル（使用中など）は飛ばして数える。
/// 全部消せたら true
pub fn remove_tree(dir: &Path, p: &Progress) -> bool {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(e) => {
            p.fail(dir, &e);
            return false;
        }
    };
    let entries: Vec<_> = rd.flatten().collect();
    let all_ok = entries
        .par_iter()
        .map(|e| {
            let path = e.path();
            let Ok(ft) = e.file_type() else { return false };
            if ft.is_symlink() {
                match remove_link(&path) {
                    Ok(()) => true,
                    Err(err) => {
                        p.fail(&path, &err);
                        false
                    }
                }
            } else if ft.is_dir() {
                remove_tree(&path, p)
            } else {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                match remove_file_force(&path) {
                    Ok(()) => {
                        p.freed.fetch_add(len, Ordering::Relaxed);
                        true
                    }
                    Err(err) => {
                        p.fail(&path, &err);
                        false
                    }
                }
            }
        })
        .reduce(|| true, |a, b| a && b);
    if !all_ok {
        return false;
    }
    match fs::remove_dir(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(dir);
            match fs::remove_dir(dir) {
                Ok(()) => true,
                Err(e) => {
                    p.fail(dir, &e);
                    false
                }
            }
        }
        Err(e) => {
            p.fail(dir, &e);
            false
        }
    }
}

/// ドライブの空き容量（バイト）
#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None).ok()? };
    Some(free)
}

#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}
