// Shared by the server API and management UI. Measurements retain their original conditions.
import metadata from './model-metadata.json' with {type:'json'};
export const {modelGuidance,referenceBenchmarks,optionalModelCandidates,groups:modelGroups}=metadata;
export function referenceSpeedText(model){
  if(model.engine==='auxiliary')return '対象外（文字出力なし）';
  const sample=referenceBenchmarks.models[model.id];
  return Number.isFinite(sample?.charactersPerSecond)
    ? `${sample.charactersPerSecond.toFixed(1)}文字/s${sample.loadIncludedInProcessing?'（ロード込み）':''}`
    : '未測定';
}
export function referenceFillerText(model,detail=false){
  if(model.engine==='auxiliary')return '対象外（文字出力なし）';
  const filler=referenceBenchmarks.models[model.id]?.filler;
  if(!Number.isFinite(filler?.candidatesMedian))return '未測定';
  const rate=Number.isFinite(filler.candidatesPer1000Characters)
    ? `${filler.candidatesPer1000Characters.toFixed(1)}回/千文字` : '文字数あたり未測定';
  return `${filler.candidatesMedian}回（${rate}）${detail?` / 3回の中央値・範囲 ${filler.candidatesMin}～${filler.candidatesMax}回`:''}`;
}
export function referenceLoadText(sample){
  if(sample?.loadIncludedInProcessing)return '処理時間に含む（区間ごとにロード）';
  return Number.isFinite(sample?.loadReadySeconds)?`${sample.loadReadySeconds.toFixed(2)}秒`:'未測定';
}
export function capacityText(bytes){
  if(!Number.isFinite(bytes)||bytes<=0)return '未確認';
  return `${(bytes/1e9).toFixed(bytes>=1e9?2:bytes>=1e7?3:4)}GB`;
}
export function memoryText(value){
  const format=(number,unit)=>String(Number((Number(number)*(unit==='GiB'?1024**3/1e9:1024**2/1e6)).toFixed(1)));
  // 「1.5～3GiB」のような2進単位の値・範囲を10進GB/MBへ換算し、「約」を外す。
  const amount=/(\d+(?:\.\d+)?)(?:～(\d+(?:\.\d+)?))?(GiB|MiB)/g;
  return String(value||'未算定').replaceAll('約','').replace(amount,(_,left,right,unit)=>
    `${format(left,unit)}${right?'～'+format(right,unit):''}${unit==='GiB'?'GB':'MB'}`);
}
export function maximumMemoryText(value){
  if(String(value).includes('CPU方式では使用しません'))return '0GB';
  const matches=String(value||'').matchAll(/([\d,]+(?:\.\d+)?)(?:[～〜–-]([\d,]+(?:\.\d+)?))?\s*(GiB|MiB|GB|MB)/g);
  const amounts=[...matches].map(([,left,right,unit])=>Number((right||left).replaceAll(',',''))*({GiB:1024**3/1e9,MiB:1024**2/1e9,GB:1,MB:1e-3}[unit]));
  if(!amounts.length)return '未確認';
  const maximum=Math.max(...amounts);
  return `${Number(maximum.toFixed(maximum>0&&maximum<0.1?3:1))}GB`;
}
