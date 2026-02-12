import { chromium } from 'playwright';

const browser = await chromium.launch({
  headless: false,
  args: ['--use-gl=angle', '--ignore-certificate-errors', '--origin-to-force-quic-on=172.18.133.111:4443'],
});
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });

page.on('pageerror', err => console.log(`[PAGE ERROR] ${err.message}`));
page.on('console', msg => console.log(`[CONSOLE] ${msg.text()}`));

await page.goto('http://localhost:5173/examples/openarm.html?relay=http://172.18.133.111:4443&depth=anon/realsense');

// Wait for page to load
await page.waitForFunction(() => {
  const logEl = document.getElementById('log');
  if (!logEl) return false;
  return logEl.innerText.includes('3D model loaded') || logEl.innerText.includes('load error');
}, { timeout: 30000 });

// Page auto-connects from query params — just wait for data to flow
await page.waitForTimeout(15000);

await page.screenshot({ path: '/tmp/openarm_depth_screenshot.png' });

// Get the log text
const logs = await page.evaluate(() => {
  const logEl = document.getElementById('log');
  return logEl ? logEl.innerText : 'no log element';
});
console.log('=== LOG OUTPUT ===');
console.log(logs);

await browser.close();
