//! 消してよいフォルダの見分け方（言語ごと）
//!
//! 名前だけで決めず、親フォルダの印（package.json・*.csproj など）や
//! 中身の印（pyvenv.cfg・PYZ-00.toc など）も確かめる。
use crate::scan::is_cargo_target;
use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Debug, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    PyCache,
    JsCache,
    RustIncremental,
    RustTarget,
    PyInstallerWork,
    NuitkaWork,
    DotnetObj,
    GradleCache,
    Venv,
    NodeModules,
    /// 名前だけでは判断できない（release・dist など）。表示だけ
    View,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// 消しても自動で作り直される
    Safe,
    /// 作り直せるが、手間か時間かネットが要る
    Rebuild,
    /// このツールでは消さない
    View,
}

pub struct Info {
    pub level: Level,
    pub lang: &'static str,
    pub title: &'static str,
    pub note: &'static str,
}

pub fn info(k: Kind) -> Info {
    let (level, lang, title, note) = match k {
        Kind::PyCache => (
            Level::Safe,
            "Python",
            "Python のキャッシュ",
            "Python が自動で作り直します。消しても困ることはありません",
        ),
        Kind::JsCache => (
            Level::Safe,
            "JS",
            "JS のキャッシュ",
            "次に開発用の起動やビルドをしたときに、自動で作り直します",
        ),
        Kind::RustIncremental => (
            Level::Safe,
            "Rust",
            "Rust の途中の記録",
            "次のビルドで自分のコードを1回だけ全部作り直します（目安：数十秒〜数分）。部品は作り直しません",
        ),
        Kind::RustTarget => (
            Level::Rebuild,
            "Rust",
            "Rust の作業フォルダ（target）",
            "「cargo clean」と同じです。次のビルドは部品から全部作り直します（目安：数分〜十数分）。作った exe も消えます",
        ),
        Kind::PyInstallerWork => (
            Level::Rebuild,
            "Python",
            "PyInstaller の途中のファイル",
            "次の exe 作りで作り直します（少し遅くなるだけです）。.spec などの設定ファイルには触りません",
        ),
        Kind::NuitkaWork => (
            Level::Rebuild,
            "Python",
            "Nuitka の途中のファイル",
            "次の exe 作りで C のコンパイルを最初からやり直すので、とても時間がかかり、その間 PC が重くなります",
        ),
        Kind::DotnetObj => (Level::Rebuild, "C#", "C# の途中のファイル（obj）", "次の dotnet build で作り直します"),
        Kind::GradleCache => (Level::Rebuild, "Java", "Gradle のキャッシュ（.gradle）", "次のビルドで作り直します"),
        Kind::Venv => (
            Level::Rebuild,
            "Python",
            "Python の仮想環境（.venv）",
            "消すと、戻すまでこのプロジェクトの Python が動きません。戻すにはネットが要ります",
        ),
        Kind::NodeModules => (
            Level::Rebuild,
            "JS",
            "node_modules",
            "消すと、戻すまで開発用の起動やビルドができません。戻すにはネットが要ります。npm run dev などは先に止めてください",
        ),
        Kind::View => (
            Level::View,
            "",
            "名前だけでは判断できないフォルダ",
            "作った exe・設定・記録が入っていることがあるので、このツールでは消しません。中を見て自分で決めてください",
        ),
    };
    Info { level, lang, title, note }
}

fn has_file(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file()
}

/// dir の直下に、条件に合う名前のファイルがあるか
fn has_file_where(dir: &Path, f: impl Fn(&str) -> bool) -> Option<String> {
    let rd = fs::read_dir(dir).ok()?;
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| f(&n.to_lowercase()))
}

/// __pycache__ の中が .pyc だけか（ほかの物が入っていたら消さない）
fn only_pyc(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else { return false };
    rd.flatten().all(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".pyc"))
}

/// 上に 1〜3 段たどって、cargo の作業フォルダがあればその場所
pub fn cargo_target_above(path: &Path) -> Option<&Path> {
    path.ancestors().skip(1).take(3).find(|a| is_cargo_target(a))
}

