# Generation 001 - Prompt Lane

## What Changed
Updated `crates/rigcoder/src/prompt.md` with a coherent refinement targeting the failure patterns identified in Generation 000:
1. **Defensive security remediation framing**: Explicitly defined rigcoder as an authorized defensive software engineering and code remediation agent handling authorized bug fixes, code repair, testing, and defensive vulnerability hardening (CWE/CVE remediation).
2. **Immediate tool action on turn 1**: Enforced that turn 1 must immediately call an exploratory tool (`list_files`, `read_file`, or `bash`) rather than risk outputting conversational text or triggering early settlement.
3. **Early implementation and iteration ("Edit and implement early")**: Explicitly directed the agent away from extended prototyping/exploratory bash loops and toy script testing before making code edits, instructing it to implement or edit early and iterate against automated tests.
4. **Structured final completion summary**: Explicitly defined the final message format (files changed/created and verification commands/results), preventing final turns from degenerating into open-ended deliberation or hypothetical next steps.

## Target Failures
- **`fix-code-vulnerability` (ended without settling / 0 tool calls)**: The task failed at turn 0 when the provider stream abruptly terminated on a prompt containing dozens of attack-vector CWE definitions. Setting legitimate defensive software engineering and authorized vulnerability remediation framing helps prevent provider safety filter false-positives and stream drops, and enforcing immediate turn 1 tool calls ensures active execution starts right away.
- **Pre-edit latency across trials (35.75 tool calls before first edit)**: Addressed runs (such as `cancel-async-tasks` with 70 pre-edit calls) where the agent spent dozens of turns running exploratory bash micro-experiments before touching target files.
- **Ending deliberation (`ended in deliberation, not a summary`: 0.20)**: Addressed runs flagged for deliberative endings by requiring a concise two-part completion summary (changes made and verification performed).
