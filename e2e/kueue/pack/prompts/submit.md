You have an MCP server named `kueue`. It submits Kubernetes workloads through Kueue in the
one namespace this run is allowed to use. Use only its tools; you have no kubectl and no
cluster credentials.

1. Call `whoami` and `queues_list`. Use the LocalQueue they report.
2. Build this batch/v1 Job and call `workload_plan` with it and that queue:

   ```json
   {
     "apiVersion": "batch/v1",
     "kind": "Job",
     "metadata": {"name": "kueue-e2e-hello"},
     "spec": {
       "backoffLimit": 0,
       "ttlSecondsAfterFinished": 3600,
       "template": {"spec": {
         "restartPolicy": "Never",
         "containers": [{
           "name": "main",
           "image": "registry.access.redhat.com/ubi9/ubi-minimal:latest",
           "command": ["/bin/sh", "-c", "echo crucible-kueue-e2e-ok"],
           "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
         }]
       }}
     }
   }
   ```

3. Call `workload_submit` with the same queue, the same manifest, and the `plan_hash` the plan
   returned.
4. Poll `workload_status` for kind `Job`, name `kueue-e2e-hello` until the Job has succeeded or
   failed. Wait about 10 seconds between polls, by running `sleep 10` in the shell. Give up after
   5 minutes.
5. Call `workload_logs` for the Job.

Finish with two lines: `WHOAMI <the whoami reply, as one line of JSON, exactly as the tool
returned it>`, then `RESULT <succeeded|failed|timeout> <the log line>`.
