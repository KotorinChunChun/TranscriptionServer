const states = {
  disabled_by_policy: '導入・実行の対象外',
  separate_runtime_required: '別途対応が必要・現在は実行不可',
  preview_unknown: '導入状態は未確認',
  installed: '導入済み',
  model_missing: 'モデル未導入',
  missing_runtime: '実行環境が未導入',
  missing_media: 'FFmpeg / FFprobeの準備が必要',
  runtime_error: '実行環境の点検に失敗',
  unsupported: 'この実行環境では非対応',
  restart_required: '実行環境を導入済み・サーバーの再起動が必要',
};
const runtimeStates = {
  activating: '導入済み・反映中',
  detected: '起動点検済み',
  restart_required: '導入済み・サーバーの再起動が必要',
  runtime_error: '起動点検に失敗',
  missing: '未導入',
};
const verbs = {
  refresh: '点検中',
  install: '導入中',
  'install-runtime': '実行環境を導入中',
  load: 'ロード中',
  unload: 'メモリから解放中',
  delete: 'モデルを削除中',
  'install-all': '導入中',
  'install-recommended': '導入中',
  'delete-all': 'モデルを削除中',
};
const isInstalled = model => model.modelInstalled === true || model.availability === 'installed';

export function modelPresentation(model, runtimes = []) {
  const fallback = model.availability === 'installed'
    ? 'installed'
    : ['download_on_first_use', 'manual_install_required'].includes(model.availability) ? 'model_missing' : 'unsupported';
  const state = model.managementState || fallback;
  const installs = (model.missingRuntimes || [])
    .map(id => runtimes.find(row => row.id === id))
    .filter(row => row?.installable && model.previewStatus !== 'unverified');
  return {state, label: states[state] || '利用不可', reason: model.statusMessage || '', installs};
}
export function runtimeLabel(runtime) {
  const state = Object.hasOwn(runtimeStates, runtime.state) ? runtimeStates[runtime.state] : '状態未確認';
  return `${runtime.label} · ${state}`;
}
const runtimeGB = bytes => bytes > 0 && bytes < 1e6 ? '0.001GB未満' : `${Number((bytes / 1e9).toFixed(3))}GB`;
export function runtimeStoragePresentation(storage = {}) {
  const known = Number.isFinite(storage.totalBytes);
  const total = known ? `${storage.complete ? '合計' : '集計済み'} ${runtimeGB(storage.totalBytes)}` : '容量未集計';
  const missing = storage.unmeasuredRuntimeCount > 0 ? `（${storage.unmeasuredRuntimeCount}種類は一部未集計）` : '';
  const reference = Number.isFinite(storage.referenceInstalledBytes) && Number.isFinite(storage.referenceDownloadBytes)
    ? `4種類を新規導入する場合の参考容量 ${runtimeGB(storage.referenceInstalledBytes)} ／ 取得 ${(storage.referenceDownloadBytes / 1024 ** 2).toFixed(1)}MiB。モデル本体・キャッシュ・旧版退避は含みません。`
    : '';
  return {total: total + missing, reference};
}
export function runtimePresentation(runtime, storage, setup = {}) {
  const failure = setup.results?.find(row => row.id === runtime.id && row.state === 'error');
  const error = runtime.healthy !== true ? failure?.message || runtime.statusMessage || '' : '';
  const size = !storage ? '未集計' : storage.capacityComplete
    ? runtimeGB(storage.measuredBytes) : `${storage.external ? '外部配置・' : ''}一部未集計${storage.measuredBytes > 0 ? `（集計済み ${runtimeGB(storage.measuredBytes)}）` : ''}`;
  return {state: runtimeStates[runtime.state] || '状態未確認', error, size};
}
export function processingChoice(methods = [], previous) {
  const ids = methods.map(m => m.id);
  return ids.includes(previous) ? previous : ids.includes('auto') ? 'auto' : ids[0] || '';
}
export function modelActions(model, runtimes = [], loaded = []) {
  const state = modelPresentation(model, runtimes);
  const installed = isInstalled(model);
  // 不足していた実行環境がすべて導入済み（再起動待ちを含む）か。
  const pending = (model.missingRuntimes || [])
    .every(id => runtimes.some(r => r.id === id && ['detected', 'restart_required'].includes(r.state)));
  const blocked = ['disabled_by_policy', 'unsupported', 'preview_unknown'].includes(state.state) || model.displayOnly;
  const canInstall = model.installable === true
    || (!blocked && (state.installs.length > 0 || (pending && state.state === 'restart_required')));
  return {
    installed,
    needsSetup: !installed || state.installs.length > 0,
    canInstall,
    canDelete: model.deletable === true && !loaded.some(row => row.id === model.id),
    installs: state.installs,
  };
}
export function operationText(operation, models = [], runtimes = []) {
  const label = models.find(m => m.id === operation.model)?.label
    || runtimes.find(r => r.id === operation.model)?.label
    || operation.model
    || 'モデル一覧';
  if (operation.state !== 'running') return operation.message || '接続済み';
  const target = operation.targetLabel || label;
  const verb = verbs[operation.action] || '操作中';
  const received = operation.receivedBytes ?? operation.downloadedBytes;
  const mib = bytes => (bytes / 1024 ** 2).toFixed(1);
  const progress = Number.isFinite(received) && operation.totalBytes > 0
    ? ` · ${mib(received)} / ${mib(operation.totalBytes)} MiB`
    : '';
  const percent = operationPercent(operation);
  const unit = operation.progressUnit === 'files' ? '（ファイル件数）' : '';
  const count = operation.totalCount > 1 ? `［${operation.completedCount}/${operation.totalCount}件完了］` : '';
  const phase = ({extract:'検証・展開中',probe:'起動点検中',activate:'導入反映中'})[operation.phase];
  return `${count}${target}：${phase || verb}${percent === null ? ' · 進捗確認中' : ` · ${percent.toFixed(0)}%${unit}`}${progress}`;
}
export function operationPercent(operation) {
  if (operation.phase && operation.phase !== 'download') return null;
  const received = operation.receivedBytes ?? operation.downloadedBytes;
  const value = Number.isFinite(received) && operation.totalBytes > 0
    ? received / operation.totalBytes * 100 : operation.progressPercent;
  return Number.isFinite(value) && value >= 0 && value <= 100 ? value : null;
}
export function bulkCandidates(value, action) {
  return (value.models || []).filter(model => {
    if (model.displayOnly || ['disabled_by_policy','unsupported','preview_unknown'].includes(model.managementState)) return false;
    const options = modelActions(model, value.runtimes || [], value.loaded || []);
    return action === 'install-all' ? options.canInstall && options.needsSetup : options.canDelete;
  });
}
export function dependencyPresentation(value, externalReady = false) {
  if (value.toolsStatus === 'unknown' || !value.tools || !Object.keys(value.tools).length) {
    return {messages: ['実サーバーの導入状態を確認できません。未導入かどうかは不明です。'], runtimes: []};
  }
  const messages = [], needed = new Set(), rows = value.runtimes || [];
  const failed = rows.filter(r => r.state === 'runtime_error');
  for (const r of failed) {
    messages.push(`${r.label}: ${r.statusMessage || '起動点検に失敗しました。'}`);
    needed.add(r.id);
  }
  if ((!value.tools.ffmpeg || !value.tools.ffprobe) && !failed.some(r => r.id === 'ffmpeg')) {
    messages.push('音声・動画の処理に必要なFFmpeg / FFprobeが不足しています。');
    needed.add('ffmpeg');
  }
  const external = value.externalApiReady ?? externalReady;
  // 台帳にない実行環境は、ツールの検出結果で判断する。
  const runtimeUsable = id => rows.some(r => r.id === id && r.state === 'detected' && r.healthy !== false)
    || (!rows.some(r => r.id === id) && value.tools[id] === true);
  const usable = external || ['whisper', 'speech', 'nemo'].some(runtimeUsable);
  if (!usable && !value.restartRequired) {
    if (!failed.some(r => r.id !== 'ffmpeg')) {
      messages.push('文字起こしの実行環境が未導入です。使用するモデルの「導入」から必要な実行環境もまとめて導入できます。');
    }
    for (const id of ['whisper', 'speech', 'nemo']) needed.add(id);
  }
  const hasModel = (value.models || []).some(m => m.engine !== 'auxiliary'
    && m.managementState !== 'disabled_by_policy' && m.runtimeReady !== false && isInstalled(m));
  if (usable && !external && !value.restartRequired && !hasModel) {
    messages.push('文字起こしモデルが未導入です。下のモデル一覧から導入してください。');
  }
  return {messages, runtimes: rows.filter(r => needed.has(r.id) && r.installable)};
}
