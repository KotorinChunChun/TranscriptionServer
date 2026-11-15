import test from 'node:test';
import assert from 'node:assert/strict';
import {referenceBenchmarks, referenceSpeedText, referenceFillerText, referenceLoadText} from '../dist/model-guidance.mjs';

test('reference speed preserves median of run ratios instead of recomputing from medians', () => {
  const model = {id: 'whisper/tiny', engine: 'whisper'};
  const sample = referenceBenchmarks.models[model.id];
  assert.equal(Object.keys(referenceBenchmarks.models).length, 12);
  assert.equal(referenceSpeedText(model), '17.4文字/s');
  assert.notEqual(sample.charactersPerSecond.toFixed(1), (sample.characters / sample.runSeconds).toFixed(1));
  assert.equal(sample.repetitions, 3);
});

test('zero filler candidates, unmeasured models and non-text models remain distinct', () => {
  assert.equal(referenceFillerText({id: 'whisper/small'}), '0回（0.0回/千文字）');
  assert.equal(referenceFillerText({id: 'not-measured'}), '未測定');
  assert.equal(referenceSpeedText({id: 'not-measured'}), '未測定');
  assert.equal(referenceFillerText({engine: 'auxiliary'}), '対象外（文字出力なし）');
  assert.match(referenceFillerText({id: 'whisper/base'}, true), /中央値・範囲 0～2回/);
  assert.equal(referenceBenchmarks.models['whisper/small'].filler.audioVerified, false);
});

test('nonresident Nemotron load stays included, while resident load is separate', () => {
  const id = 'nemotron-asr/nemotron-3.5-asr-streaming-0.6b';
  assert.equal(referenceSpeedText({id}), '10.1文字/s（ロード込み）');
  assert.equal(referenceLoadText(referenceBenchmarks.models[id]), '処理時間に含む（区間ごとにロード）');
  assert.equal(referenceLoadText(referenceBenchmarks.models['whisper/tiny']), '1.04秒');
  assert.equal(referenceLoadText(null), '未測定');
});
