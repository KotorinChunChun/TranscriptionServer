"""Validate the shipped contract and captured isolated API responses offline."""
import argparse,json,re,sys
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
def read(path):return json.loads(path.read_text(encoding='utf-8-sig'))
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--sdk-dir',type=Path,required=True)
    parser.add_argument('--evidence',type=Path,action='append',default=[]);parser.add_argument('--output',type=Path)
    args=parser.parse_args();sys.path.insert(0,str(args.sdk_dir.resolve()))
    from openapi_spec_validator import validate
    from jsonschema import Draft202012Validator
    document=read(ROOT/'docs/openapi.json');validate(document)
    assert document['info']['version']==read(ROOT/'package.json')['version']
    # Exact literal routes must not silently disappear from the document.
    source=''.join((ROOT/path).read_text(encoding='utf-8').split('#[cfg(test)]',1)[0] for path in ('src/http.rs','src/updates.rs'))
    routes=set(re.findall(r'"((?:/api/|/v1/)[A-Za-z0-9_/.-]+)"',source))
    routes={p for p in routes if not p.endswith('/')}
    assert routes<=document['paths'].keys(),sorted(routes-document['paths'].keys())
    def verify(schema,value):
        Draft202012Validator({'$ref':'#/components/schemas/'+schema,'components':document['components']}).validate(value)
    checks=[]
    for root in args.evidence:
        assert read(root/'complete.json')['passed'] is True
        checks_for_root=[]
        for name,schema in [('model.json','Model'),('sdk-verbose.json','WhisperVerbose')]:
            verify(schema,read(root/'results'/name));checks_for_root.append(name)
        error=read(root/'results/sdk-error.json');verify('V1Error',{'error':error})
        for event in read(root/'results/sdk-sse.json'):verify('SseEvent',event)
        for event in read(root/'results/sdk-websocket.json'):verify('WebSocketServerEvent',event)
        if (root/'results/parallel.json').exists():
            parallel=read(root/'results/parallel.json')
            for snapshot in parallel['snapshots']:verify('Scheduling',snapshot)
            for events in parallel['streams']:
                for event in events:verify('SseEvent' if event['type'].startswith('transcript.') else 'WebSocketServerEvent',event)
            priority=read(root/'results/priority.json');verify('Scheduling',priority['state']);verify('Job',priority['waitingJob'])
        checks.append({'root':str(root),'capturedResponsesValid':True})
    result={'passed':True,'openapi':document['openapi'],'paths':len(document['paths']),'schemas':len(document['components']['schemas']),'literalRouteCoverage':True,'evidence':checks}
    if args.output:args.output.write_text(json.dumps(result,indent=2),encoding='utf-8')
    print(json.dumps(result))
if __name__=='__main__':main()
