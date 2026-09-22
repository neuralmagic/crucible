// The API's failure bodies are either `{error: string}` (structured AppError/not-found) or a
// plain string (500 text). One narrow keeps every page's error rendering honest — no
// String(object) "[object Object]", no eslint-disable.
export function formatError(err: unknown): string {
  if (typeof err === 'string') return err;
  if (typeof err === 'object' && err !== null) {
    if ('error' in err && typeof err.error === 'string') return err.error;
    if (err instanceof Error) return err.message;
    return JSON.stringify(err);
  }
  return 'Unknown error';
}
