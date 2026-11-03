//! The two surfaces a caller sees: every MCP tool as `tools/list` returns it, and every CLI
//! subcommand's help. Both are pinned to a fixture, so a change to either is a diff in review.
//! `CRUX_UPDATE_SURFACE=1 cargo test -p crux --test surface` rewrites the fixtures.

use std::fmt::Write as _;
use std::path::Path;

fn tools_listing() -> String {
    let tools = crux::CrucibleMcp::tools();
    let mut out = serde_json::to_string_pretty(&tools).expect("tools serialize");
    out.push('\n');
    out
}

fn help_listing() -> String {
    let mut cmd = crux::cli::command();
    cmd.build();
    let mut out = String::new();
    let mut section = |name: &str, cmd: &mut clap::Command| {
        writeln!(out, "==== {name} --help ====").expect("write");
        out.push_str(&cmd.render_long_help().to_string());
        writeln!(out, "==== {name} -h ====").expect("write");
        out.push_str(&cmd.render_help().to_string());
    };
    section("crux", &mut cmd);
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    for name in names {
        let sub = cmd.find_subcommand_mut(&name).expect("listed subcommand");
        section(&format!("crux {name}"), sub);
    }
    out
}

fn pinned(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    if std::env::var_os("CRUX_UPDATE_SURFACE").is_some() {
        std::fs::write(&path, actual).expect("write fixture");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run with CRUX_UPDATE_SURFACE=1", path.display()));
    assert!(
        expected == actual,
        "{} differs from the surface this build serves; review the change, then run with \
         CRUX_UPDATE_SURFACE=1 to accept it",
        path.display()
    );
}

#[test]
fn the_mcp_tools_list_is_the_pinned_one() {
    pinned("tools.json", &tools_listing());
}

#[test]
fn every_subcommands_help_is_the_pinned_one() {
    pinned("help.txt", &help_listing());
}
