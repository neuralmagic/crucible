//! Edits to a manifest's text that keep its formatting and comments.

use crate::host_env::EnvDecl;
use crate::manifest::edition::Edition;
use crate::manifest::migrate::{MigrateError, TableRef};
use toml_edit::{Array, DocumentMut, Item, Value};

/// An `env` list longer than this on one line is written one entry per line.
const ONE_LINE: usize = 80;

/// `text` with its top-level `edition` set to `edition`. A new key goes above the first table,
/// below the file's leading comment when a blank line ends one.
pub(crate) fn set_edition(
    text: &str,
    edition: Edition,
    manifest: &str,
) -> Result<String, MigrateError> {
    let mut doc = parse(text, manifest)?;
    if let Some(value) = doc.get_mut("edition").and_then(Item::as_value_mut) {
        let decor = value.decor().clone();
        *value = Value::from(edition.number());
        *value.decor_mut() = decor;
        return Ok(doc.to_string());
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let first_item = lines
        .iter()
        .position(|l| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with('#')
        })
        .unwrap_or(lines.len());
    let at = lines[..first_item]
        .iter()
        .rposition(|l| l.trim().is_empty())
        .map_or(0, |blank| blank + 1);
    let mut out = String::with_capacity(text.len() + 16);
    out.extend(lines[..at].iter().copied());
    out.push_str(&format!("edition = {}\n", edition.number()));
    if at < lines.len() && !lines[at].trim().is_empty() {
        out.push('\n');
    }
    out.extend(lines[at..].iter().copied());
    Ok(out)
}

/// `text` with `decls` appended to `table`'s `env` list, which is created when absent.
pub(crate) fn declare(
    text: &str,
    table: &TableRef,
    decls: &[EnvDecl],
    manifest: &str,
) -> Result<String, MigrateError> {
    let mut doc = parse(text, manifest)?;
    let mut path = Vec::new();
    if let Some(variant) = &table.variant {
        path.push("variant".to_owned());
        path.push(variant.to_string());
    }
    path.push(table.table.key().to_owned());
    let mut item = doc.as_item_mut();
    for key in &path {
        item = item
            .as_table_like_mut()
            .and_then(|t| t.get_mut(key))
            .ok_or_else(|| MigrateError::NoTable {
                manifest: manifest.to_owned(),
                table: table.to_string(),
            })?;
    }
    let target = item
        .as_table_like_mut()
        .ok_or_else(|| MigrateError::NoTable {
            manifest: manifest.to_owned(),
            table: table.to_string(),
        })?;
    match target.get_mut("env").and_then(Item::as_array_mut) {
        Some(existing) => {
            for decl in decls {
                existing.push(decl.to_string());
            }
            if !existing.to_string().contains('\n') {
                existing.fmt();
            }
        }
        None => {
            target.insert("env", Item::Value(Value::Array(env_list(decls))));
        }
    }
    Ok(doc.to_string())
}

fn env_list(decls: &[EnvDecl]) -> Array {
    let mut array: Array = decls.iter().map(ToString::to_string).collect();
    array.fmt();
    if array.to_string().len() > ONE_LINE {
        for value in array.iter_mut() {
            value.decor_mut().set_prefix("\n    ");
            value.decor_mut().set_suffix("");
        }
        array.set_trailing("\n");
        array.set_trailing_comma(true);
    }
    array
}

