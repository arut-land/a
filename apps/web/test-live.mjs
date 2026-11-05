import assert from 'node:assert/strict';
import { launchBrowser } from './test-browser.mjs';

const replaceConversations = () => {
  const list = document.getElementById('conversations');
  list.replaceWith(list.cloneNode(true));
};

const browser = await launchBrowser();
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.stack));
  await page.goto(process.argv[2]);
  await page.getByRole('button', { name: 'New session' }).click();
  await page.locator('#end').waitFor({ state: 'attached' });
  const parentUrl = page.url();
  assert.deepEqual(errors, [], 'opening a live chat raised a browser exception');

  const draft = page.getByRole('textbox');
  const send = page.getByRole('button', { name: 'Send', exact: true });
  assert.equal(await draft.evaluate(element => element.closest('form').parentElement.contains(document.getElementById('conversations'))), false, 'the picker is outside the composer bar');
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
  assert.deepEqual(await page.locator('[data-conversation][tabindex="0"]').evaluateAll(rows => rows.map(row => row.dataset.conversation)), [selectedChild], 'the viewed child is the initial tab stop');
  await page.locator(`[data-conversation="${selectedChild}"]`).focus();
  await draft.click();
  await page.keyboard.type('queued child message');
  assert.equal(await draft.inputValue(), 'queued child message', 'clicking the composer leaves picker focus and receives typing');
  await send.click();
  await page.locator('#queued-inputs').getByText('Queued: queued child message', {exact: false}).waitFor();
  // a successful withdrawal never replaces text typed while confirmation is pending
  await page.route('**/api/queue/edit', async route => {
    const response = await route.fetch();
    await draft.fill('newer draft while withdrawal waits');
    await route.fulfill({response});
  });
  const edited = page.waitForResponse(response => response.url().endsWith('/api/queue/edit'));
  await page.locator('#queued-inputs').getByRole('button', {name: 'Edit', exact: true}).click();
  await (await edited).finished();
  await page.waitForFunction(() => !document.querySelector('[data-edit-queued]'));
  assert.equal(await draft.inputValue(), 'newer draft while withdrawal waits', 'successful_withdrawals_never_overwrite_a_newer_composer_revision');
  await page.unroute('**/api/queue/edit');
  await page.getByRole('button', {name: 'Restore draft: queued child message', exact: true}).click();
  assert.equal(await draft.inputValue(), 'queued child message');
  await page.getByRole('button', {name: 'Restore draft: newer draft while withdrawal waits', exact: true}).click();
  assert.equal(await draft.inputValue(), 'newer draft while withdrawal waits');
  await draft.fill('');
  await draft.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('x');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.textContent.includes('interrupted'), selectedChild);
  await page.keyboard.press('Escape');
  assert.equal(await draft.evaluate(element => element === document.activeElement), true);
  await draft.fill('sleep 30');
  await send.click();
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.dataset.stop === 'true', selectedChild);
  await page.getByRole('link', {name: '← Back to parent'}).click();
  await page.waitForFunction(() => document.querySelectorAll('[data-conversation]').length === 3);
  await page.getByText(`The user resumed agent ${selectedChild}. It is running now.`, {exact: false}).first().waitFor();
  await page.locator(`#conversations [data-stop-conversation="${selectedChild}"]`).click();
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.textContent.includes('interrupted'), selectedChild);
  assert.equal(await page.locator('[data-conversation]').count(), 3, 'an interrupted child retains its terminal grace interval');
  const family = await page.locator('[data-conversation]').evaluateAll(rows => rows.map(row => ({id: row.dataset.conversation, href: row.href})));
  await draft.focus();
  await draft.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  assert.equal(await page.evaluate(() => document.activeElement.dataset.conversation), family[2].id);
  assert.deepEqual(await page.locator('[data-conversation][tabindex="0"]').evaluateAll(rows => rows.map(row => row.dataset.conversation)), [family[2].id]);
  await page.evaluate(expected => {
    window.rovingViolations = [];
    new MutationObserver(() => {
      const focus = document.activeElement?.dataset.conversation;
      const stops = [...document.querySelectorAll('[data-conversation][tabindex="0"]')].map(row => row.dataset.conversation);
      if (focus !== expected || stops.length !== 1 || stops[0] !== expected) window.rovingViolations.push({focus, stops});
    }).observe(document.querySelector('#conversations').parentNode, {subtree: true, childList: true, attributes: true, attributeFilter: ['tabindex']});
  }, family[2].id);
  const branch = await context.newPage();
  await branch.goto(family[1].href);
  await branch.locator('#composer').fill('spawn nested web child');
  await branch.getByRole('button', {name: 'Send', exact: true}).click();
  await page.waitForFunction(() => document.querySelectorAll('[data-conversation]').length === 4);
  assert.equal(await page.evaluate(() => document.activeElement.dataset.conversation), family[2].id, 'live insertion must retain focus by session id');
  assert.equal(await page.locator(`[data-conversation="${family[2].id}"]`).getAttribute('tabindex'), '0');
  assert.deepEqual(await page.evaluate(() => window.rovingViolations), [], 'every live update keeps focus and the sole tab stop on the same session');
  await page.keyboard.press('Enter');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.getAttribute('aria-selected') === 'true', family[2].id);
  await page.locator(`[data-conversation="${family[1].id}"]`).focus();
  await page.evaluate(replaceConversations);
  assert.equal(await page.evaluate(() => document.activeElement.dataset.conversation), family[1].id, 'DOM replacement restores owned focus by session id');
  await page.locator('#transcript').getByText('$ sleep 30', {exact: true}).first().click();
  assert.equal(await page.evaluate(() => document.activeElement === document.body), true, 'clicking transcript text releases focus');
  await page.evaluate(replaceConversations);
  assert.equal(await page.evaluate(() => document.activeElement === document.body), true, 'a list update must not reclaim deliberately released focus');
  assert.deepEqual(await page.locator('[data-conversation][tabindex="0"]').evaluateAll(rows => rows.map(row => row.dataset.conversation)), [family[1].id], 'releasing focus preserves the roving tab stop');
  await branch.close();
  await page.goto(parentUrl);
  await page.locator('#end').waitFor({state: 'attached'});
  await draft.fill('spawn expiry child');
  await send.click();
  const expiryRow = page.locator('[data-conversation]').filter({hasText: 'expiry child'});
  await expiryRow.waitFor();
  const expiredChild = await expiryRow.getAttribute('data-conversation');
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.textContent.includes('finished'), expiredChild);
  if (process.env.A_EVIDENCE_DIR) await page.screenshot({path: `${process.env.A_EVIDENCE_DIR}/web-footer.png`, fullPage: true});
  await expiryRow.waitFor({state: 'detached', timeout: 8000});
  const transcriptLink = page.locator(`#transcript a[href="/s/${expiredChild}"]`);
  await transcriptLink.click();
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.getAttribute('aria-selected') === 'true', expiredChild);
  assert.equal(await draft.isEnabled(), true, 'an expired child reopens with its ordinary composer');
  await page.getByRole('link', {name: '← Back to parent'}).click();
  await page.waitForFunction(id => !document.querySelector(`[data-conversation="${id}"]`), expiredChild);
  await page.locator(`#transcript a[href="/s/${expiredChild}"]`).click();
  await draft.fill('$ sleep 30');
  await send.click();
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.dataset.stop === 'true', expiredChild);
  await page.locator(`#conversations [data-stop-conversation="${expiredChild}"]`).click();
  await page.waitForFunction(id => document.querySelector(`[data-conversation="${id}"]`)?.textContent.includes('interrupted'), expiredChild);
  assert.deepEqual(errors, [], 'live chat raised a browser exception');
} finally {
  await browser.close();
}
