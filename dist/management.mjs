import {
  modelGuidance,
  capacityText,
  memoryText,
  maximumMemoryText,
  optionalModelCandidates,
  referenceBenchmarks,
  referenceSpeedText,
  referenceFillerText,
  referenceLoadText,
  modelGroups,
} from './model-guidance.mjs';
import {modelPresentation, dependencyPresentation, modelActions, operationText, operationPercent, bulkCandidates, runtimePresentation, runtimeStoragePresentation} from './management-model-state.mjs';
import {modelBytes, sortModels, nextModelSort, modelSortDirection} from './management-model-list.mjs';
import {connectionGuide} from './connection-guide.mjs';

const $ = id => document.getElementById(id);
const make = (tag, text) => {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  return e;
};
/** class付きの要素を作る。 */
const classed = (tag, className, text) => {
  const e = make(tag, text);
  e.className = className;
  return e;
};
/** 新しいタブで開く外部リンクを作る。 */
const externalLink = (text, href) => {
  const link = make('a', text);
  link.href = href;
  link.target = '_blank';
  link.rel = 'noopener noreferrer';
  return link;
};
const isActive = status => !!status.running?.length || Object.values(status.queued || {}).some(n => n > 0);

let token = '', bearer = '', snapshot = null, updating = null, pending = null, timer = null;
let lastStructure = '', externalReady = false, restarting = false, restartSupported = false;
let readOnly = false;
let modelSort = 'default';
let runtimeRendered = false;

function renderConnectionGuide(value) {
  const info = connectionGuide(value, location.origin);
  const urls = $('connectionUrls');
  const addresses = info.urls.length ? info.urls : (info.localUrl ? [info.localUrl] : []);
  // Keep selection/focus intact while the management poll updates preparation.
  if (urls.dataset.addresses !== JSON.stringify(addresses)) {
    urls.dataset.addresses = JSON.stringify(addresses);
    urls.replaceChildren();
    for (const address of addresses) {
      const input = make('input');
      input.type = 'text'; input.readOnly = true; input.value = address;
      input.setAttribute('aria-label', 'クライアントに設定する接続先URL');
      input.onclick = () => input.select();
      urls.append(input);
    }
  }
  $('connectionAddressTitle').textContent = info.urls.length ? '別PCのクライアントに設定するURL' : '現在の管理画面のURL';
  $('connectionAddressNote').textContent = info.urls.length
    ? 'クライアントPCと同じLANのURLを選んでください。複数の候補は、それぞれ異なるネットワークのアドレスです。'
    : '別PC用URLは確認できません。同PC限定・待受アドレス・HTTPS証明書の対象名を管理者が確認してください。';
  $('connectionAuth').textContent = info.auth;
  $('connectionPreparation').textContent = info.preparation;
  $('connectionTls').hidden = !info.https;
}
function openConnectionGuide() {
  renderConnectionGuide(snapshot);
  if (!$('connectionGuide').open) $('connectionGuide').showModal();
}

function message(text, error = false) {
  $('message').textContent = text;
  $('message').hidden = !text;
  $('message').classList.toggle('error', error);
}
function renderReferenceEnvironment() {
  const environment = $('referenceEnvironment');
  environment.replaceChildren();
  for (const [label, value] of [
    ['測定CPU', `${referenceBenchmarks.cpu}（${referenceBenchmarks.cores}コア / ${referenceBenchmarks.logicalProcessors}論理スレッド）`],
    ['メモリ', `${referenceBenchmarks.memoryGB}GB`],
    ['測定日・版', `${referenceBenchmarks.referenceDate} / TranscriptionServer v${referenceBenchmarks.serverVersion}`],
    ['素材', referenceBenchmarks.sampleDescription],
    ['実行条件', referenceBenchmarks.executionCondition],
    ['参考速度', referenceBenchmarks.definition],
    ['処理時間', referenceBenchmarks.processingDefinition],
    ['ロード', referenceBenchmarks.loadDefinition],
    ['補助モデル', referenceBenchmarks.auxiliaryCondition],
    ['フィラー', referenceBenchmarks.fillerDefinition],
    ['評価辞書', `${referenceBenchmarks.dictionary.label} / ${referenceBenchmarks.dictionary.tokenizer}`],
  ]) {
    environment.append(make('dt', label), make('dd', value));
  }
  $('referenceCaution').textContent = `${referenceBenchmarks.caution} ${referenceBenchmarks.fillerCaution}`;
  const samples = $('referenceSamples');
  samples.replaceChildren();
  const breakdown = make('details');
  breakdown.append(make('summary', `モデル別の測定内訳（${Object.keys(referenceBenchmarks.models).length}件）`));
  for (const [id, sample] of Object.entries(referenceBenchmarks.models)) {
    const model = snapshot?.models.find(row => row.id === id);
    breakdown.append(make('p', `${model?.label || id}：ロード ${referenceLoadText(sample)} / 処理 ${sample.runSeconds.toFixed(2)}秒（中央値、${sample.processingMinSeconds.toFixed(2)}～${sample.processingMaxSeconds.toFixed(2)}秒） / 出力 ${sample.characters}文字（中央値） / フィラー候補 ${referenceFillerText({id}, true)}`));
  }
  samples.append(breakdown);
}

