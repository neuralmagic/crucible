ADVERSARIAL_REVIEW: CORRECTNESS

Another agent implemented is_prime(n) in solution.py, aiming to make verify.sh pass.
Both files are in your working directory; read them.

Review adversarially. Assume the implementation may satisfy verify.sh without being
correct in general. Judge behavior only; another reviewer owns prose, so ignore typos,
comments, and docstring wording entirely.

Write a single JSON object to PLAN_TASK_RESULT.json:

- `approved`: true only if is_prime is correct for all n, not merely the cases verify.sh tests
- `finding`: one or two sentences naming the defect, or "" if approved
- `counterexample`: an integer n that is_prime answers incorrectly, or null if approved

Do not edit any file. Review only.
