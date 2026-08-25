; Inno Setup script for Henry's Shadowing App.
;
; Build it with packaging\build_installer.ps1 (which compiles the release exe,
; stages bin\, and passes AppVersion in) rather than compiling this directly -
; the defaults below only exist so opening this file in the Inno Setup IDE
; still works.
;
; The installer is per-user on purpose: it installs under %LOCALAPPDATA% and
; never asks for admin rights, so "download it and run it" works on a locked-
; down machine and SmartScreen is the only speed bump.

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif

#define AppName        "Henry's Shadowing App"
#define AppPublisher   "Henry"
#define AppUrl         "https://github.com/henry4324234/henrys_shadowing_app"
#define AppExeName     "henrys_shadowing_app.exe"
#define SourceExe      "..\target\release\" + AppExeName
#define StagedBin      "staging\bin"

[Setup]
; Identity of the installed product. Keep AppId stable forever: it is what
; makes the next version upgrade this one in place instead of installing
; alongside it.
AppId={{81032F94-0587-4C8A-AF86-3FF8DC6136E1}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}
VersionInfoDescription={#AppName} setup

; Per-user install: no UAC prompt, lands in %LOCALAPPDATA%\Programs.
PrivilegesRequired=lowest
DefaultDirName={autopf}\Henrys Shadowing App
DefaultGroupName={#AppName}
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}
AllowNoIcons=yes
DisableProgramGroupPage=yes

; 64-bit only - the bundled ffmpeg/deno/yt-dlp and the transcription engine
; the app downloads are all x64, and eframe wants Windows 10 or newer.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

WizardStyle=modern
SetupIconFile=..\assets\app.ico
Compression=lzma2/max
SolidCompression=yes
LZMANumBlockThreads=4
OutputDir=dist
OutputBaseFilename=HenrysShadowingApp-Setup-{#AppVersion}

; Offer to shut the app down if it is running during an upgrade, instead of
; failing on a locked file or demanding a reboot.
CloseApplications=yes
RestartApplications=no

; Code signing, if a certificate is ever configured. Define a "signtool" named
; SignTool in the Inno Setup IDE (or pass /Ssigntool=... to ISCC) and remove
; the semicolons to have setup and uninstaller signed automatically.
;SignTool=SignTool
;SignedUninstaller=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}";        DestDir: "{app}";     Flags: ignoreversion
; ffmpeg / deno / yt-dlp, staged by stage_bin.ps1. The app looks for these in
; a bin\ folder next to its own exe before falling back to its downloader or
; PATH (see resolve_program in src\download.rs), so the folder name matters.
Source: "{#StagedBin}\*.exe";  DestDir: "{app}\bin"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}";        Filename: "{app}\{#AppExeName}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
// The transcription engine the app downloads on first run is several GB and
// lives outside {app} (%LOCALAPPDATA%\henrys_shadowing_app), so uninstalling
// would otherwise leave it behind silently. Offer to remove it, along with
// the settings file, but default to keeping it: someone reinstalling a newer
// version should not have to download 1.4 GB again.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ManagedDir, SettingsDir: String;
begin
  if CurUninstallStep <> usPostUninstall then
    Exit;

  ManagedDir := ExpandConstant('{localappdata}\henrys_shadowing_app');
  SettingsDir := ExpandConstant('{userappdata}\henrys_shadowing_app');

  if not (DirExists(ManagedDir) or DirExists(SettingsDir)) then
    Exit;

  // Suppressible so /VERYSILENT uninstalls take the default (keep the data).
  if SuppressibleMsgBox(
       'Also delete the downloaded transcription engine and your saved settings?'#13#10#13#10 +
       ManagedDir + #13#10 +
       SettingsDir + #13#10#13#10 +
       'Keep them if you plan to reinstall - the engine is a multi-gigabyte download.',
       mbConfirmation, MB_YESNO, IDNO) = IDYES then
  begin
    DelTree(ManagedDir, True, True, True);
    DelTree(SettingsDir, True, True, True);
  end;
end;
