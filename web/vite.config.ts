import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"
import path from "node:path"

// Dev mode proxies the machine-view API to a live load balancer's metrics
// listener. Point it anywhere with UI_PROXY_TARGET, e.g. an SSH tunnel to
// node06:  ssh -N -L 8007:127.0.0.1:8007 node06
//          UI_PROXY_TARGET=http://127.0.0.1:8007 npm run dev
const proxyTarget = process.env.UI_PROXY_TARGET ?? "http://127.0.0.1:9090"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // The production bundle is served by the LB under /ui/.
  base: "/ui/",
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    proxy: {
      "/api": {
        target: proxyTarget,
        changeOrigin: true,
      },
    },
  },
})
