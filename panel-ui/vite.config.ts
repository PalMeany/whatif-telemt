import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// The bundle is embedded in the telemt binary and served from the panel's own
// origin, so assets stay relative and no external CDN is ever referenced.
export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    sourcemap: false,
    chunkSizeWarningLimit: 900,
  },
  server: {
    port: 5273,
    proxy: {
      "/panel/api": { target: "http://127.0.0.1:8443", changeOrigin: false },
    },
  },
});
