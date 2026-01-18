import { Subscriber } from "../src/index.js";

async function main() {
  const decoder = new TextDecoder();

  const subscriber = new Subscriber({
    url: "https://localhost:4443",
    path: "xoq",
    trackName: "serial",
    onData: (data) => {
      // Log raw bytes or decode as text
      console.log("Received:", decoder.decode(data));
    },
    onError: (error) => {
      console.error("Error:", error);
    },
    onClose: () => {
      console.log("Connection closed");
    },
  });

  console.log("Starting subscriber...");
  await subscriber.start();
}

main().catch(console.error);
