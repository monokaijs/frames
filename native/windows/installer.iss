; Per-user installation keeps the signed updater able to replace Frames.exe.
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef AppSource
  #define AppSource SourcePath + "\..\..\dist\windows\Frames.exe"
#endif
#ifndef AppIcon
  #define AppIcon SourcePath + "\..\..\assets\icons\Frames.ico"
#endif
#ifndef FramesAppId
  #define FramesAppId "dev.creaton.frames"
#endif
#ifndef ShortcutName
  #define ShortcutName "Frames"
#endif

[Setup]
AppId={#FramesAppId}
AppName=Frames
AppVersion={#AppVersion}
AppPublisher=Anh Nhan Nguyen
AppPublisherURL=https://github.com/monokaijs/frames
AppSupportURL=https://github.com/monokaijs/frames/issues
AppUpdatesURL=https://github.com/monokaijs/frames/releases/latest
DefaultDirName={localappdata}\Programs\Frames
DefaultGroupName=Frames
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
SetupIconFile={#AppIcon}
UninstallDisplayIcon={app}\Frames.exe
UninstallDisplayName=Frames
VersionInfoProductName=Frames
VersionInfoDescription=Frames Setup
OutputDir=..\..\dist\release
OutputBaseFilename=Frames-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
UsePreviousAppDir=yes
SetupLogging=yes

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Messages]
FinishedLabel=Frames is installed. Frames needs FFmpeg for remote media; install FFmpeg separately before starting a session if it is not already available.

[Files]
Source: "{#AppSource}"; DestDir: "{app}"; DestName: "Frames.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#ShortcutName}"; Filename: "{app}\Frames.exe"
Name: "{autodesktop}\{#ShortcutName}"; Filename: "{app}\Frames.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\Frames.exe"; Description: "Open Frames"; Flags: nowait postinstall skipifsilent

[Code]
function InstallationIsWritable: Boolean;
var
  Stream: TFileStream;
  Executable: String;
begin
  Executable := ExpandConstant('{app}\Frames.exe');
  Result := True;
  if FileExists(Executable) then
  begin
    try
      Stream := TFileStream.Create(Executable, fmOpenReadWrite or fmShareExclusive);
      Stream.Free;
    except
      Result := False;
    end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not InstallationIsWritable then
    Result := 'Close all Frames windows and sessions using this installation, then try again. Frames will not stop running sessions.';
end;

function InitializeUninstall: Boolean;
begin
  Result := InstallationIsWritable;
  if not Result and not UninstallSilent then
    MsgBox('Close all Frames windows and sessions before uninstalling Frames.', mbError, MB_OK);
end;
