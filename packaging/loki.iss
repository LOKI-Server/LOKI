; Build from the repository root (Inno Setup 6.3 or newer):
;   cargo build --release --locked -p loki-server
;   cargo xtask bundle
;   ISCC.exe packaging/loki.iss
;
; Source and output paths below are relative to this script's directory.
; The installation contains executables/assets only. LOKI selects its data
; directory and registers optional sign-in startup through its setup/web UI.

#define AppVersion "0.4.0"
#define BundleDir "..\target\bundle"

[Setup]
AppId=Aspenini.LOKI
AppName=LOKI Server
AppVersion={#AppVersion}
AppPublisher=Aspenini
AppPublisherURL=https://github.com/LOKI-Server/LOKI
AppSupportURL=https://github.com/LOKI-Server/LOKI/issues
AppUpdatesURL=https://github.com/LOKI-Server/LOKI/releases
DefaultDirName={localappdata}\Programs\LOKI
DefaultGroupName=LOKI
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\LICENSE
OutputDir=..\target\dist
OutputBaseFilename=LOKI-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
SetupIconFile=icon\loki.ico
UninstallDisplayIcon={app}\loki-server.exe
VersionInfoVersion={#AppVersion}.0
VersionInfoCompany=Aspenini
VersionInfoDescription=LOKI Server installer

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
; An explicit runtime allowlist keeps developer tools and sources out of the installer.
Source: "{#BundleDir}\loki-server.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BundleDir}\caddy.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BundleDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BundleDir}\CADDY-LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BundleDir}\CADDY-SOURCE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\LOKI Server"; Filename: "{app}\loki-server.exe"; WorkingDir: "{app}"
Name: "{group}\LOKI Dashboard"; Filename: "http://127.0.0.1:7070"
Name: "{group}\Uninstall LOKI"; Filename: "{uninstallexe}"
Name: "{autodesktop}\LOKI Server"; Filename: "{app}\loki-server.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\loki-server.exe"; WorkingDir: "{app}"; Description: "Start LOKI Server (use its tray icon to open setup or the dashboard)"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  StartupCommand: String;
  InstalledExecutable: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    // LOKI's startup registration uses a quoted executable path. Remove it only
    // when it belongs to this installation; another installation may own it.
    if RegQueryStringValue(HKCU,
      'Software\Microsoft\Windows\CurrentVersion\Run', 'LOKI', StartupCommand) then
    begin
      InstalledExecutable := '"' + ExpandConstant('{app}\loki-server.exe') + '"';
      if CompareText(Copy(Trim(StartupCommand), 1, Length(InstalledExecutable)),
        InstalledExecutable) = 0 then
        RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'LOKI');
    end;
  end;
end;
