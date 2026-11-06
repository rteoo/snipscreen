; Inno Setup script for SnipScreen — per-user install, no admin required.
;
; Build: run `cargo build --release` in app\ first (produces
; app\target\release\snipscreen.exe), then build_installer.bat (compiles this
; script into installer\Output\).
;
; SnipScreen keeps no settings or data of its own: snips and recordings live in
; the user's Pictures and Videos folders and are never touched on uninstall.

#define MyAppName "SnipScreen"
#define MyAppVersion "0.1.1"
#define MyAppPublisher "Project Contributors"
#define MyAppExeName "snipscreen.exe"
#define MyAppIcon "..\app\assets\snipscreen.ico"
#define MyBuildDir "..\app\target\release"

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
; SnipScreen has no single-instance mutex; Restart Manager closes a running
; copy so an upgrade can replace the executable.
CloseApplications=yes

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; No "start with Windows" task: SnipScreen opens straight into area picking.
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "{#MyBuildDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent
