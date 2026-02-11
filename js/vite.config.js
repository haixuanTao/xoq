import { resolve } from "path";
import { defineConfig } from "vite";

export default defineConfig({
  root: "examples",
  base: "/",
  build: {
    outDir: "../dist-pages",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        index: resolve(__dirname, "examples/index.html"),
        openarm: resolve(__dirname, "examples/openarm.html"),
        camera: resolve(__dirname, "examples/camera.html"),
        publish: resolve(__dirname, "examples/publish.html"),
        subscribe: resolve(__dirname, "examples/subscribe.html"),
        urdf_viewer: resolve(__dirname, "examples/urdf_viewer.html"),
        video_player: resolve(__dirname, "examples/video_player.html"),
      },
    },
  },
});
