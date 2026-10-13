import { defineConfig } from "vite-plus";
import { quillContent } from "./website/content-plugin.mjs";

export default defineConfig({
  appType: "mpa",
  publicDir: false,
  plugins: [quillContent()],
  server: {
    host: "0.0.0.0",
    port: 4173,
    strictPort: true,
    headers: { "Cache-Control": "no-store" },
    watch: { ignored: ["**/target/**", "**/.git/**", "**/.wrangler/**"] },
  },
  preview: { host: "0.0.0.0", port: 4173, strictPort: true },
  build: {
    outDir: "target/website",
    emptyOutDir: true,
    rollupOptions: {
      input: [
        "index.html",
        "compatibility.html",
        "project.html",
        "document.html",
      ],
    },
  },
});
