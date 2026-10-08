@echo off
REM rust.cmd - run cargo on this machine without fighting the PowerShell execution policy.
REM
REM     scripts\rust.cmd test
REM     scripts\rust.cmd test --no-fail-fast
REM     scripts\rust.cmd run --bin battle_replay -- --capture capture.ndjson --sweep
REM
REM It does three things:
REM   1. puts %USERPROFILE%\.cargo\bin on PATH (rustup installs there; a fresh
REM      shell spawned by a long-running host may not inherit the new PATH);
REM   2. finds the WinLibs MinGW gcc that winget installed and uses it as the
REM      linker for the GNU target (Windows ships no linker of its own);
REM   3. invokes cargo with the GNU toolchain, which is the smallest setup that
REM      can link this crate on a box without Visual Studio Build Tools.
REM
REM One-time dependencies:
REM   winget install --id Rustlang.Rustup -e
REM   winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e
REM   rustup toolchain install stable-x86_64-pc-windows-gnu
REM
REM NOTE: keep this file pure ASCII. cmd.exe parses batch files with the OEM
REM code page, so non-ASCII comments corrupt the line stream and break parsing.

setlocal enabledelayedexpansion

set "CARGO_BIN=%USERPROFILE%\.cargo\bin"
if not exist "%CARGO_BIN%\cargo.exe" (
  echo [rust.cmd] missing %CARGO_BIN%\cargo.exe
  echo [rust.cmd] install it first: winget install --id Rustlang.Rustup -e
  exit /b 9009
)

set "MINGW_BIN="
REM Prefer the winget package directory; fall back to any mingw64\bin\gcc.exe found there.
for /d %%d in ("%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs*") do (
  if exist "%%~fd\mingw64\bin\gcc.exe" set "MINGW_BIN=%%~fd\mingw64\bin"
)
if not defined MINGW_BIN (
  for /f "delims=" %%i in ('dir /b /s "%LOCALAPPDATA%\Microsoft\WinGet\Packages\gcc.exe" 2^>nul') do (
    if not defined MINGW_BIN (
      echo %%~dpi | findstr /i /c:"mingw64\bin\" >nul && set "MINGW_BIN=%%~dpi"
    )
  )
)

REM Allow an explicit toolchain override: `rust.cmd +nightly test`.
set "TOOLCHAIN=+stable-x86_64-pc-windows-gnu"
set "FIRST=%1"
if "%FIRST:~0,1%"=="+" set "TOOLCHAIN="

if defined MINGW_BIN (
  set "PATH=%CARGO_BIN%;%MINGW_BIN%;%PATH%"
  set "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=%MINGW_BIN%gcc.exe"
) else (
  echo [rust.cmd] WARNING: no MinGW gcc.exe found; the GNU target will fail to link.
  echo [rust.cmd] install it: winget install --id BrechtSanders.WinLibs.POSIX.UCRT -e
  set "PATH=%CARGO_BIN%;%PATH%"
)

set "CARGO_TERM_COLOR=never"
cargo %TOOLCHAIN% %*
exit /b %ERRORLEVEL%
