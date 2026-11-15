#Requires -Version 7.4
[CmdletBinding()]
param(
  [string]$BindAddress='0.0.0.0',
  [ValidateRange(1,65535)][int]$Port=8769,
  [string]$DataDirectory=(Join-Path $env:LOCALAPPDATA 'TranscriptionServer/data'),
  [switch]$Server,
  [switch]$Lan,
  [switch]$Local,
  [string[]]$AllowedNetwork=@(),
  [string[]]$AllowedOrigin=@(),
  [string]$Ffmpeg,
  [string]$Ffprobe,
  [string]$Speech
)
$ErrorActionPreference='Stop'
$exe=Join-Path $PSScriptRoot 'TranscriptionServer.exe'
if(!(Test-Path -LiteralPath $exe)){throw '展開した配布フォルダーのstart-server.ps1を実行してください。'}
if($Server -and !$env:MEETING_SERVER_API_KEY){throw 'サーバーモードではMEETING_SERVER_API_KEYを環境変数へ設定してください。'}
if($Server -and $Lan){throw 'LANモードとBearerモードは同時に指定できません。'}
if($Local -and ($Server -or $Lan)){throw '同PC限定とLAN/Bearerは同時に指定できません。'}
if(!$Local -and !$Server){$Lan=$true}
if($Local -and !$PSBoundParameters.ContainsKey('BindAddress')){$BindAddress='127.0.0.1'}
if($Lan -and !$PSBoundParameters.ContainsKey('BindAddress')){$BindAddress='0.0.0.0'}
$launchArgs=@('--host',$BindAddress,'--port',"$Port",'--data-dir',$DataDirectory,'--no-browser')
if($Server){$launchArgs+='--server'}
if($Lan){$launchArgs+='--lan'}
if($Local){$launchArgs+='--local'}
foreach($network in $AllowedNetwork){$launchArgs+=@('--allow-network',$network)}
foreach($origin in $AllowedOrigin){$launchArgs+=@('--allow-origin',$origin)}
if($Ffmpeg){$launchArgs+=@('--ffmpeg',$Ffmpeg)}
if($Ffprobe){$launchArgs+=@('--ffprobe',$Ffprobe)}
if($Speech){$launchArgs+=@('--speech',$Speech)}
& $exe @launchArgs
exit $LASTEXITCODE
