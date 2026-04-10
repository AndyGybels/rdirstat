; Inno Setup script for rdirstat (Windows installer)
; Compiled in CI via: iscc /DMyAppVersion=x.y.z installer\windows.iss

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif

#define MyAppName "rdirstat"
#define MyAppPublisher "Andy Gybels"
#define MyAppURL "https://github.com/AndyGybels/rdirstat"
#define MyAppGuiExe "rdirstat-gui.exe"
#define MyAppTuiExe "rdirstat.exe"

[Setup]
AppId={{6B4A0E6A-8C1E-4E3C-9B1D-2A5F3C7D1E90}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputBaseFilename=rdirstat-setup-x86_64
Compression=lzma
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut (GUI)"; GroupDescription: "Additional icons:"
Name: "addtopath"; Description: "Add rdirstat to the system PATH (TUI)"; GroupDescription: "Command line:"

[Files]
Source: "..\dist\{#MyAppGuiExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\{#MyAppTuiExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName} (GUI)"; Filename: "{app}\{#MyAppGuiExe}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppGuiExe}"; Tasks: desktopicon

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
    ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
    Check: NeedsAddPath(ExpandConstant('{app}')); Tasks: addtopath

[Run]
Filename: "{app}\{#MyAppGuiExe}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment',
    'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Param + ';', ';' + OrigPath + ';') = 0;
end;
