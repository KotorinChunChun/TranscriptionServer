"""Whisper-only JSON Lines worker. No HTTP server, database or job scheduler."""
from contextlib import contextmanager
import argparse
import importlib.metadata
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

MODELS=('tiny','base','small','medium','large-v3-turbo','large-v3')
CHUNK_SECONDS=300
MODEL_DIR=None
EMIT_LOCK=threading.Lock()

def emit(value):
    with EMIT_LOCK:print(json.dumps(value,ensure_ascii=True),flush=True)

def download(model):
    # Keep faster-whisper's fixed repository/pattern selection; expose its real tqdm counters.
    import faster_whisper.utils as utils
    from tqdm.auto import tqdm
    class DownloadProgress(tqdm):
        def __init__(self,*args,**kwargs):
            self._reported_at=0
            kwargs['disable']=False
            super().__init__(*args,**kwargs)
        def display(self,*args,**kwargs):
            total=getattr(self,'total',0);current=getattr(self,'n',0)
            if not total or total<=0:return
            now=time.monotonic()
            if now-self._reported_at<0.25 and current<total:return
            self._reported_at=now
            emit({'type':'download_progress','percent':round(min(100,max(0,current/total*100)),2),
                  'current':current,'total':total,'unit':'bytes' if getattr(self,'unit','')=='B' else 'files',
                  'label':str(getattr(self,'desc','取得'))[:120]})
    previous=utils.disabled_tqdm
    try:
        utils.disabled_tqdm=DownloadProgress
        return utils.download_model(model,cache_dir=MODEL_DIR)
    finally:utils.disabled_tqdm=previous

@contextmanager
def stage(config,kind):
    report=config.get('reportStages') is True
    if report:emit({'type':'stage','event':'start','stage':kind})
    try:
        yield
    except Exception:
        if report:emit({'type':'stage','event':'error','stage':kind})
        raise
    else:
        if report:emit({'type':'stage','event':'complete','stage':kind})


def gpu_compute():
    import ctranslate2
    try:
        if ctranslate2.get_cuda_device_count()>0:
            types=ctranslate2.get_supported_compute_types('cuda')
            return next((kind for kind in ('float16','float32','int8_float32') if kind in types),None)
    except Exception:pass
    return None

def probe():
    from faster_whisper.utils import download_model,available_models
    import ctranslate2
    compute=gpu_compute();gpu=compute is not None
    result={}
    for model in MODELS:
        state='download_on_first_use'
        try:
            path=Path(download_model(model,local_files_only=True,cache_dir=MODEL_DIR))
            if all((path/name).is_file() for name in ('model.bin','config.json','tokenizer.json')) and any(p.is_file() for p in path.glob('vocabulary.*')):
                state='installed'
        except Exception:pass
        result[model]=state
    return {'models':[m for m in MODELS if m in available_models()],'availability':result,'gpu':gpu,'gpuComputeType':compute,
            'requestOptions':['language','prompt','temperature'],
            'gpuNote':'FP32は旧世代GPU向け候補です。CUDAライブラリー/実ロードの検証が必要です。' if compute=='float32' else ''}

def load(config,loaded=None):
    from faster_whisper import WhisperModel
    import ctranslate2
    model_id=config['model'];requested=config['device']
    if model_id not in MODELS or requested not in {'auto','cpu','gpu'}:raise ValueError('Whisperの選択が不正です。')
    gpu_type=gpu_compute() if requested!='cpu' else None;cuda=gpu_type is not None
    if requested=='gpu' and not cuda:raise RuntimeError('WhisperのGPU実行環境を確認できません。')
    device='cuda' if cuda else 'cpu';compute=gpu_type if cuda else 'int8';key=(model_id,device,config.get('threads'))
    started=time.monotonic();reused=loaded is not None and loaded[0]==key
    if not reused:
        loaded=None
        kwargs={'device':device,'compute_type':compute}
        if MODEL_DIR:kwargs['download_root']=MODEL_DIR
        if config.get('threads'):kwargs['cpu_threads']=int(config['threads'])
        model=WhisperModel(model_id,**kwargs)
    else:model=loaded[1]
    emit({'type':'ready','version':importlib.metadata.version('faster-whisper'),'device':device,'computeType':compute,
          'modelReused':reused,'modelLoadSeconds':round(time.monotonic()-started,4)})
    return key,model

