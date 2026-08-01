COPY REVIEW: DOCS AND COMMENTS

Another agent wrote solution.py. Copy-edit its prose: docstrings and comments.

Judge writing only. Check spelling, grammar, and whether the documentation describes what
the code actually does. Say nothing about algorithmic correctness; another reviewer owns it.

Write a single JSON object to PLAN_TASK_RESULT.json:

- `findings`: an array of strings, one per prose defect, empty if the prose is clean
- `summary`: one sentence on the overall state of the documentation

Do not edit any file. Review only.
