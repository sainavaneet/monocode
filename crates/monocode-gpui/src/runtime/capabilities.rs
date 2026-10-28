//! Keep app-local capabilities out of persisted/displayed tool results.
use monocode_core::tool::ToolCall;
use std::collections::HashMap;

pub(super) struct Redactor(Vec<String>);
impl Redactor {
    pub(super) fn new(environment: &HashMap<String, String>) -> Self {
        Self(
            ["MONOCODE_APP_TOKEN", "MONOCODE_VISUAL_TOKEN"]
                .into_iter()
                .filter_map(|key| environment.get(key))
                .filter(|value| !value.is_empty())
                .cloned()
                .collect(),
        )
    }
    /// Assistant and reasoning text: a model that read the values with a
    /// tool must not be able to echo them into the saved transcript either.
    pub(super) fn text(&self, text: String) -> String {
        if self.0.is_empty() || !self.0.iter().any(|secret| text.contains(secret.as_str())) {
            return text;
        }
        let mut text = text;
        for secret in &self.0 {
            text = text.replace(secret, "[REDACTED APP CAPABILITY]");
        }
        text
    }
    pub(super) fn tool(&self, tool: ToolCall) -> ToolCall {
        if self.0.is_empty() {
            return tool;
        }
        let Ok(mut encoded) = serde_json::to_string(&tool) else {
            return tool;
        };
        for secret in &self.0 {
            encoded = encoded.replace(secret, "[REDACTED APP CAPABILITY]");
        }
        serde_json::from_str(&encoded).unwrap_or(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_environment_dump_and_metadata_do_not_expose_capabilities() {
        let env = HashMap::from([
            ("MONOCODE_APP_TOKEN".into(), "fake-app-token".into()),
            ("MONOCODE_VISUAL_TOKEN".into(), "fake-visual-token".into()),
        ]);
        let mut tool = ToolCall::new("a", "Bash");
        tool.output =
            "MONOCODE_APP_TOKEN=fake-app-token\nMONOCODE_VISUAL_TOKEN=fake-visual-token\nQA_OK"
                .into();
        tool.display = Some(serde_json::json!({"detail":"fake-app-token"}));
        let result = Redactor::new(&env).tool(tool);
        let saved = serde_json::to_string(&result).unwrap();
        assert!(!saved.contains("fake-app-token"));
        assert!(!saved.contains("fake-visual-token"));
        assert!(saved.contains("QA_OK"));
        let redactor = Redactor::new(&env);
        let echoed = redactor.text("The token is fake-app-token, visual fake-visual-token.".into());
        assert_eq!(
            echoed,
            "The token is [REDACTED APP CAPABILITY], visual [REDACTED APP CAPABILITY]."
        );
        assert_eq!(redactor.text("plain".into()), "plain");
    }
}
