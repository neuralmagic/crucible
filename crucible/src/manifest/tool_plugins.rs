//! The `[[agent.tool_plugins]]` chain a pack declares for its agent turns. The engine runs the
//! chain over every tool and retry event; this module is only its declared shape.

use serde::Deserialize;
use std::num::NonZeroUsize;

/// One `[[agent.tool_plugins]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "plugin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolPluginSpec {
    /// Stop the turn when one call, or one call and its result, repeats `limit` times in a row.
    RepeatGuard { limit: NonZeroUsize },
    /// Tag each call as read, write, exec, build, network, mcp, agent or other, for `tool_stats`.
    Classify {},
    /// Report each tool's calls, failures and wall time, the classes `classify` tagged, the
    /// provider retries, and the longest run of failed calls, at the end of the turn.
    ToolStats {},
}

/// How many identical consecutive tool calls the default chain's `repeat_guard` allows.
pub const DEFAULT_REPEAT_LIMIT: NonZeroUsize = match NonZeroUsize::new(25) {
    Some(limit) => limit,
    None => NonZeroUsize::MIN,
};

/// The chain a turn runs when the manifest declares none.
pub fn default_chain() -> Vec<ToolPluginSpec> {
    vec![
        ToolPluginSpec::RepeatGuard {
            limit: DEFAULT_REPEAT_LIMIT,
        },
        ToolPluginSpec::Classify {},
        ToolPluginSpec::ToolStats {},
    ]
}

#[cfg(test)]
mod tests {
    use crate::manifest::{DEFAULT_REPEAT_LIMIT, ToolPluginSpec, default_chain};
    use std::num::NonZeroUsize;

    #[derive(Debug, serde::Deserialize)]
    struct Agent {
        tool_plugins: Option<Vec<ToolPluginSpec>>,
    }

    fn parse(toml: &str) -> Result<Option<Vec<ToolPluginSpec>>, String> {
        toml::from_str::<Agent>(toml)
            .map(|agent| agent.tool_plugins)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn plugins_parse_in_declared_order_with_their_parameters() {
        assert_eq!(
            parse(
                r#"
                [[tool_plugins]]
                plugin = "classify"
                [[tool_plugins]]
                plugin = "repeat_guard"
                limit = 40
                "#,
            ),
            Ok(Some(vec![
                ToolPluginSpec::Classify {},
                ToolPluginSpec::RepeatGuard {
                    limit: NonZeroUsize::new(40).unwrap()
                },
            ]))
        );
    }

    #[test]
    fn an_absent_chain_and_an_empty_one_are_different_declarations() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("tool_plugins = []"), Ok(Some(Vec::new())));
        assert_eq!(
            default_chain(),
            vec![
                ToolPluginSpec::RepeatGuard {
                    limit: DEFAULT_REPEAT_LIMIT
                },
                ToolPluginSpec::Classify {},
                ToolPluginSpec::ToolStats {},
            ]
        );
        assert_eq!(DEFAULT_REPEAT_LIMIT.get(), 25);
    }

    #[test]
    fn an_unknown_plugin_names_the_ones_that_exist() {
        let err = parse("[[tool_plugins]]\nplugin = \"write_scope\"").unwrap_err();
        assert!(err.contains("write_scope"), "{err}");
        assert!(
            err.contains("repeat_guard") && err.contains("tool_stats"),
            "{err}"
        );
    }

    #[test]
    fn a_plugin_refuses_parameters_it_does_not_take_or_lacks() {
        let extra = parse("[[tool_plugins]]\nplugin = \"classify\"\nlimit = 3").unwrap_err();
        assert!(extra.contains("limit"), "{extra}");
        let missing = parse("[[tool_plugins]]\nplugin = \"repeat_guard\"").unwrap_err();
        assert!(missing.contains("limit"), "{missing}");
        let zero = parse("[[tool_plugins]]\nplugin = \"repeat_guard\"\nlimit = 0").unwrap_err();
        assert!(zero.contains("zero"), "{zero}");
    }
}
