"""Opt-in isolated real-Whisper check. Copies one existing cache; never downloads.

No installer, production service, shared settings, or shared model mutation.
Supply a fresh work directory and an already available synthetic audio sample.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time
from urllib.request import Request, urlopen
import preservation


def main():
    parser = argparse.ArgumentParser()
    for key in ('exe', 'python', 'worker', 'model-cache', 'audio', 'ffmpeg', 'ffprobe', 'work-dir'):
        parser.add_argument('--' + key, required=True)
    args = parser.parse_args()
    work = Path(args.work_dir).resolve()
    if work.exists():
        raise SystemExit('Use a fresh isolated work directory')
    source = Path(args.model_cache).resolve()
    if source.name != 'models--Systran--faster-whisper-tiny':
        raise SystemExit('Only the existing Whisper Tiny cache is allowed')
    source_files = preservation.tree_files(source)
    original = {str(p): preservation.file_hash(p) for p in source_files}
    work.mkdir(parents=True)
    app, home, data = work / 'app', work / 'settings', work / 'data'
    for folder in (app, home, data, work / 'tmp'):
        folder.mkdir()
    target = home / 'models/huggingface/hub' / source.name
    shutil.copytree(source, target)
    executable = app / 'TranscriptionServer.exe'
    shutil.copy2(args.exe, executable)
    # Explicit development receipts, not a pretend installed/released build.
    (app / 'BUILD.json').write_text(json.dumps({'kind': 'isolated-development', 'sourceExeHash': preservation.file_hash(executable)}))
    (app / 'INSTALLATION.json').write_text(json.dumps({'scope': 'isolated-development', 'installed': False}))
    settings = {'auto_install_runtimes': False, 'data_dir': str(data), 'instance_id': 'parallel-acceptance',
                'execution': {'maxJobs': 10, 'realtimeReserved': 2, 'maxGpuJobs': 2, 'ramLimitMiB': 0, 'vramLimitMiB': 0},
                'acceptanceSentinel': 'must-survive-restart'}
    settings_file = home / 'settings.json'
    settings_file.write_text(json.dumps(settings), encoding='utf-8')
    settings_hash = preservation.file_hash(settings_file)
    audio = Path(args.audio).read_bytes()
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        port = s.getsockname()[1]
    if port == 8769:
        raise SystemExit('Reserved production port')
    url = f'http://127.0.0.1:{port}'
    command = [str(executable), '--instance-id', 'parallel-acceptance', '--local', '--host', '127.0.0.1',
               '--port', str(port), '--settings-dir', str(home), '--data-dir', str(data), '--process-record', str(work / 'process.json'),
               '--no-browser', '--no-auto-runtimes', '--whisper-python', str(Path(args.python).resolve()),
               '--whisper-worker', str(Path(args.worker).resolve()), '--ffmpeg', str(Path(args.ffmpeg).resolve()),
               '--ffprobe', str(Path(args.ffprobe).resolve()), '--speech', str(work / 'unavailable-speech.exe'),
               '--nemo-speech', str(work / 'unavailable-nemo.exe')]
    env = dict(os.environ, HF_HUB_OFFLINE='1', TRANSFORMERS_OFFLINE='1', HF_HUB_DISABLE_IMPLICIT_TOKEN='1',
               TEMP=str(work / 'tmp'), TMP=str(work / 'tmp'))
    required = preservation.requirements(app, settings_file, data)

    def request(path, value=None, token=None, raw=False):
        headers = {'X-Local-Token': token} if token else {}
        if value is not None:
            headers['Content-Type'] = 'application/octet-stream' if raw else 'application/json'
            value = value if raw else json.dumps(value).encode()
        with urlopen(Request(url + path, data=value, headers=headers), timeout=20) as response:
            return json.load(response)

    def launch(number):
        nonlocal child
        stdout = (work / f'stdout-{number}.log').open('wb')
        stderr = (work / f'stderr-{number}.log').open('wb')
        child = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr, creationflags=0x08000000)
        stdout.close(); stderr.close()
        for _ in range(180):
            if child.poll() is not None:
                raise RuntimeError('Isolated candidate exited; inspect stderr')
            try:
                health = request('/api/health')
                if health['acceptingJobs'] and health['startupState'] != 'preparing':
                    preservation.check_required(required)
                    assert health['instanceId'] == 'parallel-acceptance'
                    assert health['capabilities']['concurrentInference']
                    return child, health['token']
            except (OSError, ValueError):
                pass
            time.sleep(.25)
        child.terminate(); child.wait(timeout=15)
        raise RuntimeError('Isolated startup timed out')

    child = None
    jobs = []
    token = None
    try:
        child, token = launch(1)
        catalog = request('/api/models')
        tiny = next(m for m in catalog['models'] if m['id'] == 'whisper/tiny')
        assert tiny['modelInstalled'], 'Copied Tiny must be detected; no download permitted'
        def submit(index):
            workload = 'file' if index < 5 else 'realtime'
            return request(f'/api/jobs?engine=whisper&model=tiny&device=cpu&diarize=false&clientId=isolated-{index}&workload={workload}', audio, token, True)['job']['id']
        with ThreadPoolExecutor(max_workers=7) as pool:
            jobs = list(pool.map(submit, range(7)))
        peak = 0; samples = []; deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            state = request('/api/scheduling')
            peak = max(peak, len(state['activeWorkers']))
            samples.append({'at': time.time(), 'active': len(state['activeWorkers']), 'waiting': state['waiting'], 'reserved': state['reserved']})
            results = [request('/api/jobs/' + job)['job'] for job in jobs]
            if not state['activeWorkers'] and all(r['status'] in ('complete', 'error', 'cancelled') for r in results):
                break
            time.sleep(.1)
        else:
            raise RuntimeError('Real inference exceeded the isolated test deadline')
        assert all(r['status'] == 'complete' and r['segmentCount'] > 0 for r in results), results
        assert peak >= 2, 'Must demonstrate actual concurrent inference, not just queueing'
        rows = [request('/api/jobs/' + job + '/segments')['segments'] for job in jobs]
        assert all(row and all(isinstance(s.get('text'), str) for s in row) for row in rows)
        assert all(r['metadata']['actualDevice'] == 'cpu' for r in results)
        assert preservation.file_hash(settings_file) == settings_hash
        report = {'kind': 'isolated-real-whisper', 'peakConcurrent': peak, 'submitted': len(jobs), 'port': port,
                  'jobIds': jobs, 'results': results, 'samples': samples, 'downloads': False, 'gpuTested': False,
                  'settingsUnchanged': True, 'requiredFilesPresent': True, 'productionTouched': False}
        request('/api/admin/shutdown', {}, token)
        assert child.wait(timeout=20) == 0
        child = None
        manifest = preservation.capture(required, 'cleanup', keep_trees=[target])
        (work / 'preservation.json').write_text(json.dumps(manifest, indent=2), encoding='utf-8')
        child, token = launch(2)
        assert request('/api/scheduling')['settings'] == settings['execution']
        assert all(request('/api/jobs/' + job)['job']['status'] == 'complete' for job in jobs)
        request('/api/admin/shutdown', {}, token)
        assert child.wait(timeout=20) == 0
        child = None
        preservation.verify(manifest)
        assert all(preservation.file_hash(p) == digest for p, digest in original.items())
        report['restartPersistence'] = report['sourceModelUnchanged'] = report['preservationVerified'] = True
        (work / 'result.json').write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding='utf-8')
        print(json.dumps({k: report[k] for k in ('peakConcurrent','submitted','settingsUnchanged','preservationVerified','sourceModelUnchanged')},ensure_ascii=False), flush=True)
    finally:
        if child is not None and child.poll() is None:
            try:
                for job in jobs:
                    request('/api/jobs/' + job + '/cancel', {}, token)
                request('/api/admin/shutdown', {}, token)
                child.wait(timeout=20)
            except Exception:
                # Only the explicitly launched candidate's owned process tree.
                subprocess.run(['taskkill', '/PID', str(child.pid), '/T', '/F'], capture_output=True, timeout=15)
                child.wait(timeout=15)


if __name__ == '__main__':
    main()
