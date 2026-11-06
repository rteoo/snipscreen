@echo off
setlocal EnableExtensions
REM Compile the Windows installer (Setup.exe) from the release build.
REM Prerequisite: run "cargo build --release" in app\ so app\target\release\snipscreen.exe exists.

set "REPO_DIR=%~dp0"
if "%REPO_DIR:~-1%"=="\" set "REPO_DIR=%REPO_DIR:~0,-1%"
set "ISS=%REPO_DIR%\installer\snipscreen.iss"
set "EXE=%REPO_DIR%\app\target\release\snipscreen.exe"

if not exist "%EXE%" (
    echo Release build not found: "%EXE%"
    echo Run "cargo build --release" in app\ first, then re-run this script.
    if not defined SNIPSCREEN_BUILD_NONINTERACTIVE pause
    exit /b 1
)

REM Locate the Inno Setup 6 command-line compiler (ISCC.exe).
set "ISCC="
where iscc >nul 2>&1 && set "ISCC=iscc"
if not defined ISCC if exist "%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" set "ISCC=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe"
if not defined ISCC if exist "%ProgramFiles%\Inno Setup 6\ISCC.exe" set "ISCC=%ProgramFiles%\Inno Setup 6\ISCC.exe"
if not defined ISCC (
    echo Inno Setup compiler ISCC.exe was not found.
    echo Install Inno Setup 6 ^(free^) from https://jrsoftware.org/isdl.php and re-run.
    if not defined SNIPSCREEN_BUILD_NONINTERACTIVE pause
    exit /b 1
)

echo Compiling installer with "%ISCC%"...
"%ISCC%" "%ISS%"
if errorlevel 1 (
    echo Installer compilation failed.
    if not defined SNIPSCREEN_BUILD_NONINTERACTIVE pause
    exit /b 1
)

echo.
echo Done. The installer is in installer\Output\
if not defined SNIPSCREEN_BUILD_NONINTERACTIVE pause
endlocal
