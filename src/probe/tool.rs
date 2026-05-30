//! Tool trait + Registry — extracted from agnt-core, simplified for probe.

use serde_json::Value;

/// A tool the probe agent can invoke (erased form).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn call(&self, args: Value) -> Result<String, String>;
}

/// A collection of tools with name-based dispatch.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn dispatch(&self, name: &str, args: Value) -> Result<String, String> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| format!("unknown tool: {}", name))?
            .call(args)
    }

    /// Return the tool definitions in OpenAI function-calling JSON format.
    pub fn as_openai_tools(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name(),
                            "description": t.description(),
                            "parameters": t.schema(),
                        }
                    })
                })
                .collect(),
        )
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
