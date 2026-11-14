import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    // Dev-mode proxy: lets `npm run dev` hit the Rust API without CORS at all.
    // The CORS layer on the backend covers the non-proxied case.
    proxy: {
      "/api": { target: "http://127.0.0.1:3000", changeOrigin: true },
    },
  },
  build: {
    // Served by the Rust server from web-dash/dist via fallback_service.
    outDir: "dist",
    emptyOutDir: true,
  },
});
