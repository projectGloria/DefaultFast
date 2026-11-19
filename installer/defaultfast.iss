; Inno Setup script for DefaultFast (Windows x86_64)
; Invoked by .github/workflows/build-installers.yml (or locally via ISCC.exe).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#ifndef NumericVersion
  #define NumericVersion "0.0.0.0"
#endif

#ifndef BinaryPath
  #define BinaryPath "..\target\x86_64-pc-windows-msvc\release\DefaultFast.exe"
#endif

#ifndef OutputDir
  #define OutputDir "..\installers"
#endif

#ifndef OutputBaseFilename
  #define OutputBaseFilename "DefaultFast-0.0.0-windows-x86_64"
#endif

[Setup]
AppId={{B4E7F8A2-6D1C-4E9B-8A3F-2C5D9E1A7B40}
AppName=DefaultFast
AppVersion={#AppVersion}
AppVerName=DefaultFast {#AppVersion}
AppPublisher=DefaultFast Contributors
AppPublisherURL=https://github.com/projectGloria/DefaultFast
AppSupportURL=https://github.com/projectGloria/DefaultFast/issues
AppUpdatesURL=https://github.com/projectGloria/DefaultFast/releases
DefaultDirName={autopf}\DefaultFast
DefaultGroupName=DefaultFast
AllowNoIcons=yes
LicenseFile=..\LICENSE
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseFilename}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\DefaultFast.exe
UninstallDisplayName=DefaultFast
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=DefaultFast
VersionInfoProductVersion={#NumericVersion}
VersionInfoDescription=DefaultFast Installer
VersionInfoCopyright=MIT License
#ifdef IconPath
SetupIconFile={#IconPath}
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#BinaryPath}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\DefaultFast"; Filename: "{app}\DefaultFast.exe"
Name: "{group}\{cm:UninstallProgram,DefaultFast}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\DefaultFast"; Filename: "{app}\DefaultFast.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\DefaultFast.exe"; Description: "{cm:LaunchProgram,DefaultFast}"; Flags: nowait postinstall skipifsilent
