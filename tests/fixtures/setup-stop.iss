#ifndef ReleaseDir
  #error ReleaseDir required
#endif
[Setup]
AppId=TranscriptionServer-Isolated-Stop-Regression
AppName=TranscriptionServer 停止待ち検証
AppVersion=0.1.15
DefaultDirName={tmp}\transcription-stop-regression
CreateAppDir=no
Uninstallable=no
PrivilegesRequired=lowest
OutputDir={#ReleaseDir}
OutputBaseFilename=stop-regression
SetupLogging=yes
WizardStyle=modern
ShowLanguageDialog=no
[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
[Code]
#include "..\..\installer\SetupControl.iss"
function PrepareToInstall(var NeedsRestart: Boolean): String;
var Progress: TOutputProgressWizardPage; Value, Work: String;
begin
  Work := ExpandConstant('{param:WorkDir}');
  StopReport := Work + '\report.json';
  StopCancel := Work + '\report.cancel';
  Progress := CreateOutputProgressPage('停止待ち検証', '専用ヘルパーだけを実行しています。');
  Progress.Show; WaitingForServer := True;
  try
    Value := WaitForStopHelper(ExpandConstant('{param:FixtureHelper}'),
      '"' + StopReport + '" ' + ExpandConstant('{param:Mode}'), Progress);
    if Value = '' then Value := 'SUCCESS';
    SaveStringToFile(Work + '\result.txt', Utf8Encode(Value), False);
  finally
    WaitingForServer := False; Progress.Hide;
  end;
  Result := '';
end;