async function api(path, body, retryToken = true) {
  const headers = {};
  if (bearer) headers.Authorization = `Bearer ${bearer}`;
  else if (body !== undefined) headers['X-Local-Token'] = token;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const response = await fetch(path, {
    method: body === undefined ? 'GET' : 'POST',
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: 'omit',
    redirect: 'error',
    signal: AbortSignal.timeout(restarting ? 3000 : 15000),
  });
  const value = await response.json();
  const errorMessage = value.error?.message || value.error || `HTTP ${response.status}`;
  const staleLocalToken = response.status === 403 && errorMessage === '接続を更新してから操作してください。';
  // 同PC/信頼LANの操作トークンが古い場合は、healthで取り直して1回だけ再送する。
  if ((response.status === 401 || staleLocalToken) && !bearer && body !== undefined && retryToken) {
    const fresh = await api('/api/health');
    if (fresh.token && ['local', 'lan'].includes(fresh.authMode)) {
      token = fresh.token;
      return api(path, body, false);
    }
  }
  if (response.status === 401) {
    $('auth').hidden = false;
    throw Error('接続キーを入力してください。');
  }
  if (!response.ok) throw Error(errorMessage);
  return value;
}
async function health() {
  const value = await api('/api/health');
  token = value.token || '';
  readOnly = value.preview === true;
  externalReady = value.engines?.some(e => e.externalApi && e.ready) === true;
  restartSupported = value.capabilities?.serverRestart === true;
  $('version').textContent = `v${value.serverVersion}`;
  document.title = `サーバー管理 - TranscriptionServer - v${value.serverVersion}`;
  $('auth').hidden = true;
  return value;
}
function ask(title, text, action) {
  $('confirmTitle').textContent = title;
  $('confirmText').textContent = text;
  pending = action;
  $('confirm').showModal();
}
$('back').onclick = () => {
  $('confirm').close();
  pending = null;
};
$('accept').onclick = async () => {
  const task = pending;
  pending = null;
  $('confirm').close();
  if (task) await task();
};
async function operate(action, model, device = 'cpu', lane = 0, models) {
  try {
    await api('/api/management', {
      action,
      ...(action === 'install-runtime' ? {runtime: model} : {model}),
      ...(action === 'delete' ? {confirm: 'delete-model'} : {}),
      ...(action === 'delete-all' ? {confirm: 'delete-all-models', models} : {}),
      device,
      lane,
    });
    await refresh();
  } catch (error) {
    message(error.message, true);
  }
}
function progressView(operation, value) {
  const box = classed('div', 'operation-progress');
  box.append(classed('span', 'operation-spinner'));
  const content = make('div');
  content.append(make('div', operationText(operation, value.models, value.runtimes)));
  const bar = make('progress');
  bar.max = 100;
  const percent = operationPercent(operation);
  if (percent !== null) bar.value = percent;
  bar.setAttribute('aria-label', '現在の取得対象の進捗');
  content.append(bar);
  box.append(content);
  return box;
}
function renderDependencies(value) {
  const panel = $('dependencies');
  panel.replaceChildren();
  const state = dependencyPresentation(value, externalReady);
  for (const text of state.messages) panel.append(make('p', text));
  // Model cards install their own prerequisites in one operation. Keep the warning concise.
  panel.hidden = !panel.childNodes.length;
}

