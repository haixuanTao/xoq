import { chromium } from 'playwright';

const browser = await chromium.launch({
  headless: false,
  args: ['--use-gl=angle'],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });

page.on('pageerror', err => console.log(`[PAGE ERROR] ${err.message}`));

await page.goto('http://localhost:5173/openarm.html');

await page.waitForFunction(() => {
  const logEl = document.getElementById('log');
  if (!logEl) return false;
  return logEl.innerText.includes('3D model loaded') || logEl.innerText.includes('load error');
}, { timeout: 60000 });

// Orbit the camera to get a better 3/4 view - use mouse drag on the canvas
const canvas = await page.$('#threeCanvas');
const box = await canvas.boundingBox();
const cx = box.x + box.width / 2;
const cy = box.y + box.height / 2;

// Scroll to zoom out a bit
await page.mouse.move(cx, cy);
for (let i = 0; i < 5; i++) {
  await page.mouse.wheel(0, 200);
  await page.waitForTimeout(100);
}

// Orbit slightly (left-drag)
await page.mouse.move(cx, cy);
await page.mouse.down();
await page.mouse.move(cx + 100, cy + 50, { steps: 10 });
await page.mouse.up();

await page.waitForTimeout(2000);
await page.screenshot({ path: '/tmp/openarm_orbited.png' });

console.log('Orbited screenshot saved');
await browser.close();
