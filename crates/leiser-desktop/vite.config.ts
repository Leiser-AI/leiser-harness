import { defineConfig } from "vite";

// Tauri loads the UI from this dev server (`devUrl` in tauri.conf.json)
// and embeds `dist` in release builds (`frontendDist`).
export default defineConfig({
  root: "web",
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { outDir: "../dist", emptyOutDir: true, target: "es2022" },
});
