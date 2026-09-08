import { defineConfig } from "vite";

export default defineConfig({
  base: "/",
  resolve: {
    alias: {
      "comparison-recorder": new URL(
        "../../probes/compare/recorder.mjs",
        import.meta.url,
      ).pathname,
    },
  },
  build: {
    assetsDir: "assets",
    emptyOutDir: true,
    outDir: "dist",
  },
});
