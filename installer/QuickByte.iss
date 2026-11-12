; QuickByte — installer for the Rust build (Inno Setup 6.3+).
;
; Normally compiled by scripts\build-release.ps1, which builds the release
; binaries, stages them under their shipped names and runs:
;   ISCC.exe /DAppVersion=1.27.1 /DSourceDir=dist\stage installer\QuickByte.iss
;
; THE UPGRADE FROM THE C# BUILD (MIGRATION.md §6) RESTS ON THIS FILE:
;   * AppId is the C# installer's AppId. Inno therefore treats this setup as an
;     upgrade of the installed QuickByte: same folder, same Apps & features entry,
;     same uninstaller (its log is appended to), and the tasks the user picked
;     last time (desktopicon / addtopath) are remembered. NEVER change it.
;   * Per-user, no elevation, like the C# build: %LOCALAPPDATA%\Programs\QuickByte.
;   * Everything the .NET build left beside the executables (assemblies,
;     runtimeconfig/deps files) is deleted, and anything that holds a file open —
;     the app, the windowless scheduler agent, qb — is closed first.
;   * %APPDATA%\QuickByte (downloads, queues, settings, chunks) is never touched.
;   * No .NET runtime check: the Rust executables need nothing installed.

#ifndef AppVersion
  #error AppVersion is not defined. Compile with ISCC /DAppVersion=x.y.z (scripts\build-release.ps1 does this).
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "Output"
#endif

#define AppName "QuickByte"
#define AppExeName "QuickByte.exe"
#define AgentExeName "QuickByte.Agent.exe"
#define CliExeName "qb.exe"
#define AppPublisher "Cyberliberty, Inc."
#define AppUrl "https://quickbyte-app.pages.dev/"

; Set once the extension has a Chrome Web Store listing (see browser\chrome-extension\STORE.md).
; The browser-integration task and registry entries stay inert until then.
;#define ChromeExtensionId "the-id-the-store-gave-you"
#define ChromeWebStoreUpdateUrl "https://clients2.google.com/service/update2/crx"

#if !FileExists(AddBackslash(SourceDir) + AppExeName)
  #error QuickByte.exe not found in SourceDir. Run scripts\build-release.ps1.
#endif
#if !FileExists(AddBackslash(SourceDir) + AgentExeName)
  #error QuickByte.Agent.exe not found in SourceDir: QuickByte would ship without its background scheduler.
#endif
#if !FileExists(AddBackslash(SourceDir) + CliExeName)
  #error qb.exe not found in SourceDir.
#endif

