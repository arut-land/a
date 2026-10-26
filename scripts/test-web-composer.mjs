import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

// Execute the expression the chat page installs, rather than a second reducer.
const source = readFileSync(new URL('../apps/web/src/view.rs', import.meta.url), 'utf8');
const expression = source.match(/data-effect="([^"]+)"/)[1];
const apply = new Function('$draft', '$admitted', `${expression}; return { draft: $draft, admitted: $admitted };`);

test('admission clears only the submitted draft', () => {
  assert.deepEqual(apply('submitted', 'submitted'), { draft: '', admitted: null });
  assert.deepEqual(apply('edited while pending', 'submitted'), {
    draft: 'edited while pending', admitted: null,
  });
});

test('rejection and consumed acknowledgements preserve subsequent drafts', () => {
  assert.deepEqual(apply('submitted', null), { draft: 'submitted', admitted: null });
  const acknowledged = apply('new text', 'submitted');
  assert.deepEqual(apply('submitted', acknowledged.admitted), {
    draft: 'submitted', admitted: null,
  });
});
