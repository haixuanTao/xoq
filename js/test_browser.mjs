// Playwright test: opens a headed Chrome, loads MoQ test page via Vite, pipes console to terminal.
// Usage: node test_browser.mjs [sub|pub] [--relay URL] [--path PATH] [--track TRACK]
import { chromium } from "playwright";
import { createServer } from "vite";

// Parse args
const args = process.argv.slice(2);
let mode = "sub";
let relay = "https://172.18.133.111:4443";
let path = "anon/xoq-test";
let track = "video";

for (let i = 0; i < args.length; i++) {
  if (args[i] === "--relay") relay = args[++i];
  else if (args[i] === "--path") path = args[++i];
  else if (args[i] === "--track") track = args[++i];
  else if (!args[i].startsWith("--")) mode = args[i];
}

// Start Vite dev server
const vite = await createServer({
  root: new URL("./examples", import.meta.url).pathname,
  server: { port: 0 },  // auto-pick port
  logLevel: "warn",
});
await vite.listen();
const port = vite.config.server.port || vite.httpServer.address().port;
console.log(`Vite dev server on http://localhost:${port}`);

// Launch headed Chrome with WebTransport flags
const browser = await chromium.launch({
  headless: false,
  args: [
    "--ignore-certificate-errors",
    "--origin-to-force-quic-on=" + new URL(relay).host,
  ],
});

const context = await browser.newContext({ ignoreHTTPSErrors: true });
const page = await context.newPage();

// Pipe browser console to terminal
page.on("console", (msg) => {
  const type = msg.type();
  const text = msg.text();
  if (type === "error") console.error(`[browser] ${text}`);
  else if (type === "warning") console.warn(`[browser] ${text}`);
  else console.log(`[browser] ${text}`);
});

page.on("pageerror", (err) => console.error(`[browser error] ${err.message}`));

// Navigate
const page_file = mode === "camera" ? "camera.html" : "moq_test.html";
const url = `http://localhost:${port}/${page_file}?relay=${encodeURIComponent(relay)}&path=${encodeURIComponent(path)}&track=${encodeURIComponent(track)}`;
console.log(`Opening ${url}`);
await page.goto(url);

// Keep running until Ctrl+C
console.log("Browser open. Press Ctrl+C to stop.");
process.on("SIGINT", async () => {
  console.log("\nShutting down...");
  await browser.close();
  await vite.close();
  process.exit(0);
});

// Keep process alive
await new Promise(() => {});
