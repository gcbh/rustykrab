use crate::skill_md::SkillMd;
use openclaw_core::types::ToolSchema;

/// Builds optimized system prompts that improve model tool-use reliability.
///
/// Weaker models often fail at tool use because:
/// 1. They don't know WHEN to use a tool vs. answer directly
/// 2. They hallucinate tool argument schemas
/// 3. They don't chain multi-step tool calls effectively
///
/// This builder injects structured guidance that significantly improves
/// tool-use accuracy across all models, especially open-source ones.
///
/// Meta-Harness integration: the builder can incorporate execution trace
/// data so the model learns from its own session history — tools that
/// keep failing get flagged, successful patterns are reinforced.
pub struct SystemPromptBuilder {
    sections: Vec<String>,
}

impl SystemPromptBuilder {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    /// Add the base agent identity and behavior rules.
    pub fn with_identity(mut self, name: &str, description: &str) -> Self {
        self.sections.push(format!(
            "You are {name}, {description}\n\n\
             CORE RULES:\n\
             - Always use tools when you need external information. Never guess or make up data.\n\
             - If a tool call fails, read the error message carefully and try a different approach.\n\
             - Think step by step. For complex tasks, break them into smaller tool calls.\n\
             - When you have the answer, respond directly. Do not make unnecessary tool calls.\n\
             - MEMORY: Use memory_save to store important facts, decisions, user preferences, \
               and error resolutions with descriptive tags. Your context window is limited — \
               facts you don't save will be lost when old messages scroll out. When you learn \
               something important, save it immediately. Relevant memories are automatically \
               recalled based on conversation keywords."
        ));
        self
    }

    /// Add tool-use guidance derived from the available tool schemas.
    /// This explicitly tells the model what tools exist and when to use them,
    /// dramatically improving tool selection accuracy on weaker models.
    pub fn with_tool_guidance(mut self, tools: &[ToolSchema]) -> Self {
        if tools.is_empty() {
            return self;
        }

        let mut guidance = String::from("AVAILABLE TOOLS:\n");
        for tool in tools {
            guidance.push_str(&format!(
                "\n- **{}**: {}\n  Parameters: {}\n",
                tool.name,
                tool.description,
                summarize_params(&tool.parameters),
            ));
        }

        guidance.push_str(
            "\nTOOL USE GUIDELINES:\n\
             - Select the most specific tool for the task.\n\
             - Provide all required parameters. Check the schema before calling.\n\
             - You may call multiple tools in a single turn if they are independent.\n\
             - If a tool returns an error, do NOT retry with the same arguments. \
               Analyze the error and adjust your approach.",
        );

        self.sections.push(guidance);
        self
    }

    /// Add trace-informed tool guidance.
    ///
    /// This is the key Meta-Harness insight: by showing the model its own
    /// execution history, it can adapt its strategy. Tools with high failure
    /// rates get explicit warnings; the model sees which tools it has used
    /// most and can adjust.
    pub fn with_trace_summary(mut self, trace_summary: &str) -> Self {
        if !trace_summary.is_empty() {
            self.sections.push(trace_summary.to_string());
        }
        self
    }

    /// Add a task-type-specific guidance section.
    pub fn with_task_guidance(mut self, guidance: &str) -> Self {
        self.sections.push(guidance.to_string());
        self
    }

    /// Add a skill's system prompt fragment.
    pub fn with_skill(mut self, skill_prompt: &str) -> Self {
        self.sections.push(skill_prompt.to_string());
        self
    }

    /// Add conversation memory context.
    pub fn with_memory(mut self, summary: &str) -> Self {
        self.sections.push(format!(
            "CONVERSATION CONTEXT (from earlier messages):\n{summary}"
        ));
        self
    }

    /// Add chain-of-thought guidance for complex tasks.
    pub fn with_chain_of_thought(mut self) -> Self {
        self.sections.push(
            "REASONING APPROACH:\n\
             For complex tasks, think through these steps:\n\
             1. What is the user asking for?\n\
             2. What information do I need that I don't have?\n\
             3. Which tool(s) can get me that information?\n\
             4. What order should I call them in?\n\
             5. After getting results, do I need more information or can I answer?\n\n\
             Show your reasoning briefly before making tool calls."
                .to_string(),
        );
        self
    }

    /// Inject a compact `<available_skills>` XML catalog of SKILL.md skills.
    ///
    /// This is appended at prompt build time so the model knows which skills
    /// exist without loading their full body.
    pub fn with_available_skills(mut self, skills: &[&SkillMd]) -> Self {
        if skills.is_empty() {
            return self;
        }
        let mut xml = String::from("<available_skills>\n");
        for s in skills {
            let name = escape_xml(&s.frontmatter.name);
            let desc = escape_xml(&s.frontmatter.description);
            let loc = escape_xml(&s.path.display().to_string());
            xml.push_str(&format!(
                "  <skill name=\"{name}\" description=\"{desc}\" location=\"{loc}\" />\n"
            ));
        }
        xml.push_str("</available_skills>");
        self.sections.push(xml);
        self
    }

    /// Wrap a skill's full body in `<skill_instructions>` XML.
    ///
    /// Used JIT when a skill is activated during a conversation turn.
    pub fn with_active_skill(mut self, name: &str, body: &str) -> Self {
        self.sections.push(format!(
            "<skill_instructions name=\"{}\">\n{body}\n</skill_instructions>",
            escape_xml(name)
        ));
        self
    }

    /// Build the final system prompt.
    pub fn build(self) -> String {
        self.sections.join("\n\n---\n\n")
    }
}

impl Default for SystemPromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Escape XML special characters to prevent injection in skill names/descriptions.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
     .replace('\'', "&apos;")
}

/// Produce a human-readable summary of a JSON Schema parameters object.
fn summarize_params(schema: &serde_json::Value) -> String {
    let props = match schema.get("properties") {
        Some(p) => p,
        None => return "none".to_string(),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut parts = Vec::new();
    if let Some(obj) = props.as_object() {
        for (key, val) in obj {
            let typ = val
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let req = if required.contains(&key.as_str()) {
                " (required)"
            } else {
                ""
            };
            parts.push(format!("`{key}`: {typ}{req}"));
        }
    }

    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}
