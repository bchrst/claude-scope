mod db;
mod import;
mod interactive;

use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use std::path::{Path, PathBuf};

/// The program's name, as used for its command, data directory and messages.
const APP: &str = env!("CARGO_PKG_NAME");

#[derive(Parser)]
#[command(name = APP)]
#[command(about = "Archive Claude Code conversations into SQLite with FTS5 full-text search")]
#[command(version)]
struct Cli {
    /// Path to the SQLite database file
    #[arg(long, env = "CLAUDE_SCOPE_DB")]
    db: Option<PathBuf>,

    /// Without a command, open the interactive menu (`interactive` with its defaults)
    #[command(subcommand)]
    command: Option<Commands>,
}

/// Default for `interactive --limit`, also used when no command is given.
const DEFAULT_INTERACTIVE_LIMIT: usize = 200;

#[derive(Subcommand)]
enum Commands {
    /// Import all JSONL files from ~/.claude/projects/
    Import {
        /// Path to Claude config directory (default: ~/.claude)
        #[arg(long)]
        claude_dir: Option<PathBuf>,
    },
    /// Import a single JSONL session file
    ImportFile {
        /// Path to the JSONL file
        path: PathBuf,
        /// Project name
        #[arg(long)]
        project: Option<String>,
    },
    /// Search conversations using full-text search
    Search {
        /// Search query (FTS5 syntax)
        query: String,
        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,
        /// Filter by project name (substring match)
        #[arg(short, long)]
        project: Option<String>,
        /// Filter by role (user or assistant)
        #[arg(short, long, value_parser = ["user", "assistant"])]
        role: Option<String>,
        /// Only show results after this date (e.g. 2024-01-01)
        #[arg(long)]
        since: Option<String>,
        /// Only show results up to this date or time, inclusive (e.g. 2024-12-31, or 2024-06 for all of June)
        #[arg(long)]
        until: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Include tool_use lines in results (hidden by default)
        #[arg(long)]
        include_tools: bool,
    },
    /// Search and browse conversations from a cscope-like menu, reading a question and its answer in $EDITOR (the default)
    #[command(visible_alias = "i")]
    Interactive {
        #[command(flatten)]
        menu: MenuArgs,
    },
    /// Import new conversations from ~/.claude/projects/, then open the interactive menu
    Update {
        /// Path to Claude config directory (default: ~/.claude)
        #[arg(long)]
        claude_dir: Option<PathBuf>,
        #[command(flatten)]
        menu: MenuArgs,
    },
    /// Export a session in various formats
    Export {
        /// Session ID or prefix (e.g. "47cf1f2e")
        #[arg(required_unless_present = "last")]
        session_id: Option<String>,
        /// Export the Nth most recent session (1 = latest, 2 = second latest, ...)
        #[arg(long, default_missing_value = "1", num_args = 0..=1, value_name = "N")]
        last: Option<usize>,
        /// Output format
        #[arg(short, long, default_value = "markdown")]
        format: ExportFormat,
    },
    /// List sessions
    List {
        /// Number of sessions to show (0 = all)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Filter by project name (substring match)
        #[arg(short, long)]
        project: Option<String>,
        /// Only show sessions after this date (e.g. 2024-01-01)
        #[arg(long)]
        since: Option<String>,
        /// Only show sessions up to this date or time, inclusive (e.g. 2024-12-31, or 2024-06 for all of June)
        #[arg(long)]
        until: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and all its messages
    Delete {
        /// Session ID or prefix
        session_id: String,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show database statistics
    Stats,
    /// Verify database integrity
    Verify,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

/// The options of the interactive menu, shared by `interactive` and `update`.
#[derive(Args)]
struct MenuArgs {
    /// Start by searching all conversations for this (FTS5 syntax)
    query: Option<String>,
    /// Maximum number of matches per search (0 = all)
    #[arg(short, long, default_value_t = DEFAULT_INTERACTIVE_LIMIT)]
    limit: usize,
    /// Only search and list projects whose name contains this; "Search in a session" then offers their sessions instead of the current directory's
    #[arg(short, long)]
    project: Option<String>,
    /// Only match messages by this role when searching (user or assistant)
    #[arg(short, long, value_parser = ["user", "assistant"])]
    role: Option<String>,
    /// Only list sessions started, and match messages written, after this date (e.g. 2024-01-01)
    #[arg(long)]
    since: Option<String>,
    /// Only list sessions started, and match messages written, up to this date or time, inclusive (e.g. 2024-12-31, or 2024-06 for all of June)
    #[arg(long)]
    until: Option<String>,
    /// Include tool_use lines in the answer (hidden by default)
    #[arg(long)]
    include_tools: bool,
}

impl Default for MenuArgs {
    /// As when no command is given.
    fn default() -> Self {
        MenuArgs {
            query: None,
            limit: DEFAULT_INTERACTIVE_LIMIT,
            project: None,
            role: None,
            since: None,
            until: None,
            include_tools: false,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum ExportFormat {
    Markdown,
    Json,
    Text,
}

fn format_project_name(raw: &str) -> String {
    db::format_project_name(raw)
}

/// Remove lines that start with `[tool_use: ` from content.
fn strip_tool_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.starts_with(db::TOOL_USE_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Interactive mode has nothing to show: say why and how to fix it, and exit.
fn exit_with(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

/// The `--db` argument that commands suggested to the user need: a database
/// given with --db or CLAUDE_SCOPE_DB has to be given again.
fn db_arg(db_path: &Path, explicit_db: bool) -> String {
    if explicit_db {
        format!(" --db {}", shell_quote(db_path))
    } else {
        String::new()
    }
}

fn import_first_message(db_path: &Path, explicit_db: bool) -> String {
    let db_arg = db_arg(db_path, explicit_db);
    format!(
        "No conversations archived yet in {}.\n\
         Run `{APP}{db_arg} import` first to archive your Claude Code conversations, then `{APP}{db_arg}` to browse them.",
        db_path.display()
    )
}

/// The project whose sessions interactive mode lists by default: that of the
/// current directory or, if it has no archived sessions, of the closest parent
/// directory that has some, short of the home directory. Returns the project's
/// name and directory.
fn current_project(conn: &rusqlite::Connection, cwd: &Path) -> Result<Option<(String, PathBuf)>> {
    // As the current directory is: $HOME may lead through a symlink.
    let home = dirs::home_dir().map(|home| home.canonicalize().unwrap_or(home));
    for dir in cwd.ancestors() {
        let at_limit = dir.parent().is_none() || home.as_deref() == Some(dir);
        if dir != cwd && at_limit {
            break;
        }
        let name = import::project_dir_name(dir);
        if db::project_exists(conn, &name)? {
            return Ok(Some((name, dir.to_path_buf())));
        }
    }
    Ok(None)
}

/// A directory for display, with the home directory shortened to `~`.
fn display_dir(dir: &Path) -> String {
    let home = dirs::home_dir();
    // Directories read from the filesystem don't go through a symlink that
    // $HOME may go through.
    let real_home = home.as_deref().and_then(|home| home.canonicalize().ok());
    let shortened = [real_home.as_deref(), home.as_deref()]
        .into_iter()
        .find_map(|home| shorten_home(dir, home?));
    shortened.unwrap_or_else(|| dir.display().to_string())
}

/// `dir` with `home` shortened to `~`, if it's in there and home isn't the
/// root, which every path would start with.
fn shorten_home(dir: &Path, home: &Path) -> Option<String> {
    home.parent()?;
    let rest = dir.strip_prefix(home).ok()?;
    if rest.as_os_str().is_empty() {
        Some("~".to_string())
    } else {
        Some(format!("~/{}", rest.display()))
    }
}

/// A path as a POSIX shell word: as is when it's plain, single-quoted otherwise.
fn shell_quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c);
    if !path.is_empty() && path.chars().all(plain) {
        path.into_owned()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

fn default_db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Could not determine data directory")?;
    let path = data_dir.join(APP).join("vault.db");
    // Keep using an archive made before the project was renamed from claude-vault.
    let previous = data_dir.join("claude-vault").join("vault.db");
    if !path.exists() && previous.exists() {
        return Ok(previous);
    }
    std::fs::create_dir_all(data_dir.join(APP))?;
    Ok(path)
}

/// Open the interactive menu with the options `menu`, showing `status` first.
fn open_menu(conn: &rusqlite::Connection, menu: MenuArgs, status: Option<String>) -> Result<()> {
    let MenuArgs {
        query,
        limit,
        project,
        role,
        since,
        until,
        include_tools,
    } = menu;
    // Without a current directory (it was deleted), there's no current project.
    let current = match std::env::current_dir() {
        Ok(cwd) => current_project(conn, &cwd)?,
        Err(_) => None,
    };
    let (projects, projects_scope) = match &project {
        Some(name) => (
            Some(db::ProjectFilter::Containing(name.as_str())),
            format!("projects matching \"{name}\""),
        ),
        None => (None, "all projects".to_string()),
    };
    let (sessions, sessions_scope) = match &current {
        _ if projects.is_some() => (projects, projects_scope.clone()),
        Some((name, dir)) => (
            Some(db::ProjectFilter::Exact(name.as_str())),
            display_dir(dir),
        ),
        None => (None, "all projects".to_string()),
    };
    let opts = interactive::Options {
        limit: if limit == 0 { usize::MAX } else { limit },
        projects,
        projects_scope,
        sessions,
        sessions_scope,
        current_project: current.as_ref().map(|(name, _)| name.as_str()),
        role,
        since,
        until,
        include_tools,
    };
    interactive::run(conn, query, status, &opts)
}

/// What `update` imported, for the menu's status line.
fn update_summary(stats: &import::ImportStats) -> String {
    let files = plural(stats.files, "file", "files");
    let mut summary = match stats.imported {
        0 => format!("Up to date: nothing new in {files}"),
        n => format!(
            "Updated: {} archived from {files}",
            plural(n, "new message", "new messages")
        ),
    };
    if stats.errors > 0 {
        summary.push_str(&format!(" ({})", plural(stats.errors, "error", "errors")));
    }
    summary
}

/// "1 file", "2 files".
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn default_claude_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".claude"))
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    let command = match cli.command {
        Some(command) => command,
        // Without a terminal there's nothing to interact with: show the help and
        // fail, as a missing command did before interactive mode became the default.
        None if !interactive::has_terminal() => {
            eprint!("{}", Cli::command().render_help());
            std::process::exit(2);
        }
        None => Commands::Interactive {
            menu: MenuArgs::default(),
        },
    };

    // Handle completions before opening DB (no DB needed)
    if let Commands::Completions { shell } = &command {
        let mut cmd = Cli::command();
        generate(*shell, &mut cmd, APP, &mut std::io::stdout());
        return Ok(());
    }

    // CLAUDE_VAULT_DB is the variable from before the rename from claude-vault.
    let db = cli.db.or_else(|| {
        std::env::var_os("CLAUDE_VAULT_DB")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let explicit_db = db.is_some();
    let db_path = match db {
        Some(p) => p,
        None => default_db_path()?,
    };

    // Fail before importing anything, rather than after.
    if matches!(command, Commands::Update { .. }) && !interactive::has_terminal() {
        bail!(
            "`update` opens the interactive menu, which needs a terminal; use `{APP} import` in scripts"
        );
    }
    let interactive = matches!(command, Commands::Interactive { .. });
    // Check before opening, which would create an empty database.
    if interactive && !db_path.exists() {
        exit_with(&import_first_message(&db_path, explicit_db));
    }

    let conn = db::open_db(&db_path)?;
    if interactive && db::stats(&conn)?.1 == 0 {
        exit_with(&import_first_message(&db_path, explicit_db));
    }

    match command {
        Commands::Import { claude_dir } => {
            let claude_dir = match claude_dir {
                Some(d) => d,
                None => default_claude_dir()?,
            };
            import::import_all(&conn, &claude_dir)?;
        }
        Commands::Update { claude_dir, menu } => {
            let claude_dir = match claude_dir {
                Some(d) => d,
                None => default_claude_dir()?,
            };
            let projects = claude_dir.join("projects");
            eprintln!(
                "Archiving new conversations from {}...",
                display_dir(&projects)
            );
            let imported = import::import_all(&conn, &claude_dir);
            if db::stats(&conn)?.1 == 0 {
                imported?;
                exit_with(&format!(
                    "No conversations to archive in {}.",
                    projects.display()
                ));
            }
            // What's archived can be browsed anyway, e.g. after the transcripts
            // were deleted.
            let status = match imported {
                Ok(stats) => update_summary(&stats),
                Err(e) => format!("Could not archive new conversations: {e:#}"),
            };
            open_menu(&conn, menu, Some(status))?;
        }
        Commands::ImportFile { path, project } => {
            let project = project.unwrap_or_else(|| "unknown".to_string());
            let stats = import::import_jsonl_file(&conn, &path, &project)?;
            println!(
                "Imported {} messages ({} skipped, {} filtered, {} errors)",
                stats.imported, stats.skipped, stats.filtered, stats.errors
            );
        }
        Commands::Search {
            query,
            limit,
            project,
            role,
            since,
            until,
            json,
            include_tools,
        } => {
            let results = db::search(
                &conn,
                &query,
                limit,
                project.as_deref(),
                role.as_deref(),
                since.as_deref(),
                until.as_deref(),
            )?;
            let strip = !include_tools;
            if json {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .filter_map(|r| {
                        let content = if strip {
                            strip_tool_lines(&r.content)
                        } else {
                            r.content.clone()
                        };
                        if strip && content.trim().is_empty() {
                            return None;
                        }
                        Some(serde_json::json!({
                            "session_id": r.session_id,
                            "project": format_project_name(&r.project),
                            "role": r.role,
                            "content": content,
                            "timestamp": r.timestamp,
                        }))
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_results)?);
                return Ok(());
            }
            if results.is_empty() {
                println!("No results found.");
                return Ok(());
            }
            let mut printed = 0;
            for r in results.iter() {
                let content = if strip {
                    strip_tool_lines(&r.content)
                } else {
                    r.content.clone()
                };
                if strip && content.trim().is_empty() {
                    continue;
                }
                if printed > 0 {
                    println!("---");
                }
                let project = format_project_name(&r.project);
                println!(
                    "[{}] {} | {} | {}",
                    r.role,
                    project,
                    r.session_id,
                    r.timestamp.as_deref().unwrap_or("unknown")
                );
                let content: String = if content.chars().count() > 300 {
                    let truncated: String = content.chars().take(300).collect();
                    format!("{truncated}...")
                } else {
                    content
                };
                println!("{}", content);
                printed += 1;
            }
            if printed == 0 {
                println!("No results found.");
            }
        }
        Commands::Interactive { menu } => open_menu(&conn, menu, None)?,
        Commands::Export {
            session_id,
            last,
            format,
        } => {
            let resolved_id = match (session_id, last) {
                (_, Some(0)) => bail!("--last must be at least 1"),
                (_, Some(n)) => db::nth_recent_session_id(&conn, n.saturating_sub(1))?,
                (Some(prefix), None) => db::resolve_session_id(&conn, &prefix)?,
                (None, None) => bail!("Specify a session ID or use --last"),
            };
            let messages = db::get_session_messages(&conn, &resolved_id)?;
            if messages.is_empty() {
                bail!("No messages found for session: {resolved_id}");
            }
            let session_id = &resolved_id;
            let project = format_project_name(&messages[0].project);
            match format {
                ExportFormat::Markdown => {
                    println!("# Session: {session_id}");
                    println!("**Project:** {project}  ");
                    if let Some(ts) = &messages[0].timestamp {
                        println!("**Started:** {ts}  ");
                    }
                    println!();
                    for m in &messages {
                        let role_label = match m.role.as_str() {
                            "user" => "User",
                            "assistant" => "Assistant",
                            _ => &m.role,
                        };
                        println!("## {role_label}");
                        if let Some(ts) = &m.timestamp {
                            println!("*{ts}*\n");
                        }
                        println!("{}\n", m.content);
                    }
                }
                ExportFormat::Json => {
                    let json_messages: Vec<serde_json::Value> = messages
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "role": m.role,
                                "content": m.content,
                                "timestamp": m.timestamp,
                            })
                        })
                        .collect();
                    let output = serde_json::json!({
                        "session_id": session_id,
                        "project": project,
                        "messages": json_messages,
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
                ExportFormat::Text => {
                    println!("Session: {session_id}");
                    println!("Project: {project}");
                    println!();
                    for m in &messages {
                        let ts = m.timestamp.as_deref().unwrap_or("");
                        println!("[{}] {} {}", m.role, ts, m.content);
                        println!();
                    }
                }
            }
        }
        Commands::List {
            limit,
            project,
            since,
            until,
            json,
        } => {
            let limit = if limit == 0 { usize::MAX } else { limit };
            let sessions = db::list_sessions(
                &conn,
                limit,
                project.as_deref(),
                since.as_deref(),
                until.as_deref(),
            )?;
            if json {
                let json_sessions: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "session_id": s.session_id,
                            "project": format_project_name(&s.project),
                            "started_at": s.started_at,
                            "message_count": s.message_count,
                            "first_user_message": s.first_user_message,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_sessions)?);
                return Ok(());
            }
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }
            println!(
                "{:<10} {:<22} {:>5}  {:<25} PREVIEW",
                "ID", "DATE", "MSGS", "PROJECT"
            );
            println!("{}", "-".repeat(100));
            for s in &sessions {
                let ts = s
                    .started_at
                    .as_deref()
                    .unwrap_or("unknown")
                    .get(..19)
                    .unwrap_or("unknown");
                let short_id = &s.session_id[..8.min(s.session_id.len())];
                let project = format_project_name(&s.project);
                let project_display: String = if project.chars().count() > 25 {
                    project.chars().take(22).collect::<String>() + "..."
                } else {
                    project
                };
                let preview = s
                    .first_user_message
                    .as_deref()
                    .unwrap_or("")
                    .replace('\n', " ");
                let preview: String = if preview.chars().count() > 50 {
                    preview.chars().take(50).collect::<String>() + "..."
                } else {
                    preview
                };
                println!(
                    "{:<10} {:<22} {:>5}  {:<25} {}",
                    short_id, ts, s.message_count, project_display, preview
                );
            }
            println!("\nExport: {APP} export <ID>");
        }
        Commands::Delete { session_id, yes } => {
            let resolved_id = db::resolve_session_id(&conn, &session_id)?;
            let messages = db::get_session_messages(&conn, &resolved_id)?;
            let msg_count = messages.len();
            let project = messages
                .first()
                .map(|m| format_project_name(&m.project))
                .unwrap_or_default();

            if !yes {
                eprintln!("Delete session {resolved_id} ({project}, {msg_count} messages)? [y/N] ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            let deleted = db::delete_session(&conn, &resolved_id)?;
            println!("Deleted session {resolved_id} ({deleted} messages removed)");
        }
        Commands::Stats => {
            let (sessions, messages) = db::stats(&conn)?;
            println!("Database: {}", db_path.display());
            println!("Sessions: {}", sessions);
            println!("Messages: {}", messages);
        }
        Commands::Verify => {
            db::verify(&conn)?;
        }
        Commands::Completions { .. } => unreachable!(),
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_no_command_starts_interactive_mode() {
        let cli = Cli::try_parse_from([APP]).unwrap();
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from([APP, "--db", "vault.db"]).unwrap();
        assert!(cli.command.is_none());
        assert!(matches!(
            Cli::try_parse_from([APP, "i", "docker"]).unwrap().command,
            Some(Commands::Interactive {
                menu: MenuArgs {
                    query: Some(_),
                    limit: DEFAULT_INTERACTIVE_LIMIT,
                    ..
                }
            })
        ));
    }

    #[test]
    fn test_update_takes_the_menu_options() {
        let cli = Cli::try_parse_from([APP, "update", "docker", "-p", "app", "--claude-dir", "/c"]);
        let Some(Commands::Update { claude_dir, menu }) = cli.unwrap().command else {
            panic!("not update");
        };
        assert_eq!(claude_dir.as_deref(), Some(Path::new("/c")));
        assert_eq!(menu.query.as_deref(), Some("docker"));
        assert_eq!(menu.project.as_deref(), Some("app"));
        assert_eq!(menu.limit, DEFAULT_INTERACTIVE_LIMIT);
    }

    #[test]
    fn test_update_summary() {
        let stats = |imported, errors, files| import::ImportStats {
            imported,
            errors,
            files,
            ..Default::default()
        };
        assert_eq!(
            update_summary(&stats(0, 0, 12)),
            "Up to date: nothing new in 12 files"
        );
        assert_eq!(
            update_summary(&stats(1, 0, 1)),
            "Updated: 1 new message archived from 1 file"
        );
        assert_eq!(
            update_summary(&stats(37, 2, 12)),
            "Updated: 37 new messages archived from 12 files (2 errors)"
        );
    }

    #[test]
    fn test_import_first_message() {
        let default = import_first_message(Path::new("/data/vault.db"), false);
        assert!(default.starts_with("No conversations archived yet in /data/vault.db."));
        assert!(default.contains(&format!("Run `{APP} import` first")));

        let spaced = import_first_message(Path::new("/Users/me/My Archive/it's.db"), true);
        assert!(spaced.contains(r"--db '/Users/me/My Archive/it'\''s.db' import"));

        let custom = import_first_message(Path::new("/tmp/my.db"), true);
        assert!(custom.contains(&format!("Run `{APP} --db /tmp/my.db import` first")));
        assert!(custom.ends_with(&format!("then `{APP} --db /tmp/my.db` to browse them.")));
    }

    #[test]
    fn test_current_project_looks_in_parent_directories() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = db::open_db(tmp.path()).unwrap();
        db::upsert_session(&conn, "s1", "-srv-work-app", None).unwrap();
        let found = |cwd: &Path| {
            current_project(&conn, cwd)
                .unwrap()
                .map(|(name, dir)| (name, dir.display().to_string()))
        };
        let app = Some(("-srv-work-app".to_string(), "/srv/work/app".to_string()));
        assert_eq!(found(Path::new("/srv/work/app")), app);
        assert_eq!(found(Path::new("/srv/work/app/src/db")), app);
        assert_eq!(found(Path::new("/srv/work")), None);
        assert_eq!(found(Path::new("/srv/work/app2")), None);

        // Sessions started in the home directory are only its own.
        let home = dirs::home_dir().unwrap();
        let home_project = import::project_dir_name(&home);
        db::upsert_session(&conn, "s2", &home_project, None).unwrap();
        assert_eq!(found(&home).map(|(name, _)| name), Some(home_project));
        assert_eq!(found(&home.join("elsewhere")), None);
    }

    #[test]
    fn test_display_dir_shortens_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(display_dir(&home), "~");
        assert_eq!(display_dir(&home.join("src").join("app")), "~/src/app");
        assert_eq!(display_dir(Path::new("/srv/app")), "/srv/app");
        let root = Path::new("/");
        assert_eq!(shorten_home(Path::new("/srv/app"), root), None);
        assert_eq!(shorten_home(Path::new("/"), root), None);
        let home = Path::new("/data/home/me");
        assert_eq!(
            shorten_home(Path::new("/data/home/me/app"), home).as_deref(),
            Some("~/app")
        );
    }

    #[test]
    fn test_help_command_lists_the_commands() {
        let help = Cli::try_parse_from([APP, "help"])
            .err()
            .expect("help exits with its text");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let text = help.to_string();
        for command in ["search", "interactive", "update", "list", "help"] {
            assert!(
                text.contains(&format!("  {command} ")),
                "{command} missing:\n{text}"
            );
        }
    }

    #[test]
    fn test_strip_tool_lines_removes_tool_use() {
        let input = "Here is my plan\n[tool_use: Edit] {\"file\":\"foo.rs\"}\nDone editing.";
        assert_eq!(strip_tool_lines(input), "Here is my plan\nDone editing.");
    }

    #[test]
    fn test_strip_tool_lines_preserves_plain_text() {
        let input = "Just a normal message\nwith multiple lines";
        assert_eq!(strip_tool_lines(input), input);
    }

    #[test]
    fn test_strip_tool_lines_all_tools_becomes_empty() {
        let input = "[tool_use: Bash] {\"command\":\"ls\"}\n[tool_use: Edit] {\"file\":\"x\"}";
        assert_eq!(strip_tool_lines(input), "");
    }

    #[test]
    fn test_strip_tool_lines_preserves_brackets_in_text() {
        let input = "Use [this] syntax for arrays\n[tool_use: Write] {\"file\":\"x\"}\nEnd";
        assert_eq!(strip_tool_lines(input), "Use [this] syntax for arrays\nEnd");
    }

    #[test]
    fn test_strip_tool_lines_empty_input() {
        assert_eq!(strip_tool_lines(""), "");
    }

    #[test]
    fn test_strip_tool_lines_single_tool_line() {
        let input = "[tool_use: Bash] {\"command\":\"cargo test\"}";
        assert_eq!(strip_tool_lines(input), "");
    }
}
