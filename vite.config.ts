import { defineConfig } from "vite";

// Tauri expects a fixed dev port and should not clear the screen on errors.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    host: host || "localhost",
    port: 1420,
    strictPort: true,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "esnext",
    minify: "esbuild",
    sourcemap: false,
  },
});
