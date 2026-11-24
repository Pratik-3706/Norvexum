// ===========================================================================
// Schedule — which calls of one reply have to wait for an earlier one
//
// A reply can hold several tool calls. They run in parallel, except where a
// later call touches what an earlier one changes: "stop, then list", two edits
// to one file, or "write the script, then run it" only mean anything in order.
// Each call declares what it uses; a call waits for every earlier call that
// clashes with it, and for nothing else.
// ===========================================================================

use std::path::{Component, Path, PathBuf};

use super::subagent;
use crate::tools::{canonical_tool_name, mcp};

/// Something calls share.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resource {
    /// A file, or a folder and everything in it.
    Files(PathBuf),
    Processes,
    Git,
    Browser,
    Memory,
    Agents,
    /// The MCP server list.
    McpConfig,
    McpServer(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Only looks.
    Read,
    /// Creates something new or may touch anything (a command, a download):
    /// these don't wait for each other, but looks and changes wait for them.
    Add,
    /// Changes what is there.
    Write,
}

type Access = (Resource, Mode);

/// For each call, the earlier calls it has to wait for.
pub fn dependencies<'a>(
    calls: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
    cwd: &Path,
) -> Vec<Vec<usize>> {
    let accesses: Vec<Vec<Access>> = calls
        .into_iter()
        .map(|(name, args)| accesses(name, args, cwd))
        .collect();
    accesses
        .iter()
        .enumerate()
        .map(|(i, mine)| {
            (0..i)
                .filter(|&j| {
                    accesses[j]
                        .iter()
                        .any(|a| mine.iter().any(|b| clashes(a, b)))
                })
                .collect()
        })
        .collect()
}

fn clashes(a: &Access, b: &Access) -> bool {
    let harmless = a.1 == b.1 && a.1 != Mode::Write;
    !harmless && overlaps(&a.0, &b.0)
}

fn overlaps(a: &Resource, b: &Resource) -> bool {
    match (a, b) {
        (Resource::Files(a), Resource::Files(b)) => a.starts_with(b) || b.starts_with(a),
        // (Re)connecting or removing servers affects every server's tools.
        (Resource::McpConfig, Resource::McpServer(_))
        | (Resource::McpServer(_), Resource::McpConfig) => true,
        _ => a == b,
    }
}

