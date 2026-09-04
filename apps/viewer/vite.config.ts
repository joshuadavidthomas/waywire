import tailwindcss from "@tailwindcss/vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

export default defineConfig({
  base: "/",
  plugins: [tailwindcss(), svelte()],
  build: {
    assetsDir: "assets",
    emptyOutDir: true,
    outDir: "../../bridge/web",
  },
});
