const endpoint = '/api/admin/history';
const statusLabels = {
  uploading: '受信中', queued: '待機中', running: '処理中', complete: '完了',
  completed: '完了', failed: '失敗', error: '失敗', canceled: '取消', cancelled: '取消',
};
const $ = id => document.getElementById(id);
const element = (tag, text) => {
  const node = document.createElement(tag);
  node.textContent = text;
  return node;
};
const statusText = job => statusLabels[job.status] || job.status || '不明';
const modelText = job => [job.model, job.modelId !== job.model ? job.modelId : ''].filter(Boolean).join(' / ') || '不明';
const fileText = job => job.fileName || 'ファイル名なし';
const resultText = job => job.segmentCount ? `${job.segmentCount}区間` : '結果なし';
const dateText = value => value && Number.isFinite(Date.parse(value)) ? new Date(value).toLocaleString('ja-JP') : '';
const seconds = value => Number.isFinite(value) ? `${value.toFixed(1)}秒` : '';
const stamp = value => {
  const time = Math.max(0, Math.floor(value));
  return `${Math.floor(time / 60)}:${String(time % 60).padStart(2, '0')}`;
};

export function bindHistory(api) {
  if (!['localhost', '[::1]'].includes(location.hostname) && !/^127(?:\.\d{1,3}){3}$/.test(location.hostname)) return;

  const dialog = $('jobHistory');
  const audioContainer = $('historyAudio');
  const transcript = $('historyTranscript');
  const more = $('moreHistoryText');
  let listRequest = 0;
  let detailRequest = 0;
  let offset = 0;
  let nextOffset = null;
  let selected = '';
  let shown = 0;
  let after = 0;
  let resultVersion = '';

  function clearDetail() {
    detailRequest++;
    selected = '';
    const media = audioContainer.querySelector('audio, video');
    if (media) {
      media.pause();
      media.removeAttribute('src');
      media.load();
    }
    audioContainer.replaceChildren();
    transcript.replaceChildren();
    $('historyMetadata').replaceChildren();
    $('historyTextStatus').textContent = '';
    $('historyResult').hidden = true;
    more.hidden = true;
  }

  async function loadText(request) {
    more.disabled = true;
    try {
      const page = await api(`${endpoint}/${encodeURIComponent(selected)}/segments?after=${after}`);
      if (request !== detailRequest) return;
      const version = `${page.revision}/${page.resultGeneration}`;
      if (after && resultVersion !== version) {
        transcript.replaceChildren();
        after = shown = 0;
        return await loadText(request);
      }
      resultVersion = version;
      for (const row of page.segments) {
        const item = element('li', '');
        const labels = [Number.isFinite(row.start) ? stamp(row.start) : '', row.speaker].filter(Boolean);
        if (labels.length) item.append(element('span', labels.join(' · ')));
        if (row.text) item.append(element('div', row.text));
        transcript.append(item);
      }
      shown += page.segments.length;
      after = page.next;
      $('historyTextStatus').textContent = shown ? `${shown} / ${Math.max(shown, page.total || 0)}区間` : '認識結果はまだありません。';
      more.hidden = !page.segments.length || shown >= page.total;
    } catch (error) {
      if (request === detailRequest) $('historyTextStatus').textContent = error.message;
    } finally {
      if (request === detailRequest) more.disabled = false;
    }
  }

  async function openJob(id, moveFocus) {
    clearDetail();
    selected = id;
    const request = detailRequest;
    after = shown = 0;
    resultVersion = '';
    for (const button of $('historyJobs').querySelectorAll('button')) {
      button.setAttribute('aria-pressed', String(button.dataset.id === id));
    }
    $('historySelection').textContent = 'ジョブを読み込んでいます…';
    try {
      const job = await api(`${endpoint}/${encodeURIComponent(id)}`);
      if (request !== detailRequest) return;
      $('historySelection').textContent = '';
      $('historyFileName').textContent = fileText(job);
      const values = [
        ['状態', statusText(job)], ['モデル', modelText(job)], ['エンジン', job.engine], ['実行環境', job.runtime],
        ['話者分離モデル', job.diarizationModel], ['実行デバイス', job.device], ['言語', job.language],
        ['クライアント', job.clientId], ['受付日時', dateText(job.createdAt)], ['終了日時', dateText(job.finishedAt)],
        ['音声の長さ', seconds(job.duration)], ['処理時間', seconds(job.runSeconds)], ['ジョブID', job.id],
      ];
      for (const [label, value] of values) {
        if (typeof value === 'string' && value) $('historyMetadata').append(element('dt', label), element('dd', value));
      }
      if (job.media) {
        const media = document.createElement(job.media.type.startsWith('video/') ? 'video' : 'audio');
        media.controls = true;
        media.preload = 'none';
        media.setAttribute('aria-label', '入力音声');
        const message = element('p', '入力音声を再生できませんでした。');
        message.hidden = true;
        media.addEventListener('error', () => { message.hidden = false; });
        media.src = job.media.path;
        audioContainer.append(media, message);
      } else {
        audioContainer.append(element('p', '再生できる入力音声がありません。'));
      }
      $('historyResult').hidden = false;
      if (moveFocus) $('historyFileName').focus();
      $('historyTextStatus').textContent = '認識結果を読み込んでいます…';
      await loadText(request);
    } catch (error) {
      if (request === detailRequest) $('historySelection').textContent = error.message;
    }
  }

  async function loadPage(start, stayOnSelection = false) {
    const request = ++listRequest;
    const current = stayOnSelection ? selected : '';
    clearDetail();
    const list = $('historyJobs');
    list.replaceChildren();
    $('historySelection').textContent = '';
    $('historyStatus').textContent = '読み込んでいます…';
    $('olderJobs').disabled = $('newerJobs').disabled = true;
    try {
      const page = await api(`${endpoint}?offset=${start}`);
      if (request !== listRequest) return;
      if (!page.jobs.length && start && page.total) return await loadPage(0);
      offset = page.offset;
      nextOffset = page.nextOffset;
      $('olderJobs').hidden = offset === 0;
      $('newerJobs').hidden = nextOffset === null;
      $('historyStatus').textContent = page.total ? `${offset + 1}–${offset + page.jobs.length}件 / 全${page.total}件` : '処理履歴はありません。';
      for (const job of page.jobs) {
        const item = element('li', '');
        const button = element('button', '');
        button.type = 'button';
        button.dataset.id = job.id;
        button.setAttribute('aria-pressed', 'false');
        button.append(
          element('strong', fileText(job)),
          element('span', [dateText(job.createdAt), statusText(job), resultText(job)].filter(Boolean).join(' · ')),
          element('span', [modelText(job), job.clientId].filter(Boolean).join(' · ')),
        );
        button.onclick = () => openJob(job.id, true);
        item.append(button);
        list.append(item);
      }
      const job = page.jobs.find(job => job.id === current);
      if (job) await openJob(job.id, false);
      else if (page.jobs.length) $('historySelection').textContent = 'ジョブを選択してください。';
    } catch (error) {
      if (request === listRequest) $('historyStatus').textContent = error.message;
    } finally {
      if (request === listRequest) $('olderJobs').disabled = $('newerJobs').disabled = false;
    }
  }

  $('openHistory').hidden = false;
  $('openHistory').onclick = () => {
    dialog.showModal();
    loadPage(0);
  };
  $('closeHistory').onclick = () => dialog.close();
  dialog.addEventListener('close', () => {
    listRequest++;
    clearDetail();
    $('historyJobs').replaceChildren();
  });
  $('reloadHistory').onclick = () => loadPage(offset, true);
  $('olderJobs').onclick = () => loadPage(Math.max(0, offset - 25));
  $('newerJobs').onclick = () => loadPage(nextOffset);
  more.onclick = () => loadText(detailRequest);
}
