// Headless Edge with an isolated profile. Never disables certificate verification.
import assert from 'node:assert/strict';
import {readFile,writeFile} from 'node:fs/promises';
import {pathToFileURL} from 'node:url';
const [root,library]=process.argv.slice(2);
const launch=JSON.parse(await readFile(root+'/browser-ready.json','utf8'));
const url=launch.url;
assert(url.startsWith('https://127.0.0.1:')&&!url.endsWith(':8769'));
const {chromium}=await import(pathToFileURL(library+'/node_modules/playwright-core/index.mjs').href);
const browser=await chromium.launch({executablePath:'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',headless:true});
try {
 const context=await browser.newContext();
 await context.route('**/*',route=>route.request().url().startsWith(url+'/')?route.continue():route.abort());
 const page=await context.newPage();
 await page.goto(url+'/',{waitUntil:'domcontentloaded'});
 const audio=(await readFile(root+'/audio.wav')).toString('base64');
 const pcm=(await readFile(root+'/audio.pcm')).toString('base64');
 const result=await page.evaluate(async({url,audio,pcm})=>{
   if(!isSecureContext)throw Error('Not a secure context');
   const decode=raw=>Uint8Array.from(atob(raw),c=>c.charCodeAt(0));
   const health=await(await fetch('/api/health')).json();
   const models=await(await fetch('/v1/models')).json();
   if(!models.data.some(m=>m.id==='whisper/tiny'))throw Error('Model missing');
   const form=new FormData();form.set('model','whisper/tiny');form.set('device','cpu');form.set('stream','true');form.set('file',new Blob([decode(audio)]),'short.wav');
   const response=await fetch('/v1/audio/transcriptions',{method:'POST',headers:{'X-Local-Token':health.token},body:form});
   if(!response.ok||!response.headers.get('content-type').includes('text/event-stream'))throw Error('SSE failed');
   const raw=await response.text();const events=raw.split('\n').filter(s=>s.startsWith('data:')).map(s=>JSON.parse(s.slice(5)));
   const done=events.filter(e=>e.type==='transcript.text.done');
   if(done.length!==1||events.filter(e=>e.type==='transcript.text.delta').map(e=>e.delta).join('')!==done[0].text)throw Error('SSE mismatch');
   const ws=new WebSocket(url.replace('https:','wss:')+'/v1/realtime?intent=transcription',['realtime','openai-insecure-api-key.'+health.token]);
   const queue=[],waiters=[];
   ws.onmessage=e=>{const v=JSON.parse(e.data);if(waiters.length)waiters.shift()(v);else queue.push(v);};
   const next=()=>queue.length?Promise.resolve(queue.shift()):new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('WSS timeout')),30000);waiters.push(v=>{clearTimeout(timer);resolve(v);});});
   const first=await next();if(first.type!=='session.created')throw Error('WSS session failed');
   ws.send(JSON.stringify({type:'session.update',session:{type:'transcription',audio:{input:{format:{type:'audio/pcm',rate:24000},transcription:{model:'whisper/tiny'},turn_detection:null}}}}));
   if((await next()).type!=='session.updated')throw Error('WSS update failed');
   const bytes=decode(pcm);
   for(let i=0;i<bytes.length;i+=64000){const chunk=bytes.subarray(i,i+64000);let s='';for(const v of chunk)s+=String.fromCharCode(v);ws.send(JSON.stringify({type:'input_audio_buffer.append',audio:btoa(s)}));}
   ws.send(JSON.stringify({type:'input_audio_buffer.commit'}));let item,text='',count=0;
   for(;;){const e=await next();if(e.type==='error'||e.type.endsWith('.failed'))throw Error('WSS recognition failed');if(e.type==='input_audio_buffer.committed')item=e.item_id;if(e.type.endsWith('.delta'))text+=e.delta;if(e.type.endsWith('.completed')){if(e.item_id!==item||e.transcript!==text)throw Error('WSS mismatch');count++;break;}}
   ws.close();
   return {passed:true,secureContext:isSecureContext,https:true,sse:true,wss:true,completionCount:count};
 },{url,audio,pcm});
 assert(result.passed);await writeFile(root+'/browser-result.json',JSON.stringify({...result,browser:await browser.version()},null,2));
 await context.close();
} finally {await browser.close();await writeFile(root+'/browser-done','done');}
