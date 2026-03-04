use std::fmt;

/// Commands that are recognized inside the interactive demo REPL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DemoCommand {
    Show,
    Json,
    Input { field: String, value: String },
    Click { action_id: String },
    Setup { provider: Option<String> },
    Back,
    Quit,
    Help,
}

/// Error returned when the input line cannot be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandParseError {
    Unknown(String),
    InvalidFormat(String),
}

impl fmt::Display for CommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandParseError::Unknown(command) => {
                write!(f, "unknown command '{}'; try @show or @help", command)
            }
            CommandParseError::InvalidFormat(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CommandParseError {}

/// Parse the trimmed input line into a REPL command.
pub fn parse_command(line: &str) -> Result<DemoCommand, CommandParseError> {
    let trimmed = line.trim();
    if !trimmed.starts_with('@') {
        return Err(CommandParseError::InvalidFormat(
            "commands must start with '@'".to_string(),
        ));
    }
    let mut parts = trimmed[1..].trim().splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.next().unwrap_or("").trim();

    match name.as_str() {
        "show" => Ok(DemoCommand::Show),
        "json" => Ok(DemoCommand::Json),
        "back" => Ok(DemoCommand::Back),
        "help" => Ok(DemoCommand::Help),
        "quit" => Ok(DemoCommand::Quit),
        "setup" => Ok(DemoCommand::Setup {
            provider: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        }),
        "click" => {
            if rest.is_empty() {
                Err(CommandParseError::InvalidFormat(
                    "@click requires an action id".to_string(),
                ))
            } else {
                Ok(DemoCommand::Click {
                    action_id: rest.to_string(),
                })
            }
        }
        "input" => {
            if rest.is_empty() {
                return Err(CommandParseError::InvalidFormat(
                    "@input requires <field>=<value>".to_string(),
                ));
            }
            let mut kv = rest.splitn(2, '=');
            let field = kv.next().unwrap_or("").trim();
            let value = kv.next().unwrap_or("").trim();
            if field.is_empty() || value.is_empty() {
                return Err(CommandParseError::InvalidFormat(
                    "@input requires a field and a value separated by '='".to_string(),
                ));
            }
            Ok(DemoCommand::Input {
                field: field.to_string(),
                value: value.to_string(),
            })
        }
        unknown => Err(CommandParseError::Unknown(unknown.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_input_command() {
        let command = parse_command("@input foo=bar").unwrap();
        assert_eq!(
            command,
            DemoCommand::Input {
                field: "foo".into(),
                value: "bar".into()
            }
        );
    }

    #[test]
    fn parses_click_command() {
        let command = parse_command("@click submit").unwrap();
        assert_eq!(
            command,
            DemoCommand::Click {
                action_id: "submit".into()
            }
        );
    }

    #[test]
    fn handles_unknown_command() {
        let err = parse_command("@foo").unwrap_err();
        assert!(matches!(err, CommandParseError::Unknown(_)));
    }

    #[test]
    fn rejects_invalid_input_format() {
        let err = parse_command("@input missing").unwrap_err();
        assert!(matches!(err, CommandParseError::InvalidFormat(_)));
    }

    #[test]
    fn parses_back_command() {
        let command = parse_command("@back").unwrap();
        assert_eq!(command, DemoCommand::Back);
    }
}
