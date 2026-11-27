#define MyAppName "SoundHop"
#define MyAppVersion "2.8.6"
#define MyAppPublisher "SoundHop Contributors"
#define MyAppURL "https://github.com/teufortressIndustries/soundhop"
#define MyAppExeName "soundhop-gui.exe"
#define MyAppCliName "soundhop.exe"

#ifndef BuildDir
  #define BuildDir "..\..\target\release"
#endif

#ifndef StageDir
  #define StageDir BuildDir
#endif

#ifndef OutputDirPath
  #define OutputDirPath "..\..\dist"
#endif

[Setup]
; Unique application identifier (escaped opening brace for Inno Setup)
AppId={{7A4E9C1B-5F82-4D3A-9E61-2C8B4F0D1A3E}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
LicenseFile=..\..\LICENSE
SetupIconFile=..\..\assets\soundhop.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
; Per-user install by default (no UAC needed); dialog allows choosing all-users
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDirPath}
OutputBaseFilename=SoundHop-{#MyAppVersion}-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=yes
CloseApplicationsFilter=soundhop-gui.exe,soundhop.exe
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "Start SoundHop when Windows starts"; GroupDescription: "Startup & Integration:"; Flags: unchecked
Name: "addtopath"; Description: "Add CLI (soundhop.exe) to user PATH"; GroupDescription: "Startup & Integration:"

[Files]
Source: "{#StageDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\{#MyAppCliName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\*.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\lib\gstreamer-1.0\*.dll"; DestDir: "{app}\lib\gstreamer-1.0"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Comment: "Share speakers across your local network"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon; Comment: "Share speakers across your local network"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "SoundHop"; ValueData: """{app}\{#MyAppExeName}"" tray"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvironmentKey = 'Environment';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  RunValueName = 'SoundHop';

function NormalizePathEntry(const Value: string): string;
begin
  Result := RemoveBackslashUnlessRoot(Trim(Value));
end;

procedure EnvAddPath(const NewPath: string);
var
  Paths: string;
  NormalizedNew: string;
  Remaining: string;
  Entry: string;
  SepPos: Integer;
begin
  NormalizedNew := NormalizePathEntry(NewPath);
  if NormalizedNew = '' then
    Exit;

  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Paths := '';

  Remaining := Paths;
  while Remaining <> '' do
  begin
    SepPos := Pos(';', Remaining);
    if SepPos > 0 then
    begin
      Entry := Copy(Remaining, 1, SepPos - 1);
      Delete(Remaining, 1, SepPos);
    end
    else
    begin
      Entry := Remaining;
      Remaining := '';
    end;

    if CompareText(NormalizePathEntry(Entry), NormalizedNew) = 0 then
      Exit;
  end;

  if (Paths <> '') and (Paths[Length(Paths)] <> ';') then
    Paths := Paths + ';';
  Paths := Paths + NormalizedNew;

  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure EnvRemovePath(const RemovePath: string);
var
  Paths: string;
  NormalizedTarget: string;
  Remaining: string;
  Entry: string;
  Updated: string;
  SepPos: Integer;
  Changed: Boolean;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Exit;

  NormalizedTarget := NormalizePathEntry(RemovePath);
  Remaining := Paths;
  Updated := '';
  Changed := False;

  while Remaining <> '' do
  begin
    SepPos := Pos(';', Remaining);
    if SepPos > 0 then
    begin
      Entry := Copy(Remaining, 1, SepPos - 1);
      Delete(Remaining, 1, SepPos);
    end
    else
    begin
      Entry := Remaining;
      Remaining := '';
    end;

    if CompareText(NormalizePathEntry(Entry), NormalizedTarget) = 0 then
    begin
      Changed := True;
    end
    else if Trim(Entry) <> '' then
    begin
      if Updated <> '' then
        Updated := Updated + ';';
      Updated := Updated + Entry;
    end;
  end;

  if Changed then
    RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Updated);
end;

procedure StopRunningSoundHop();
var
  CliPath: string;
  ResultCode: Integer;
begin
  CliPath := ExpandConstant('{app}\{#MyAppCliName}');
  if FileExists(CliPath) then
    Exec(CliPath, 'stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);

  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#MyAppExeName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#MyAppCliName}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CleanupFirewallRules();
var
  NetshPath: string;
  ResultCode: Integer;
begin
  if not IsAdmin then
    Exit;

  NetshPath := ExpandConstant('{sys}\netsh.exe');
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop Audio (4713/tcp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop Control (4714/tcp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop Pairing (4715/tcp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop RTP (4716/udp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop Web (4717/tcp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop WebRTC (4718/udp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop Discovery (47777/udp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(NetshPath, 'advfirewall firewall delete rule name="SoundHop mDNS (5353/udp)"', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopRunningSoundHop();
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopRunningSoundHop();
    RegDeleteValue(HKEY_CURRENT_USER, RunKey, RunValueName);
  end
  else if CurUninstallStep = usPostUninstall then
  begin
    EnvRemovePath(ExpandConstant('{app}'));
    CleanupFirewallRules();
  end;
end;
