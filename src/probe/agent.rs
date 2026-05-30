//! AgentLoop — the core loop that drives the probe agent.
//!
//! Loop:
//!   1. Send messages + tools to LLM
//!   2. If response has tool_calls → dispatch each, append results to messages
//!   3. If response has no tool_calls → LLM is done, return content (JSON report)
//!
//! Safety: max_steps=30, loop detection at 3 repeated identical calls.

use std::collections::HashMap;

use crate::probe::backend::OpenAiBackend;
use crate::probe::tool::Registry;
use crate::probe::types::Message;

pub struct AgentLoop {
    backend: OpenAiBackend,
    tools: Registry,
    messages: Vec<Message>,
    max_steps: usize,
    /// Tracks (tool_name, args) → call count for loop detection.
    call_history: HashMap<String, u32>,
}

impl AgentLoop {
    pub fn new(
        backend: OpenAiBackend,
        tools: Registry,
        system_prompt: &str,
        user_prompt: &str,
        max_steps: usize,
    ) -> Self {
        let messages = vec![
            Message::system(system_prompt),
            Message::user(user_prompt),
        ];
        Self {
            backend,
            tools,
            messages,
            max_steps,
            call_history: HashMap::new(),
        }
    }

    /// Run the agent loop. Returns the final text content from the LLM
    /// (which should be the JSON probe report).
    pub async fn run(&mut self) -> Result<String, String> {
        let tools_schema = self.tools.as_openai_tools();

        for _step in 0..self.max_steps {
            let response = self
                .backend
                .chat(&self.messages, &tools_schema)
                .await?;

            if response.has_tool_calls() {
                // Collect results for all tool calls in this response
                let mut result_messages: Vec<Message> = Vec::new();

                for tc in &response.tool_calls {
                    let call_key = format!("{}|{}", tc.function.name, tc.function.arguments);

                    // Loop detection: inject warning if same call 3+ times
                    let count = self.call_history.entry(call_key.clone()).or_insert(0);
                    *count += 1;
                    if *count >= 3 {
                        let warning = format!(
                            "[WARNING] You have called '{}' with the same arguments {} times. \
                             This is a repeated identical call. If the previous results didn't work, \
                             try a different approach, search pattern, or read a different file. \
                             If you are done reviewing, output the JSON report now.",
                            tc.function.name, count
                        );
                        self.messages.push(Message::user(&warning));
                    }

                    // Dispatch tool call
                    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::Null);

                    eprintln!(
                        "[probe] step {}: calling {} (id={})",
                        _step + 1,
                        tc.function.name,
                        tc.id
                    );

                    let result = match self.tools.dispatch(&tc.function.name, args) {
                        Ok(output) => {
                            let short: String = output.chars().take(200).collect();
                            eprintln!(
                                "[probe]   → {} OK ({} chars)",
                                tc.function.name,
                                output.len()
                            );
                            if output.len() > 200 {
                                eprintln!("[probe]     first 200: {}", short);
                            } else {
                                eprintln!("[probe]     {}", output);
                            }
                            output
                        }
                        Err(e) => {
                            eprintln!("[probe]   → {} ERROR: {}", tc.function.name, e);
                            format!("ERROR: {}", e)
                        }
                    };

                    result_messages.push(Message::tool_result(&tc.id, &result));
                }

                // Add assistant message (tool_calls) + tool result messages
                self.messages
                    .push(Message::assistant_with_tool_calls(response.tool_calls.clone()));
                self.messages.extend(result_messages);
            } else {
                // No tool calls → LLM is done
                eprintln!("[probe] done — LLM returned final response (no tool calls)");
                return Ok(response.content);
            }
        }

        Err(format!(
            "probe agent reached max_steps ({}) without producing a report",
            self.max_steps
        ))
    }
}
