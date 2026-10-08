@echo off
REM push_github.cmd - create the remote and push this repo to GitHub.
REM
REM     scripts\push_github.cmd <github-username> [repo-name]
REM
REM Steps it performs:
REM   1. checks the repo is a clean-ish git repo with commits;
REM   2. builds the remote URL from your username (so you never hand-type a URL
REM      and never paste a placeholder by accident);
REM   3. pushes main. Git Credential Manager will pop a browser window to sign in
REM      to GitHub - no Personal Access Token needed on this machine.
REM
REM Before running: create an EMPTY repo on https://github.com/new with the same
REM name (no README, no .gitignore, no license), then run this script.
REM
REM NOTE: keep this file pure ASCII (cmd.exe parses batch with the OEM code page).

setlocal enabledelayedexpansion

if "%~1"=="" (
  echo usage: scripts\push_github.cmd ^<github-username^> [repo-name]
  echo.
  echo   1^) open https://github.com/new and create an EMPTY repo named MXrader-DFM
  echo   2^) run: scripts\push_github.cmd yourname
  echo   3^) sign in in the browser window Git Credential Manager opens
  exit /b 2
)

set "GH_USER=%~1"
set "GH_REPO=%~2"
if "%GH_REPO%"=="" set "GH_REPO=MXrader-DFM"

REM Validate the username without relying on "echo x | findstr ^...$":
REM cmd inserts a space before the pipe, which breaks the end anchor - that bug
REM rejected a perfectly valid name like "octocat" during testing.
REM Instead: string substitution detects forbidden characters, and a findstr for
REM "any char outside printable ASCII" catches the Chinese placeholder case.
set "CHK=%GH_USER%"
set "BAD="
if "%CHK%"=="" set "BAD=empty"
if not "%CHK%"=="%CHK:<=%" set "BAD=angle bracket"
if not "%CHK%"=="%CHK:>=%" set "BAD=angle bracket"
if not "%CHK%"=="%CHK: =%" set "BAD=space"
if not "%CHK%"=="%CHK:_=%" set "BAD=underscore"
if not "%CHK%"=="%CHK:/=%" set "BAD=slash"
if not "%CHK%"=="%CHK:\=%" set "BAD=backslash"
REM NOTE: there is deliberately no %% check - percent substitution inside a batch
REM file is unreliable enough to cause false rejections.
echo %CHK%|findstr /r "[^ -~]" >nul && set "BAD=non-ASCII"
if defined BAD (
  echo [push] "%GH_USER%" is not a valid GitHub username ^(%BAD%^).
  echo [push] expected letters/digits/hyphens only, e.g. "octocat".
  echo [push] do NOT paste a placeholder - use your real account name.
  exit /b 2
)

cd /d "%~dp0.." || exit /b 1

git rev-parse --is-inside-work-tree >nul 2>&1
if errorlevel 1 (
  echo [push] not a git repository: %CD%
  exit /b 1
)

for /f "delims=" %%b in ('git branch --show-current') do set "BRANCH=%%b"
if "%BRANCH%"=="" set "BRANCH=main"

git rev-parse --verify HEAD >nul 2>&1
if errorlevel 1 (
  echo [push] no commits yet - nothing to push.
  exit /b 1
)

set "URL=https://github.com/%GH_USER%/%GH_REPO%.git"

REM Replace any stale origin (e.g. one added with a placeholder URL).
git remote remove origin >nul 2>&1
git remote add origin "%URL%"

echo [push] repo   : %CD%
echo [push] branch : %BRANCH%
echo [push] remote : %URL%
echo.
echo [push] pushing... (a browser window will open for GitHub sign-in; do not close it)
git push -u origin %BRANCH%
if errorlevel 1 (
  echo.
  echo [push] FAILED. Common causes:
  echo   * the repo does not exist on GitHub yet - create it at https://github.com/new
  echo   * the repo name differs from "%GH_REPO%" - pass it as the 2nd argument
  echo   * username typo
  exit /b 1
)

echo.
echo [push] done. Next:
echo   1^) open https://github.com/%GH_USER%/%GH_REPO%/actions
echo   2^) pick "iOS IPA" on the left, then "Run workflow" -^> Run
echo   3^) when it finishes, download the artifact "MXrader-unsigned-ipa"
echo   4^) sideload that .ipa with Sideloadly (see docs\BUILD_IPA.md)
exit /b 0
