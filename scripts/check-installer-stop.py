"""Run the production Inno waiting/UTF-8 functions with isolated controller processes.

No application files, user settings, installed mutex, or actual server are changed.
"""
import argparse, json, os, subprocess, time
from pathlib import Path

def main():
    p=argparse.ArgumentParser();p.add_argument('--iscc',type=Path,required=True);p.add_argument('--work-dir',type=Path,required=True);a=p.parse_args()
    root=Path(__file__).resolve().parents[1];work=a.work_dir.resolve()
    base=(root.parent/'runtime').resolve()
    assert work.is_relative_to(base) and work!=base and not work.exists(),'Use a fresh isolated directory under Services/runtime'
    work.mkdir(parents=True);helper=work/'fixture-helper.exe'
    csc=Path(os.environ['WINDIR'])/'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    subprocess.run([str(csc),'/nologo','/r:System.Web.Extensions.dll','/out:'+str(helper),str(root/'tests/fixtures/setup-stop-helper.cs')],check=True,capture_output=True)
    result=subprocess.run([str(a.iscc),'/DReleaseDir='+str(work),str(root/'tests/fixtures/setup-stop.iss')],text=True,capture_output=True)
    (work/'compile.log').write_text(result.stdout+result.stderr,encoding='utf-8');assert result.returncode==0,result.stdout[-3000:]+result.stderr
    results=[]
    for mode in ('complete','long','error','cancel','crash'):
        case=work/mode;case.mkdir();start=time.monotonic()
        done=subprocess.run([str(work/'stop-regression.exe'),'/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-',
            '/WorkDir='+str(case),'/FixtureHelper='+str(helper),'/Mode='+mode,'/LOG='+str(case/'setup.log')],timeout=30,creationflags=0x08000000)
        elapsed=time.monotonic()-start;assert done.returncode==0
        text=(case/'result.txt').read_text(encoding='utf-8')
        assert 'helperProcessId' not in text and '"state"' not in text and '\ufffd' not in text
        if mode in ('complete','long'):
            assert text=='SUCCESS',text
            assert (case/'result.txt').stat().st_mtime_ns >= (case/'report.exiting').stat().st_mtime_ns,'Returned before actual helper exit'
            assert elapsed >= (4.4 if mode=='long' else 1.9)
        elif mode=='error':assert 'モデルの処理中です。' in text and '安全に停止できません。' in text
        elif mode=='cancel':assert text=='更新を取り消しました。'
        else:assert text=='停止処理が異常終了しました。更新を続行しません。'
        results.append({'mode':mode,'elapsedSeconds':round(elapsed,3),'text':text,'passed':True})
    (work/'result.json').write_text(json.dumps({'cases':results,'productionServerTouched':False},ensure_ascii=False,indent=2),encoding='utf-8')
    print(json.dumps({'passed':len(results),'work':str(work),'productionServerTouched':False}))
if __name__=='__main__':main()
