import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  base: "/m/",
  build: {
    outDir: "../public/m",
    emptyOutDir: true,
    target: "es2020",
  },
});