def recognize(config,loaded=None):
    start=float(config['start']);end=float(config['end'])
    if not math.isfinite(start) or not math.isfinite(end) or not 0<=start<end:raise ValueError('Whisperの範囲が不正です。')
    with stage(config,"model_load"):
        key,model=load(config,loaded)
    with tempfile.TemporaryDirectory(prefix='transcription-server-whisper-') as temporary:
        audio=Path(temporary)/'chunk.wav';offset=start
        while offset<end:
            length=min(CHUNK_SECONDS,end-offset)
            with stage(config,"audio_preparation"):
                result=subprocess.run([config['ffmpeg'],'-hide_banner','-nostdin','-v','error','-y','-ss',str(offset),'-i',config['source'],
                                       '-t',str(length),'-map','0:a:0','-vn','-ac','1','-ar','16000','-c:a','pcm_s16le',str(audio)],
                                      stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=max(120,length*2),
                                      creationflags=0x08000000 if os.name=='nt' else 0)
                if result.returncode:raise RuntimeError('Whisperの音声変換に失敗しました。')
            with stage(config,"recognition"):
                options=config.get('recognitionOptions',{})
                language=options.get('language','ja')
                kwargs={'language':None if language=='auto' else language,'task':'transcribe','beam_size':5,'vad_filter':True,
                        'initial_prompt':options.get('prompt') or None}
                if 'temperature' in options:kwargs['temperature']=options['temperature']
                segments,info=model.transcribe(str(audio),**kwargs)
                emit({'type':'language','language':info.language})
                for segment in segments:
                    text=str(segment.text).strip();left=max(offset,min(end,offset+float(segment.start)));right=min(end,offset+float(segment.end))
                    if text and math.isfinite(left) and math.isfinite(right) and right>left:
                        emit({'type':'segment','segment':{'start':round(left,3),'end':round(right,3),'text':text,'speaker':'',
                             'speakerSource':'unassigned','speakerNeedsReview':False,'timestampType':'whisper_segment',
                             'transcriptionNeedsReview':(offset>start and left-offset<1) or (offset+length<end and offset+length-right<1),
                             'avg_logprob':float(segment.avg_logprob),'compression_ratio':float(segment.compression_ratio),
                             'no_speech_prob':float(segment.no_speech_prob),'seek':int(segment.seek),
                             'temperature':float(segment.temperature),'tokens':list(segment.tokens)}})
            offset+=length;emit({'type':'progress','processedUntil':offset})
    return key,model

def main():
    global MODEL_DIR
    parser=argparse.ArgumentParser();parser.add_argument('--probe',action='store_true');parser.add_argument('--download',choices=MODELS);parser.add_argument('--model-dir');parser.add_argument('--runtime-check',action='store_true');args=parser.parse_args()
    if args.model_dir:
        MODEL_DIR=str(Path(args.model_dir).resolve())
        os.environ['HF_HUB_DISABLE_IMPLICIT_TOKEN']='1'
        os.environ['HF_HOME']=str(Path(MODEL_DIR)/'hub-home')
        os.environ['HUGGINGFACE_HUB_CACHE']=MODEL_DIR
    if args.runtime_check:
        import faster_whisper,ctranslate2,av,tokenizers,onnxruntime,numpy
        names=('faster-whisper','ctranslate2','av','tokenizers','onnxruntime','numpy')
        emit({'ok':True,'python':sys.version.split()[0],'versions':{name:importlib.metadata.version(name) for name in names}});return 0
    if args.probe:emit(probe());return 0
    if args.download:
        download(args.download);emit({'type':'done','model':args.download});return 0
    loaded=None
    for raw in sys.stdin:
        try:
            if len(raw)>65536:raise ValueError('Whisper設定が大きすぎます。')
            config=json.loads(raw)
            loaded=load(config,loaded) if config.get('action')=='load' else recognize(config,loaded)
            emit({'type':'done'})
        except Exception as exc:
            emit({'type':'error','message':str(exc)[:2000]});return 1
    return 0

if __name__=='__main__':
    import multiprocessing
    multiprocessing.freeze_support()
    raise SystemExit(main())