/// What a call uses. Tools that share nothing (web, skills, …) use nothing.
fn accesses(name: &str, args: &serde_json::Value, cwd: &Path) -> Vec<Access> {
    use Mode::{Add, Read, Write};
    let file = |key: &str| Resource::Files(normalize(cwd, args[key].as_str().unwrap_or(".")));
    let path = || {
        let given = ["path", "file_path", "TargetFile"]
            .iter()
            .find_map(|key| args[*key].as_str());
        Resource::Files(normalize(cwd, given.unwrap_or(".")))
    };
    let project = || Resource::Files(normalize(cwd, "."));

    match canonical_tool_name(name) {
        "read_file" | "view_image" | "ls" | "grep" | "find" => vec![(path(), Read)],
        "batch_view_images" => args["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
            .map(|p| (Resource::Files(normalize(cwd, p)), Read))
            .collect(),
        "write_file" | "edit_file" | "multi_edit" | "touch" | "rm" => vec![(path(), Write)],
        "mv" => vec![(file("source"), Write), (file("destination"), Write)],
        "cp" => vec![(file("source"), Read), (file("destination"), Write)],
        "download_image" | "batch_download_images" | "generate_image" => {
            vec![(file("output_dir"), Add)]
        }
        // Finds task.md wherever it is in the project.
        "manage_tasks" => vec![(project(), Write)],
        // Commands can read and write anything and start processes, but stay
        // parallel to each other (a build next to a test run).
        "run_command" => vec![
            (project(), Add),
            (Resource::Processes, Add),
            (Resource::Git, Add),
        ],
        "manage_processes" => {
            let mode = if args["action"] == "stop" {
                Write
            } else {
                Read
            };
            vec![(Resource::Processes, mode)]
        }
        "git_status" | "git_diff" | "git_log" => {
            vec![(Resource::Git, Read), (project(), Read)]
        }
        "git_commit" | "git_push" | "git" => vec![(Resource::Git, Write), (project(), Write)],
        "browser_interact" => vec![(Resource::Browser, Write), (project(), Read)],
        "store_in_memory" => vec![(Resource::Memory, Write)],
        "read_from_memory" => vec![(Resource::Memory, Read)],
        subagent::TOOL_NAME | subagent::MESSAGE_TOOL_NAME => vec![(Resource::Agents, Add)],
        subagent::WAIT_TOOL_NAME => vec![(Resource::Agents, Read)],
        mcp::MANAGE_TOOL_NAME => {
            let mode = if args["action"] == "list" {
                Read
            } else {
                Write
            };
            vec![(Resource::McpConfig, mode)]
        }
        // A server may drive something with state (an editor, a 3D scene).
        other => match other
            .strip_prefix(mcp::PREFIX)
            .and_then(|rest| rest.split_once("__"))
        {
            Some((server, _)) => vec![(Resource::McpServer(server.to_string()), Write)],
            None => Vec::new(),
        },
    }
}

/// The path with `.` and `..` folded, for comparing. The file may not exist
/// yet, and the tool itself still checks the sandbox.
fn normalize(cwd: &Path, path: &str) -> PathBuf {
    let joined = cwd.join(path.trim());
    // Drop the `\\?\` form so both spellings of an absolute path match.
    let text = joined.to_string_lossy();
    let joined = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text));
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            // Windows paths ignore case.
            part if cfg!(windows) => {
                out.push(part.as_os_str().to_string_lossy().to_lowercase());
            }
            part => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn deps(calls: &[(&str, Value)]) -> Vec<Vec<usize>> {
        let cwd = std::env::temp_dir();
        dependencies(calls.iter().map(|(n, a)| (*n, a)), &cwd)
    }

    #[test]
    fn independent_calls_stay_parallel() {
        let plan = deps(&[
            ("read_file", json!({"path": "a.txt"})),
            ("read_file", json!({"path": "a.txt"})),
            ("web_search", json!({"query": "x"})),
            ("edit_file", json!({"path": "b.txt"})),
            ("edit_file", json!({"path": "src/c.txt"})),
            ("web_fetch", json!({"url": "https://example.com"})),
            ("spawn_agent", json!({"task": "x"})),
            ("spawn_agent", json!({"task": "y"})),
            ("mcp__one__do", json!({})),
            ("mcp__two__do", json!({})),
        ]);
        assert!(plan.iter().all(Vec::is_empty), "{plan:?}");

        let plan = deps(&[
            ("run_command", json!({"command": "cargo build"})),
            ("run_command", json!({"command": "npm test"})),
            ("download_image", json!({"output_dir": "img"})),
            ("generate_image", json!({"output_dir": "img"})),
        ]);
        assert!(plan.iter().all(Vec::is_empty), "{plan:?}");
    }

    #[test]
    fn calls_that_depend_on_each_other_keep_their_order() {
        // The reported case: stop, then list.
        assert_eq!(
            deps(&[
                ("manage_processes", json!({"action": "stop", "pid": 1})),
                ("manage_processes", json!({"action": "list"})),
                ("manage_processes", json!({"action": "logs", "pid": 2})),
            ]),
            [vec![], vec![0], vec![0]]
        );
        // Edits to one file, however its path is written.
        assert_eq!(
            deps(&[
                ("edit_file", json!({"path": "src/a.rs"})),
                ("multi_edit", json!({"path": "./src/x/../a.rs"})),
                ("edit_file", json!({"file_path": "src/a.rs"})),
                ("edit_file", json!({"path": "src/b.rs"})),
            ]),
            [vec![], vec![0], vec![0, 1], vec![]]
        );
        // Write it, run it, read what it produced, commit.
        assert_eq!(
            deps(&[
                ("write_file", json!({"path": "gen.py"})),
                ("run_command", json!({"command": "python gen.py"})),
                ("read_file", json!({"path": "out.txt"})),
                ("git_commit", json!({"message": "x"})),
            ]),
            [vec![], vec![0], vec![1], vec![0, 1, 2]]
        );
        // Moving or removing a folder concerns what is inside it.
        assert_eq!(
            deps(&[
                ("write_file", json!({"path": "old/a.txt"})),
                ("mv", json!({"source": "old", "destination": "new"})),
                ("ls", json!({"path": "new/sub"})),
                ("ls", json!({"path": "newer"})),
            ]),
            [vec![], vec![0], vec![1], vec![]]
        );
        // One browser, one MCP server, one task list.
        assert_eq!(
            deps(&[
                ("browser_interact", json!({"action": "click"})),
                ("browser_interact", json!({"action": "type"})),
            ]),
            [vec![], vec![0]]
        );
        assert_eq!(
            deps(&[
                ("mcp_manage", json!({"action": "add"})),
                ("mcp__blender__make", json!({})),
                ("mcp__blender__look", json!({})),
            ]),
            [vec![], vec![0], vec![0, 1]]
        );
        assert_eq!(
            deps(&[
                ("spawn_agent", json!({"task": "x"})),
                ("wait_for_agents", json!({})),
            ]),
            [vec![], vec![0]]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_without_case_or_prefix() {
        let cwd = Path::new(r"\\?\C:\Proj");
        assert_eq!(
            normalize(cwd, "A.txt"),
            normalize(Path::new(r"C:\proj"), "a.TXT")
        );
        assert_eq!(normalize(cwd, r"c:\PROJ\a.txt"), normalize(cwd, "a.txt"));
    }
}
