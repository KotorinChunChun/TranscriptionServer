import test from 'node:test';
import assert from 'node:assert/strict';
import {connectionGuide} from '../dist/connection-guide.mjs';

test('LAN guide advertises only usable server URLs and optional key', () => {
  const result = connectionGuide({network:{authMode:'lan', urls:['https://192.168.21.12:8769/', 'https://192.168.21.12:8769/', 'http://127.0.0.1:8769/', 'http://0.0.0.0:8769/', 'javascript:alert(1)', 'https://secret:key@example.com/']}, runtimeSetup:{state:'preparing'}}, 'https://127.0.0.1:8769');
  assert.deepEqual(result.urls, ['https://192.168.21.12:8769/']);
  assert.match(result.auth, /空欄/);
  assert.match(result.preparation, /初期準備中/);
  assert.equal(result.https, true);
});
test('local guide never presents a LAN URL and bearer requires configured key', () => {
  const local = connectionGuide({network:{authMode:'local', urls:['http://192.168.21.12:8769/']}}, 'http://127.0.0.1:8769');
  assert.deepEqual(local.urls, []);
  assert.match(local.auth, /同じPC/);
  assert.equal(local.localUrl, 'http://127.0.0.1:8769/');
  const bearer = connectionGuide({network:{authMode:'bearer', urls:['https://server.example:8769/']}, runtimeSetup:{state:'incomplete'}}, 'https://server.example:8769');
  assert.match(bearer.auth, /接続キー/);
  assert.match(bearer.preparation, /未完了/);
});
test('unknown server does not imply keyless access or completed preparation', () => {
  const result = connectionGuide(null, 'http://localhost:48010');
  assert.deepEqual(result.urls, []);
  assert.match(result.auth, /確認中/);
  assert.doesNotMatch(result.preparation, /完了しました/);
});
