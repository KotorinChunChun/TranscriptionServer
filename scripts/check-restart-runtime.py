"""Owned loopback fixture for crash/replay/drain acceptance; never uses real models.

init/start/kill/release/block/stop/scenario operate only on the explicitly created
fixture directory. The process image and Windows creation time must match before
force termination. Artifacts are retained for inspection, never auto-cleaned.
"""
import argparse, ctypes, hashlib, io, json, os, shutil, socket, sqlite3, subprocess, time, wave
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request,urlopen

def save(path,value): path.write_text(json.dumps(value,indent=2,ensure_ascii=False),encoding='utf-8')
def read(path): return json.loads(path.read_text(encoding='utf-8-sig'))
def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()
def identity(pid):
    from ctypes import wintypes
    kernel=ctypes.WinDLL('kernel32',use_last_error=True)
    kernel.OpenProcess.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.DWORD];kernel.OpenProcess.restype=wintypes.HANDLE
    kernel.CloseHandle.argtypes=[wintypes.HANDLE]
    kernel.GetProcessTimes.argtypes=[wintypes.HANDLE,*([ctypes.POINTER(wintypes.FILETIME)]*4)]
    kernel.QueryFullProcessImageNameW.argtypes=[wintypes.HANDLE,wintypes.DWORD,wintypes.LPWSTR,ctypes.POINTER(wintypes.DWORD)]
    kernel.GetExitCodeProcess.argtypes=[wintypes.HANDLE,ctypes.POINTER(wintypes.DWORD)]
    handle=kernel.OpenProcess(0x1000,False,pid)
    if not handle:return None
    try:
        code=wintypes.DWORD();assert kernel.GetExitCodeProcess(handle,ctypes.byref(code))
        if code.value!=259:return None
        values=[wintypes.FILETIME() for _ in range(4)]
        assert kernel.GetProcessTimes(handle,*(ctypes.byref(v) for v in values))
        text=ctypes.create_unicode_buffer(32768);length=wintypes.DWORD(len(text))
        assert kernel.QueryFullProcessImageNameW(handle,0,text,ctypes.byref(length))
        return {'pid':pid,'image':str(Path(text.value).resolve()),'created':(values[0].dwHighDateTime<<32)|values[0].dwLowDateTime}
    finally:kernel.CloseHandle(handle)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('action',choices=['init','start','kill','release','block','stop','scenario','status']);parser.add_argument('--work-dir',type=Path,required=True);a=parser.parse_args()
    work=a.work_dir.resolve();repo=Path(__file__).resolve().parents[1]
    if a.action=='init':
        if work.exists():raise RuntimeError('A fresh fixture directory is required')
        work.mkdir(parents=True);(work/'app').mkdir();(work/'settings').mkdir();(work/'data').mkdir()
        exe=work/'app/TranscriptionServer.exe';shutil.copy2(repo/'target/debug/TranscriptionServer.exe',exe)
        shutil.copytree(repo/'dist',work/'app/dist',ignore=shutil.ignore_patterns('downloads'))
        worker=work/'app/fixture.exe';csc=Path(os.environ['WINDIR'])/'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
        subprocess.run([str(csc),'/nologo','/r:System.Web.Extensions.dll','/out:'+str(worker),str(repo/'tests/fixtures/concurrent-runtime.cs')],check=True,creationflags=0x08000000)
        (work/'app/recovery-mode').write_text('fixture, not real ASR')
        save(work/'app/BUILD.json',{'version':'0.1.15','distribution':'isolated-development-fixture','sourceExeSha256':sha(exe)})
        save(work/'app/INSTALLATION.json',{'scope':'isolated-fixture','installed':False})
        save(work/'settings/settings.json',{'auto_install_runtimes':False,'execution':{'maxJobs':2,'realtimeReserved':1,'maxGpuJobs':1,'ramLimitMiB':8192,'vramLimitMiB':0},'preservationSentinel':'must-survive'})
        with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
        assert port!=8769
        save(work/'fixture.json',{'kind':'owned-crash-replay-fixture','port':port,'settingsSha256':sha(work/'settings/settings.json'),'exeSha256':sha(exe)})
        print(json.dumps({'work':str(work),'port':port}));return
    record=read(work/'fixture.json');assert record['kind']=='owned-crash-replay-fixture'
    exe=work/'app/TranscriptionServer.exe';worker=work/'app/fixture.exe';url='http://127.0.0.1:'+str(record['port'])
    assert sha(exe)==record['exeSha256']
    def request(path,value=None,raw=False):
        headers={}
        if value is not None:
            health=request('/api/health');headers['X-Local-Token']=health['token']
            headers['Content-Type']='audio/wav' if raw else 'application/json'
            value=value if raw else json.dumps(value).encode()
        with urlopen(Request(url+path,data=value,headers=headers),timeout=10) as response:return json.load(response)
    def wait(check,seconds=30):
        deadline=time.monotonic()+seconds
        while time.monotonic()<deadline:
            try:
                result=check()
                if result:return result
            except OSError:pass
            time.sleep(.1)
        raise RuntimeError('Fixture wait timed out')
    def start():
        if (work/'owned.json').exists():
            previous=read(work/'owned.json')
            assert identity(previous['pid'])!=previous,'Fixture already running'
        args=[str(exe),'--instance-id','recovery-fixture','--local','--host','127.0.0.1','--port',str(record['port']),'--settings-dir',str(work/'settings'),'--data-dir',str(work/'data'),'--process-record',str(work/'process.json'),'--no-auto-runtimes','--no-browser','--whisper-worker',str(worker),'--ffmpeg',str(worker),'--ffprobe',str(worker),'--speech',str(work/'missing-speech.exe'),'--nemo-speech',str(work/'missing-nemo.exe')]
        with (work/'stdout.log').open('ab') as out,(work/'stderr.log').open('ab') as err:
            child=subprocess.Popen(args,stdout=out,stderr=err,creationflags=0x08000000)
        owned=identity(child.pid);assert owned and Path(owned['image']).samefile(exe);save(work/'owned.json',owned)
        health=wait(lambda:request('/api/health'))
        assert health['processId']==child.pid and health['instanceId']=='recovery-fixture' and health['serverVersion']=='0.1.15'
        return owned
    def kill():
        owned=read(work/'owned.json');assert identity(owned['pid'])==owned
        assert Path(owned['image']).samefile(exe)
        result=subprocess.run(['taskkill','/PID',str(owned['pid']),'/T','/F'],capture_output=True,creationflags=0x08000000)
        save(work/'termination.json',{'pid':owned['pid'],'exitCode':result.returncode,'stdout':result.stdout.decode(errors='replace'),'stderr':result.stderr.decode(errors='replace')})
        if result.returncode and identity(owned['pid'])==owned:raise RuntimeError('Owned fixture force termination failed; inspect termination.json')
        wait(lambda:identity(owned['pid'])!=owned)
    def stop():
        owned=read(work/'owned.json');assert identity(owned['pid'])==owned
        request('/api/admin/drain',{'action':'shutdown','confirm':'drain-server'})
        wait(lambda:identity(owned['pid'])!=owned)
    if a.action=='start':print(json.dumps({'url':url,**start()}));return
    if a.action=='kill':kill();print('Owned fixture process forcibly terminated');return
    if a.action=='release':(work/'app/release').write_text('release');print('Fixture barrier released');return
    if a.action=='block':(work/'app/release').unlink(missing_ok=True);return
    if a.action=='stop':stop();print('Owned fixture process stopped normally');return
    if a.action=='status':
        health=request('/api/health');health.pop('token',None);print(json.dumps(health));return
    import sys
    sys.path.insert(0,str(repo/'scripts'));import preservation
    preservation.check_required(preservation.requirements(work/'app',work/'settings/settings.json',work/'data')) if (work/'data/workbench.sqlite3').exists() else None
    start();audio=io.BytesIO()
    with wave.open(audio,'wb') as w:w.setnchannels(1);w.setsampwidth(2);w.setframerate(16000);w.writeframes(bytes(32000))
    def submit():return request('/api/jobs?engine=whisper&model=tiny&device=cpu&diarize=false&clientId=restart-check',audio.getvalue(),True)['job']['id']
    def job(id):return request('/api/jobs/'+id)['job']
    def segments(id):return request('/api/jobs/'+id+'/segments')
    (work/'app/release').write_text('release');completed=submit();wait(lambda:job(completed)['status']=='complete')
    complete_before=segments(completed)
    (work/'app/release').unlink();running=submit();wait(lambda:segments(running)['total']==1)
    queued=submit();assert job(queued)['status']=='queued'
    cancelled=submit();request('/api/jobs/'+cancelled+'/cancel',{})
    old=segments(running);assert old['resultGeneration']==0
    kill();start()
    wait(lambda:segments(running)['total']==1)
    assert job(running)['metadata']['resultGeneration']==1 and job(queued)['metadata']['resultGeneration']==1
    assert segments(running)['resultGeneration']==1 and segments(running)['segments'][0]['id']==1
    assert job(cancelled)['status']=='cancelled' and segments(completed)==complete_before
    request('/api/admin/drain',{'action':'shutdown','confirm':'drain-server'})
    try:submit();raise AssertionError('Drain accepted new upload')
    except HTTPError as error:assert error.code==503 and error.headers['Retry-After']=='5'
    owned=read(work/'owned.json');assert identity(owned['pid'])==owned
    (work/'app/release').write_text('release');wait(lambda:identity(owned['pid'])!=owned)
    preservation.check_required(preservation.requirements(work/'app',work/'settings/settings.json',work/'data'))
    db=sqlite3.connect(work/'data/workbench.sqlite3');assert db.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
    for id in (running,queued):
        data=json.loads(db.execute('SELECT payload FROM jobs WHERE id=?',[id]).fetchone()[0]);assert data['status']=='complete'
        rows=[json.loads(r[0]) for r in db.execute('SELECT payload FROM segments WHERE job_id=? ORDER BY id',[id])]
        assert len(rows)==2 and [r['id'] for r in rows]==[1,2] and len({r['text'] for r in rows})==2
    assert db.execute('SELECT count(*) FROM restart_segments WHERE job_id=?',[running]).fetchone()[0]==1
    db.close();assert sha(work/'settings/settings.json')==record['settingsSha256']
    save(work/'result.json',{'sameIdReplay':True,'subtitlesNotDuplicated':True,'generationIncremented':True,'completedAndCancelledPreserved':True,'drainWaitedForAcceptedJobs':True,'newUpload503RetryAfter':True,'settingsAndRequiredFilesPreserved':True,'databaseIntegrity':'ok','jobs':{'running':running,'queued':queued,'completed':completed,'cancelled':cancelled},'realModelUsed':False,'productionTouched':False})
    print((work/'result.json').read_text(encoding='utf-8'))

if __name__=='__main__':main()
