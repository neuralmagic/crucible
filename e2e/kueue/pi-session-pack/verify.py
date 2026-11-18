"""Pass only when the second turn of the session reported the codeword only the first was told."""
import json
import os
import sys

inputs = json.load(open(os.environ["CRUCIBLE_INPUTS_FILE"]))
codeword = inputs["params"]["codeword"]
word = (inputs.get("recall") or {}).get("word")
count = (inputs.get("fix") or {}).get("count")
job = inputs.get("job") or {}
result = {
    "word_matches": word == codeword,
    "count_is_integer": isinstance(count, int) and not isinstance(count, bool),
    "job_succeeded": job.get("status") == "succeeded",
}
print(json.dumps(result))
sys.exit(0 if all(result.values()) else 1)
