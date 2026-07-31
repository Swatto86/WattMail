import { defineConfig } from "vite";
import { resolve } from "path";

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
  // Multi-page: the app shell (index.html) and the pop-out message window
  // (message.html) are separate entry points sharing the same src modules.
  build: {
    target: "esnext",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        message: resolve(__dirname, "message.html"),
      },
    },
  },
});
