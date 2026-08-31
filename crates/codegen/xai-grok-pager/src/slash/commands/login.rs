//! `/login` -- log in or re-authenticate, optionally against an alternative
//! subscription provider (Claude Pro/Max, ChatGPT Plus/Pro).
//!
//! Bare `/login` keeps its historical meaning (re-auth the xAI session), so
//! muscle memory is unaffected. Typing `/login ` opens the provider dropdown,
//! which is the in-TUI equivalent of `grok login --provider <id>`.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};

/// Provider ids accepted as `/login` arguments, with the labels shown in the
/// dropdown. Mirrors `SubscriptionProvider` in the shell; kept as plain data
/// because the pager does not depend on the shell's auth types.
const PROVIDERS: &[(&str, &str, &str)] = &[
    ("xai", "Grok (xAI)", "Sign in to your xAI account"),
    (
        "anthropic",
        "Claude (Pro/Max)",
        "Sign in with your Anthropic subscription",
    ),
    (
        "openai-codex",
        "ChatGPT (Plus/Pro)",
        "Sign in with your OpenAI Codex subscription",
    ),
];

/// Accept the canonical id plus the aliases the CLI takes, so the two front
/// ends stay interchangeable.
fn normalize_provider(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "xai" | "grok" => Some("xai"),
        "anthropic" | "claude" => Some("anthropic"),
        "openai-codex" | "openai_codex" | "openai" | "codex" | "chatgpt" => Some("openai-codex"),
        _ => None,
    }
}

/// One-step sign-in for a single provider (`/login-claude`, `/login-codex`).
///
/// `/login` alone requires picking a provider, which only helps someone who
/// already knows the providers exist. The first thing a new user actually does
/// is search the slash menu for the thing they want — reported from the field:
/// "i searched /claude and /codex first but didn't find any indicator". Nothing
/// matched, because the only route was a `/login` *argument*, and the menu
/// cannot search a command's arguments.
///
/// These give each provider a name of its own, plus aliases so the obvious
/// searches (`claude`, `codex`, `chatgpt`, `gpt`, `anthropic`) all land
/// somewhere useful. Aliases get their own dropdown rows, so the search that
/// previously came up empty now shows the description.
pub struct ProviderLoginCommand {
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    /// Canonical provider id handed to [`Action::LoginWithProvider`].
    provider: &'static str,
}

pub const LOGIN_CLAUDE: ProviderLoginCommand = ProviderLoginCommand {
    name: "login-claude",
    aliases: &["claude", "anthropic", "login-anthropic"],
    description: "Sign in with your Claude Pro/Max subscription",
    provider: "anthropic",
};

pub const LOGIN_CODEX: ProviderLoginCommand = ProviderLoginCommand {
    name: "login-codex",
    aliases: &["codex", "chatgpt", "gpt", "login-chatgpt"],
    description: "Sign in with your ChatGPT Plus/Pro subscription",
    provider: "openai-codex",
};

impl SlashCommand for ProviderLoginCommand {
    fn name(&self) -> &str {
        self.name
    }

    fn aliases(&self) -> &[&str] {
        self.aliases
    }

    fn description(&self) -> &str {
        self.description
    }

    fn usage(&self) -> &str {
        self.name
    }

