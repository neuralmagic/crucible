You have two MCP servers. `buildit` builds container images. `kueue` submits Kubernetes workloads
through Kueue in the one namespace this run may use. You have no kubectl and no cluster
credentials; use only their tools and your shell.

Below this prompt is a line `CODEWORD: <word>`. Remember the word for a later turn of this
conversation. Do not write it to any file and do not put it in your result.

1. Call buildit `clean` with no build_id.
2. Call kueue `whoami` and `queues_list`. Use the LocalQueue they report.
3. Call kueue `workload_plan` with that queue and this batch/v1 Job:

   ```json
   {
     "apiVersion": "batch/v1",
     "kind": "Job",
     "metadata": {"name": "crucible-session-@NONCE_SHORT@"},
     "spec": {
       "backoffLimit": 0,
       "ttlSecondsAfterFinished": 3600,
       "template": {"spec": {
         "restartPolicy": "Never",
         "containers": [{
           "name": "main",
           "image": "registry.access.redhat.com/ubi9/ubi-minimal:latest",
           "command": ["/bin/sh", "-c", "echo crucible-session-ok"],
           "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
         }]
       }}
     }
   }
   ```

4. Call kueue `workload_submit` with the same queue, the same manifest, and the `plan_hash` the
   plan returned.
5. Call kueue `workload_status` for kind `Job`, name `crucible-session-@NONCE_SHORT@`, with
   `until` `finished` and `timeout_seconds` 50. Call it again while the Job has neither
   succeeded nor failed, and give up after six calls.

The result file must contain exactly these two fields:
`{"job": "crucible-session-@NONCE_SHORT@", "status": "<succeeded|failed|timeout>"}`
