import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const version = '1.64.1';
const directory = resolve(
  fileURLToPath(new URL('../../', import.meta.url)),
  process.env.CARGO_TARGET_DIR ?? join(tmpdir(), 'a-web-tests'),
  `playwright-${version}`);
const modulePath = join(directory, 'node_modules/playwright-core/index.mjs');
const chrome = [
  '/usr/bin/google-chrome',
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
].find(path => existsSync(path));
process.env.PLAYWRIGHT_BROWSERS_PATH ??= join(directory, 'browsers');

export async function launchBrowser() {
  const { chromium } = await import(pathToFileURL(modulePath));
  return chromium.launch(chrome ? { executablePath: chrome } : {});
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  if (!existsSync(modulePath)) {
    const installed = spawnSync('npm', [
      'install', '--prefix', directory, '--no-save', '--package-lock=false',
      '--ignore-scripts', '--no-audit', '--no-fund', `playwright-core@${version}`,
    ], { stdio: 'inherit' });
    if (installed.error) throw installed.error;
    if (installed.status !== 0) process.exit(installed.status ?? 1);
  }
  if (!chrome) {
    const installed = spawnSync(process.execPath, [
      join(directory, 'node_modules/playwright-core/cli.js'), 'install', 'chromium',
    ], { stdio: 'inherit' });
    if (installed.error) throw installed.error;
    if (installed.status !== 0) process.exit(installed.status ?? 1);
  }
  console.log(`web browser: ${chrome ?? 'pinned Playwright Chromium'}`);
}
