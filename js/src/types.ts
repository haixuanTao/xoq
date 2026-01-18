export interface XoqConfig {
  url: string;
  path: string;
}

export interface PublisherConfig extends XoqConfig {
  serial: SerialPort;
  trackName?: string;
  groupSize?: number;
}

export interface SubscriberConfig extends XoqConfig {
  trackName?: string;
  priority?: number;
  onData: (data: Uint8Array) => void;
  onError?: (error: Error) => void;
  onClose?: () => void;
}
