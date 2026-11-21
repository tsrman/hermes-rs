//! System prompt for the CLI.

const IDENTITY: &str = "You are Hermes Agent, built by Nous Research. Answer directly and match the level of detail to the request.";
const TOOL_GUIDANCE: &str = "Use read_file for text files in the current working directory. Base your answer on the tool result and report any errors that prevent you from completing the request.";
const PLATFORM_HINT: &str = "You are in a plain terminal (CLI). Write plain text.";

pub fn cli_system_prompt(model: &str, provider: &str) -> String {
    CliPromptContext::default().render(model, provider)
}

#[derive(Default)]
pub struct CliPromptContext {
    pub identity: Option<String>,
    /// Guidance for optional tools, placed right after the base tool guidance.
    pub tool_guidance: Vec<String>,
    pub sections: Vec<String>,
}

impl CliPromptContext {
    pub fn render(&self, model: &str, provider: &str) -> String {
        let footer = route_footer(model, provider);
        let mut blocks = vec![self.identity.as_deref().unwrap_or(IDENTITY), TOOL_GUIDANCE];
        blocks.extend(self.tool_guidance.iter().map(String::as_str));
        blocks.push(PLATFORM_HINT);
        blocks.extend(self.sections.iter().map(String::as_str));
        blocks.push(&footer);
        blocks.join("\n\n")
    }
}

pub fn update_cli_prompt_route(saved: &str, model: &str, provider: &str) -> String {
    if let Some((prefix, footer)) = saved.rsplit_once("\n\n") {
        let mut lines = footer.lines();
        if lines.next().is_some_and(|line| line.starts_with("Model: "))
            && lines
                .next()
                .is_some_and(|line| line.starts_with("Provider: "))
            && lines.next() == Some("Platform: cli")
            && lines.next().is_none()
        {
            return format!("{prefix}\n\n{}", route_footer(model, provider));
        }
    }
    saved.to_owned()
}

fn route_footer(model: &str, provider: &str) -> String {
    format!("Model: {model}\nProvider: {provider}\nPlatform: cli")
}