/** 実行環境の点検結果と、モデルを除いたファイル容量。 */
function renderRuntimes(value) {
  const rows = $('runtimeRows');
  rows.replaceChildren();
  const storage = value.runtimeStorage || {};
  const totals = runtimeStoragePresentation(storage);
  const runtimes = value.runtimes || [];
  const usable = runtimes.filter(row => row.healthy === true).length;
  $('runtimeTotal').textContent = `（${usable}/${runtimes.length}利用可能） · ${totals.total}`;
  $('runtimeReference').textContent = totals.reference;
  for (const runtime of runtimes) {
    const view = runtimePresentation(runtime, storage.runtimes?.find(row => row.id === runtime.id), value.runtimeSetup);
    const tr = classed('tr', runtime.healthy ? 'runtime-ready' : 'runtime-missing');
    const name = make('th', runtime.label);
    name.scope = 'row';
    name.append(classed('div', 'runtime-version', `管理対象版 ${runtime.version}`));
    const state = make('td', view.state);
    if (view.error) state.append(classed('div', 'runtime-error', view.error));
    if (runtime.installable) {
      const retry = make('button', runtime.repairable ? '修復' : '導入');
      retry.type = 'button';
      retry.disabled = readOnly || value.busy || isActive(value.status);
      retry.onclick = () => operate('install-runtime', runtime.id);
      state.append(retry);
    }
    tr.append(name, state, make('td', view.size));
    rows.append(tr);
  }
  if (!runtimeRendered && value.runtimeSetup?.state === 'incomplete') $('runtimeSection').open = true;
  runtimeRendered = true;
}

/** モデルの用途（一覧のグループ）。 */
function modelUsage(model) {
  if (model.engine === 'nemotron-transcribe') return 'integrated';
  if (model.engine === 'auxiliary') return model.id.endsWith('/silero-vad') ? 'vad' : 'diarization';
  return 'transcription';
}
const TRANSCRIPTION_REFERENCE =
  'CPU実測は設定欄の5分素材・各3回の参考値です。併記したWhisper公式比率はA100・英語でLargeを基準にした別条件です。精度は構成上の傾向で、正答率は未測定です。容量はモデルの保存サイズ（実行環境・RAM・VRAMは別）です。';
const INTEGRATED_MEMORY_NOTE =
  '量子化方式で容量・メモリが変わります。概算・配布元参考値・本PC実測を区別し、本PCで未測定の値は保証しません。';
const MEMORY_NOTE =
  '「概算」は構成検討用の推定、「公式参考」は別環境の公開値です。本PCの実測とは異なります。Whisper VRAMは原実装の参考値で、faster-whisperでは異なります。同時処理・長い音声では増えます。';

