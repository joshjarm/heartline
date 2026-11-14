@echo off
rem Builds Heartline (release). Rust is installed per-user and is not on PATH.
setlocal
set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
if not exist "%CARGO%" (
  echo Rust not found at %CARGO%
  echo Install it from https://rustup.rs and re-run.
  exit /b 1
)
cd /d "%~dp0"
"%CARGO%" build --release || exit /b 1
echo.
echo Built: %~dp0target\release\heartline.exe
endlocal
