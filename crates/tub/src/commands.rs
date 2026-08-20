//! Parsing for the interactive slash-command surface. Parsing stays pure so
//! command text can never accidentally cross the model boundary.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Effort(Option<String>),
    Provider(ProviderCommand),
    Resume(Option<String>),
    Init,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCommand {
    Choose(Option<String>),
    Add,
}

/// Parse an input line. `//text` escapes to a literal `/text` model prompt;
/// ordinary text returns `None`; malformed/unknown commands fail locally.
pub fn parse(input: &str) -> Result<Option<SlashCommand>, String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') || trimmed.starts_with("//") {
        return Ok(None);
    }
    let mut words = trimmed.split_whitespace();
    let name = words.next().unwrap_or_default();
    let argument = words.next().map(str::to_string);
    if words.next().is_some() {
        return Err(format!("{name} accepts at most one argument"));
    }
    match name {
        "/model" => Ok(Some(SlashCommand::Model(argument))),
        "/effort" => Ok(Some(SlashCommand::Effort(argument))),
        "/provider" => Ok(Some(SlashCommand::Provider(match argument.as_deref() {
            Some("add") => ProviderCommand::Add,
            _ => ProviderCommand::Choose(argument),
        }))),
        "/resume" => Ok(Some(SlashCommand::Resume(argument))),
        "/init" if argument.is_none() => Ok(Some(SlashCommand::Init)),
        "/help" if argument.is_none() => Ok(Some(SlashCommand::Help)),
        "/init" | "/help" => Err(format!("{name} accepts no arguments")),
        _ => Err(format!(
            "unknown command {name}; use /model, /effort, /provider, /resume, or /init"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_preserves_literal_slashes() {
        assert_eq!(parse("hello").unwrap(), None);
        assert_eq!(parse("//model").unwrap(), None);
        assert_eq!(
            parse("/model route/name").unwrap(),
            Some(SlashCommand::Model(Some("route/name".into())))
        );
        assert_eq!(
            parse("/provider add").unwrap(),
            Some(SlashCommand::Provider(ProviderCommand::Add))
        );
        assert!(parse("/wat").unwrap_err().contains("unknown command"));
    }
}
