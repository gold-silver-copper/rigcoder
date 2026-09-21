Execute the next experiment described in harness/DEVELOPMENT_AUDIT.md: determine whether task network access can be isolated from model transport without changing the development tasks.

Read AGENTS.md, harness/ITERATE_PROMPT.md, harness/DEVELOPMENT_AUDIT.md, and its evidence appendix. Inspect current code and repository state before relying on prior conclusions. Preserve unrelated changes.

Constraints:
- No paid model calls.
- No holdout access.
- No task, verifier, timeout, or task-image changes.
- No prompt tuning, history compression, or repetition cutoffs.
- No commits or PRs.
- Use the existing benchmark runner, relay, and synthetic checks.

Context:
The audit found two recorded passes that downloaded benchmark answer material. The existing network-none Docker relay canary passed with mocked model responses. However, the current isolated benchmark path requires supported host scoring, and ordinary development tasks cannot use it unchanged. Network isolation alone does not protect scoring inside a candidate-modified container.

1. Inventory legitimate network requirements.
For all twenty development tasks, inspect their instructions, environment definitions, and existing development traces. Identify package downloads, external data, local services, and other network requirements.

Produce a per-task matrix distinguishing:
- Required capabilities.
- Optional or unnecessary accesses.
- Confirmed answer acquisition.
- Unresolved requirements.

Cite evidence. Do not assume every observed download is necessary or that missing observations prove no network requirement.

2. Test the existing boundary.
Extend the existing synthetic relay/Docker checks to verify:
- Mocked model requests succeed through host-controlled transport.
- Provider credentials stay outside the candidate.
- Candidate shell and Python processes cannot reach arbitrary external endpoints, including direct-IP and redirect paths.
- Required loopback services remain usable.
- Failure and shutdown preserve correct accounting and cleanup.

Use fake credentials and controlled synthetic endpoints. Do not fetch actual benchmark answers. Keep changes confined to diagnostic tests and necessary test support; do not implement the production candidate yet.

3. Assess scoring separately.
Trace the existing scorer lifecycle and identify which candidate-controlled files, processes, and services can affect verification. Clearly distinguish protections demonstrated by tests from unresolved gaps. Do not bypass scorer eligibility checks or claim that network isolation solves scoring integrity.

4. Make one engineering decision.
Determine whether a minimal production integration can preserve every development task’s required capabilities while preventing arbitrary answer access.

If yes, specify the smallest implementation, code locations, tests, and remaining evaluation prerequisites.
If no, identify the concrete incompatibilities and recommend one bounded next experiment.

Reject tool-text filters and broad domain allowlists as enforcement. Do not alter tasks or silently exclude difficult tasks to obtain a favorable result.

Deliver:
- A concise report with the twenty-task compatibility matrix.
- The synthetic test changes and observed results.
- One recommended next step with explicit acceptance and stopping criteria.
- Any implications for the audit’s conditional paid-evaluation plan.

Run targeted checks appropriate to the changes. Report exactly what passed, failed, or remains unproven. This iteration’s provider budget is $0.
