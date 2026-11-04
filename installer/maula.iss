; maula installer (Inno Setup 6). Build: cargo build --release, then
;   "C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\maula.iss
; Output: target\installer\maula-setup-<version>.exe
; Installs per user (no admin prompt) into %LOCALAPPDATA%\Programs\maula.

#define Version "0.1.1"

[Setup]
; never change AppId: it is how upgrades and the uninstaller find an existing install
AppId={{06ECB0BF-D1DA-486E-96E5-45453DDECC5B}
AppName=maula
AppVersion={#Version}
AppPublisher=Andre Ramadhan
DefaultDirName={autopf}\maula
DefaultGroupName=maula
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=maula-setup-{#Version}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\maula.exe
SetupIconFile=..\assets\maula.ico
; a running maula is closed before its exe is replaced
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\maula.exe"; DestDir: "{app}"; Flags: ignoreversion
; Microsoft's newer ConPTY (OpenConsole): maula loads the conpty.dll next to it, which starts x64\OpenConsole.exe
Source: "..\assets\conpty\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\conpty\x64\OpenConsole.exe"; DestDir: "{app}\x64"; Flags: ignoreversion
Source: "..\assets\conpty\LICENSE.txt"; DestDir: "{app}\licenses"; DestName: "OpenConsole-LICENSE.txt"
Source: "..\assets\JetBrainsMono-OFL.txt"; DestDir: "{app}\licenses"
Source: "..\assets\Lucide-LICENSE.txt"; DestDir: "{app}\licenses"

[Icons]
Name: "{group}\maula"; Filename: "{app}\maula.exe"
Name: "{autodesktop}\maula"; Filename: "{app}\maula.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\maula.exe"; Description: "{cm:LaunchProgram,maula}"; Flags: nowait postinstall skipifsilent
