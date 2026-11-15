#Requires -Version 7.4
# Pinned native runtime only; no PATH/driver/Python changes or model downloads.
$ErrorActionPreference='Stop'
$version='0.3.0'
$name="nemo-speech-$version-windows-x86_64-vulkan.zip"
$expected='72b3b8b41bfe2be0320eff4e9d310594e4f99deefcde4a157799a039656963ce'
$downloads=Join-Path $PSScriptRoot 'runtime\downloads'
$target=Join-Path $PSScriptRoot 'runtime\nemo-speech'
New-Item -ItemType Directory -Force -Path $downloads | Out-Null
$zip=Join-Path $downloads $name
if(!(Test-Path -LiteralPath $zip)){Invoke-WebRequest "https://github.com/NVIDIA/NeMo-Speech.cpp/releases/download/v$version/$name" -OutFile $zip -TimeoutSec 300}
if((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected){throw 'NeMo runtime SHA256 mismatch; the archive was not executed.'}
$stage=Join-Path $downloads ("nemo-extracted-"+[guid]::NewGuid().ToString('N'))
Expand-Archive -LiteralPath $zip -DestinationPath $stage
$exe=Get-ChildItem -LiteralPath $stage -Recurse -Filter nemo-speech.exe | Select-Object -First 1
if(!$exe){throw 'The pinned runtime archive contains no nemo-speech.exe.'}
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path (Join-Path $exe.Directory.FullName '*') -Destination $target -Recurse -Force
$archiveRoot=$exe.Directory.Parent.FullName
foreach($path in @('LICENSE','NOTICE','THIRD_PARTY_NOTICES.md','licenses','share')){
    $source=Join-Path $archiveRoot $path
    if(Test-Path -LiteralPath $source){Copy-Item -LiteralPath $source -Destination $target -Recurse -Force}
}
$result=& (Join-Path $target 'nemo-speech.exe') --version
if($LASTEXITCODE -ne 0 -or "$result" -notmatch '^nemo-speech 0\.3\.0$'){throw 'The pinned NeMo runtime could not start.'}
$index=Join-Path $target 'share\nemo-speech\model-index.json'
if(!(Test-Path -LiteralPath $index)){throw 'The NeMo runtime model index is missing.'}
@{version=$version;archiveSha256=$expected;backend='vulkan';executableSha256=(Get-FileHash (Join-Path $target 'nemo-speech.exe')).Hash.ToLowerInvariant()} | ConvertTo-Json | Set-Content (Join-Path $target 'NEMO_RUNTIME.json')
Write-Output 'NeMo-Speech.cpp 0.3.0: CPU/Vulkan runtime prepared. Model weights are installed separately from the management page.'
