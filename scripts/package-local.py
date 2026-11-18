"""Source-only Rust/Whisper package. No media, credentials, models or local registry."""
from pathlib import Path
import json,zipfile
root=Path(__file__).resolve().parents[1];output=root/'dist'/'downloads'/'transcription-server.zip';output.parent.mkdir(parents=True,exist_ok=True)
files=[root/name for name in ('Cargo.toml','Cargo.lock','build.rs','README.md','package.json','requirements-build.txt','requirements-whisper.txt','setup-windows.ps1','setup-whisper.ps1','setup-nemo.ps1','launch-windows.cmd')]
for directory in ('src','workers','scripts','installer','docs','tests','manifests'):
    files+=sorted(path for path in (root/directory).rglob('*') if path.is_file() and path.suffix in {'.rs','.py','.ps1','.mjs','.json','.iss','.md','.cs','.ico'}
                  and '__pycache__' not in path.parts and path.name not in {'clients.json','clients.md'})
files+=[root/'dist'/name for name in json.loads((root/'manifests'/'management-assets.json').read_text(encoding='utf-8'))]
with zipfile.ZipFile(output,'w',zipfile.ZIP_DEFLATED,compresslevel=9,strict_timestamps=False) as archive:
    for file in dict.fromkeys(files):archive.write(file,Path('transcription-server')/file.relative_to(root))
print(f'{output}: {output.stat().st_size:,} bytes, {len(files)} source files')
