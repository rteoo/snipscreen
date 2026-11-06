; Inno Setup script for SnipScreen — per-user install, no admin required.
;
; Build: run `cargo build --release` in app\ first (produces
; app\target\release\snipscreen.exe), then build_installer.bat (compiles this
; script into installer\Output\).
;
; Snips and recordings live in the user's Pictures and Videos folders and are
; never touched on uninstall; neither are the settings in %APPDATA%\SnipScreen
; or the notification pictures in %LOCALAPPDATA%\SnipScreen (some of them are
; snips that were not auto-saved).
; What SnipScreen registers for itself while it runs (the Run entry for "Start
; with Windows", its notification identity and the snipscreen: link handler,
; all under HKCU) is removed on uninstall.

#define MyAppName "SnipScreen"
#define MyAppVersion "0.1.1"
#define MyAppPublisher "Project Contributors"
#define MyAppExeName "snipscreen.exe"
#define MyAppIcon "..\app\assets\snipscreen.ico"
#define MyBuildDir "..\app\target\release"
#define MyAppId "SnipScreen.App"
; tray.rs MUTEX: held by a running copy for as long as it runs.
#define MyInstanceMutex "Local\SnipScreen.Instance"

[Setup]
; Stable AppId so future versions upgrade in place instead of installing twice.
AppId={{B6321002-CFDD-4998-BC58-DDDA28AF7C2C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; Per-user install: no administrator/UAC prompt.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; DXGI duplication, WASAPI and the capture exclusion need Windows 10 2004.
MinVersion=10.0.19041
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=Output
OutputBaseFilename=SnipscreenSetup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
SetupIconFile={#MyAppIcon}
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
WizardStyle=modern
; SnipScreen stays running in the tray. [Code] asks a running copy to quit
; (snipscreen.exe --quit) before files are replaced or removed; Restart Manager
; stays on as the fallback.
CloseApplications=yes

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; No "start with Windows" task: the app's own setting (on by default) owns the
; Run entry and can be changed in its Settings window.
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "{#MyBuildDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The same AppUserModelID the app sets on itself (notify.rs), so the running
; window groups with a pinned shortcut and notifications carry the app's name.
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; AppUserModelID: "{#MyAppId}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent

[Code]
// Asks a running copy to quit and waits (up to ~15 s, enough to finish saving a
// recording) until it is gone, so its files can be replaced or removed. Only a
// copy that holds the instance mutex understands --quit; a build from before
// it would open a window instead, so without the mutex nothing is run and
// Restart Manager closes whatever is running. The --quit launch is not waited
// for: it only forwards the request, and the mutex says when the copy is gone.
procedure QuitRunningCopy();
var
  Exe: String;
  Code, Waited: Integer;
begin
  Exe := ExpandConstant('{app}\{#MyAppExeName}');
  if not FileExists(Exe) or not CheckForMutexes('{#MyInstanceMutex}') then
    Exit;
  Exec(Exe, '--quit', '', SW_HIDE, ewNoWait, Code);
  Waited := 0;
  while CheckForMutexes('{#MyInstanceMutex}') and (Waited < 15000) do
  begin
    Sleep(100);
    Waited := Waited + 100;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitRunningCopy();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  QuitRunningCopy();
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'SnipScreen');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\AppUserModelId\{#MyAppId}');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\snipscreen');
  end;
end;
