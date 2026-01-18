import { Publisher } from "../src/index.js";

async function main() {
  // Request serial port from user
  const port = await navigator.serial.requestPort();
  await port.open({ baudRate: 1_000_000 });

  const publisher = new Publisher({
    url: "https://localhost:4443",
    path: "xoq",
    serial: port,
    trackName: "serial",
    groupSize: 1024,
  });

  console.log("Starting publisher...");
  await publisher.start();
}

main().catch(console.error);