    fn takes_args(&self) -> bool {
        false
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::LoginWithProvider(self.provider.to_string()))
    }
}

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    slash_meta! {
        name: "login",
        description: "Log in or re-authenticate (optionally with Claude or ChatGPT)",
        usage: "/login [xai|anthropic|openai-codex]",
        takes_args: true,
    }

    /// Required, so Enter on a bare `/login` opens the provider dropdown
    /// instead of silently launching the xAI browser flow.
    ///
    /// Bare `/login` used to mean "re-auth xAI" back when xAI was the only
    /// account there was. With three providers signed in that is a trap: the
    /// most common reason to type `/login` is now to *choose*, and the old
    /// behavior committed to the choice before showing it. `/model` has always
    /// worked this way; `/login xai` is still the one-liner for the old
    /// meaning.
    fn args_required(&self) -> bool {
        true
    }

    fn suggest_args(&self, _ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        let query = args_query.trim().to_ascii_lowercase();
        let items: Vec<ArgItem> = PROVIDERS
            .iter()
            .filter(|(id, label, _)| {
                query.is_empty()
                    || id.contains(&query)
                    || label.to_ascii_lowercase().contains(&query)
            })
            .map(|(id, label, description)| ArgItem {
                display: (*label).to_string(),
                match_text: format!("{id} {label}"),
                insert_text: (*id).to_string(),
                description: (*description).to_string(),
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let raw = args.trim();
        if raw.is_empty() {
            // `args_required` keeps Enter from reaching here from the prompt,
            // so this is a non-interactive path (queued text, scripted input).
            // Name the choices rather than picking one silently.
            return CommandResult::Error(
                "Pick a provider: /login xai, /login anthropic, or /login openai-codex".to_string(),
            );
        }
        match normalize_provider(raw) {
            Some("xai") => CommandResult::Action(Action::Login),
            Some(provider) => {
                CommandResult::Action(Action::LoginWithProvider(provider.to_string()))
            }
            None => CommandResult::Error(format!(
                "Unknown provider '{raw}'. Valid: xai, anthropic, openai-codex"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_canonical_ids_and_cli_aliases() {
        assert_eq!(normalize_provider("anthropic"), Some("anthropic"));
        assert_eq!(normalize_provider("claude"), Some("anthropic"));
        assert_eq!(normalize_provider("  CLAUDE  "), Some("anthropic"));
        assert_eq!(normalize_provider("openai-codex"), Some("openai-codex"));
        assert_eq!(normalize_provider("codex"), Some("openai-codex"));
        assert_eq!(normalize_provider("chatgpt"), Some("openai-codex"));
        assert_eq!(normalize_provider("xai"), Some("xai"));
        assert_eq!(normalize_provider("grok"), Some("xai"));
        assert_eq!(normalize_provider("nope"), None);
        assert_eq!(normalize_provider(""), None);
    }

    /// The searches a new user actually types must reach a provider sign-in.
    /// Reported from the field: "/claude" and "/codex" matched nothing, because
    /// the only route was a `/login` argument and the menu cannot search
    /// arguments.
    #[test]
    fn obvious_provider_searches_resolve_to_a_login() {
        for (query, expected) in [
            ("claude", "anthropic"),
            ("anthropic", "anthropic"),
            ("login-claude", "anthropic"),
            ("codex", "openai-codex"),
            ("chatgpt", "openai-codex"),
            ("gpt", "openai-codex"),
            ("login-codex", "openai-codex"),
        ] {
            let cmd = [&LOGIN_CLAUDE, &LOGIN_CODEX]
                .into_iter()
                .find(|c| c.name() == query || c.aliases().contains(&query))
                .unwrap_or_else(|| panic!("no provider login reachable by {query:?}"));
            assert_eq!(
                cmd.provider, expected,
                "{query:?} routed to the wrong provider"
            );
        }
    }

    /// These take no arguments: the whole point is that one keystroke-path
    /// signs in, with no second decision to discover.
    #[test]
    fn provider_logins_are_argument_free_and_name_a_real_provider() {
        for cmd in [&LOGIN_CLAUDE, &LOGIN_CODEX] {
            assert!(!cmd.takes_args(), "{} must not require args", cmd.name());
            assert_eq!(
                normalize_provider(cmd.provider),
                Some(cmd.provider),
                "{} targets a provider /login does not accept",
                cmd.name()
            );
        }
    }

    /// Every advertised dropdown id must be one `run` accepts, or the picker
    /// would offer a value that then errors.
    #[test]
    fn every_suggested_id_is_accepted() {
        for (id, _, _) in PROVIDERS {
            assert_eq!(
                normalize_provider(id),
                Some(*id),
                "dropdown offers {id} but run() rejects it"
            );
        }
    }
}
