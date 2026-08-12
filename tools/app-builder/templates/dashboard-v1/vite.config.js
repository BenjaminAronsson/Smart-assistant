// Locked build config for the `dashboard/v1` template (F6.2, ADR-029).
//
// Single-file output is a security property, not a packaging preference: one
// document means no archive to extract, no path traversal, no second origin
// surface, and a CAS blob that is exactly what F6.4 serves (see the F6.2 threat
// note #2). Everything is inlined, and nothing may be fetched at runtime.
import { defineConfig } from "vite";
import { viteSingleFile } from "vite-plugin-singlefile";

export default defineConfig({
  plugins: [viteSingleFile()],
  build: {
    // A generated app is disposable and read at most once by a human; a source
    // map would only enlarge the blob.
    sourcemap: false,
    // Inline every asset regardless of size — an emitted asset file would be an
    // external subresource, which the host refuses.
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    cssCodeSplit: false,
    // Targets the browser the shell already requires; no legacy polyfill bundle.
    target: "es2022",
    reportCompressedSize: false,
  },
});