[Setup]
; The C# installer's AppId — the whole seamless upgrade depends on it.
AppId={{E15CA2A4-BDCB-49FB-BEE6-810D219CA5BD}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup
VersionInfoCompany={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
UsePreviousAppDir=yes
UsePreviousTasks=yes
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-Setup
SetupIconFile=..\assets\quickbyte.ico
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; Restart Manager closes whatever holds a file being replaced. "force" because the
; scheduler agent has no window to answer a polite request; PrepareToInstall below
; also ends the three executables explicitly when they run from the install folder.
CloseApplications=force
CloseApplicationsFilter=*.exe,*.dll
RestartApplications=no
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add qb (the QuickByte command line) to PATH"; GroupDescription: "Command line:"
#ifdef ChromeExtensionId
Name: "browserintegration"; Description: "Offer the QuickByte extension in Chrome and Edge"; GroupDescription: "Browser integration:"
#endif

[InstallDelete]
; The .NET build's files. Only the three native executables replace them.
Type: files; Name: "{app}\QuickByte.dll"
Type: files; Name: "{app}\QuickByte.Core.dll"
Type: files; Name: "{app}\QuickByte.Agent.dll"
Type: files; Name: "{app}\qb.dll"
Type: files; Name: "{app}\System.Security.Cryptography.ProtectedData.dll"
Type: files; Name: "{app}\*.runtimeconfig.json"
Type: files; Name: "{app}\*.deps.json"
Type: files; Name: "{app}\*.pdb"

[Files]
Source: "{#SourceDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\{#AgentExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\{#CliExeName}"; DestDir: "{app}"; Flags: ignoreversion
#if FileExists("..\..\extensions\chrome\chrome-extension.crx")
Source: "..\..\extensions\chrome\chrome-extension.crx"; DestDir: "{app}\extensions\chrome"; Flags: ignoreversion
#endif

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Comment: "Multi-connection download manager"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Registry]
; Written by the app (never by setup) but removed on uninstall, so removing
; QuickByte cannot leave a Run value pointing at an executable that is gone.
; ValueType none: setup creates nothing here.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "QuickByteScheduler"; Flags: uninsdeletevalue dontcreatekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "QuickByte"; Flags: uninsdeletevalue dontcreatekey
#ifdef ChromeExtensionId
Root: HKA; Subkey: "Software\Google\Chrome\Extensions\{#ChromeExtensionId}"; ValueType: string; ValueName: "update_url"; ValueData: "{#ChromeWebStoreUpdateUrl}"; Flags: uninsdeletekey; Tasks: browserintegration
Root: HKA; Subkey: "Software\Microsoft\Edge\Extensions\{#ChromeExtensionId}"; ValueType: string; ValueName: "update_url"; ValueData: "{#ChromeWebStoreUpdateUrl}"; Flags: uninsdeletekey; Tasks: browserintegration
#endif

[Run]
; Interactive install (by hand, or from the C# 1.26 updater, which starts setup
; with no arguments): offer to launch.
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; Silent install (from the Rust updater): bring QuickByte back if it was running.
Filename: "{app}\{#AppExeName}"; Parameters: "--minimized"; Flags: nowait runasoriginaluser; Check: ShouldRelaunchAfterSilentInstall

[UninstallRun]
; The wake timer task the app registers for scheduled queues.
Filename: "{sys}\schtasks.exe"; Parameters: "/Delete /TN ""QuickByte Queue Wake ({username})"" /F"; Flags: runhidden; RunOnceId: "DeleteWakeTask"

[UninstallDelete]
Type: dirifempty; Name: "{app}\extensions\chrome"
Type: dirifempty; Name: "{app}\extensions"
Type: dirifempty; Name: "{app}"

[Code]
var
  AppWasRunning: Boolean;

{ Ends QuickByte.exe, QuickByte.Agent.exe and qb.exe — but only copies running
  from the install folder; a development build elsewhere is left alone. Returns
  whether the app itself was running. }
function StopRunningCopies(): Boolean;
var
  Script, Folder: String;
  ResultCode: Integer;
begin
  Result := False;
  Folder := ExpandConstant('{app}');
  if not DirExists(Folder) then Exit;
  { A profile folder can contain an apostrophe (C:\Users\O'Brien): double it for
    the single-quoted PowerShell string below. }
  StringChangeEx(Folder, '''', '''''', True);

  { Probe first, so a silent update knows whether to start QuickByte again. }
  Script := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    '$f = ''' + Folder + '\''; ' +
    'if (Get-Process -Name QuickByte -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($f, [StringComparison]::OrdinalIgnoreCase) }) { exit 10 } else { exit 0 }"';
  if Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Script, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := (ResultCode = 10);

  { Ask the app to close (WM_CLOSE to its windows), give it a moment to save its
    lists, then end whatever is left. }
  Script := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    '$f = ''' + Folder + '\''; ' +
    '$p = Get-Process -Name QuickByte,QuickByte.Agent,qb -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($f, [StringComparison]::OrdinalIgnoreCase) }; ' +
    'foreach ($x in $p) { [void]$x.CloseMainWindow() }; ' +
    'if ($p) { Start-Sleep -Milliseconds 2500 }; ' +
    'foreach ($x in $p) { if (-not $x.HasExited) { Stop-Process -Id $x.Id -Force -ErrorAction SilentlyContinue } }"';
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Script, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  AppWasRunning := StopRunningCopies();
  Result := '';
end;

function ShouldRelaunchAfterSilentInstall(): Boolean;
begin
  Result := WizardSilent() and AppWasRunning;
end;

{ ---- PATH (task "addtopath"), per user ---- }

const
  EnvironmentKey = 'Environment';

function PathContains(const Paths, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToUserPath(const Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', Paths) then Paths := '';
  if PathContains(Paths, Dir) or PathContains(Paths, Dir + '\') then Exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then Paths := Paths + ';';
  RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', Paths + Dir);
end;

procedure RemoveFromUserPath(const Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', Paths) then Exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then Exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToUserPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopRunningCopies();
    RemoveFromUserPath(ExpandConstant('{app}'));
  end;
end;
