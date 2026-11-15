import {modelGuidance} from './model-guidance.mjs';

export function modelBytes(model){
  const size=modelGuidance[model.id]?.bytes??model.downloadBytes;
  return Number.isFinite(size)&&size>=0?size:null;
}
function installationOrder(model){
  if(model.modelInstalled===true||model.availability==='installed')return 0;
  if(model.modelInstalled===false||['download_on_first_use','manual_install_required'].includes(model.availability))return 1;
  return 2;
}
export function nextModelSort(column,current){
  const pairs={name:['name','name-desc'],state:['installed','missing'],size:['size-asc','size-desc']};
  const pair=pairs[column];
  return pair?current===pair[0]?pair[1]:pair[0]:current;
}
export function modelSortDirection(column,order){
  const pairs={name:['name','name-desc'],state:['installed','missing'],size:['size-asc','size-desc']};
  return pairs[column]?.[0]===order?'ascending':pairs[column]?.[1]===order?'descending':'none';
}
export function sortModels(models,order='default'){
  const rows=[...models];
  return rows.sort((a,b)=>{
    if(order==='size-asc'||order==='size-desc'){
      const left=modelBytes(a),right=modelBytes(b);
      if(left===null||right===null)return left===right?0:left===null?1:-1;
      return (left-right)*(order==='size-desc'?-1:1);
    }
    if(order==='installed'||order==='missing'){
      const left=installationOrder(a),right=installationOrder(b);
      if(left===2||right===2)return left===right?0:left===2?1:-1;
      return (left-right)*(order==='missing'?-1:1);
    }
    if(order==='name'||order==='name-desc')return String(a.label).localeCompare(String(b.label),'ja',{numeric:true})*(order==='name-desc'?-1:1);
    return 0;
  });
}
