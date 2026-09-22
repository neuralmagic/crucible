// Hand-authored (this repo doesn't pull in vite/client), matching src/explore/assets.d.ts: Vite's
// `?worker` suffix turns a module import into a Worker constructor for its own bundle.
declare module 'monaco-editor/editor/editor.worker.js?worker' {
  const WorkerFactory: { new (): Worker };
  export default WorkerFactory;
}
declare module 'monaco-editor/languages/features/json/json.worker.js?worker' {
  const WorkerFactory: { new (): Worker };
  export default WorkerFactory;
}
