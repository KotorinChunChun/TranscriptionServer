import test from 'node:test';
import assert from 'node:assert/strict';
import {modelPresentation,runtimeLabel,processingChoice,dependencyPresentation,modelActions,operationText,operationPercent,bulkCandidates,runtimeStoragePresentation,runtimePresentation} from '../dist/management-model-state.mjs';

test('runtime storage distinguishes measured totals, partial external storage and fixed references',()=>{
  assert.equal(runtimeStoragePresentation().total,'容量未集計');
  const storage={totalBytes:299960008,complete:false,unmeasuredRuntimeCount:1,referenceInstalledBytes:788060917,referenceDownloadBytes:287143787};
  const view=runtimeStoragePresentation(storage);
  assert.equal(view.total,'集計済み 0.3GB（1種類は一部未集計）');
  assert.match(view.reference,/参考容量 0.788GB/);assert.match(view.reference,/273.8MiB/);
  assert.equal(runtimeStoragePresentation({...storage,complete:true,unmeasuredRuntimeCount:0}).total,'合計 0.3GB');
  const runtime={id:'speech',label:'speech.cpp',healthy:false,state:'missing'};
  const failed=runtimePresentation(runtime,{measuredBytes:0,capacityComplete:false,external:true},{results:[{id:'speech',state:'error',message:'取得失敗'}]});
  assert.equal(failed.error,'取得失敗');assert.equal(failed.size,'外部配置・一部未集計');
  assert.equal(runtimePresentation({...runtime,healthy:true,state:'detected'},{measuredBytes:92443661,capacityComplete:true},{results:[{id:'speech',state:'error',message:'過去の失敗'}]}).error,'');
});

test('missing runtime offers only fixed installable dependencies, while unsupported models do not imply missing runtime',()=>{
  const runtimes=[{id:'speech',installable:true},{id:'ffmpeg',installable:false}];
  const missing=modelPresentation({managementState:'missing_runtime',missingRuntimes:['speech','ffmpeg','whisper']},runtimes);
  assert.equal(missing.label,'実行環境が未導入');assert.deepEqual(missing.installs,[runtimes[0]]);
  const unsupported=modelPresentation({availability:'unavailable',managementState:'unsupported',missingRuntimes:[]},runtimes);
  assert.equal(unsupported.label,'この実行環境では非対応');assert.deepEqual(unsupported.installs,[]);
});
test('pending restart, corrupt runtime, missing media and missing model remain distinct',()=>{
  const labels=['restart_required','runtime_error','missing_media','model_missing','installed'].map(managementState=>modelPresentation({managementState}).label);
  assert.equal(new Set(labels).size,5);assert.match(labels[0],/サーバーの再起動が必要/);
  assert.match(runtimeLabel({label:'speech.cpp',version:'0.10.1',state:'restart_required'}),/導入済み・サーバーの再起動が必要/);
  assert.equal(modelPresentation({availability:'unavailable'}).label,'この実行環境では非対応');
});
test('unknown observations do not request installation; broken runtimes offer repair',()=>{
  const unknown=dependencyPresentation({tools:{},runtimes:[{id:'ffmpeg',installable:true}]});
  assert.match(unknown.messages.join(),/不明/);assert.deepEqual(unknown.runtimes,[]);
  const broken={id:'speech',label:'speech.cpp',state:'runtime_error',installable:true,repairable:true};
  const result=dependencyPresentation({tools:{ffmpeg:true,ffprobe:true,speech:true},runtimes:[broken],models:[]});
  assert.equal(result.runtimes[0],broken);assert.match(result.messages.join(),/点検に失敗/);assert.doesNotMatch(result.messages.join(),/モデルが未導入/);
  assert.match(runtimeLabel(broken),/点検に失敗/);
  assert.deepEqual(modelPresentation({managementState:'disabled_by_policy',missingRuntimes:[]}).installs,[]);
});
test('device choice stays selected when available and otherwise returns to automatic',()=>{
  const methods=[{id:'auto'},{id:'cpu'},{id:'gpu'}];
  assert.equal(processingChoice(methods),'auto');assert.equal(processingChoice(methods,'gpu'),'gpu');
  assert.equal(processingChoice(methods.slice(0,2),'gpu'),'auto');assert.equal(processingChoice([{id:'cpu'}]),'cpu');
});
test('one setup action covers missing runtimes and model; deletion needs all lanes unloaded',()=>{
  const runtimes=[{id:'nemo',label:'NeMo-Speech.cpp',installable:true,state:'missing'}];
  const model={id:'nemotron-asr/test',managementState:'missing_runtime',missingRuntimes:['nemo'],modelInstalled:false,installable:false};
  const setup=modelActions(model,runtimes);
  assert.equal(setup.canInstall,true);assert.equal(setup.needsSetup,true);assert.equal(setup.installs.length,1);
  assert.equal(modelActions({...model,managementState:'disabled_by_policy'},runtimes).canInstall,false);
  const installed={...model,managementState:'installed',missingRuntimes:[],modelInstalled:true,deletable:true};
  assert.equal(modelActions(installed,runtimes).needsSetup,false);
  assert.equal(modelActions(installed,runtimes).canDelete,true);
  assert.equal(modelActions(installed,runtimes,[{id:model.id,lane:1}]).canDelete,false);
  assert.equal(modelActions({...installed,missingRuntimes:['nemo']},runtimes).needsSetup,true);
});
test('operation notices identify the model and each prerequisite without duplicate connection status',()=>{
  const models=[{id:'whisper/base',label:'Whisper Base'}];
  assert.equal(operationText({action:'install',model:'whisper/base',state:'running'},models),'Whisper Base：導入中 · 進捗確認中');
  assert.match(operationText({action:'install',model:'whisper/base',state:'running',targetLabel:'Whisper Base → Whisper専用実行環境',receivedBytes:1048576,totalBytes:2097152},models),/Whisper Base → Whisper専用実行環境：導入中 · 50% · 1.0 \/ 2.0 MiB/);
  assert.equal(operationText({state:'complete',message:'Whisper Baseの導入が完了しました。'},models),'Whisper Baseの導入が完了しました。');
  assert.equal(operationText({state:'idle'},models),'接続済み');
});
test('progress is real, phase-specific, and bulk actions exclude protected models',()=>{
  assert.equal(operationPercent({downloadedBytes:5,totalBytes:10}),50);
  assert.equal(operationPercent({progressPercent:120}),null);
  assert.equal(operationPercent({phase:'probe',receivedBytes:10,totalBytes:10}),null);
  assert.match(operationText({state:'running',action:'install-all',totalCount:3,completedCount:1,progressPercent:50,progressUnit:'files'}),/1\/3件完了.*50%（ファイル件数）/);
  const value={models:[{id:'a',installable:true},{id:'b',modelInstalled:true,deletable:true},{id:'c',modelInstalled:true,deletable:true},{id:'d',installable:true,managementState:'disabled_by_policy'}],loaded:[{id:'b'}]};
  assert.deepEqual(bulkCandidates(value,'install-all').map(m=>m.id),['a']);
  assert.deepEqual(bulkCandidates(value,'delete-all').map(m=>m.id),['c']);
});
