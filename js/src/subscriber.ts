import * as Moq from "@moq/lite";
import type { SubscriberConfig } from "./types.js";

export class Subscriber {
  private config: SubscriberConfig;
  private connection: Awaited<ReturnType<typeof Moq.Connection.connect>> | null = null;
  private running = false;

  constructor(config: SubscriberConfig) {
    this.config = {
      trackName: "serial",
      priority: 0,
      ...config,
    };
  }

  async start(): Promise<void> {
    if (this.running) {
      throw new Error("Subscriber is already running");
    }

    this.running = true;

    const { url, path, trackName, priority, onData, onError, onClose } = this.config;

    try {
      // Connect to MoQ relay
      this.connection = await Moq.Connection.connect(new URL(url));

      // Subscribe to the broadcast
      const broadcast = this.connection.consume(Moq.Path.from(path));

      // Subscribe to the track
      const track = broadcast.subscribe(trackName!, priority!);

      // Read data as it arrives
      while (this.running) {
        const group = await track.nextGroup();
        if (!group) break;

        while (this.running) {
          const frame = await group.readFrame();
          if (!frame) break;

          onData(frame);
        }
      }

      onClose?.();
    } catch (error) {
      onError?.(error instanceof Error ? error : new Error(String(error)));
    }
  }

  async stop(): Promise<void> {
    this.running = false;
    await this.connection?.close();
    this.connection = null;
  }
}
