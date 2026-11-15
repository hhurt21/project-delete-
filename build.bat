@echo off
chcp 65001 >nul
rem Rust 容量そうじの exe を作る（同時に動かすコンパイルは 6 本まで）
cd /d "%~dp0src-tauri"
cargo build --release -j 6
if errorlevel 1 (
  echo ビルドに失敗しました
  pause
  exit /b 1
)
echo できました: %~dp0src-tauri\target\release\rust-cleaner.exe
pause
