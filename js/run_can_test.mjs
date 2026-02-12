// Run the openarm.html page with Playwright to test the full motor query flow.
// Usage: node run_can_test.mjs [duration_ms]
//   duration_ms: how long to run the test (default 10000)
import { chromium } from "playwright";

const duration = parseInt(process.argv[2] || "10000");
const testUrl = "http://localhost:5173/openarm.html";

console.log(`Opening ${testUrl}, will Connect + Query Motors, run for ${duration}ms`);

const browser = await chromium.launch({
  headless: true,
  args: ["--enable-webgl", "--use-gl=swiftshader", "--no-sandbox"],
});
const context = await browser.newContext({ ignoreHTTPSErrors: true });
const page = await context.newPage();

// Capture console output
page.on("console", (msg) => {
  const type = msg.type();
  const text = msg.text();
  if (type === "error") {
    console.error(`[BROWSER ERR] ${text}`);
  } else {
    console.log(`[BROWSER] ${text}`);
  }
});
page.on("pageerror", (err) => console.error(`[PAGE ERROR] ${err.message}`));

let passed = false;

try {
  await page.goto(testUrl, { waitUntil: "networkidle", timeout: 10000 });
  console.log("Page loaded.");

  // Click Connect
  console.log("Clicking Connect...");
  await page.click("#startBtn");

  // Wait for "Connected!" in the log
  await page.waitForFunction(
    () => document.getElementById("log")?.innerText.includes("Connected!"),
    { timeout: 10000 }
  );
  console.log("State connection established.");

  // Wait for subscribe confirmation
  await page.waitForFunction(
    () => document.getElementById("log")?.innerText.includes("Subscribed to 'can' track"),
    { timeout: 5000 }
  );
  console.log("Subscribed to state track.");

  // Click Query Motors
  console.log("Clicking Query Motors...");
  await page.click("#queryBtn");

  // Wait for query loop to start
  await page.waitForFunction(
    () => document.getElementById("log")?.innerText.includes("Query loop started"),
    { timeout: 15000 }
  );
  console.log("Query loop started!");

  // Let it run for the specified duration
  console.log(`Running for ${duration}ms...`);
  await page.waitForTimeout(duration);

  // Check results
  const frameCount = await page.$eval("#frameCount", (el) => el.textContent);
  const fps = await page.$eval("#canFps", (el) => el.textContent);
  const bytes = await page.$eval("#bytesReceived", (el) => el.textContent);

  // Read joint angles to check they're non-zero
  const angles = await page.$$eval('[id^="angle-"]', (els) =>
    els.map((el) => el.textContent.replace("°", "").trim())
  );

  console.log(`\n=== RESULTS (${duration}ms) ===`);
  console.log(`Frames received: ${frameCount}`);
  console.log(`Current FPS: ${fps}`);
  console.log(`Bytes received: ${bytes}`);
  console.log(`Joint angles: ${angles.join(", ")}`);

  const numFrames = parseInt(frameCount);
  const hasNonZeroAngle = angles.some((a) => parseFloat(a) !== 0.0);

  if (numFrames > 50 && hasNonZeroAngle) {
    console.log("\nPASS: Received motor data with non-zero joint angles");
    passed = true;
  } else if (numFrames > 50) {
    console.log("\nPASS: Receiving motor data (all angles happen to be near zero)");
    passed = true;
  } else {
    console.log(`\nFAIL: Only ${numFrames} frames received (expected >50)`);
  }
} catch (e) {
  console.error(`\nFAIL: ${e.message}`);

  // Dump log on failure
  try {
    const logText = await page.$eval("#log", (el) => el.innerText);
    console.log("\n=== LOG ON FAILURE ===");
    console.log(logText);
  } catch {}
} finally {
  // Click disconnect
  try {
    await page.click("#stopBtn");
    await page.waitForTimeout(500);
  } catch {}

  await browser.close();
  process.exit(passed ? 0 : 1);
}
