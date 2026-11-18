#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef BundleDir
  #error BundleDir is required
#endif
#ifndef ReleaseDir
  #error ReleaseDir is required
#endif

[Setup]
#ifdef AcceptanceBuild
AppId={{1F122A77-AD76-4C10-B041-E010B942A412}
AppName=TranscriptionServer 検証候補
DefaultGroupName=TranscriptionServer 検証候補
AppMutex=Local\TranscriptionServer.AcceptanceSetup
DefaultDirName={localappdata}\Programs\TranscriptionServer-Acceptance
OutputBaseFilename=TranscriptionServer-v{#AppVersion}-candidate-setup-x64
#else
AppId={{6C0A7E36-24C5-4D67-8D18-0677E953741B}
AppName=TranscriptionServer
DefaultGroupName=TranscriptionServer
; PrepareToInstall performs an owned normal shutdown instead of a blind mutex prompt.
DefaultDirName={localappdata}\Programs\TranscriptionServer
OutputBaseFilename=TranscriptionServer-v{#AppVersion}-setup-x64
#endif
AppVersion={#AppVersion}
AppPublisher=KotorinChunChun
AppPublisherURL=https://github.com/KotorinChunChun/TranscriptionServer
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#ReleaseDir}
Compression=lzma2/fast
SetupIconFile=TranscriptionServer.ico
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no
CloseApplications=no
RestartApplications=no
UninstallDisplayIcon={app}\TranscriptionServer.exe
SetupLogging=yes
VersionInfoVersion={#AppVersion}

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "デスクトップにサーバー起動ショートカットを作成"

[Files]
Source: "{#BundleDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "INSTALLATION.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BundleDir}\TranscriptionServer.exe"; DestName: "TranscriptionSetupControl.exe"; Flags: dontcopy

[Icons]
#ifndef AcceptanceBuild
Name: "{group}\TranscriptionServer"; Filename: "{app}\TranscriptionServer.exe"; Parameters: "--installed --open"; WorkingDir: "{app}"; IconFilename: "{app}\TranscriptionServer.exe"
Name: "{group}\クライアント接続方法"; Filename: "{app}\TranscriptionServer.exe"; Parameters: "--installed --connection-guide"; WorkingDir: "{app}"; IconFilename: "{app}\TranscriptionServer.exe"
Name: "{autodesktop}\TranscriptionServer"; Filename: "{app}\TranscriptionServer.exe"; Parameters: "--installed --open"; WorkingDir: "{app}"; IconFilename: "{app}\TranscriptionServer.exe"; Tasks: desktopicon
#endif

[Run]
#ifndef AcceptanceBuild
Filename: "{app}\TranscriptionServer.exe"; Parameters: "--installed --connection-guide"; Description: "サーバーを起動してクライアント接続方法を表示"; Flags: nowait postinstall skipifsilent
#endif

[InstallDelete]
; Remove only the retired application-owned transcription UI files.
Type: files; Name: "{app}\dist\app.js"
Type: files; Name: "{app}\dist\browser-transcriber.mjs"
Type: files; Name: "{app}\dist\connection-profiles.mjs"
Type: files; Name: "{app}\dist\diarization-worker.js"
Type: files; Name: "{app}\dist\engine-catalog.mjs"
Type: files; Name: "{app}\dist\index.html"
Type: files; Name: "{app}\dist\job-timing.mjs"
Type: files; Name: "{app}\dist\local-client.mjs"
Type: files; Name: "{app}\dist\operations.mjs"
Type: files; Name: "{app}\dist\speaker-edits.mjs"
Type: files; Name: "{app}\dist\speaker-utils.mjs"
Type: files; Name: "{app}\dist\styles.css"
Type: files; Name: "{app}\dist\transcript-navigation.mjs"
Type: files; Name: "{app}\dist\transcript-utils.mjs"
Type: files; Name: "{app}\dist\updates.mjs"
Type: files; Name: "{app}\dist\vendor\LICENSE-jinja.txt"
Type: files; Name: "{app}\dist\vendor\LICENSE-onnxruntime.txt"
Type: files; Name: "{app}\dist\vendor\LICENSE-tokenizers.txt"
Type: files; Name: "{app}\dist\vendor\LICENSE-transformers.txt"
Type: files; Name: "{app}\dist\vendor\THIRD-PARTY-NOTICES-onnxruntime.txt"
Type: files; Name: "{app}\dist\vendor\THIRD-PARTY-NOTICES.md"
Type: files; Name: "{app}\dist\vendor\nemotron-build-package-lock.json"
Type: files; Name: "{app}\dist\vendor\nemotron-transformers-4993a4d3.js"
Type: files; Name: "{app}\dist\whisper-models.mjs"
Type: files; Name: "{app}\dist\worker.js"
Type: filesandordirs; Name: "{app}\_internal"; Check: IsLegacyPython
Type: files; Name: "{app}\TranscriptionUpdater.exe"; Check: IsLegacyPython

; No data/config/runtime directories are removed on uninstall.

[Code]
#include "SetupControl.iss"
var
  InstallFence: THandle;

function CreateMutex(lpMutexAttributes: Integer; bInitialOwner: Boolean; lpName: String): THandle;
  external 'CreateMutexW@kernel32.dll stdcall';
function PrepareToInstall(var NeedsRestart: Boolean): String;
var Progress: TOutputProgressWizardPage;
begin
  Result := '';
#ifndef AcceptanceBuild
  if InstallFence <> 0 then Exit;
  ExtractTemporaryFile('TranscriptionSetupControl.exe');
  StopReport := ExpandConstant('{tmp}\server-stop.json');
  StopCancel := ChangeFileExt(StopReport, '.cancel');
  DeleteFile(StopReport); DeleteFile(StopCancel);
  DeleteFile(ChangeFileExt(StopReport, '.txt'));
  Progress := CreateOutputProgressPage('サーバーの更新準備', '受付済み処理の完了と正常終了を待っています。強制終了は行いません。');
  Progress.Show;
  WaitingForServer := True;
  try
    Result := WaitForStopHelper(ExpandConstant('{tmp}\TranscriptionSetupControl.exe'),
      '__prepare-setup "' + ExpandConstant('{app}') + '" "' + StopReport + '"', Progress);
    if Result <> '' then Exit;
    { Fence normal installed starts during file replacement. }
    if CheckForMutexes('Local\TranscriptionServer.Installed') then begin
      Result := '別のサーバーが起動しました。更新を再実行してください。'; Exit;
    end;
    InstallFence := CreateMutex(0, False, 'Local\TranscriptionServer.Installed');
    if (InstallFence <> 0) and (DLLGetLastError = 183) then begin
      CloseHandle(InstallFence); InstallFence := 0;
      Result := '別のサーバーが起動しました。更新を再実行してください。';
    end else if InstallFence = 0 then Result := '更新中の起動抑止を確保できません。';
  finally
    WaitingForServer := False; Progress.Hide;
  end;
#endif
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and (InstallFence <> 0) then begin
    CloseHandle(InstallFence); InstallFence := 0;
  end;
end;

procedure DeinitializeSetup();
begin
  if InstallFence <> 0 then CloseHandle(InstallFence);
end;

function IsLegacyPython(): Boolean;
var Build: AnsiString;
begin
  Result := False;
  if LoadStringFromFile(ExpandConstant('{app}\BUILD.json'), Build) then
    Result := (Pos('"runtime": "rust"', String(Build)) = 0) and
      ((Pos('"version": "4.1.0"', String(Build)) > 0) or (Pos('"version": "4.2.0"', String(Build)) > 0));
end;

function VersionPart(var Value: String): Integer;
var P: Integer; Part: String;
begin
  P := Pos('.', Value);
  if P = 0 then begin Part := Value; Value := ''; end
  else begin Part := Copy(Value, 1, P-1); Delete(Value, 1, P); end;
  Result := StrToIntDef(Part, 0);
end;

function InitializeSetup(): Boolean;
var Current, Incoming: String; I, A, B: Integer;
begin
  Result := True;
#ifdef AcceptanceBuild
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{1F122A77-AD76-4C10-B041-E010B942A412}_is1', 'DisplayVersion', Current) then begin
#else
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{6C0A7E36-24C5-4D67-8D18-0677E953741B}_is1', 'DisplayVersion', Current) then begin
#endif
    Incoming := '{#AppVersion}';
    { v0.1.0 is the explicitly authorized Rust generation, replacing Python v4. }
    if ((Incoming = '0.1.0') or (Incoming = '0.2.0')) and ((Current = '4.1.0') or (Current = '4.2.0')) then Exit;
    for I := 1 to 4 do begin
      A := VersionPart(Current); B := VersionPart(Incoming);
      if A > B then begin
        SuppressibleMsgBox('新しい版がインストールされています。旧版による上書きはできません。', mbError, MB_OK, IDOK);
        Result := False; Exit;
      end;
      if A < B then Exit;
    end;
  end;
end;
