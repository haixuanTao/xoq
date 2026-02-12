import { chromium } from "playwright";
import { createServer } from "vite";

const vite = await createServer({
  root: new URL("./examples", import.meta.url).pathname,
  server: { port: 0 },
  logLevel: "warn",
});
await vite.listen();
const port = vite.config.server.port || vite.httpServer.address().port;

const browser = await chromium.launch({ headless: false, args: ["--ignore-certificate-errors"] });
const page = await (await browser.newContext({ ignoreHTTPSErrors: true })).newPage();
page.on("console", (msg) => console.log(`[browser] ${msg.text()}`));
page.on("pageerror", (err) => console.error(`[browser ERROR] ${err.message}`));

await page.goto(`http://localhost:${port}/test_minimal.html`);
console.log("Waiting 20s (3s delay + connection + frames)...");
await page.waitForTimeout(20000);
await browser.close();
await vite.close();
