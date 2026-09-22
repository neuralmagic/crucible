// Hand-authored (this repo doesn't pull in vite/client): Vite's `?url` suffix turns an asset import
// into its emitted same-origin URL string. Declared per-module, matching the .module.css.d.ts style.
declare module '@duckdb/duckdb-wasm/dist/duckdb-eh.wasm?url' {
  const url: string;
  export default url;
}
declare module '@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js?url' {
  const url: string;
  export default url;
}
