// One self-contained HTML file: every script and style inlined, nothing
// fetched at run time (the host frame's CSP allows no network).
import { defineConfig } from 'vite';
import { viteSingleFile } from 'vite-plugin-singlefile';

export default defineConfig({
  plugins: [viteSingleFile({ removeViteModuleLoader: true })],
  build: {
    target: 'es2022',
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    modulePreload: false,
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
  },
});
