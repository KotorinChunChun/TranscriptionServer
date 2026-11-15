"""Black-box Rust/Whisper acceptance with explicitly supplied synthetic audio.

This is a development test tool, never a server runtime dependency.
"""
import argparse,json,os,secrets,socket,subprocess,time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request,urlopen

def main():
    p=argparse.ArgumentParser()
    for name in ('exe','audio','data-dir','ffmpeg','ffprobe'):p.add_argument('--'+name,required=True)
    p.add_argument('--speech');p.add_argument('--port',type=int);p.add_argument('--expected-version',default=json.loads((Path(__file__).resolve().parents[1]/'package.json').read_text())['version']);a=p.parse_args();work=Path(a.data_dir).resolve()
    if work.exists():raise SystemExit('Use a fresh test-only folder')
    work.mkdir(parents=True);audio=Path(a.audio).read_bytes()
    with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    port=a.port or port
    url=f'http://127.0.0.1:{port}';exe=Path(a.exe).resolve()
    args=[str(exe),'--local','--host','127.0.0.1','--port',str(port),'--no-browser','--no-auto-runtimes','--data-dir',str(work/'data'),'--ffmpeg',a.ffmpeg,'--ffprobe',a.ffprobe,'--threads','2','--process-record',str(work/'process.json')]
    if a.speech:args+=['--speech',str(Path(a.speech).resolve())]
    with (work/'out.log').open('wb') as out,(work/'err.log').open('wb') as err:
        child=subprocess.Popen(args,cwd=exe.parent,stdout=out,stderr=err,stdin=subprocess.DEVNULL,creationflags=0x08000200)
        try:
            def request(path,data=None,headers=None,method=None):
                with urlopen(Request(url+path,data=data,headers=headers or {},method=method),timeout=60) as response:return json.load(response)
            for _ in range(160):
                if child.poll() is not None:raise RuntimeError('Rust server exited; see err.log')
                try:health=request('/api/health');break
                except Exception:time.sleep(.2)
            else:raise RuntimeError('startup timeout')
            assert health['runtime']=='rust' and health['serverVersion']==a.expected_version and health['authMode']=='local'
            token={'X-Local-Token':health['token'],'Content-Type':'application/octet-stream'}
            def manage(action,model='whisper/tiny'):
                accepted=request('/api/management',json.dumps({'action':action,'model':model,'lane':0,'device':'cpu'}).encode(),{'X-Local-Token':health['token'],'Content-Type':'application/json'})
                for _ in range(1800):
                    state=request('/api/management')
                    if state['operation'].get('id')==accepted['id'] and state['operation']['state']!='running' and not state['busy']:
                        assert state['operation']['state']=='complete',state['operation'];return state
                    time.sleep(.2)
                raise RuntimeError('model management timed out')
            manage('install');loaded=manage('load');assert any(m['id']=='whisper/tiny' and m['pinned'] for m in loaded['loaded'])
            models=request('/v1/models');assert any(m['id']=='whisper/tiny' for m in models['data'])
            try:request('/api/jobs?name=synthetic.wav&engine=whisper&model=tiny&device=cpu&diarize=false',b'');raise AssertionError('missing token accepted')
            except HTTPError as e:assert e.code==403
            def submit(lane='file',diarize=False):
                return request(f'/api/jobs?name=synthetic.wav&engine=whisper&model=tiny&device=cpu&diarize={str(diarize).lower()}&clientId=rust-acceptance&workload={lane}',audio,token)['job']['id']
            def wait(id):
                for _ in range(1200):
                    job=request('/api/jobs/'+id)['job']
                    if job['status'] not in {'uploading','queued','running'}:
                        assert job['status']=='complete',job.get('error');return job
                    time.sleep(.1)
                raise RuntimeError('recognition timeout')
            first_id=submit();first=wait(first_id);assert first['segmentCount']>0 and first['metadata']['modelReused'] is True
            second=wait(submit());assert second['metadata']['modelReused'] is True
            assert second['metadata']['modelLoadSeconds']<first['metadata']['modelLoadSeconds']+.01
            realtime=wait(submit('realtime'));assert realtime['workload']=='realtime'
            segments=request('/api/jobs/'+first_id+'/segments')['segments'];segment=segments[0];original=(segment['start'],segment['end'])
            edits=json.dumps({'edits':[{'id':segment['id'],'text':'Rust編集の保持確認','speaker':'検証話者'}]}).encode()
            request('/api/jobs/'+first_id+'/segments',edits,{'X-Local-Token':health['token'],'Content-Type':'application/json'},'PATCH')
            changed=request('/api/jobs/'+first_id+'/segments')['segments'][0]
            assert changed['text']=='Rust編集の保持確認' and (changed['start'],changed['end'])==original
            before=request('/api/jobs/'+first_id+'/segments')
            try:
                request('/api/jobs/'+first_id+'/segments',json.dumps({'edits':[{'id':segment['id'],'text':'must-not-apply'},{'id':999999,'text':'absent'}]}).encode(),{'X-Local-Token':health['token'],'Content-Type':'application/json'},'PATCH');raise AssertionError('partial edit accepted')
            except HTTPError as e:assert e.code==404
            assert request('/api/jobs/'+first_id+'/segments')==before
            with urlopen(Request(url+'/api/jobs/'+first_id+'/media',headers={'Range':'bytes=0-7'}),timeout=10) as response:
                assert response.status==206 and response.read()==audio[:8]
            pending=[submit() for _ in range(4)]
            for id in pending:request('/api/jobs/'+id+'/cancel',b'{}',{'X-Local-Token':health['token'],'Content-Type':'application/json'})
            for id in pending:assert request('/api/jobs/'+id)['job']['status'] in {'cancelled','complete'}
            # A discarded session after cancellation must recover on the next job.
            recovered=wait(submit());assert recovered['segmentCount']>0
            page=request('/api/jobs?limit=2&offset=0&clientId=rust-acceptance&workload=file');assert len(page['jobs'])==2 and page['total']>=3
            request('/api/jobs/'+first_id+'/remove-media',b'{"confirm":"remove-media"}',{'X-Local-Token':health['token'],'Content-Type':'application/json'})
            assert request('/api/jobs/'+first_id+'/segments')['segments']==before['segments']
            diar=None
            if a.speech:
                diar=wait(submit(diarize=True));assert diar['metadata']['diarizationStatus'] in {'complete','no_speakers'}
                assert diar['metadata']['diarizationModel']=='nemotron-3-diarization'
                native_loaded=manage('load','haas/haas-ja-v1.0');assert any(m['id']=='haas/haas-ja-v1.0' for m in native_loaded['loaded'])
                native_id=request('/api/jobs?name=synthetic.wav&engine=haas&model=haas-ja-v1.0&device=cpu&diarize=false',audio,token)['job']['id']
                native=wait(native_id);assert native['segmentCount']>0 and native['metadata']['modelReused'] is True
                manage('unload','haas/haas-ja-v1.0')
                manage('load','haas/haas-ja-v1.0')
            boundary='rust-'+secrets.token_hex(10)
            body=(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="synthetic.wav"\r\nContent-Type: audio/wav\r\n\r\n'.encode()+audio+
                  f'\r\n--{boundary}\r\nContent-Disposition: form-data; name="model"\r\n\r\nwhisper/tiny\r\n--{boundary}\r\nContent-Disposition: form-data; name="device"\r\n\r\ncpu\r\n--{boundary}\r\nContent-Disposition: form-data; name="response_format"\r\n\r\nverbose_json\r\n--{boundary}--\r\n'.encode())
            payload=request('/v1/audio/transcriptions',body,{'Authorization':'Bearer '+health['token'],'Content-Type':'multipart/form-data; boundary='+boundary})
            assert payload['segments'] and payload['language']=='japanese'
            if a.speech:assert not any(m['id']=='haas/haas-ja-v1.0' for m in request('/api/management')['loaded'])
            report={'rust':True,'version':a.expected_version,'pythonOnlyWhisperWorker':True,'realWhisperCpu':True,'reuse':True,'firstLoad':first['metadata']['modelLoadSeconds'],
                    'secondLoad':second['metadata']['modelLoadSeconds'],'realtime':True,'cancelRecovery':True,'atomicEdit':True,'timingPreserved':True,'range':True,'paging':True,'removeMediaKeepsSubtitles':True,'multipart':True,'diarization':diar['metadata']['diarizationStatus'] if diar else 'not requested'}
            request('/api/admin/shutdown',b'{}',{'X-Local-Token':health['token'],'Content-Type':'application/json'})
            assert child.wait(timeout=12)==0
            report['gracefulShutdown']=True
            report['modelManagement']={'whisperInstallAndLoad':True,'preloadedWhisperReused':True,'nativeLoadReuseAndUnload':bool(a.speech)}
            (work/'result.json').write_text(json.dumps(report,indent=2),encoding='utf-8');print(json.dumps(report),flush=True)
        finally:
            if child.poll() is None:
                subprocess.run(['taskkill','/PID',str(child.pid),'/T','/F'],capture_output=True,timeout=15);child.wait(timeout=15)

if __name__=='__main__':main()