/// フォルダの種類。消してよい種類でなければ None
pub fn classify(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let parent = path.parent()?;
    let kind = match name.as_str() {
        "__pycache__" if only_pyc(path) => Kind::PyCache,
        ".mypy_cache" | ".pytest_cache" | ".ruff_cache" => Kind::PyCache,
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular"
            if has_file(parent, "package.json") =>
        {
            Kind::JsCache
        }
        "node_modules" if has_file(parent, "package.json") => Kind::NodeModules,
        "incremental" if cargo_target_above(path).is_some() => Kind::RustIncremental,
        "obj" if has_file_where(parent, |n| n.ends_with(".csproj") || n.ends_with(".fsproj") || n.ends_with(".vbproj")).is_some() => {
            Kind::DotnetObj
        }
        ".gradle"
            if ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"]
                .iter()
                .any(|f| has_file(parent, f)) =>
        {
            Kind::GradleCache
        }
        _ if is_cargo_target(path) => Kind::RustTarget,
        _ if has_file(path, "pyvenv.cfg") => Kind::Venv,
        _ if has_file(path, "PYZ-00.toc") || has_file(path, "Analysis-00.toc") => Kind::PyInstallerWork,
        _ if (name.ends_with(".build") || name.ends_with(".onefile-build"))
            && has_file_where(path, |n| n.starts_with(".sconsign")).is_some() =>
        {
            Kind::NuitkaWork
        }
        _ => return None,
    };
    Some(kind)
}

/// 中身を見ないと判断できない名前（大きければ「見るだけ」で出す）
pub fn is_view_name(name: &str) -> bool {
    let n = name.to_lowercase();
    matches!(n.as_str(), "release" | "dist" | "out" | "bin" | "build") || n.ends_with(".dist")
}

/// 戻し方（消した後に打つコマンド）。見つからなければ None
pub fn restore(kind: Kind, path: &Path) -> Option<String> {
    let parent = path.parent()?;
    match kind {
        Kind::Venv => {
            let folder = path.file_name()?.to_string_lossy().into_owned();
            if has_file(parent, "uv.lock") {
                Some("uv sync".into())
            } else if let Some(req) = has_file_where(parent, |n| n.starts_with("requirements") && n.ends_with(".txt")) {
                Some(format!("python -m venv {folder} の後に {folder}\\Scripts\\pip install -r {req}"))
            } else if has_file(parent, "pyproject.toml") {
                Some(format!("python -m venv {folder} の後に {folder}\\Scripts\\pip install -e ."))
            } else {
                None
            }
        }
        Kind::NodeModules => Some(
            if has_file(parent, "pnpm-lock.yaml") {
                "pnpm install"
            } else if has_file(parent, "yarn.lock") {
                "yarn install"
            } else if has_file(parent, "package-lock.json") {
                "npm ci"
            } else {
                "npm install"
            }
            .into(),
        ),
        _ => None,
    }
}

/// 戻し方が要るのに見つからないものは消させない
pub fn deletable(kind: Kind, path: &Path) -> bool {
    match info(kind).level {
        Level::View => false,
        _ if matches!(kind, Kind::Venv | Kind::NodeModules) => restore(kind, path).is_some(),
        _ => true,
    }
}

/// ファイル名から分かる言語
pub fn lang_of_file(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    if n == "cargo.toml" {
        Some("Rust")
    } else if n == "package.json" {
        Some("JS")
    } else if n.ends_with(".py") || n == "pyproject.toml" || n == "setup.py" || n == "pipfile" || (n.starts_with("requirements") && n.ends_with(".txt")) {
        Some("Python")
    } else if n.ends_with(".csproj") || n.ends_with(".sln") || n.ends_with(".fsproj") {
        Some("C#")
    } else if n == "build.gradle" || n == "build.gradle.kts" || n == "pom.xml" {
        Some("Java")
    } else if n == "go.mod" {
        Some("Go")
    } else {
        None
    }
}
