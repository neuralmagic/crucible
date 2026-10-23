You have two MCP servers. `buildit` builds container images from your workspace and publishes
them. `kueue` submits Kubernetes workloads through Kueue. Use only their tools for anything
on the cluster; you have no kubectl and no cluster credentials.

1. Make up a nonce: 12 random lowercase hex characters.
2. In your workspace, create a directory `smoke/` with:
   - `main.sh`: a POSIX shell script that prints `crucible-build-run-ok <nonce>` and then
     `uname -m`.
   - `Containerfile`:
     ```
     FROM registry.access.redhat.com/ubi9/ubi-minimal:latest
     COPY main.sh /main.sh
     ENTRYPOINT ["/bin/sh", "/main.sh"]
     ```
3. Call buildit `build` with `context: "smoke"` and `dockerfile: "Containerfile"`. Poll
   `status` with its `build_id` (run `sleep 10` in the shell between polls) until it reports
   `succeeded` or `failed`. Give up after 10 minutes. If it failed, read `logs` and stop.
4. Call buildit `push` with the `build_id`. Keep the returned `repository@sha256` digest.
5. Call kueue `whoami`. Use the namespace it reports, and a LocalQueue from `queues_list`.
6. Call kueue `workload_plan` with this Job, using the digest from step 4 as the image:
   ```json
   {
     "apiVersion": "batch/v1",
     "kind": "Job",
     "metadata": {"name": "build-run-smoke"},
     "spec": {
       "backoffLimit": 0,
       "template": {"spec": {
         "restartPolicy": "Never",
         "imagePullSecrets": [{"name": "quay-wseaton-pull"}],
         "containers": [{
           "name": "main",
           "image": "<digest from step 4>",
           "resources": {"requests": {"cpu": "100m", "memory": "64Mi"}, "limits": {"memory": "64Mi"}}
         }]
       }}
     }
   }
   ```
   Then call `workload_submit` with the same queue, the same manifest, and the `plan_hash`.
7. Call `workload_status` for the Job until it has succeeded or failed (use its wait option if
   it has one, otherwise `sleep 10` between calls). Give up after 5 minutes.
8. Call `workload_logs` for the Job.
9. Call buildit `clean` with the `build_id`.

Finish with exactly these lines:
- `IMAGE <the repository@sha256 digest>`
- `WHOAMI <the kueue whoami reply, as one line of JSON>`
- `RESULT <succeeded|failed|timeout> <the crucible-build-run-ok line from the Job's log, or none>`
- `NONCE <the nonce you chose>`
