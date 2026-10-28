# Digitaltwin task recovery proposal

Task: `plan-6829932253101104-a872524c8554b213a6148762037a6b16`

This is a reviewable recovery proposal, not an applied database migration. The task remains blocked. No worker should resume from the old routing.

## Preserve

Back up the task database with SQLite backup before any mutation. Preserve all prompts, dependencies, criteria, events, and the existing clean knowledge-graph worktree. Keep Mono's conversation MCP source at `/Users/navaneet/Developer/airobotics/knowledge-graph`; task workspace routing is separate.

## Routing

Set the task workspace to `/Users/navaneet/Developer/airobotics` and assign structured repository fields:

| Assignments | Repository | Initial reviewed base |
| --- | --- | --- |
| task-1, task-2, task-3, task-4, task-5, task-10 | DigitalTwin | b00d1e044417bbf81f5efdee73217fb2d3912d9f |
| task-6, task-7, task-8a, task-8b, task-9a, task-9b | cellforge | 4220f41c4450cafefd4d88a61e4890f87ee7d6ee |

These bases are appropriate only for initial independent assignments. Successors must use a reviewed integration commit containing their accepted same-repository dependencies. Do not reuse the initial base for those successors.

Retire the incorrect task-1 workspace association only after rechecking that no attempt ran and the worktree is still clean. Preserve the directory and branch. Append a recovery event describing the corrected association; do not erase its history.

## Worker instructions

Retain original prompts, but add a reviewed correction overriding their shared branch instructions: each worker uses only its coordinator-provided isolated worktree/branch. Dependency workspaces are read-only references. Integrate accepted changes separately per repository; do not merge histories across repositories.

## Authentication and authority

Default Claude passed a real subscription request. The named Claude profile is signed out. Clear only the verified default account's stale authentication failure, using the health API after the fixed launch environment is installed. Do not clear the named profile or alter its credentials.

The saved policy allows commits and up to 8 workers, with full-access execution and allowed-provider fallback. It disallows merge, push and deploy. Prompts asking for push/draft PR/pack deployment conflict with that policy. Keep those actions blocked pending a specific policy decision; do not infer authority from a worker prompt.

## Resume gate

Show the repaired task plan, account eligibility, repository/base checks and remaining integration decision in Dev. Validate against a copied task database first. Apply to Main only after that review; resume via the coordinator lifecycle API rather than writing an active lifecycle directly.
