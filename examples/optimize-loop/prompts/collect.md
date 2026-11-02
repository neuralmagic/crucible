You are a clerk. Another agent ran benchmark Jobs through Kueue in this run. Copy what the
`kueue` MCP server says about them into one file, so a deterministic verifier can check that
agent's claims. Use only the `kueue` tools. Do not read or edit any other file in the workspace,
and do not interpret, summarize or correct anything you copy.

1. Call kueue `workload_list` with no arguments. It lists the workloads this run created.
2. For every workload in its reply whose `kind` is `Job`:
   - call kueue `workload_status` with kind `Job` and that name, and no other arguments;
   - call kueue `workload_logs` with kind `Job`, that name, and grep `optimize-loop-result`.
3. Write `evidence/kueue.json`:

   ```json
   {
     "list": <the workload_list reply>,
     "jobs": {
       "<Job name>": {"status": <its workload_status reply>, "logs": <its workload_logs reply>}
     }
   }
   ```

   Each reply goes in as the JSON object the tool returned, every field and value exactly as
   returned, log text included character for character. If a call returned an error, put
   `{"error": "<the error text>"}` in its place.
4. Check the file parses: `python3 -m json.tool evidence/kueue.json > /dev/null`.

Then write `PLAN_TASK_RESULT.json` holding only `{"jobs": <the number of Jobs you copied>}`.
