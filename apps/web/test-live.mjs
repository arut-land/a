import assert from 'node:assert/strict';
import { launchBrowser } from './test-browser.mjs';

const browser = await launchBrowser();
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.stack));
  await page.goto(process.argv[2]);
  await page.getByRole('button', { name: 'New session' }).click();
  await page.locator('#end').waitFor({ state: 'attached' });
  assert.deepEqual(errors, [], 'opening a live chat raised a browser exception');

  const draft = page.getByRole('textbox');
  const send = page.getByRole('button', { name: 'Send', exact: true });
  // Hold the real admission response so an edit can race it deterministically.
  await page.route('**/api/send', async route => {
    const response = await route.fetch();
    await draft.fill('edited while pending');
    await route.fulfill({ response });
  });
  await draft.fill('first echo');
  const admitted = page.waitForResponse(response => response.url().endsWith('/api/send'));
  await send.click();
  await (await admitted).finished();
  await page.waitForFunction(() =>
    document.querySelector('#transcript').textContent.split('first echo').length === 3);
  assert.equal(await draft.inputValue(), 'edited while pending');
  await page.unroute('**/api/send');

  // Reusing an acknowledged draft must still clear it on the next admission.
  for (const [text, occurrences] of [['first echo', 4], ['small live echo', 2]]) {
    await draft.fill(text);
    await send.click();
    await page.waitForFunction(() => document.querySelector('input[name=text]').value === '');
    await page.waitForFunction(
      ([t, count]) => document.querySelector('#transcript').textContent.split(t).length === count + 1,
      [text, occurrences]);
  }
  for (let child = 0; child < 2; child++) {
    await draft.fill('call background_task {"prompt":"$ sleep 30","messages":[]}');
    await send.click();
    await page.waitForFunction(count => document.querySelectorAll('[data-conversation][data-stop="true"]').length === count, child + 1);
    await page.waitForFunction(() => document.querySelector('#composer').value === '');
  }
  await draft.focus();
  await draft.press('ArrowDown');
  assert.equal(await page.locator('[data-conversation]').first().evaluate(element => element === document.activeElement), true);
  await page.keyboard.press('ArrowDown');
  const selectedChild = await page.evaluate(() => document.activeElement.dataset.conversation);
  await page.keyboard.press('Enter');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.getAttribute('aria-selected') === 'true', selectedChild);
  await page.waitForFunction(() => document.querySelector('#composer').placeholder.startsWith('Message @'));
  assert.equal(await page.getByRole('link', {name: '← Back to parent'}).count(), 1);
  assert.equal(await page.locator('[data-conversation]').count(), 3);
  await draft.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('x');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.textContent.includes('interrupted'), selectedChild);
  await page.keyboard.press('Escape');
  assert.equal(await draft.evaluate(element => element === document.activeElement), true);
  await draft.fill('follow up from the child composer');
  await send.click();
  await page.waitForFunction(() => document.querySelector('#transcript').textContent.split('follow up from the child composer').length === 3);
  await page.getByRole('link', {name: '← Back to parent'}).click();
  await page.waitForFunction(() => document.querySelectorAll('[data-conversation]').length === 3);
  await page.locator('#conversations [data-stop-conversation]').click();
  await page.waitForFunction(() => !document.querySelector('[data-conversation][data-stop="true"]'));
  assert.equal(await page.locator('[data-conversation]').count(), 3, 'finished children stay listed');
  const family = await page.locator('[data-conversation]').evaluateAll(rows => rows.map(row => ({id: row.dataset.conversation, href: row.href})));
  await draft.focus();
  await draft.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  assert.equal(await page.evaluate(() => document.activeElement.dataset.conversation), family[2].id);
  const branch = await context.newPage();
  await branch.goto(family[1].href);
  await branch.locator('#composer').fill('spawn nested web child');
  await branch.getByRole('button', {name: 'Send', exact: true}).click();
  await page.waitForFunction(() => document.querySelectorAll('[data-conversation]').length === 4);
  assert.equal(await page.evaluate(() => document.activeElement.dataset.conversation), family[2].id, 'live insertion must retain focus by session id');
  assert.equal(await page.locator(`[data-conversation="${family[2].id}"]`).getAttribute('tabindex'), '0');
  await page.keyboard.press('Enter');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.getAttribute('aria-selected') === 'true', family[2].id);
  await branch.close();
  assert.deepEqual(errors, [], 'live chat raised a browser exception');
} finally {
  await browser.close();
}