/** 用途ごとの折り畳み・説明・表の枠を作り、行を入れるtbodyを返す。 */
function renderGroup(container, usage, label, count, expanded, description, value) {
  const group = make('details');
  group.className = 'model-group';
  group.dataset.usage = usage;
  group.open = expanded.get(usage) ?? true;
  const summary = make('summary');
  summary.append(make('h3', `${label}（${count}件）`));
  group.append(summary);
  const terms = classed('dl', 'usage-description detail-only');
  for (const [term, text] of description) terms.append(make('dt', term), make('dd', text));
  group.append(terms);
  if (usage === 'transcription') {
    const reference = classed('p', 'guidance-note detail-only', TRANSCRIPTION_REFERENCE);
    reference.append(' ', externalLink('Whisper公式の比較条件', 'https://github.com/openai/whisper#available-models-and-languages'));
    group.append(reference);
  }
  const memoryNote = classed('p', 'guidance-note detail-only', usage === 'integrated' ? INTEGRATED_MEMORY_NOTE : MEMORY_NOTE);
  if (usage === 'transcription') {
    memoryNote.append(' ', externalLink('メモリの公式参考', 'https://github.com/SYSTRAN/faster-whisper#benchmark'));
  }
  group.append(memoryNote);
  const wrap = classed('div', 'model-table-wrap');
  const table = classed('table', 'model-table');
  table.setAttribute('aria-label', `${label}一覧`);
  const head = make('thead');
  head.append(renderHeading(container, usage, label, value));
  const grid = make('tbody');
  table.append(head, grid);
  wrap.append(table);
  group.append(wrap);
  container.append(group);
  return grid;
}
/** 表の見出し行。モデル名と保存容量は押すと並べ替える。 */
function renderHeading(container, usage, label, value) {
  const headRow = make('tr');
  for (const [text, column] of [['モデル', 'name'], ['保存容量', 'size'], ['RAM概算', null], ['VRAM概算', null], ['操作', null]]) {
    const cell = make('th');
    cell.scope = 'col';
    if (['保存容量', 'RAM概算', 'VRAM概算'].includes(text)) cell.className = 'simple-only';
    if (column) {
      const direction = modelSortDirection(column, modelSort);
      cell.setAttribute('aria-sort', direction);
      const button = make('button', text);
      button.type = 'button';
      button.className = 'sort-heading';
      button.dataset.sortKey = column;
      button.setAttribute('aria-label', `${label}：${text}で並べ替え`);
      const arrow = make('span', direction === 'ascending' ? ' ▲' : direction === 'descending' ? ' ▼' : ' ↕');
      arrow.setAttribute('aria-hidden', 'true');
      button.append(arrow);
      button.onclick = () => {
        modelSort = nextModelSort(column, modelSort);
        lastStructure = '';
        renderModels(snapshot || value);
        container.querySelector(`details[data-usage="${usage}"] button[data-sort-key="${column}"]`)?.focus({preventScroll: true});
      };
      cell.append(button);
    } else if (text === '操作') {
      cell.append(classed('span', 'operation-heading', text));
    } else {
      cell.textContent = text;
    }
    headRow.append(cell);
  }
  return headRow;
}
function rowState(model) {
  if (model.previewStatus === 'unverified' || model.displayOnly || model.managementState === 'disabled_by_policy') {
    return 'model-unchecked';
  }
  return model.modelInstalled === true || model.availability === 'installed' ? 'model-installed' : 'model-missing';
}
/** モデル1件の行（簡易表示）と詳細行を作る。 */
function renderModelRow(grid, model, value, simple, active) {
  const card = make('tr');
  card.dataset.modelId = model.id;
  card.className = `model-row ${rowState(model)}`;
  const name = make('th');
  name.scope = 'row';
  name.className = 'model-name';
  name.append(make('h4', model.label));
  const stateCell = make('div'), size = make('td'), ram = make('td'), vram = make('td'), operationCell = make('td');
  stateCell.className = 'model-status-detail';
  size.className = 'model-size simple-only';
  ram.className = 'model-ram simple-only';
  vram.className = 'model-vram simple-only';
  operationCell.className = 'model-operations';
  card.append(name, size, ram, vram, operationCell);
  const presentation = modelPresentation(model, value.runtimes || []);
  const loaded = value.loaded.filter(row => row.id === model.id);
  if (presentation.reason) stateCell.append(make('div', presentation.reason));
  const guidance = modelGuidance[model.id];
  size.textContent = capacityText(modelBytes(model));
  ram.textContent = maximumMemoryText(guidance?.ram);
  vram.textContent = maximumMemoryText(guidance?.vram);
  const compact = classed('div', 'model-summary simple-only');
  compact.append(make('span', `容量 ${size.textContent}`), make('span', `RAM ${ram.textContent}`), make('span', `VRAM ${vram.textContent}`));
  name.append(compact);
  const sample = referenceBenchmarks.models[model.id];
  const speed = classed('div', 'model-reference-speed simple-only', `参考速度: ${referenceSpeedText(model)}`);
  if (sample) {
    speed.title = `${referenceBenchmarks.cpu} / ${sample.referenceDate} / v${sample.serverVersion} / ${sample.condition}`;
  }
  name.append(speed);
  const current = value.operation;
  const running = current?.state === 'running' && current.model === model.id;
  if (running) {
    card.setAttribute('aria-busy', 'true');
    name.append(progressView(current, value));
  } else {
    const result = value.modelResults?.[model.id];
    if (result) name.append(classed('div', `model-result ${result.state}`, result.message));
    else if (current?.state === 'running' && current.targets?.includes(model.id)) name.append(classed('div', 'model-result', '順番待ち'));
  }
  if (model.restartRequired) name.append(classed('div', 'model-restart-required', 'このモデルを利用するにはサーバーの再起動が必要です。'));
  const detail = classed('tr', `model-detail detail-only ${card.className}`);
  const detailCell = make('td');
  detailCell.colSpan = simple ? 5 : 2;
  detailCell.append(stateCell);
  detail.append(detailCell);
  const metrics = classed('dl', 'model-guidance');
  for (const [term, text] of [
    ['計算時間', guidance?.time || '未測定'],
    ['精度の目安', guidance?.quality || '共通条件での比較なし'],
    ['保存容量', size.textContent],
    ['RAM目安', memoryText(guidance?.ram)],
    ['VRAM目安', memoryText(guidance?.vram)],
  ]) {
    metrics.append(make('dt', term), make('dd', text));
  }
  const condition = sample ? ` / ${sample.referenceDate}・v${sample.serverVersion}・${sample.condition}` : '';
  metrics.append(make('dt', '参考速度'), make('dd', `${referenceSpeedText(model)}${condition}`));
  if (sample) {
    metrics.append(make('dt', '読込・準備'), make('dd', referenceLoadText(sample)));
    metrics.append(make('dt', '処理時間の範囲'), make('dd', `${sample.processingMinSeconds.toFixed(2)}～${sample.processingMaxSeconds.toFixed(2)}秒（3回）`));
  }
  detailCell.append(metrics);
  if (guidance) detailCell.append(externalLink('モデル資料', guidance.source));
  if (model.displayOnly || presentation.state === 'disabled_by_policy') {
    operationCell.textContent = '対象外';
    grid.append(card, detail);
    return;
  }
  for (const row of loaded) {
    const lane = row.lane === 0 ? 'ファイル' : 'リアルタイム';
    stateCell.append(classed('div', 'loaded', `ロード済み · ${lane} · ${row.device.toUpperCase()} · ${row.requests}回使用`));
  }
  if (model.loadPolicy === 'on_demand') stateCell.append(make('div', '認識処理時にロード・解放'));
  renderFileActions(operationCell, model, value, active, loaded);
  if (model.loadPolicy !== 'on_demand') renderResidentControls(operationCell, model, value, active, loaded);
  grid.append(card, detail);
}
/** 「モデル: 導入/削除」の行。 */
function renderFileActions(operationCell, model, value, active, loaded) {
  const controls = classed('div', 'actions model-file-actions');
  const actions = modelActions(model, value.runtimes || [], value.loaded);
  const install = make('button', '導入'), remove = make('button', '削除');
  install.type = 'button';
  remove.type = 'button';
  remove.disabled = value.busy || active || !actions.canDelete;
  if (loaded.length) remove.title = 'ロード済みのモードをアンロードしてから削除できます。';
  install.disabled = value.busy || active || !actions.canInstall || !actions.needsSetup;
  if (actions.installed && !actions.needsSetup) install.title = '導入済みです。';
  install.onclick = () => operate('install', model.id);
  remove.onclick = () => ask(
    `${model.label}を削除`,
    `${model.label}の保存ファイルを削除します。使い直すときは再導入が必要です。録音・字幕・設定・実行環境は保持します。`,
    () => operate('delete', model.id),
  );
  controls.append(install, remove);
  const fileRow = classed('div', 'model-file-row');
  fileRow.append(make('span', 'モデル:'), controls);
  operationCell.append(fileRow);
}
/** ファイル処理・リアルタイムごとの「CPU/GPU/アンロード」切り替え。 */
function renderResidentControls(operationCell, model, value, active, loaded) {
  const resident = classed('div', 'resident-controls');
  const modes = classed('div', 'resident-modes');
  for (const [lane, label] of [[0, 'ファイル処理'], [1, 'リアルタイム']]) {
    const current = loaded.find(row => row.lane === lane), mode = classed('div', 'resident-mode');
    const toggles = classed('div', 'mode-toggles');
    toggles.setAttribute('role', 'group');
    toggles.setAttribute('aria-label', `${model.label}の${label}`);
    for (const [kind, text] of [['cpu', 'CPU'], ['gpu', 'GPU'], ['unload', 'アンロード']]) {
      const selected = kind === 'unload' ? !current : current?.device === kind;
      const toggle = make('button', text);
      toggle.type = 'button';
      toggle.setAttribute('aria-pressed', String(selected));
      toggle.setAttribute('aria-label', `${model.label}の${label}：${text}`);
      const supported = kind === 'unload' || model.processingMethods?.some(method => method.id === kind);
      toggle.disabled = value.busy || active || !supported || (kind === 'unload' ? !current : !model.loadable);
      if (kind === 'gpu' && !supported) toggle.title = 'GPUの実行候補がありません。';
      toggle.onclick = () => {
        if (!selected) operate(kind === 'unload' ? 'unload' : 'load', model.id, kind === 'unload' ? 'cpu' : kind, lane);
      };
      toggles.append(toggle);
    }
    mode.append(make('span', `${label}:`), toggles);
    modes.append(mode);
  }
  resident.append(modes);
  operationCell.append(resident);
}
function renderModels(value) {
  const structure = JSON.stringify([
    value.models, value.loaded, value.busy, value.status.running, value.status.queued, value.runtimes, value.operation, value.modelResults, modelSort, $('modelView').value,
  ]);
  if (structure === lastStructure) return;
  lastStructure = structure;
  const container = $('models');
  const simple = $('modelView').value === 'simple';
  $('modelSection').classList.toggle('model-view-simple', simple);
  const expanded = new Map([...container.querySelectorAll('details.model-group')].map(group => [group.dataset.usage, group.open]));
  container.replaceChildren();
  const drain = value.lifecycle || {};
    const local = ['127.0.0.1','localhost','[::1]'].includes(location.hostname);
    $('stopServer').disabled = readOnly || !local || drain.active || value.busy;
    $('stopServer').title = local ? '受付済み処理の完了を待って正常終了します。' : '停止はサーバーPCから操作してください。';
    $('drainMessage').hidden = !drain.active;
    $('drainMessage').textContent = drain.active ? `新規受付を停止しています。残り${drain.jobsRemaining ?? '確認中'}件／転送${drain.uploadsRemaining ?? 0}件：${drain.state === 'stopping' ? '正常終了中' : '完了待ち'}` : '';
    $('cancelDrain').hidden = !drain.active || drain.state === 'stopping';
    $('cancelDrain').disabled = readOnly || !local;
    const active = drain.active || isActive(value.status);
  const groups = new Map();
  const displayModels = [...value.models, ...optionalModelCandidates.filter(candidate => !value.models.some(m => m.id === candidate.id))];
  for (const model of displayModels) {
    const usage = modelUsage(model);
    if (!groups.has(usage)) groups.set(usage, []);
    groups.get(usage).push(model);
  }
  const descriptions = Object.fromEntries(modelGroups.map(group => [group.id, group.description]));
  for (const {id: usage, label} of modelGroups) {
    const models = sortModels(groups.get(usage) || [], modelSort);
    if (!models.length) continue;
    const grid = renderGroup(container, usage, label, models.length, expanded, descriptions[usage], value);
    for (const model of models) renderModelRow(grid, model, value, simple, active);
  }
}
let schedulingDirty = false;
const executionFields = {maxJobs:'maxJobs', realtimeReserved:'realtimeReserved', maxGpuJobs:'maxGpuJobs', ramLimit:'ramLimitMiB', vramLimit:'vramLimitMiB'};
const memoryGB = value => `${((value || 0) * 1048576 / 1e9).toFixed(1)}GB`;
function renderScheduling(value) {
  const state = value.scheduling;
  $('executionStatus').hidden = !state;
  $('executionSettings').hidden = !state;
  if (!state) return;
  const workers = state.activeWorkers || [];
  const waiting = Object.values(state.waiting || {});
  $('executionCount').textContent = `実行 ${workers.filter(w => !w.quarantined).length} / ${state.settings.maxJobs}件・待機 ${waiting.length}件`;
  const budget = `予約RAM ${memoryGB(state.reserved.ramMiB)} / ${memoryGB(state.limits.ramMiB)}・予約VRAM ${memoryGB(state.reserved.vramMiB)} / ${memoryGB(state.limits.vramMiB)}`;
  $('executionBudget').textContent = budget;
  $('executionLimits').textContent = `有効上限 RAM ${memoryGB(state.limits.ramMiB)}・VRAM ${state.limits.vramMiB ? memoryGB(state.limits.vramMiB) : '未確認（上限を指定してください）'}`;
  const reasons = new Map();
  for (const reason of waiting) reasons.set(reason, (reasons.get(reason) || 0) + 1);
  $('executionWaiting').replaceChildren(...[...reasons].map(([reason, count]) => {
    const p = document.createElement('p'); p.textContent = `${reason}: ${count}件`; return p;
  }));
  const models = new Map();
  for (const w of workers) {
    const label = value.models?.find(m => m.id === w.model)?.label || w.model;
    const key = `${label}・${w.workload === 'realtime' ? 'リアルタイム' : 'ファイル処理'}${w.quarantined ? '（終了未確認）' : ''}`;
    models.set(key, (models.get(key) || 0) + 1);
  }
  $('executionModels').replaceChildren(...[...models].map(([label, count]) => {
    const p = document.createElement('p'); p.textContent = `${label}: ${count}件`; return p;
  }));
  if (!schedulingDirty) for (const [id, key] of Object.entries(executionFields)) {
    $(id).value = id.endsWith('Limit') ? Number((state.settings[key] * 1048576 / 1e9).toFixed(1)) : state.settings[key];
  }
  $('saveExecution').disabled = readOnly || value.busy || isActive(value.status) || workers.length > 0;
}
$('executionForm').oninput = () => { schedulingDirty = true; };
$('executionForm').onsubmit = async event => {
  event.preventDefault();
  if (readOnly) return;
  const settings = {};
  for (const [id, key] of Object.entries(executionFields)) {
    const value = Number($(id).value);
    settings[key] = id.endsWith('Limit') ? Math.round(value * 1e9 / 1048576) : value;
  }
  $('saveExecution').disabled = true;
  try {
    await api('/api/management/scheduling', settings);
    schedulingDirty = false;
    $('executionMessage').textContent = '並列処理設定を保存しました。';
    await refresh();
  } catch (error) {
    $('executionMessage').textContent = error.message;
    $('saveExecution').disabled = false;
  }
};
async function refresh() {
  if (restarting) return;
  try {
    snapshot = await api('/api/management');
    const value = snapshot;
    if ($('connectionGuide').open) renderConnectionGuide(value);
    renderDependencies(value);
    renderRuntimes(value);
    renderScheduling(value);
    $('runtimeRestart').hidden = !value.restartRequired;
    if (value.restartRequired) {
      const names = (value.runtimes || []).filter(r => r.restartRequired).map(r => r.label).join('、');
      const models = value.models.filter(m => m.restartRequired).map(m => m.label).join('、');
      $('runtimeRestart').textContent = `${names}の反映${models ? `と、${models}の利用` : ''}にはサーバーの再起動が必要です。`;
    }
    $('restartPanel').classList.toggle('restart-required', value.restartRequired === true);
    $('restartServer').classList.toggle('restart-needed', value.restartRequired === true);
    const drain = value.lifecycle || {};
    const local = ['127.0.0.1','localhost','[::1]'].includes(location.hostname);
    $('stopServer').disabled = readOnly || !local || drain.active || value.busy;
    $('stopServer').title = local ? '受付済み処理の完了を待って正常終了します。' : '停止はサーバーPCから操作してください。';
    $('drainMessage').hidden = !drain.active;
    $('drainMessage').textContent = drain.active ? `新規受付を停止しています。残り${drain.jobsRemaining ?? '確認中'}件／転送${drain.uploadsRemaining ?? 0}件：${drain.state === 'stopping' ? '正常終了中' : '完了待ち'}` : '';
    $('cancelDrain').hidden = !drain.active || drain.state === 'stopping';
    $('cancelDrain').disabled = readOnly || !local;
    const active = drain.active || isActive(value.status);
    $('restartServer').disabled = !restartSupported || value.busy || active || value.restartBlocked === true;
    const operation = value.operation;
    $('activeOperation').hidden = operation.state !== 'running';
    $('activeOperation').replaceChildren();
    if (operation.state === 'running') $('activeOperation').append(progressView(operation, value));
    // Completion belongs to the affected row; only connectivity/preview notices stay here.
    message(readOnly ? operation.message || 'UIプレビュー（表示専用・操作は無効）' : '');
    $('cancel').hidden = operation.state !== 'running';
    $('cancel').disabled = readOnly;
    $('refresh').disabled = value.busy;
    const recommended = bulkCandidates(value, 'install-all').filter(model => (value.recommendedModels || []).includes(typeof model === 'string' ? model : model.id));
    $('installRecommended').disabled = readOnly || value.busy || active || !recommended.length;
    for (const [button, action] of [['installAll','install-all'],['deleteAll','delete-all']]) {
      const targets = bulkCandidates(value, action);
      $(button).disabled = readOnly || value.busy || active || !targets.length;
      $(button).title = `${targets.length}件が対象。対象外・導入済み・ロード中などは必要に応じて除外します。`;
    }
    renderModels(value);
    const migrated = value.migration ? ' ／ 旧名から移行済み・旧データ保持' : '';
    $('storage').textContent = `データ保存先: ${value.dataDirectory || value.dataFolderName || ''}` + migrated;
  } catch (error) {
    message(error.message, true);
  }
}
$('auth').onsubmit = async event => {
  event.preventDefault();
  bearer = $('key').value;
  $('key').value = '';
  await start();
};
$('refresh').onclick = () => operate('refresh');
$('cancel').onclick = () => operate('cancel');
$('installAll').onclick = () => operate('install-all');
$('installRecommended').onclick = () => operate('install-recommended');
$('deleteAll').onclick = () => {
  const targets = bulkCandidates(snapshot || {}, 'delete-all');
  ask('すべてのモデルを削除', `${targets.length}件の保存ファイルを削除します：${targets.map(m => m.label).join('、')}。録音・字幕・設定・実行環境は保持します。ロード中のモデルと対象外のモデルは削除しません。`,
    () => operate('delete-all', undefined, 'cpu', 0, targets.map(m => m.id)));
};
$('restartServer').onclick = () => ask(
  '再起動の確認',
  'TranscriptionServerを再起動します。録音・字幕・設定は保持されます。一時的に接続が切れ、起動後に再接続します。',
  restartServer,
);
async function restartServer() {
  restarting = true;
  clearInterval(timer);
  $('restartServer').disabled = true;
  try {
    const accepted = await api('/api/management/restart', {confirm: 'restart-server'});
    message('TranscriptionServerを再起動しています。再接続を待っています…');
    // 新しいプロセスIDのhealthが返るまで最大120秒待つ。
    const deadline = Date.now() + 120000;
    while (Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      try {
        const current = await health();
        if (current.processId !== accepted.previousProcessId) {
          restarting = false;
          lastStructure = '';
          await refresh();
          timer = setInterval(refresh, 3000);
          return;
        }
      } catch {}
    }
    throw Error('再起動後の接続を確認できません。サーバーの起動状態を確認してください。');
  } catch (error) {
    restarting = false;
    timer = setInterval(refresh, 3000);
    message(error.message, true);
  }
}
$('modelView').onchange = () => {
  lastStructure = '';
  if (snapshot) renderModels(snapshot);
};
$('storageRefresh').onclick = async () => {
  try {
    const value = await api('/api/storage');
    const used = (value.usedBytes / 1024 ** 3).toFixed(2), free = (value.freeBytes / 1024 ** 3).toFixed(1);
    $('storage').append(make('div', `保存 ${value.jobCount}件 ／ 使用 ${used} GB ／ 空き ${free} GB`));
  } catch (error) {
    message(error.message, true);
  }
};
async function updateStatus() {
  for (const id of ['updateCheck', 'updateApply', 'githubToken', 'githubSave', 'githubConnect', 'githubForget']) $(id).disabled = true;
  try {
    const value = await api('/api/updates');
    updating = value;
    $('updateVersion').textContent = `現在 v${value.currentVersion}`;
    $('updateMessage').textContent = value.message || '更新の確認を実行できます。';
    $('githubState').textContent = value.credentialConfigured ? '設定済み' : '未設定';
    const busy = ['checking','downloading','draining','applying'].includes(value.state);
    for (const id of ['updateCheck', 'githubToken', 'githubSave', 'githubConnect']) $(id).disabled = readOnly || busy;
    $('githubForget').disabled = readOnly || busy || !value.credentialConfigured;
    $('updateApply').hidden = !value.available || !value.installed;
    $('updateApply').disabled = readOnly || busy;
  } catch (error) {
    updating = null;
    $('updateMessage').textContent = error.message;
    $('updateApply').hidden = true;
  }
}
/** 更新関連の操作を行い、結果を更新状態として表示し直す。失敗は更新欄に出す。 */
async function updateAction(action) {
  try {
    await action();
  } catch (error) {
    $('updateMessage').textContent = error.message;
  }
}
$('openSettings').onclick = () => {
  renderReferenceEnvironment();
  $('settings').showModal();
  updateStatus();
};
$('openConnectionGuide').onclick = openConnectionGuide;
$('closeConnectionGuide').onclick = () => $('connectionGuide').close();
$('connectionGuide').addEventListener('close', () => {
  const url = new URL(location.href);
  url.searchParams.delete('guide');
  history.replaceState(null, '', url);
  $('openConnectionGuide').focus();
});
$('closeSettings').onclick = () => $('settings').close();
$('settings').addEventListener('close', () => {
  $('githubToken').value = '';
});
$('updateCheck').onclick = () => updateAction(async () => {
  await api('/api/updates/check', {});
  await updateStatus();
});
$('github').onsubmit = async event => {
  event.preventDefault();
  try {
    await updateAction(async () => {
      await api('/api/updates/credentials', {token: $('githubToken').value, save: $('githubSave').checked});
      await updateStatus();
    });
  } finally {
    $('githubToken').value = '';
  }
};
$('githubForget').onclick = () => ask('更新用GitHub接続を解除', '保存した更新用の接続情報を削除します。', () => updateAction(async () => {
  await api('/api/updates/forget', {});
  await updateStatus();
}));
$('updateApply').onclick = () => {
  if (!updating?.available) return;
  ask('更新して再起動', `v${updating.available.version}を適用します。`, () => updateAction(async () => {
    await api('/api/updates/apply', {version: updating.available.version});
    $('updateMessage').textContent = 'TranscriptionServerを更新して再起動しています…';
    $('updateApply').disabled = true;
    setTimeout(() => location.reload(), 6000);
  }));
};
async function start() {
  try {
    await health();
    await refresh();
    if (timer) clearInterval(timer);
    timer = setInterval(refresh, 3000);
  } catch (error) {
    message(error.message, true);
  }
}
if (new URL(location.href).searchParams.get('guide') === '1') openConnectionGuide();
await start();
