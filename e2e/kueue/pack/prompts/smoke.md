You have two MCP servers. `buildit` builds container images from your workspace and pushes them.
`kueue` submits Kubernetes workloads through Kueue in the one namespace this run may use. You
have no kubectl, no registry credentials and no cluster credentials; use only their tools and
your shell.

1. In your workspace, create a directory `smoke` with two files:
   - `smoke/main.sh`, a shell script that prints exactly this line and exits 0:
     `crucible-smoke-ok @NONCE@`
   - `smoke/Containerfile`:
     ```
     FROM registry.access.redhat.com/ubi9/ubi-minimal:latest
     COPY main.sh /main.sh
     ENTRYPOINT ["/bin/sh", "/main.sh"]
     ```
2. Call buildit `build` with context `smoke` and dockerfile `Containerfile`. Poll buildit
   `status` for its build_id until it succeeds or fails, running `sleep 10` in the shell between
   polls. If it fails, read buildit `logs` and fix the files once.
3. Call buildit `push` with the build_id. Keep the `image` it returns: a `repository@sha256:...`
   reference.
4. Call kueue `whoami` and `queues_list`. Use the LocalQueue they report.
5. Call kueue `workload_plan` with that queue and this batch/v1 Job, with `IMAGE` replaced by the
   pushed image:

   ```json
   {
     "apiVersion": "batch/v1",
     "kind": "Job",
     "metadata": {"name": "crucible-smoke-@NONCE_SHORT@"},
     "spec": {
       "backoffLimit": 0,
       "ttlSecondsAfterFinished": 3600,
       "template": {"spec": {
         "restartPolicy": "Never",
         "imagePullSecrets": [{"name": "@PULL_SECRET@"}],
         "containers": [{
           "name": "main",
           "image": "IMAGE",
           "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
         }]
       }}
     }
   }
   ```

6. Call kueue `workload_submit` with the same queue, the same manifest, and the `plan_hash` the
   plan returned.
7. Poll kueue `workload_status` for kind `Job`, name `crucible-smoke-@NONCE_SHORT@` until the Job has succeeded
   or failed, running `sleep 10` between polls. Give up after 5 minutes.
8. Call kueue `workload_logs` for the Job.
9. Call buildit `clean` with the build_id.

Finish with three lines: `WHOAMI <the kueue whoami reply, as one line of JSON, exactly as the tool
returned it>`, then `IMAGE <the pushed image>`, then `RESULT <succeeded|failed|timeout> <the Job's
log line>`.
