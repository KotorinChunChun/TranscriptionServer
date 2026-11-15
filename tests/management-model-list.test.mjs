import test from 'node:test';
import assert from 'node:assert/strict';
import {modelBytes,sortModels,nextModelSort,modelSortDirection} from '../dist/management-model-list.mjs';

test('capacity sorting compares bytes across units, leaves unknown last in both directions, and preserves input',()=>{
  const models=[{id:'large',downloadBytes:2*1024**3},{id:'unknown'},{id:'small',downloadBytes:75*1024**2},{id:'medium',downloadBytes:800*1024**2}];
  assert.deepEqual(sortModels(models,'size-asc').map(m=>m.id),['small','medium','large','unknown']);
  assert.deepEqual(sortModels(models,'size-desc').map(m=>m.id),['large','medium','small','unknown']);
  assert.deepEqual(models.map(m=>m.id),['large','unknown','small','medium']);
  assert.equal(modelBytes({id:'whisper/tiny'}),78203619);
});
test('installation sorting distinguishes unknown status and retains stable order for peers',()=>{
  const models=[{id:'unknown',modelInstalled:null},{id:'missing',modelInstalled:false},{id:'installed',modelInstalled:true},{id:'also-installed',availability:'installed'}];
  assert.deepEqual(sortModels(models,'installed').map(m=>m.id),['installed','also-installed','missing','unknown']);
  assert.deepEqual(sortModels(models,'missing').map(m=>m.id),['missing','installed','also-installed','unknown']);
});
test('name sorting uses numeric model labels and standard order restores original order',()=>{
  const models=[{id:'ten',label:'モデル10'},{id:'two',label:'モデル2'}];
  assert.deepEqual(sortModels(models,'name').map(m=>m.id),['two','ten']);
  assert.deepEqual(sortModels(models,'name-desc').map(m=>m.id),['ten','two']);
  assert.deepEqual(sortModels(models).map(m=>m.id),['ten','two']);
});
test('table headers toggle direction and reset to the initial direction when changing column',()=>{
  for(const [column,ascending,descending] of [['name','name','name-desc'],['state','installed','missing'],['size','size-asc','size-desc']]){
    assert.equal(nextModelSort(column,'default'),ascending);
    assert.equal(nextModelSort(column,ascending),descending);
    assert.equal(nextModelSort(column,descending),ascending);
    assert.equal(modelSortDirection(column,ascending),'ascending');
    assert.equal(modelSortDirection(column,descending),'descending');
    assert.equal(modelSortDirection(column,'default'),'none');
  }
  assert.equal(nextModelSort('size','name-desc'),'size-asc');
  assert.equal(modelSortDirection('state','size-asc'),'none');
});
