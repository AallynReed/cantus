; Built by the release workflow: iscc /DAppVersion=... /DBinary=... /DOutputName=...

[Setup]
AppName=Cantus
AppVersion={#AppVersion}
AppPublisher=Cantus
DefaultDirName={autopf}\Cantus
DefaultGroupName=Cantus
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
LicenseFile=..\..\LICENSE
OutputDir=..\..
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName=Cantus

[Files]
Source: "{#Binary}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Cantus"; Filename: "{app}\cantus.exe"
Name: "{autodesktop}\Cantus"; Filename: "{app}\cantus.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\cantus.exe"; Description: "Start Cantus"; Flags: nowait postinstall skipifsilent
