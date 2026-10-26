use serde_json::Value;
/// The worker result's reference lists: commits, checks, evidence, remaining
/// work and limitations.
pub const WORKER_LISTS: [&str; 5] = [
    "commits",
    "validation",
    "evidence",
    "remainingWork",
    "limitations",
];
pub const WORKER_LIST_ITEMS: usize = 50;
pub const WORKER_LIST_ITEM_BYTES: usize = 4_000;
/// The assignments a resume or steer names.
pub const MAX_ASSIGNMENT_IDS: usize = 100;
const MAX_ASSIGNMENT_ID_BYTES: usize = 512;
/// Reject misspelled fields and ambiguous types before any app mutation.
pub fn validate(action: &str, input: &Value) -> Result<(), String> {
    let fields=match action{
        "models.list"|"soul.read"|"habits.list"=>"",
        "sessions.list"|"folders.list"|"worktrees.list"=>"project",
        "sessions.read"=>"sessionId project before limit maxChars",
        "sessions.start"=>"prompt project harness model modelSettings effort runtimeMode title draft reveal workspaceMode worktreeBase worktreeCwd placement besideSessionId notifyOnComplete",
        "sessions.send"=>"sessionId prompt project notifyOnComplete",
        "sessions.draft"=>"sessionId prompt project",
        "sessions.stop"|"sessions.archive"|"sessions.delete"=>"sessionId project",
        "worktrees.create"=>"branch base existing project",
        "folders.move"=>"sessionId folderId newFolderName project",
        "notes.list"=>"limit offset","notes.read"=>"id","notes.write"=>"id title body tags",
        "artifacts.list"=>"kind limit offset","artifacts.read"=>"id","artifacts.write"=>"id kind title body summary",
        "soul.update"=>"text expectedHash","memory.read"=>"topic","memory.search"=>"query since",
        "memory.add"=>"fact topic until","memory.replace"=>"find fact topic until","memory.remove"=>"find topic",
        "habits.add"=>"name instructions schedule","habits.update"=>"id name instructions schedule enabled","habits.run"|"habits.remove"=>"id",
        "chat.card"=>"type repo number note sessionId options name instructions schedule",
        "tasks.policy.show"=>"taskId",
        "tasks.policy.apply"|"tasks.unblock"=>"taskId revision",
        "tasks.worker.status"=>"",
        "tasks.worker.ask"=>"question context scope",
        "tasks.worker.result"=>"outcome summary workspace commits validation evidence remainingWork limitations",
        "tasks.worker.blocked"=>"reason",
        "tasks.decisions.list"=>"",
        "tasks.decisions.answer"=>"decisionId revision answer",
        "tasks.decisions.escalate"=>"decisionId revision question why recommendation alternatives",
        "tasks.review.accept"=>"taskId assignmentId revision",
        "tasks.review.sendBack"=>"taskId assignmentId revision reason",
        "tasks.criteria.verify"=>"taskId criterionId check assignmentId",
        "tasks.pause"|"tasks.cancel"=>"taskId revision",
        "tasks.resume"=>"taskId revision assignmentIds confirmedStopped",
        "tasks.steer"=>"taskId revision requirements assignmentIds plan",
        _=>return Err("Unknown app action. Run app help".into()),
    };
    for (key, value) in input.as_object().ok_or("Input must be an object")? {
        if !fields.split_whitespace().any(|f| f == key) {
            return Err(format!("{action} does not take {key}"));
        }
        if [
            "draft",
            "reveal",
            "notifyOnComplete",
            "existing",
            "enabled",
            "confirmedStopped",
        ]
        .contains(&key.as_str())
            && !value.is_boolean()
        {
            return Err(format!("{key} must be true or false"));
        }
        if ["limit", "offset", "maxChars", "number", "revision"].contains(&key.as_str())
            && value.as_u64().is_none()
        {
            return Err(format!("{key} must be a non-negative integer"));
        }
        if key == "before" && !value.is_string() && value.as_u64().is_none() {
            return Err("before must be a turn ID".into());
        }
        if matches!(key.as_str(), "id" | "sessionId" | "besideSessionId")
            && !value.is_string()
            && value.as_u64().is_none()
        {
            return Err(format!("{key} must be an ID"));
        }
        if ![
            "draft",
            "reveal",
            "notifyOnComplete",
            "existing",
            "enabled",
            "confirmedStopped",
            "before",
            "limit",
            "offset",
            "maxChars",
            "number",
            "revision",
            "alternatives",
            "assignmentIds",
            "plan",
            "id",
            "sessionId",
            "besideSessionId",
            "modelSettings",
            "schedule",
            "tags",
            "options",
        ]
        .contains(&key.as_str())
            && !WORKER_LISTS.contains(&key.as_str())
            && !value.is_string()
        {
            return Err(format!("{key} must be text"));
        }
    }
    if input.get("modelSettings").is_some_and(|value| {
        value
            .as_object()
            .is_none_or(|values| values.values().any(|v| !v.is_string()))
    }) {
        return Err("modelSettings must be an object of strings".into());
    }
    for key in WORKER_LISTS {
        if input.get(key).is_some_and(|v| {
            v.as_array().is_none_or(|a| {
                a.len() > WORKER_LIST_ITEMS
                    || a.iter()
                        .any(|v| v.as_str().is_none_or(|s| s.len() > WORKER_LIST_ITEM_BYTES))
            })
        }) {
            return Err(format!(
                "{key} must be an array of up to {WORKER_LIST_ITEMS} texts of up to {WORKER_LIST_ITEM_BYTES} bytes"
            ));
        }
    }
    if input.get("alternatives").is_some_and(|v| {
        v.as_array().is_none_or(|a| {
            a.len() > crate::app_access::MAX_ALTERNATIVES
                || a.iter().any(|v| {
                    v.as_str()
                        .is_none_or(|s| s.len() > crate::app_access::MAX_ALTERNATIVE_BYTES)
                })
        })
    }) {
        return Err(format!(
            "alternatives must be an array of up to {} texts of up to {} bytes",
            crate::app_access::MAX_ALTERNATIVES,
            crate::app_access::MAX_ALTERNATIVE_BYTES
        ));
    }
    if input.get("assignmentIds").is_some_and(|v| {
        v.as_array().is_none_or(|a| {
            a.len() > MAX_ASSIGNMENT_IDS
                || a.iter().any(|v| {
                    v.as_str()
                        .is_none_or(|s| s.trim().is_empty() || s.len() > MAX_ASSIGNMENT_ID_BYTES)
                })
        })
    }) {
        return Err(format!(
            "assignmentIds must be an array of up to {MAX_ASSIGNMENT_IDS} assignment ids"
        ));
    }
    if input.get("plan").is_some_and(|v| !v.is_object()) {
        return Err("plan must be the revised plan object ({title,summary,tasks})".into());
    }
    if input["draft"] == true && input["notifyOnComplete"] == true {
        return Err("An unsent draft cannot send a completion notification".into());
    }
    if input.get("runtimeMode").is_some_and(|v| {
        !matches!(
            v.as_str(),
            Some("read-only" | "supervised" | "auto-accept-edits" | "auto" | "full-access")
        )
    }) {
        return Err("Unknown runtimeMode".into());
    }
    if input
        .get("workspaceMode")
        .is_some_and(|v| !matches!(v.as_str(), Some("current" | "checkout" | "worktree")))
    {
        return Err("workspaceMode must be current or worktree".into());
    }
    if input
        .get("placement")
        .is_some_and(|v| !matches!(v.as_str(), Some("tab" | "right" | "down")))
    {
        return Err("placement must be tab, right or down".into());
    }
    if input.get("besideSessionId").is_some()
        && !matches!(input["placement"].as_str(), Some("right" | "down"))
    {
        return Err("besideSessionId requires placement right or down".into());
    }
    if input.get("tags").is_some_and(|v| {
        v.as_array().is_none_or(|a| {
            a.len() > crate::notes::MAX_TAGS
                || a.iter().any(|v| {
                    v.as_str()
                        .is_none_or(|s| s.len() > crate::notes::MAX_TAG_LENGTH)
                })
        })
    }) {
        return Err("tags must be an array of up to 20 short strings".into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn validates_before_mutation() {
        assert!(validate("sessions.start", &json!({"prompt":"x","draft":"false"})).is_err());
        assert!(validate("sessions.delete", &json!({"sessionId":1,"force":true})).is_err());
        assert!(validate("sessions.start", &json!({"runtimeMode":"unsafe"})).is_err());
        assert!(validate(
            "sessions.start",
            &json!({"prompt":"x","project":"/code","draft":true})
        )
        .is_ok());
    }
    #[test]
    fn mono_task_commands_validate_their_fields() {
        let escalate = json!({"decisionId":"d","revision":2,"why":"w","recommendation":"r",
            "alternatives":["a","b"],"question":"q"});
        assert!(validate("tasks.decisions.escalate", &escalate).is_ok());
        assert!(validate(
            "tasks.decisions.answer",
            &json!({"decisionId":"d","revision":-1})
        )
        .is_err());
        assert!(validate("tasks.decisions.escalate", &json!({"alternatives":"a"})).is_err());
        assert!(validate("tasks.decisions.escalate", &json!({"alternatives":[1]})).is_err());
        let many = vec!["x"; crate::app_access::MAX_ALTERNATIVES + 1];
        assert!(validate("tasks.decisions.escalate", &json!({"alternatives": many})).is_err());
        assert!(validate(
            "tasks.review.sendBack",
            &json!({"taskId":"t","assignmentId":"a","revision":1,"reason":"r"})
        )
        .is_ok());
        assert!(validate(
            "tasks.review.accept",
            &json!({"taskId":"t","attemptId":"x"})
        )
        .is_err());
        assert!(validate(
            "tasks.criteria.verify",
            &json!({"taskId":"t","criterionId":"c","check":"true","assignmentId":"a"})
        )
        .is_ok());
    }
    #[test]
    fn task_lifecycle_commands_validate_their_fields() {
        assert!(validate("tasks.pause", &json!({"taskId":"t","revision":3})).is_ok());
        assert!(validate("tasks.cancel", &json!({"taskId":"t","revision":"3"})).is_err());
        assert!(validate("tasks.pause", &json!({"taskId":"t","reason":"x"})).is_err());
        assert!(validate(
            "tasks.resume",
            &json!({"taskId":"t","revision":3,"assignmentIds":["a"]})
        )
        .is_ok());
        assert!(validate("tasks.resume", &json!({"assignmentIds":"a"})).is_err());
        assert!(validate("tasks.resume", &json!({"confirmedStopped":true})).is_ok());
        assert!(validate("tasks.resume", &json!({"confirmedStopped":"yes"})).is_err());
        assert!(validate("tasks.resume", &json!({"assignmentIds":[" "]})).is_err());
        let steer = json!({"taskId":"t","revision":3,"requirements":"r","assignmentIds":["a"],
            "plan":{"title":"P","summary":"","tasks":[]}});
        assert!(validate("tasks.steer", &steer).is_ok());
        assert!(validate("tasks.steer", &json!({"plan":"text"})).is_err());
        let many = vec!["a"; MAX_ASSIGNMENT_IDS + 1];
        assert!(validate("tasks.steer", &json!({"assignmentIds": many})).is_err());
    }
    #[test]
    fn worker_commands_validate_their_fields_and_never_take_ids() {
        assert!(validate("tasks.worker.status", &json!({})).is_ok());
        assert!(validate("tasks.worker.status", &json!({"attemptId":"x"})).is_err());
        assert!(validate(
            "tasks.worker.ask",
            &json!({"question":"?","context":"why","scope":"dependents"})
        )
        .is_ok());
        assert!(validate("tasks.worker.ask", &json!({"question":"?","taskId":"t"})).is_err());
        let result = json!({"outcome":"failed","summary":"s","workspace":"/w","commits":["a"],
            "validation":["v"],"evidence":["e"],"remainingWork":["r"],"limitations":["l"]});
        assert!(validate("tasks.worker.result", &result).is_ok());
        assert!(validate("tasks.worker.result", &json!({"commits":"abc"})).is_err());
        assert!(validate("tasks.worker.result", &json!({"limitations":[3]})).is_err());
        assert!(validate("tasks.worker.blocked", &json!({"reason":"r"})).is_ok());
        assert!(validate("tasks.worker.blocked", &json!({"reason":7})).is_err());
    }
}
