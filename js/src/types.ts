export interface WserConfig {
  url: string;
  path: string;
}

export interface PublisherConfig extends WserConfig {
  serial: SerialPort;
  trackName?: string;
  groupSize?: number;
}

export interface SubscriberConfig extends WserConfig {
  trackName?: string;
  priority?: number;
  onData: (data: Uint8Array) => void;
  onError?: (error: Error) => void;
  onClose?: () => void;
}
