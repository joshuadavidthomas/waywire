declare module "@novnc/novnc" {
  interface RFBOptions {
    shared?: boolean;
    credentials?: {
      username?: string;
      password?: string;
      target?: string;
    };
    repeaterID?: string;
    wsProtocols?: string[];
  }

  interface RFBEventMap {
    connect: CustomEvent<Record<string, never>>;
    disconnect: CustomEvent<{ clean: boolean }>;
  }

  export default class RFB {
    constructor(
      target: HTMLElement,
      urlOrChannel: string | WebSocket | RTCDataChannel,
      options?: RFBOptions,
    );

    scaleViewport: boolean;
    clipViewport: boolean;
    resizeSession: boolean;

    addEventListener<K extends keyof RFBEventMap>(
      type: K,
      listener: (event: RFBEventMap[K]) => void,
    ): void;

    sendCtrlAltDel(): void;
    clipboardPasteFrom(text: string): void;
    disconnect(): void;
  }
}
