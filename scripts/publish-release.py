"""Explicit private GitHub publication via existing Git credentials, no secrets saved."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request,urlopen
from installer_signing import verify_installer

ROOT=Path(__file__).resolve().parents[1]

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--repository',required=True,help='owner/name; new private repository only')
    p.add_argument('--notes',type=Path,required=True)
    p.add_argument('--publish',action='store_true',help='create/push/upload and publish after verifying assets')
    p.add_argument('--draft-only',action='store_true',help='stage installer assets without publishing a stable release')
    p.add_argument('--replace-draft-assets',action='store_true',help='replace changed unpublished assets only')
    a=p.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+',a.repository):p.error('owner/name required')
    version=json.loads((ROOT/'package.json').read_text(encoding='utf-8-sig'))['version'];tag='v'+version
    output=ROOT/'release'/tag
    assets=[output/f'TranscriptionServer-v{version}-windows-x64.zip',output/f'TranscriptionServer-v{version}-source.zip',
            output/f'TranscriptionServer-v{version}-setup-x64.exe',output/'SHA256SUMS.txt']
    if any(not f.is_file() for f in assets):raise SystemExit('Build release assets first')
    verify_installer(assets[2])
    for line in assets[-1].read_text().splitlines():
        expected,name=line.split('  ',1)
        if hashlib.file_digest((output/name).open('rb'),'sha256').hexdigest()!=expected:raise SystemExit('Checksum mismatch')
    if not a.publish:
        print(json.dumps({'repository':a.repository,'private':True,'tag':tag,'assets':[f.name for f in assets]}));return
    env=dict(os.environ,GIT_TERMINAL_PROMPT='0',GCM_INTERACTIVE='Never')
    credential=subprocess.run(['git','-c','credential.interactive=never','credential','fill'],input='protocol=https\nhost=github.com\n\n',text=True,capture_output=True,env=env,timeout=20)
    fields=dict(line.split('=',1) for line in credential.stdout.splitlines() if '=' in line)
    token=os.environ.get('GH_TOKEN') or fields.get('password')
    if not token:raise SystemExit('Authenticate GitHub first; credentials were not saved or printed')
    def api(path,method='GET',data=None,binary=False):
        url=path if path.startswith('https://uploads.github.com/') else 'https://api.github.com'+path
        headers={'Authorization':'Bearer '+token,'User-Agent':'TranscriptionServer-release','Accept':'application/vnd.github+json'}
        if data is not None:headers['Content-Type']='application/octet-stream' if binary else 'application/json'
        body=data if binary else json.dumps(data).encode() if data is not None else None
        with urlopen(Request(url,data=body,method=method,headers=headers),timeout=180) as response:
            raw=response.read();return json.loads(raw) if raw else None
    owner,name=a.repository.split('/')
    if api('/user')['login'].lower()!=owner.lower():raise SystemExit('Authenticated owner differs from requested owner')
    def git(*args):
        return subprocess.run(['git','-c',f'safe.directory={ROOT.as_posix()}',*args],cwd=ROOT,env=env,check=True,capture_output=True,text=True).stdout.strip()
    if git('status','--porcelain'):raise SystemExit('Commit reviewed source before publication')
    marker=ROOT/'runtime'/'github-repository.json'
    try:
        repo=api('/repos/'+a.repository)
        if not marker.exists() or json.loads(marker.read_text())['id']!=repo['id']:raise SystemExit('Repository already exists and is not owned by this release workflow')
    except HTTPError as e:
        if e.code!=404:raise
        repo=api('/user/repos','POST',{'name':name,'private':True,'description':'Shared transcription server with bounded Whisper reuse and independent job lanes'})
        marker.parent.mkdir(exist_ok=True);marker.write_text(json.dumps({'id':repo['id'],'url':repo['html_url']}),encoding='utf-8')
    if not repo['private']:raise SystemExit('Refusing publication to a public repository')
    remote='https://github.com/'+a.repository+'.git'
    existing=subprocess.run(['git','-c',f'safe.directory={ROOT.as_posix()}','remote','get-url','origin'],cwd=ROOT,capture_output=True,text=True)
    if existing.returncode:git('remote','add','origin',remote)
    elif existing.stdout.strip()!=remote:raise SystemExit('Existing remote does not match')
    if not a.draft_only:
        try:git('rev-parse','--verify','refs/tags/'+tag)
        except subprocess.CalledProcessError:git('tag','-a',tag,'-m','TranscriptionServer '+tag)
        if git('rev-list','-n','1',tag)!=git('rev-parse','HEAD'):raise SystemExit('Tag is not the reviewed HEAD')
    git('push','--set-upstream','origin','main')
    if not a.draft_only:git('push','origin',tag)
    releases=api('/repos/'+a.repository+'/releases')
    draft_marker=ROOT/'runtime'/f'github-draft-{version}.json'
    staged=json.loads(draft_marker.read_text()) if draft_marker.exists() else {}
    release=next((r for r in releases if r['tag_name']==tag or r['id']==staged.get('id')),None)
    if release and not release['draft']:raise SystemExit('Published release exists; refusing to overwrite')
    if release is None:release=api('/repos/'+a.repository+'/releases','POST',{'tag_name':tag+'-candidate' if a.draft_only else tag,
        'target_commitish':git('rev-parse','HEAD'),'name':'TranscriptionServer '+tag,'draft':True,'body':a.notes.read_text(encoding='utf-8-sig')})
    rid=release['id']
    draft_marker.write_text(json.dumps({'id':rid,'version':version}),encoding='utf-8')
    for file in assets:
        digest='sha256:'+hashlib.file_digest(file.open('rb'),'sha256').hexdigest()
        listed=api('/repos/'+a.repository+f'/releases/{rid}/assets')
        prior=next((v for v in listed if v['name']==file.name),None)
        if prior:
            if prior.get('digest')==digest:continue
            if not a.replace_draft_assets:raise SystemExit('Draft asset differs; refusing to overwrite without explicit flag')
            api('/repos/'+a.repository+f"/releases/assets/{prior['id']}",'DELETE')
        item=api(f'https://uploads.github.com/repos/{a.repository}/releases/{rid}/assets?name={quote(file.name)}','POST',file.read_bytes(),True)
        if item['size']!=file.stat().st_size or item.get('digest')!=digest:raise SystemExit('Uploaded asset verification failed')
    if a.draft_only:
        print(json.dumps({'repository':repo['html_url'],'draftId':rid,'draft':True,'tag':release['tag_name']}));return
    published=api('/repos/'+a.repository+f'/releases/{rid}','PATCH',{'draft':False,'make_latest':'true','tag_name':tag,'target_commitish':git('rev-parse','HEAD'),'body':a.notes.read_text(encoding='utf-8-sig')})
    print(json.dumps({'repository':repo['html_url'],'private':repo['private'],'release':published['html_url'],'tag':tag,'assets':[f.name for f in assets]}))

if __name__=='__main__':main()
