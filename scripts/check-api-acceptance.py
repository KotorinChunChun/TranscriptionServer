"""Opt-in isolated HTTPS/SDK/streaming checks. No production or remote API calls.

Invoke using an existing developer Python and a dedicated --sdk-dir installed
with openai[realtime], openapi-spec-validator and jsonschema. TLS paths are passed
to the server/SSL library only; no certificate/key contents are logged or copied.
"""
import argparse, asyncio, hashlib, importlib.metadata, json, os, shutil, socket, ssl, subprocess, sys, time, wave
from pathlib import Path
from urllib.request import Request, urlopen

REPO=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(REPO/'scripts'))
import preservation

def save(path,value):
    path.write_text(json.dumps(value,ensure_ascii=False,indent=2),encoding='utf-8')

def main():
    p=argparse.ArgumentParser()
    for name in ('work-dir','sdk-dir','tls-cert','tls-key','ca-file'):p.add_argument('--'+name,required=True,type=Path)
    p.add_argument('--mode',choices=('fixture','real'),required=True)
    p.add_argument('--python',type=Path)
    p.add_argument('--model-cache',type=Path)
    p.add_argument('--audio',type=Path)
    p.add_argument('--ffmpeg',type=Path)
    p.add_argument('--wait-browser',action='store_true')
    p.add_argument('--contract-only',action='store_true')
    a=p.parse_args();root=a.work_dir.resolve()
    if root.exists():raise SystemExit('Use a fresh test-only directory')
    sys.path.insert(0,str(a.sdk_dir.resolve()))
    import httpx2, openai
    for part in ('app','settings','data','tmp','results'):(root/part).mkdir(parents=True)
    exe=root/'app/TranscriptionServer.exe';shutil.copy2(REPO/'target/debug/TranscriptionServer.exe',exe)
    source_commit=subprocess.check_output(['git','-c','safe.directory='+REPO.as_posix(),'rev-parse','HEAD'],cwd=REPO,text=True).strip()
    save(root/'app/BUILD.json',{'kind':'isolated-acceptance','sourceCommit':source_commit,'executableSha256':preservation.file_hash(exe)})
    save(root/'app/INSTALLATION.json',{'scope':'isolated-development','installed':False})
    if a.mode=='fixture':
        csc=Path(os.environ['WINDIR'])/'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
        fixture=root/'app/fixture.exe'
        subprocess.run([str(csc),'/nologo','/r:System.Web.Extensions.dll','/out:'+str(fixture),str(REPO/'tests/fixtures/streaming-runtime.cs')],check=True,creationflags=0x08000000)
        worker=fixture;ff=fixture;fp=fixture
        with wave.open(str(root/'audio.wav'),'wb') as wav:
            wav.setnchannels(1);wav.setsampwidth(2);wav.setframerate(24000);wav.writeframes(b'\0\0'*24000*2)
        pcm=b'\0\0'*24000*2
    else:
        if not all((a.python,a.model_cache,a.audio,a.ffmpeg)):raise SystemExit('real mode requires python/model-cache/audio/ffmpeg')
        shutil.copytree(a.model_cache.resolve(),root/'settings/models/huggingface/hub'/a.model_cache.name)
        worker=root/'app/whisper.py';shutil.copy2(REPO/'workers/whisper_worker.py',worker)
        ff=a.ffmpeg.resolve();fp=ff.with_name('ffprobe.exe')
        subprocess.run([str(ff),'-v','error','-nostdin','-i',str(a.audio.resolve()),'-t','4','-ac','1','-ar','24000',str(root/'audio.wav')],check=True,creationflags=0x08000000)
        with wave.open(str(root/'audio.wav'),'rb') as wav:pcm=wav.readframes(wav.getnframes())
    with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
    assert port not in (8769,18769,18770)
    url=f'https://127.0.0.1:{port}'
    settings={'auto_install_runtimes':False,'instance_id':'api-acceptance-'+a.mode,'data_dir':str(root/'data'),'threads':2,
              'tls_cert':str(a.tls_cert.resolve()),'tls_key':str(a.tls_key.resolve()),'https_only':True,
              'execution':{'maxJobs':10,'realtimeReserved':2,'maxGpuJobs':2,'ramLimitMiB':0,'vramLimitMiB':0}}
    settings_file=root/'settings/settings.json';save(settings_file,settings);settings_hash=preservation.file_hash(settings_file)
    # Files must exist before launch; DB is required from first successful startup.
    for path in (exe,settings_file,root/'app/BUILD.json',root/'app/INSTALLATION.json'):preservation.file_hash(path)
    context=ssl.create_default_context(cafile=str(a.ca_file.resolve()))
    (root/'audio.pcm').write_bytes(pcm)
    context.minimum_version=ssl.TLSVersion.TLSv1_2
    command=[str(exe),'--instance-id',settings['instance_id'],'--local','--host','127.0.0.1','--port',str(port),'--settings-dir',str(root/'settings'),'--data-dir',str(root/'data'),'--process-record',str(root/'process.json'),'--no-auto-runtimes','--no-browser','--whisper-worker',str(worker),'--ffmpeg',str(ff),'--ffprobe',str(fp),'--speech',str(root/'missing-speech.exe'),'--nemo-speech',str(root/'missing-nemo.exe')]
    if a.python:command+=['--whisper-python',str(a.python.resolve())]
    env=dict(os.environ,TEMP=str(root/'tmp'),TMP=str(root/'tmp'),HF_HUB_OFFLINE='1',HF_HUB_DISABLE_IMPLICIT_TOKEN='1')
    for key in ('OPENAI_API_KEY','TRANSCRIPTION_API_KEY','TRANSCRIPTION_API_MODEL','MEETING_SERVER_API_KEY','TRANSCRIPTION_GITHUB_TOKEN'):env.pop(key,None)
    out=(root/'stdout.log').open('wb');err=(root/'stderr.log').open('wb')
    child=subprocess.Popen(command,env=env,stdout=out,stderr=err,creationflags=0x08000000)
    save(root/'launch.json',{'args':command,'pid':child.pid,'baseUrl':url})
    def request(path,body=None):
        headers={}
        if body is not None:headers={'X-Local-Token':token,'Content-Type':'application/json'}
        data=None if body is None else json.dumps(body).encode()
        with urlopen(Request(url+path,data=data,headers=headers),context=context,timeout=30) as response:return json.load(response)
    try:
        for _ in range(300):
            assert child.poll() is None,'isolated server exited'
            try:health=request('/api/health');break
            except Exception:time.sleep(.1)
        else:raise RuntimeError('HTTPS startup failed; see isolated stderr.log')
        assert health['processId']==child.pid and health['instanceId']==settings['instance_id'];token=health['token']
        required=preservation.requirements(root/'app',settings_file,root/'data');before_required=preservation.check_required(required)
        save(root/'required-before.json',{'required':required,'hashes':before_required})
        sdk={'openai':openai.__version__,'httpx2':importlib.metadata.version('httpx2'),'websockets':importlib.metadata.version('websockets'),'python':sys.version.split()[0]}
        save(root/'sdk-versions.json',sdk)
        if a.contract_only:
            document=request('/v1/openapi.json')
            assert document==json.loads((REPO/'docs/openapi.json').read_text(encoding='utf-8'))
            save(root/'results/served-openapi.json',document)
            def manage(action):
                ticket=request('/api/management',{'action':action,'model':'whisper/tiny','lane':0,'device':'auto'})
                for _ in range(300):
                    state=request('/api/management')
                    if state['operation'].get('id')==ticket['id'] and not state['busy'] and state['operation']['state']!='running':
                        assert state['operation']['state']=='complete',state['operation'];return state
                    time.sleep(.05)
                raise AssertionError('management timeout')
            state=manage('load');assert any(m['id']=='whisper/tiny' and m['device']=='cpu' for m in state['loaded'])
            save(root/'results/manual-auto-cpu.json',state);manage('unload')
            transport=httpx2.Client(verify=context,trust_env=False,timeout=30)
            with openai.OpenAI(api_key=token,base_url=url+'/v1',http_client=transport,max_retries=0,_strict_response_validation=True) as sdk_client:
                basic=sdk_client.audio.transcriptions.create(model='whisper/tiny',file=('same.wav',(root/'audio.wav').read_bytes()))
                assert isinstance(basic.text,str) and basic.text
                save(root/'results/sdk-json.json',basic.model_dump())
            request('/api/admin/drain',{'action':'shutdown','confirm':'drain-server'});child.wait(timeout=30)
            assert child.returncode==0 and preservation.file_hash(settings_file)==settings_hash
            save(root/'required-after.json',{'required':required,'hashes':preservation.check_required(required)})
            save(root/'complete.json',{'passed':True,'contractOnly':True,'tlsVerification':True,'settingsBytesUnchanged':True,'exitCode':0})
            print('Isolated HTTPS OpenAPI endpoint verified');return
        raw_http=httpx2.Client(verify=context,trust_env=False,timeout=30)
        client=openai.OpenAI(api_key=token,base_url=url+'/v1',http_client=raw_http,max_retries=0,_strict_response_validation=True)
        listed=client.models.list();assert any(row.id=='whisper/tiny' for row in listed.data)
        from urllib.error import URLError
        rejected_context=ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        try:
            with urlopen(url+'/api/models',context=rejected_context,timeout=5):pass
            raise AssertionError('untrusted TLS certificate accepted')
        except URLError as e:assert isinstance(e.reason,ssl.SSLCertVerificationError)
        model=client.models.retrieve('whisper/tiny');assert model.id=='whisper/tiny';save(root/'results/model.json',model.model_dump())
        try:
            client.audio.transcriptions.create(model='whisper/tiny',file=(root/'audio.wav').read_bytes(),language='zz')
            raise AssertionError('invalid language accepted')
        except openai.BadRequestError as e:assert e.body.get('param')=='language';save(root/'results/sdk-error.json',e.body)
        audio=(root/'audio.wav').read_bytes()
        result=client.audio.transcriptions.create(model='whisper/tiny',file=('same.wav',audio),language='auto',prompt='TranscriptCutter',temperature=0,response_format='verbose_json')
        assert isinstance(result.text,str) and result.language!='auto';save(root/'results/sdk-verbose.json',result.model_dump())
        events=list(client.audio.transcriptions.create(model='whisper/tiny',file=('same.wav',audio),stream=True))
        assert [e.type for e in events].count('transcript.text.done')==1
        assert ''.join(e.delta for e in events if e.type=='transcript.text.delta')==events[-1].text
        save(root/'results/sdk-sse.json',[e.model_dump() for e in events])
        client.close()
        asyncio.run(async_checks(a,root,url,token,context,pcm,request))
        if a.wait_browser:
            save(root/'browser-ready.json',{'url':url,'pid':child.pid})
            for _ in range(300):
                if (root/'browser-done').exists():break
                time.sleep(1)
            else:raise RuntimeError('browser handoff timeout')
            assert preservation.object_json(root/'browser-result.json')['passed'] is True
        request('/api/admin/drain',{'action':'shutdown','confirm':'drain-server'});child.wait(timeout=40);assert child.returncode==0
        if a.mode=='fixture':
            current=preservation.object_json(settings_file)
            assert current==settings,(current,settings)
        else:assert preservation.file_hash(settings_file)==settings_hash
        save(root/'required-after.json',{'required':required,'hashes':preservation.check_required(required)})
        save(root/'complete.json',{'passed':True,'mode':a.mode,'settingsValuesRestored':True,
             'settingsBytesUnchanged':preservation.file_hash(settings_file)==settings_hash,
             'exitCode':0,'sdk':sdk,'tlsVerification':True,'baseUrl':url})
        print(json.dumps({'passed':True,'mode':a.mode,'url':url,'sdk':sdk}),flush=True)
    finally:
        if child.poll() is None:
            try:request('/api/admin/drain',{'action':'shutdown','confirm':'drain-server'});child.wait(timeout=10)
            except Exception:child.terminate();child.wait(timeout=15)
        out.close();err.close()

