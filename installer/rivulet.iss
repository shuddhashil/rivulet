; Builds dist\RivuletSetup.exe: run  ISCC installer\rivulet.iss  after  cargo build --release
; Installs for the current user only (no admin prompt), into a folder the app can write its settings to.

#define Version "0.4.0"

[Setup]
AppId={{B8CF3FCF-1B0B-45E2-AB31-6F1D58C537C1}
AppName=Rivulet
AppVersion={#Version}
AppPublisher=Rivulet
DefaultDirName={localappdata}\Programs\Rivulet
PrivilegesRequired=lowest
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
WizardStyle=modern
OutputDir=..\dist
OutputBaseFilename=RivuletSetup
UninstallDisplayIcon={app}\rivulet.exe
UninstallDisplayName=Rivulet
Compression=lzma2
SolidCompression=yes

[Tasks]
Name: login; Description: "Start Rivulet by itself every time I sign in"

[Files]
Source: "..\target\release\rivulet.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Rivulet"; Filename: "{app}\rivulet.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\rivulet.exe"; Parameters: "--install-login"; Tasks: login; Flags: runhidden
Filename: "{app}\rivulet.exe"; Description: "Start Rivulet now"; Flags: postinstall nowait

[UninstallRun]
Filename: "schtasks.exe"; Parameters: "/Delete /TN Rivulet /F"; Flags: runhidden; RunOnceId: "login"

[UninstallDelete]
Type: filesandordirs; Name: "{app}"

[Messages]
FinishedLabel=Rivulet is installed.%n%nIf it says "Auto arrange icons" is on: right-click an empty spot on the desktop, choose View, and untick Auto arrange icons. Then start Rivulet again from the Start menu.

[Code]
procedure StopRivulet(Args: String);
var Code: Integer;
begin
  if FileExists(ExpandConstant('{app}\rivulet.exe')) then
    Exec(ExpandConstant('{app}\rivulet.exe'), Args, '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

{ An update replaces the exe, so the running copy has to close first. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopRivulet('--quit');
  Result := '';
end;

{ Put the icons back where they were before Rivulet, when it saved them; otherwise just close it. }
procedure CurUninstallStepChanged(Step: TUninstallStep);
begin
  if Step = usUninstall then
    if FileExists(ExpandConstant('{app}\state\positions.tsv')) then
      StopRivulet('--undo')
    else
      StopRivulet('--quit');
end;