fn parse(text: &str, manifest: &str) -> Result<DocumentMut, MigrateError> {
    text.parse::<DocumentMut>()
        .map_err(|e| MigrateError::Parse {
            manifest: manifest.to_owned(),
            message: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use crate::host_env::{EnvDecl, HostTable};
    use crate::manifest::edition::Edition;
    use crate::manifest::migrate::TableRef;
    use crate::manifest::migrate::document::{declare, set_edition};
    use crate::manifest::variant::VariantName;

    fn decls(names: &[&str]) -> Vec<EnvDecl> {
        names
            .iter()
            .map(|n| EnvDecl::try_from((*n).to_owned()).unwrap())
            .collect()
    }

    fn base(table: HostTable) -> TableRef {
        TableRef {
            variant: None,
            table,
        }
    }

    #[test]
    fn a_new_edition_goes_below_the_file_header_and_above_the_first_table() {
        let text =
            "# A pack.\n# Second line.\n\n# About the workspace.\n[workspace]\ndir = \"w\"\n";
        assert_eq!(
            set_edition(text, Edition::Two, "crucible.toml").unwrap(),
            "# A pack.\n# Second line.\n\nedition = 2\n\n# About the workspace.\n[workspace]\ndir = \"w\"\n"
        );
    }

    #[test]
    fn a_comment_attached_to_the_first_table_stays_with_it() {
        let text = "# About the workspace.\n[workspace]\n";
        assert_eq!(
            set_edition(text, Edition::Two, "crucible.toml").unwrap(),
            "edition = 2\n\n# About the workspace.\n[workspace]\n"
        );
        assert_eq!(
            set_edition("[agent]\n", Edition::Two, "crucible.toml").unwrap(),
            "edition = 2\n\n[agent]\n"
        );
    }

    #[test]
    fn an_explicit_edition_is_rewritten_in_place_with_its_comment() {
        let text = "edition = 1 # the old contract\n\n[agent]\n";
        assert_eq!(
            set_edition(text, Edition::Two, "crucible.toml").unwrap(),
            "edition = 2 # the old contract\n\n[agent]\n"
        );
    }

    #[test]
    fn a_short_env_list_is_one_line_and_a_long_one_is_one_entry_per_line() {
        let text = "[workflow]\ntype = \"playbook\" # lane\n\n# Next.\n[[secret]]\nname = \"s\"\n";
        let short = declare(
            text,
            &base(HostTable::Workflow),
            &decls(&["GH_TOKEN?", "JIRA_URL?"]),
            "crucible.toml",
        )
        .unwrap();
        assert_eq!(
            short,
            "[workflow]\ntype = \"playbook\" # lane\nenv = [\"GH_TOKEN?\", \"JIRA_URL?\"]\n\n# Next.\n[[secret]]\nname = \"s\"\n"
        );
        let long = declare(
            text,
            &base(HostTable::Workflow),
            &decls(&[
                "RELEASE_GITHUB_TOKEN?",
                "GITHUB_TOKEN?",
                "JIRA_API_TOKEN?",
                "JIRA_EMAIL?",
                "SLACK_WEBHOOK_URL?",
            ]),
            "crucible.toml",
        )
        .unwrap();
        assert!(
            long.contains(
                "env = [\n    \"RELEASE_GITHUB_TOKEN?\",\n    \"GITHUB_TOKEN?\",\n    \"JIRA_API_TOKEN?\",\n    \"JIRA_EMAIL?\",\n    \"SLACK_WEBHOOK_URL?\",\n]\n"
            ),
            "{long}"
        );
    }

    #[test]
    fn a_variant_declares_in_its_own_copy_of_the_table() {
        let text = "[workflow]\nfile = \"a.star\"\n\n[variant.gpu.workflow]\nfile = \"b.star\"\n";
        let table = TableRef {
            variant: Some(VariantName::parse("gpu").unwrap()),
            table: HostTable::Workflow,
        };
        let out = declare(text, &table, &decls(&["GPUS?"]), "crucible.toml").unwrap();
        assert_eq!(
            out,
            "[workflow]\nfile = \"a.star\"\n\n[variant.gpu.workflow]\nfile = \"b.star\"\nenv = [\"GPUS?\"]\n"
        );
    }

    #[test]
    fn declaring_into_a_table_the_manifest_lacks_is_refused_naming_it() {
        let error = declare(
            "[agent]\n",
            &base(HostTable::World),
            &decls(&["X?"]),
            "crucible.toml",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "crucible.toml has no [world] table to declare its commands' variables in"
        );
    }
}
