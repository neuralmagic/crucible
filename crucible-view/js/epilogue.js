
// Reads what the host asks for on standard input and writes the answer to standard output,
// after a record separator so the host can tell it from anything the plugin printed.
{
  const chunks = [];
  let total = 0;
  for (;;) {
    const chunk = new Uint8Array(65536);
    const read = Javy.IO.readSync(0, chunk);
    if (read === 0) break;
    chunks.push(chunk.subarray(0, read));
    total += read;
  }
  const bytes = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.length;
  }
  let reply;
  try {
    const asked = JSON.parse(new TextDecoder().decode(bytes));
    const plain = (given) => {
      const params = {};
      for (const [id, value] of Object.entries(given ?? {})) {
        params[id] = Object.values(value)[0];
      }
      return params;
    };
    if ("view" in asked) {
      reply = { view: view({ files: asked.view.files, params: plain(asked.view.params) }) };
    } else if ("runs_view" in asked) {
      const { runs: launches, total, params } = asked.runs_view;
      reply = { view: runs({ runs: launches, total, params: plain(params) }) };
    } else if ("runs_inputs" in asked) {
      const drawn = typeof runs === "function" && typeof runsInputs !== "undefined";
      reply = { runs_inputs: drawn ? runsInputs : null };
    } else {
      reply = { inputs };
    }
  } catch (error) {
    const stack = error && error.stack ? `\n${error.stack}` : "";
    reply = { error: `${error}${stack}`.trim() };
  }
  Javy.IO.writeSync(1, new TextEncoder().encode("\u001e" + JSON.stringify(reply)));
}
