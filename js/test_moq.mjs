import * as Moq from "@moq/lite";

const url = new URL("https://172.18.133.111:4443/anon/xoq-test");
console.log("Connecting to", url.toString());

const conn = await Moq.Connection.connect(url);
console.log("Connected!");

const broadcast = conn.consume(Moq.Path.from("anon/xoq-test"));
const track = broadcast.subscribe("video");
console.log("Subscribed to track 'video', waiting for data...");

const group = await track.nextGroup();
if (group) {
  const frame = await group.readFrame();
  if (frame) {
    console.log("Got frame:", new TextDecoder().decode(frame));
  }
}

await conn.close();
