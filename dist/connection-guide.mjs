/** Connection guidance uses the server's advertised addresses and auth policy. */
export function connectionGuide(value, currentOrigin) {
  const network = value?.network;
  const mode = network?.authMode;
  const urls = [];
  if (mode && mode !== 'local') {
    for (const advertised of network.urls || []) {
      try {
        const url = new URL(advertised);
        if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) continue;
        const host = url.hostname.toLowerCase();
        if (host === 'localhost' || host === '[::1]' || host === '0.0.0.0' || host === '[::]' || /^127\./.test(host)) continue;
        const address = url.origin + '/';
        if (!urls.includes(address)) urls.push(address);
      } catch { /* Invalid advertisements cannot become client connection URLs. */ }
    }
  }
  let localUrl = '';
  try {
    const url = new URL(currentOrigin);
    if (['http:', 'https:'].includes(url.protocol) && !url.username && !url.password) localUrl = url.origin + '/';
  } catch { /* No invented address when preview/server data is missing. */ }
  const auth = mode === 'bearer'
    ? 'Bearer認証です。管理者から渡された接続キーをクライアントへ設定してください。'
    : mode === 'lan' ? '信頼LAN接続です。接続キーは空欄で利用できます。'
    : mode === 'local' ? '同じPCからのみ接続できます。別PCで使うにはサーバー管理者がLAN公開を設定してください。'
    : '接続方式を確認中です。接続キーが必要かどうかは、サーバーの応答後に表示します。';
  const state = value?.runtimeSetup?.state;
  const preparation = state === 'preparing' ? '初期準備中です。実行環境と初期モデルの導入が終わるまで文字起こしを開始できません。'
    : state === 'incomplete' ? '初期準備が一部未完了です。管理画面の実行環境とモデルのエラーを確認してください。'
    : state === 'complete' ? '初期準備が完了しました。クライアントから利用できます。'
    : '実行環境とモデルの導入状況は管理画面で確認してください。';
  return {urls, localUrl, auth, preparation, https: urls.some(url => url.startsWith('https:')) || localUrl.startsWith('https:')};
}