async def async_checks(args,root,url,token,context,pcm,request):
    import openai,httpx2
    import base64
    def record(name,value):save(root/'results'/name,value)
    http=httpx2.AsyncClient(verify=context,trust_env=False,timeout=60)
    async with openai.AsyncOpenAI(api_key=token,base_url=url+'/v1',websocket_base_url=url.replace('https:','wss:')+'/v1',http_client=http,max_retries=0) as client:
        async def receive(connection):return await asyncio.wait_for(connection.recv(),30)
        def manager():return client.realtime.connect(extra_query={'intent':'transcription'},websocket_connection_options={'ssl':context,'proxy':None})
        from websockets.exceptions import InvalidStatus
        try:
            async with client.realtime.connect(extra_query={'intent':'transcription'},extra_headers={'Origin':'https://untrusted.invalid'},websocket_connection_options={'ssl':context,'proxy':None}):pass
            raise AssertionError('untrusted Origin accepted')
        except InvalidStatus as error:assert error.response.status_code==403
        try:
            async with client.realtime.connect(extra_query={'intent':'transcription'},extra_headers={'Authorization':'Bearer invalid'},websocket_connection_options={'ssl':context,'proxy':None}):pass
            raise AssertionError('invalid WebSocket token accepted')
        except InvalidStatus as error:assert error.response.status_code==403
        record('security.json',{'untrustedTlsRejected':True,'untrustedOriginRejected':True,'invalidWebsocketTokenRejected':True})
        async def configure(c):
            first=await receive(c);assert first.type=='session.created'
            await c.session.update(session={'type':'transcription','audio':{'input':{'format':{'type':'audio/pcm','rate':24000},'transcription':{'model':'whisper/tiny'},'turn_detection':None}}})
            assert (await receive(c)).type=='session.updated'
        async def commit(c):
            for at in range(0,len(pcm),64000):await c.input_audio_buffer.append(audio=base64.b64encode(pcm[at:at+64000]).decode())
            await c.input_audio_buffer.commit()
            e=await receive(c);assert e.type=='input_audio_buffer.committed',e
            return e
        async with manager() as c:
            await configure(c);committed=await commit(c);events=[];text=''
            while True:
                e=await receive(c);events.append(e.model_dump())
                assert e.type not in ('error','conversation.item.input_audio_transcription.failed'),e
                if e.type.endswith('.delta'):text+=e.delta
                if e.type.endswith('.completed'):
                    assert e.item_id==committed.item_id and e.transcript==text;break
            record('sdk-websocket.json',events)
        if args.mode=='real':return
        hold=root/'app/hold';hold.write_text('hold')
        streams=[];connections=[];tasks=[];ids=[]
        async def consume(stream,id):
            events=[]
            async for e in stream:events.append(e.model_dump())
            done=[e for e in events if e['type']=='transcript.text.done'];assert len(done)==1
            assert ''.join(e['delta'] for e in events if e['type']=='transcript.text.delta')==done[0]['text']
            assert done[0]['text']==id+':part1\n'+id+':part2'
            return events
        async def consume_ws(c,committed):
            events=[];text=''
            while True:
                e=await receive(c);events.append(e.model_dump())
                assert e.type not in ('error','conversation.item.input_audio_transcription.failed'),e
                if e.type.endswith('.delta'):text+=e.delta
                if e.type.endswith('.completed'):
                    assert e.item_id==committed.item_id and text==e.transcript
                    id=committed.x_meeting['jobId'];assert text==id+':part1\n'+id+':part2';return events
        try:
            for i in range(5):
                stream=await client.audio.transcriptions.create(model='whisper/tiny' if i%2==0 else 'whisper/base',file=('same.wav',(root/'audio.wav').read_bytes()),stream=True)
                id=stream.response.headers['x-meeting-job-id'];streams.append(stream);ids.append(id)
            for i in range(5):
                m=manager();c=await m.__aenter__();connections.append((m,c));await configure(c);committed=await commit(c);ids.append(committed.x_meeting['jobId'])
                tasks.append(asyncio.create_task(consume_ws(c,committed)))
            snapshots=[]
            for _ in range(100):
                state=await asyncio.to_thread(request,'/api/scheduling');snapshots.append(state)
                if len(state['activeWorkers'])>=5:break
                await asyncio.sleep(.05)
            assert 5<=len(state['activeWorkers'])<=10,state
            assert state['reserved']['ramMiB']<=state['limits']['ramMiB']
            for id in ids:
                j=(await asyncio.to_thread(request,'/api/jobs/'+id))['job'];assert j['device']=='auto' and j['metadata']['executionDevice']=='cpu'
                assert j['metadata']['gpuFallbackReasonCode']=='gpu_capacity_unknown'
            await streams[0].close();await connections[0][1].close();tasks[0].cancel()
            cancelled=[ids[0],ids[5]]
            for _ in range(100):
                statuses=[(await asyncio.to_thread(request,'/api/jobs/'+id))['job']['status'] for id in cancelled]
                if all(v=='cancelled' for v in statuses):break
                await asyncio.sleep(.05)
            assert statuses==['cancelled','cancelled'],statuses
            hold.unlink()
            results=await asyncio.gather(*(consume(s,id) for s,id in zip(streams[1:],ids[1:5])),*tasks[1:])
            record('parallel.json',{'submitted':10,'activeAtBarrier':len(state['activeWorkers']),'snapshots':snapshots,'ids':ids,'cancelled':cancelled,'streams':results})
        finally:
            hold.unlink(missing_ok=True)
            for stream in streams:await stream.close()
            for m,c in connections:await m.__aexit__(None,None,None)
            for task in tasks:
                if not task.done():task.cancel()
            await asyncio.gather(*tasks,return_exceptions=True)
        for _ in range(100):
            if not (await asyncio.to_thread(request,'/api/scheduling'))['activeWorkers']:break
            await asyncio.sleep(.05)
        await asyncio.to_thread(request,'/api/management/scheduling',{'maxJobs':2,'realtimeReserved':1,'maxGpuJobs':1,'ramLimitMiB':0,'vramLimitMiB':0})
        hold.write_text('hold');pending=[]
        try:
            for _ in range(2):pending.append(await client.audio.transcriptions.create(model='whisper/tiny',file=('priority.wav',(root/'audio.wav').read_bytes()),stream=True))
            file_ids=[s.response.headers['x-meeting-job-id'] for s in pending]
            async with manager() as c:
                await configure(c);committed=await commit(c);rt=committed.x_meeting['jobId']
                for _ in range(100):
                    state=await asyncio.to_thread(request,'/api/scheduling')
                    if any(v['jobId']==rt for v in state['activeWorkers']):break
                    await asyncio.sleep(.05)
                assert any(v['jobId']==rt for v in state['activeWorkers'])
                second=(await asyncio.to_thread(request,'/api/jobs/'+file_ids[1]))['job']
                assert second['phase']=='waiting_resources',second
                record('priority.json',{'fileIds':file_ids,'realtimeId':rt,'state':state,'waitingJob':second})
                hold.unlink();await asyncio.gather(*(consume(s,id) for s,id in zip(pending,file_ids)),consume_ws(c,committed))
        finally:
            hold.unlink(missing_ok=True)
            for stream in pending:await stream.close()
        for _ in range(100):
            if not (await asyncio.to_thread(request,'/api/scheduling'))['activeWorkers']:break
            await asyncio.sleep(.05)
        await asyncio.to_thread(request,'/api/management/scheduling',{'maxJobs':10,'realtimeReserved':2,'maxGpuJobs':2,'ramLimitMiB':0,'vramLimitMiB':0})

if __name__=='__main__':main()
