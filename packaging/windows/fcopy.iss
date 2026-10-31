#ifndef AppVersion
  #error AppVersion must be supplied by build.ps1
#endif
#ifndef PayloadDir
  #error PayloadDir must be supplied by build.ps1
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by build.ps1
#endif

[Setup]
AppId={{8EEC91B4-CA64-4EBC-BCFB-B406FC694692}
AppName=FCOPY
AppVersion={#AppVersion}
AppVerName=FCOPY {#AppVersion} (unsigned)
AppPublisher=DMG Workshop
AppPublisherURL=https://github.com/DMG-Workshop/FCOPY
AppSupportURL=https://github.com/DMG-Workshop/FCOPY/issues
DefaultDirName={localappdata}\Programs\FCOPY
DefaultGroupName=FCOPY
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir={#OutputDir}
OutputBaseFilename=FCOPY-{#AppVersion}-windows-x86_64-unsigned-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
WizardSizePercent=100
InfoBeforeFile={#PayloadDir}\UNSIGNED.txt
UninstallDisplayIcon={app}\fcopy-gui.exe
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
VersionInfoDescription=FCOPY unsigned per-user installer

[Tasks]
Name: explorer; Description: "Add ""Copy with FCOPY"" to Explorer (current user)"; Flags: unchecked
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\fcopy.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\fcopy-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\UNSIGNED.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\FCOPY\FCOPY"; Filename: "{app}\fcopy-gui.exe"
Name: "{userdesktop}\FCOPY"; Filename: "{app}\fcopy-gui.exe"; Tasks: desktopicon

[Code]
procedure ConfigureExplorer(Install: Boolean);
var
  Arguments: String;
  ResultCode: Integer;
begin
  if Install then Arguments := '--install-integration'
  else Arguments := '--uninstall-integration';
  if not Exec(ExpandConstant('{app}\fcopy-gui.exe'), Arguments, '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode) then
    RaiseException('Cannot start FCOPY Explorer integration: ' + SysErrorMessage(ResultCode));
  if ResultCode <> 0 then
    RaiseException('FCOPY Explorer integration failed. Exit code: ' + IntToStr(ResultCode));
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    ConfigureExplorer(WizardIsTaskSelected('explorer'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    ConfigureExplorer(False);
end;
