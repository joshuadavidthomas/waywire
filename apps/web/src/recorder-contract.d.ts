declare module "comparison-recorder" {
  export interface ComparisonRecorder {
    recordStats(stats: import("./sdk/waymote.ts").WaymoteStats): void;
  }

  export function installRecorder(options: {
    readonly transport: "rust";
    readonly canvas: () => HTMLCanvasElement;
  }): ComparisonRecorder;
}
