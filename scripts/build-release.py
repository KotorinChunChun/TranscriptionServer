"""Build program-only Rust server and per-user Windows installer."""
import argparse,hashlib,json,shutil,subprocess,sys,zipfile
from pathlib import Path
from installer_signing import check_installer_signing, sign_installer
ROOT=Path(__file__).resolve().parents[1]
def git(*args):
    return subprocess.run(['git','-c',f'safe.directory={ROOT.as_posix()}',*args],cwd=ROOT,text=True,capture_output=True,check=True).stdout.strip()
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--iscc');parser.add_argument('--candidate-dir',type=Path);parser.add_argument('--program-only',action='store_true');a=parser.parse_args()
    if a.program_only and not a.candidate_dir:parser.error('--program-only requires --candidate-dir')
    if not a.program_only and not a.iscc:parser.error('--iscc is required for an installer')
    if git('status','--porcelain'):raise SystemExit('Commit reviewed source before building release assets')
    if not a.program_only:check_installer_signing()
    version=json.loads((ROOT/'package.json').read_text(encoding='utf-8-sig'))['version'];commit=git('rev-parse','HEAD')
    if a.candidate_dir:
        output=a.candidate_dir.resolve();base=(ROOT.parent/'runtime').resolve()
        if output==base or not output.is_relative_to(base) or output.exists():raise SystemExit('Candidate output must be a new child under Services/runtime')
    else:
        output=(ROOT/'release'/('v'+version)).resolve();assert output.is_relative_to((ROOT/'release').resolve())
        if any(output.glob('*.zip')) or any(output.glob('*setup*.exe')):raise SystemExit('Published release assets are never overwritten; select a new version or candidate directory')
    output.mkdir(parents=True,exist_ok=True);bundle=output/'TranscriptionServer';bundle.mkdir(exist_ok=True)
    target='x86_64-pc-windows-msvc'
    subprocess.run(['cargo','build','--release','--locked','--target',target,'--config','target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]','-j','4'],cwd=ROOT,check=True)
    core=ROOT/'target'/target/'release'/'TranscriptionServer.exe'
    import pefile
    imports=[entry.dll.decode().lower() for entry in pefile.PE(str(core)).DIRECTORY_ENTRY_IMPORT]
    if any('vcruntime' in name or 'python' in name for name in imports):raise SystemExit('Rust core must not require Python or a separate VC runtime')
    shutil.copy2(core,bundle/'TranscriptionServer.exe')
    ui=bundle/'dist';ui.mkdir(exist_ok=True)
    for name in json.loads((ROOT/'manifests'/'management-assets.json').read_text(encoding='utf-8')):
        shutil.copy2(ROOT/'dist'/name,ui/name)
    for file in ('README.md','launch-windows.cmd'):shutil.copy2(ROOT/file,bundle/file)
    for name in ('start-server.ps1','Configure-LanFirewall.ps1'):shutil.copy2(ROOT/'scripts'/name,bundle/name)
    checks=bundle/'scripts';checks.mkdir(exist_ok=True)
    shutil.copy2(ROOT/'scripts'/'preservation.py',checks/'preservation.py')
    docs=bundle/'docs';docs.mkdir(exist_ok=True)
    for file in (ROOT/'docs').glob('*.md'):
        if file.name!='clients.md':shutil.copy2(file,docs/file.name)
    shutil.copy2(ROOT/'docs'/'openapi.json',docs/'openapi.json')
    (bundle/'BUILD.json').write_text(json.dumps({'version':version,'runtime':'rust','generation':'rust-v1','gitCommit':commit,'sourceDirty':False,
        'distribution':'program-only','releaseStatus':'unreleased-candidate' if a.candidate_dir else 'release','bundledRecognitionRuntimes':False,'bundledModels':False,'bundledMediaTools':False,'pythonRole':'Online-installed Whisper recognition worker only','platform':'windows-x64'},indent=2),encoding='utf-8')
    licenses=bundle/'licenses';licenses.mkdir(exist_ok=True)
    cargo=json.loads(subprocess.run(['cargo','metadata','--format-version','1','--locked'],cwd=ROOT,capture_output=True,text=True,check=True).stdout)
    rust_deps=[]
    for package in cargo['packages']:
        if package['name']=='transcription-server':continue
        rust_deps.append({k:package.get(k) for k in ('name','version','license','repository')});path=Path(package['manifest_path']).parent
        for file in path.iterdir():
            if file.is_file() and file.name.lower().startswith(('license','notice','copyright')):
                target=licenses/'rust'/f"{package['name']}-{package['version']}"/file.name;target.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(file,target)
    (bundle/'rust-dependencies.json').write_text(json.dumps(rust_deps,indent=2),encoding='utf-8')
    if any((bundle/name).exists() for name in ('_internal','TranscriptionUpdater.exe','worker','runtime','runtimes','models')):raise SystemExit('Program-only bundle contains an unexpected runtime or model')
    if not a.program_only:
        subprocess.run([str(Path(a.iscc).resolve()),f'/DAppVersion={version}',f'/DBundleDir={bundle}',f'/DReleaseDir={output}',str(ROOT/'installer'/'TranscriptionServer.iss')],cwd=ROOT,check=True)
        sign_installer(output/f'TranscriptionServer-v{version}-setup-x64.exe')
    suffix=f'-candidate-{commit[:8]}' if a.candidate_dir else ''
    binary=output/f'TranscriptionServer-v{version}{suffix}-windows-x64.zip'
    with zipfile.ZipFile(binary,'w',zipfile.ZIP_DEFLATED,compresslevel=6,strict_timestamps=False) as archive:
        for file in sorted(bundle.rglob('*')):
            if file.is_file():archive.write(file,Path('TranscriptionServer')/file.relative_to(bundle))
    if a.program_only:
        (output/'SHA256SUMS.txt').write_text(f'{hashlib.file_digest(binary.open("rb"),"sha256").hexdigest()}  {binary.name}\n',encoding='utf-8')
        print(json.dumps({'version':version,'commit':commit,'binary':str(binary),'status':'unreleased-candidate'}));return
    subprocess.run([sys.executable,str(ROOT/'scripts'/'package-local.py')],cwd=ROOT,check=True)
    source=output/f'TranscriptionServer-v{version}-source.zip';shutil.copy2(ROOT/'dist'/'downloads'/'transcription-server.zip',source)
    sums=[]
    for file in (binary,source,output/f'TranscriptionServer-v{version}-setup-x64.exe'):
        with file.open('rb') as stream:digest=hashlib.file_digest(stream,'sha256').hexdigest()
        sums.append(f'{digest}  {file.name}')
    (output/'SHA256SUMS.txt').write_text('\n'.join(sums)+'\n',encoding='utf-8')
    print(json.dumps({'version':version,'runtime':'rust','commit':commit,'binary':str(binary),'setup':str(output/f'TranscriptionServer-v{version}-setup-x64.exe'),'source':str(source)}))
if __name__=='__main__':main()
