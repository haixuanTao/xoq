import { chromium } from 'playwright';

const browser = await chromium.launch({
  args: ['--use-gl=angle', '--use-angle=metal', '--ignore-gpu-blocklist'],
  headless: false,
});
const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });

page.on('console', msg => {
  const t = msg.type();
  if (t !== 'debug') console.log(`[${t.toUpperCase()}] ${msg.text()}`);
});

await page.goto('http://localhost:5173/openarm.html');
console.log('Waiting 3s...');
await page.waitForTimeout(3000);

// Fill in depth path and relay
await page.fill('#depthPath', 'anon/realsense');
await page.fill('#relayUrl', 'https://cdn.1ms.ai');

// Click Connect
await page.click('button:has-text("Connect")');
console.log('Clicked Connect, waiting 5s for connection...');
await page.waitForTimeout(5000);

// Click Query Motors
await page.click('button:has-text("Query")');
console.log('Clicked Query Motors, waiting 15s...');
await page.waitForTimeout(15000);

const state = await page.evaluate(() => {
  const log = document.getElementById('log');
  const frames = document.getElementById('frameCount');
  return {
    logs: log ? log.textContent.slice(-2000) : '',
    frames: frames ? frames.textContent : '?',
  };
});
console.log('\nLogs:', state.logs);
console.log('Frames:', state.frames);

await page.screenshot({ path: '/tmp/openarm_query.png' });
await browser.close();
