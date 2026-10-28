//! Shared lead policy and a validated, provider-neutral worker handoff.
use serde::{Deserialize, Serialize};

/// The lead playbook: how a Mono decides what to answer itself, what to hand
/// to a quick worker, and (with task coordination on) what to plan as a
/// durable task. One text, so the three never disagree.
pub fn policy(durable_plans: bool) -> String {
    let durable = if durable_plans {
        "3. Durable task plan: implementation that changes code in several dependent or overlapping places, spans repositories, needs review and verification gates, or should survive an app restart. Reply with a plan in the format below; the user starts it from the plan card, and from then on you lead it through the tasks.* actions as its events reach you (worker questions, reviews, the final report). Ordinary conversation needs no plan, and a plan-only request launches nothing.\n"
    } else {
        ""
    };
    let default_route = if durable_plans {
        "a durable task plan for implementation, a quick worker for a self-contained piece"
    } else {
        "a quick worker"
    };
    format!(
        "Delegation policy: you are the user's lead. Decide how each request gets done:\n\
1. Answer directly: questions, explanations, status, repeating a saved plan, facts from memory. Launch nothing and do not probe MCP, repositories or accounts merely to repeat an answer.\n\
2. Quick worker (sessions.start with brief): one self-contained piece of work with no dependency on other work, such as research, a report, a one-off script, a contained fix or a check. One worker per independent piece; run several at once only when their files do not overlap, and use workspaceMode worktree for edits. The worker cannot ask you anything: its brief carries everything it needs, it states its assumptions, and it reports a blocker instead of waiting. You get one completion report when every worker started in that turn has stopped, with each worker's outcome and final message; read it, inspect the result (sessions.read, the worktree's changes), then tell the user what was verified and what was not.\n\
{durable}\
For substantial implementation, debugging or research, delegate ({default_route}) rather than doing the work yourself, unless the user asks you to do it yourself. Acknowledge the request and name the next concrete action before tools. Gather only enough read-only context to write a precise brief; delegate deeper investigation too. Use the assigned project's saved configuration. Distinguish verified facts from assumptions and missing access.\n\
For sessions.start, supply brief with nonempty objective, context, constraints, deliverable and verification strings, plus prompt and an explicit project. Context names relevant files or evidence and what is uncertain; constraints preserve the user's scope, permissions and unrelated work; verification describes observable acceptance evidence. Research may produce a report without code changes. Never invent evidence, never include credentials, and give the worker a descriptive title.\n\
Only say a worker started after the launch was accepted. Then briefly say who is doing what and end your turn so the user can talk to you while workers run. Never poll delegated work, never launch duplicates for work already active, and never evade policy limits by creating more sessions; a status question must not restart, cancel or duplicate work. Report failures honestly and review returned evidence before declaring success. Ask for missing information only when it actually blocks progress."
    )
}

/// The playbook without durable plans (task coordination off).
pub fn quick_policy() -> String {
    policy(false)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Brief {
    pub objective: String,
    pub context: String,
    pub constraints: String,
    pub deliverable: String,
    pub verification: String,
}

pub fn parse(value: &serde_json::Value) -> Result<Brief, String> {
    let brief: Brief =
        serde_json::from_value(value.clone()).map_err(|e| format!("Invalid worker brief: {e}"))?;
    for (name, text) in [
        ("objective", &brief.objective),
        ("context", &brief.context),
        ("constraints", &brief.constraints),
        ("deliverable", &brief.deliverable),
        ("verification", &brief.verification),
    ] {
        if text.trim().is_empty() || text.len() > 16_000 {
            return Err(format!(
                "Worker brief {name} must contain 1–16000 bytes of nonblank text"
            ));
        }
    }
    Ok(brief)
}

pub fn worker_prompt(
    prompt: &str,
    project: &str,
    brief: Option<&serde_json::Value>,
) -> Result<String, String> {
    let brief = brief.map(parse).transpose()?;
    let mut result =
        format!("{prompt}\n\nMonoCode delegated assignment\nAssigned project: {project}\n");
    if let Some(b) = brief {
        result.push_str(&format!("\nObjective\n{}\n\nContext and evidence\n{}\n\nConstraints\n{}\n\nDeliverable\n{}\n\nVerification\n{}\n", b.objective, b.context, b.constraints, b.deliverable, b.verification));
    }
    result.push_str("\nStay within this assignment and the user's authorization. Read applicable project instructions before changes. Do not print credentials. You cannot ask the lead questions during this assignment: choose reasonable defaults, state each assumption in your report, and if something blocks you, stop and report the blocker rather than waiting or assuming success; permission prompts are answered by the user. Do not change unrelated settings. End with a concise report: what changed or was found, where (files, branch, commits), the verification actually performed with its outcomes, assumptions made, and remaining limitations. Do not claim checks you did not run.\n");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_playbook_names_each_route_once_and_only_offers_plans_when_they_exist() {
        let quick = policy(false);
        let durable = policy(true);
        for text in [&quick, &durable] {
            assert!(text.contains("1. Answer directly"));
            assert!(text.contains("2. Quick worker (sessions.start with brief)"));
            assert!(text.contains("cannot ask you anything"));
            assert!(text.contains("one completion report"));
            assert!(text.contains("Never poll delegated work"));
            assert!(!text.contains("Prepare an orchestration plan"));
        }
        assert!(!quick.contains("3. Durable task plan"));
        assert!(quick.contains("delegate (a quick worker)"));
        assert!(durable.contains("3. Durable task plan"));
        assert!(durable.contains("delegate (a durable task plan for implementation"));
        assert!(durable.contains("Ordinary conversation needs no plan"));
        assert_eq!(quick_policy(), quick);
    }
    #[test]
    fn the_worker_prompt_tells_the_worker_it_cannot_ask_the_lead() {
        let rendered = worker_prompt("Do it", "/assigned", None).unwrap();
        assert!(rendered.contains("cannot ask the lead"));
        assert!(rendered.contains("state each assumption"));
        assert!(rendered.contains("report the blocker"));
    }
    #[test]
    fn rejects_incomplete_handoffs_and_preserves_evidence() {
        let mut b = serde_json::json!({"objective":"Fix parser", "context":"src/parser.rs; cause unconfirmed", "constraints":"No network changes", "deliverable":"Patch", "verification":"Run parser tests"});
        let rendered = worker_prompt("Investigate first", "/assigned", Some(&b)).unwrap();
        assert!(rendered.contains("cause unconfirmed"));
        assert!(rendered.contains("/assigned"));
        assert!(rendered.starts_with("Investigate first"));
        b["verification"] = " ".into();
        assert!(worker_prompt("Work", "/assigned", Some(&b)).is_err());
        b.as_object_mut().unwrap().remove("verification");
        assert!(parse(&b).is_err());
        assert!(worker_prompt("Legacy request", "/assigned", None)
            .unwrap()
            .contains("Legacy request"));
    }
}
