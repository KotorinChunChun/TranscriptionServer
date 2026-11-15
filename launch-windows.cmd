@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"
if exist "TranscriptionServer.exe" (
  "TranscriptionServer.exe" --lan --open
  exit /b !errorlevel!
)
if exist "target\release\TranscriptionServer.exe" (
  "target\release\TranscriptionServer.exe" --lan --open
  exit /b !errorlevel!
)
echo Rust executable not found. Use the installer or run cargo build --release.
exit /b 1
