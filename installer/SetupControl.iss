{ Shared with the isolated Inno regression harness. Exec(ewNoWait) does NOT return a PID. }
var
  WaitingForServer: Boolean;
  StopReport, StopCancel: String;

function CloseHandle(hObject: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';
function OpenProcess(dwDesiredAccess: LongWord; bInheritHandle: Boolean; dwProcessId: LongWord): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(hHandle: THandle; dwMilliseconds: LongWord): LongWord;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function GetExitCodeProcess(hProcess: THandle; var lpExitCode: LongWord): Boolean;
  external 'GetExitCodeProcess@kernel32.dll stdcall';

procedure CancelButtonClick(CurPageID: Integer; var Cancel, Confirm: Boolean);
begin
  if WaitingForServer then begin
    SaveStringToFile(StopCancel, 'cancel', False);
    Cancel := False; Confirm := False;
  end;
end;

function ReadStopReport(): String;
var Raw: AnsiString;
begin
  Result := '';
  if LoadStringFromFile(StopReport, Raw) then begin
    Result := Utf8Decode(Raw);
    StringChangeEx(Result, ' ', '', True);
    StringChangeEx(Result, #9, '', True);
    StringChangeEx(Result, #10, '', True);
    StringChangeEx(Result, #13, '', True);
  end;
end;

function StopHelperPid(const Report: String): Integer;
var Offset: Integer; Digits: String;
begin
  Result := 0;
  Offset := Pos('"helperProcessId":', Report);
  if Offset = 0 then Exit;
  Offset := Offset + Length('"helperProcessId":');
  Digits := '';
  while Offset <= Length(Report) do begin
    if (Report[Offset] < '0') or (Report[Offset] > '9') then Break;
    Digits := Digits + Report[Offset];
    Offset := Offset + 1;
  end;
  Result := StrToIntDef(Digits, 0);
end;

function StopFailure(const Fallback: String): String;
var Lines: TArrayOfString; I: Integer;
begin
  Result := Fallback;
  { LoadStringsFromFile explicitly supports UTF-8; never show the internal JSON. }
  if LoadStringsFromFile(ChangeFileExt(StopReport, '.txt'), Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do
      Result := Result + #13#10 + Lines[I];
end;

function WaitForStopHelper(const Filename, Params: String;
  Progress: TOutputProgressWizardPage): String;
var LaunchCode, HelperPid, StartupPolls: Integer; Process: THandle;
    Report: String; ExitCode, WaitCode: LongWord;
begin
  Result := '';
  Process := 0; StartupPolls := 0; ExitCode := 0;
  try
    if not Exec(Filename, Params, '', SW_HIDE, ewNoWait, LaunchCode) then begin
      Result := '正常停止ヘルパーを開始できません。' + #13#10 + SysErrorMessage(LaunchCode); Exit;
    end;
    repeat
      Report := ReadStopReport();
      if Process = 0 then begin
        HelperPid := StopHelperPid(Report);
        if HelperPid > 0 then Process := OpenProcess($00101000, False, HelperPid);
        if Process = 0 then begin
          if (Pos('"state":"complete"', Report) > 0) or
             (Pos('"state":"error"', Report) > 0) then Break;
          StartupPolls := StartupPolls + 1;
          if StartupPolls >= 50 then begin
            SaveStringToFile(StopCancel, 'cancel', False);
            Result := '停止ヘルパーの起動を確認できません。更新を続行しません。'; Exit;
          end;
        end;
      end;
      if Process <> 0 then begin
        WaitCode := WaitForSingleObject(Process, 0);
        if WaitCode = 0 then Break;
        if WaitCode <> $00000102 then begin
          SaveStringToFile(StopCancel, 'cancel', False);
          Result := '停止ヘルパーの動作を確認できません。更新を続行しません。'; Exit;
        end;
      end;
      Progress.SetProgress(0, 1);
      Sleep(200);
    until False;
    Report := ReadStopReport();
    if FileExists(StopCancel) then begin Result := '更新を取り消しました。'; Exit; end;
    if Process <> 0 then
      if not GetExitCodeProcess(Process, ExitCode) then begin
        Result := '停止ヘルパーの終了コードを確認できません。更新を続行しません。'; Exit;
      end;
    if Pos('"state":"error"', Report) > 0 then begin
      Result := StopFailure('サーバーを正常停止できませんでした。更新を続行しません。'); Exit;
    end;
    if ExitCode <> 0 then begin
      Result := '停止処理が異常終了しました。更新を続行しません。'; Exit;
    end;
    if Pos('"state":"complete"', Report) = 0 then
      Result := '停止完了を確認できません。更新を続行しません。';
  finally
    if Process <> 0 then CloseHandle(Process);
  end;
end;
