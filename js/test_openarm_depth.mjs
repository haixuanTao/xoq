import { chromium } from "playwright";
import { createServer } from "vite";

const relay = "https://cdn.1ms.ai";
const depthPath = "anon/realsense";

const vite = await createServer({
  root: new URL("./examples", import.meta.url).pathname,
  server: { port: 0 },
  logLevel: "warn",
});
await vite.listen();
const port = vite.config.server.port || vite.httpServer.address().port;

const browser = await chromium.launch({ headless: false, args: ["--ignore-certificate-errors"] });
const page = await (await browser.newContext({ ignoreHTTPSErrors: true })).newPage();
page.on("console", (msg) => {
  const text = msg.text();
  if (msg.type() === "error") console.error(`[browser] ${text}`);
  else console.log(`[browser] ${text}`);
});
page.on("pageerror", (err) => console.error(`[browser error] ${err.message}`));

const url = `http://localhost:${port}/openarm.html?relay=${encodeURIComponent(relay)}&left=&right=&depth=${encodeURIComponent(depthPath)}`;
await page.goto(url);

// Wait for delayed auto-connect (3s) + connection + frames
console.log("Waiting 18s...");
await page.waitForTimeout(18000);

await page.screenshot({ path: "/tmp/openarm_test.png", fullPage: false });
console.log("Screenshot: /tmp/openarm_test.png");
await browser.close();
await vite.close();
