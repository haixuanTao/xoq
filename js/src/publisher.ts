import * as Moq from "@moq/lite";
import type { PublisherConfig } from "./types.js";

export class Publisher {
  private config: PublisherConfig;
  private connection: Awaited<ReturnType<typeof Moq.Connection.connect>> | null = null;
  private broadcast: Moq.Broadcast | null = null;
  private abortController: AbortController | null = null;
  private running = false;

  constructor(config: PublisherConfig) {
    this.config = {
      trackName: "serial",
      groupSize: 1024,
      ...config,
    };
  }

  async start(): Promise<void> {
    if (this.running) {
      throw new Error("Publisher is already running");
    }

    this.running = true;
    this.abortController = new AbortController();

    const { serial, url, path, trackName, groupSize } = this.config;

    // Connect to MoQ relay
    this.connection = await Moq.Connection.connect(new URL(url));
    this.broadcast = new Moq.Broadcast();

    // Publish the broadcast
    this.connection.publish(Moq.Path.from(path), this.broadcast);

    // Handle subscription requests
    this.handleRequests(trackName!);

    // Start reading from serial port
    await this.readSerial(serial, trackName!, groupSize!);
  }

  private async handleRequests(trackName: string): Promise<void> {
    if (!this.broadcast) return;

    try {
      for (;;) {
        const request = await this.broadcast.requested();
        if (!request || !this.running) break;

        if (request.track.name === trackName) {
          // Track will be written to by readSerial
          this.publishToTrack(request.track);
        } else {
          request.track.close(new Error("track not found"));
        }
      }
    } catch {
      // Connection closed
    }
  }

  private activeTrack: Moq.Track | null = null;

  private publishToTrack(track: Moq.Track): void {
    this.activeTrack = track;
  }

  private async readSerial(
    serial: SerialPort,
    _trackName: string,
    groupSize: number
  ): Promise<void> {
    const reader = serial.readable?.getReader();
    if (!reader) {
      throw new Error("Serial port is not readable");
    }

    let currentGroup: Moq.Group | null = null;
    let bytesInGroup = 0;

    try {
      while (this.running) {
        const { value, done } = await reader.read();
        if (done || this.abortController?.signal.aborted) {
          break;
        }

        if (value && this.activeTrack) {
          // Create new group if needed
          if (!currentGroup || bytesInGroup >= groupSize) {
            currentGroup?.close();
            currentGroup = this.activeTrack.appendGroup();
            bytesInGroup = 0;
          }

          // Write data to current group
          currentGroup.writeFrame(value);
          bytesInGroup += value.byteLength;
        }
      }

      // Clean up
      currentGroup?.close();
      this.activeTrack?.close();
    } finally {
      reader.releaseLock();
    }
  }

  async stop(): Promise<void> {
    this.running = false;
    this.abortController?.abort();
    this.abortController = null;

    this.broadcast?.close();
    this.broadcast = null;

    await this.connection?.close();
    this.connection = null;
  }
}
