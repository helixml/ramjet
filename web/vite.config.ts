import { defineConfig, loadEnv } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"
import path from "node:path"

export default defineConfig(({ mode }) => {
  // Dev mode proxies /api and /metrics to a live load balancer's metrics
  // listener. The default lives in web/.env (node06 on the tailnet); a
  // shell UI_PROXY_TARGET wins over it for pointing at another box or a
  // locally running LB.
  const fileEnv = loadEnv(mode, __dirname, "")
  const proxyTarget =
    process.env.UI_PROXY_TARGET ??
    fileEnv.UI_PROXY_TARGET ??
    "http://127.0.0.1:9090"

  return {
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
      "/metrics": {
        target: proxyTarget,
        changeOrigin: true,
      },
    },
  },
  }
})
