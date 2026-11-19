; Inno Setup 6 script for the Candy-Corn Windows installer.
;
; Build it with installer\build.ps1. The script builds a release exe with the C runtime linked statically and
; passes its path and the Cargo.toml version:
;   ISCC.exe /DAppVersion=0.1.0 /DAppExe=<path>\Candy-Corn.exe installer\candy-corn.iss

#define AppName      "Candy-Corn"
#define AppExeName   "Candy-Corn.exe"
#define AppPublisher "lightgo-la"
#define AppURL       "https://github.com/lightgo-la/Kiroween-2026_Candy-Corn"

#ifndef AppVersion
  #error Define AppVersion, e.g. ISCC /DAppVersion=0.1.0 (installer\build.ps1 does this)
#endif
#ifndef AppExe
  #error Define AppExe, the path of the release Candy-Corn.exe (installer\build.ps1 does this)
#endif

[Setup]
; Upgrades match on AppId, so it must never change. "{{" is a literal "{"; the closing brace is single.
AppId={{298BB4B3-5DB8-43D7-898C-14C40A12E51D}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppCopyright=Copyright (C) 2026 yhlee
VersionInfoVersion={#AppVersion}
; Per-user install without an admin prompt (%LOCALAPPDATA%\Programs\Candy-Corn). /ALLUSERS installs for all users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName} {#AppVersion}
OutputDir=..\dist
OutputBaseFilename={#AppName}-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
WizardSmallImageFile=..\assets\icon-256.png
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; The exe is x86_64-pc-windows-msvc, which needs 64-bit Windows 10 or later.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Offer to close a running Candy-Corn instead of failing to replace the exe.
CloseApplications=yes
RestartApplications=no
; English or Korean, picked from the Windows display language; the dialog appears only when neither matches.
ShowLanguageDialog=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\samples\*"; DestDir: "{app}\samples"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"
; The graph tool has no other entry point than the --canvas flag.
Name: "{group}\{#AppName} Graph Tool"; Filename: "{app}\{#AppExeName}"; Parameters: "--canvas"; WorkingDir: "{app}"
Name: "{group}\{#AppName} Samples"; Filename: "{app}\samples"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent runasoriginaluser
