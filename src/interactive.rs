//! cscope-style browser for archived conversations. As in cscope, the main menu
//! is a column of input fields: search all conversations, search in a project or
//! a session (chosen first, then searched as often as the user likes), or browse
//! a project's sessions. Results are numbered lists, newest first, to pick from
//! with the arrow keys or by number; the chosen conversation's question and
//! answer open in `$EDITOR`. Esc goes back a level.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use rusqlite::Connection;
use unicode_width::UnicodeWidthChar;

use crate::db::{
    self, Exchange, ProjectFilter, ProjectInfo, SearchFilters, SearchResult, SessionInfo,
    CONTEXT_LINES, MATCH_END, MATCH_START,
};
use crate::{import, APP};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const REVERSE: &str = "\x1b[7m";
const RESET: &str = "\x1b[0m";
/// Columns before an entry's number.
const INDENT: usize = 2;
/// Columns kept for an entry's text before other columns are dropped.
const MIN_TEXT_WIDTH: usize = 20;
/// Width of a date and time formatted with `DATE_TIME`.
const DATE_TIME: &str = "%Y-%m-%d %H:%M";
const DATE_TIME_WIDTH: usize = 16;
/// Widest the project column gets, when entries of several projects are listed.
const MAX_PROJECT_WIDTH: usize = 24;
/// Columns kept for the text typed in a form's field before its label is cut.
const MIN_FIELD_WIDTH: usize = 10;

/// Shown above the main menu's fields, room permitting.
const INTRO: [&str; 3] = [
    "Type what to look for in one of the fields below and press Enter.",
    "Search in a project or a session: choose which first, then type what to look \
     for in it, as often as you like. Anything typed in the field already is \
     searched for as soon as you choose.",
    "Project sessions: choose a project, then one of its sessions to browse. With \
     something typed, only the projects with matches are listed, then their matches.",
];

pub struct Options<'a> {
    /// Maximum matches per search.
    pub limit: usize,
    /// The projects searched and listed; `None` for all.
    pub projects: Option<ProjectFilter<'a>>,
    /// What to call them, e.g. "all projects".
    pub projects_scope: String,
    /// The projects whose sessions "Search in a session" lists: by default the
    /// current directory's. `None` for all.
    pub sessions: Option<ProjectFilter<'a>>,
    /// What to call them, e.g. "~/src/app".
    pub sessions_scope: String,
    /// The current directory's project, selected first in lists of projects.
    pub current_project: Option<&'a str>,
    pub role: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub include_tools: bool,
}

impl Options<'_> {
    /// How `--since` and `--until` restrict the sessions listed, for messages.
    fn when(&self) -> &'static str {
        if self.since.is_some() || self.until.is_some() {
            " from that time"
        } else {
            ""
        }
    }
}

/// The main menu's fields, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    All,
    Project,
    Session,
    ProjectSessions,
}

impl Field {
    const ALL: [Field; 4] = [
        Field::All,
        Field::Project,
        Field::Session,
        Field::ProjectSessions,
    ];

    fn label(self) -> &'static str {
        match self {
            Field::All => "Search all conversations:",
            Field::Project => "Search in a project:",
            Field::Session => "Search in a session:",
            Field::ProjectSessions => "Project sessions:",
        }
    }

    /// Shown, dimmed, in the field while it's empty.
    fn hint(self, opts: &Options) -> String {
        match self {
            Field::All => format!("(in {})", opts.projects_scope),
            Field::Project => "(choose a project, then search it)".to_string(),
            Field::Session => format!(
                "(choose a session in {}, then search it)",
                opts.sessions_scope
            ),
            Field::ProjectSessions => "(choose a project, then one of its sessions)".to_string(),
        }
    }
}

/// A screen of input fields at the bottom, as cscope's main menu, under a
/// header and as much of an introduction as fits.
struct Form {
    title: String,
    info: Vec<String>,
    /// Paragraphs shown above the fields, whole or not at all.
    intro: Vec<String>,
    /// Each field's label, and the hint shown dimmed while it's empty.
    fields: Vec<(String, String)>,
    /// The text typed in each field.
    texts: Vec<String>,
    /// The field being typed in.
    focus: usize,
    status: Option<String>,
    /// Whether Esc clears the field before leaving the form, as in the main
    /// menu, where leaving quits.
    esc_clears: bool,
    /// What leaving the form does, for the footer: "quit" or "back".
    leave: &'static str,
}

/// What the user did in a form.
enum FormChoice {
    /// Enter in this field.
    Search(usize),
    /// Esc: back a level, which from the main menu quits.
    Back,
    Quit,
}

/// How a level of the browser was left.
enum Flow {
    /// Esc: back to the level above.
    Back,
    Quit,
    /// It had nothing to show: the message to show in the level above instead.
    Failed(String),
}

/// What the user chose in a list.
enum Choice {
    /// The entry at this index.
    Open(usize),
    /// Esc: back a level.
    Back,
    Quit,
}

enum Input {
    Key(KeyEvent),
    /// Pasted text, delivered whole so its newlines and digits aren't taken as keys.
    Paste(String),
    Redraw,
}

/// How times are shown in a list of conversations.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Times {
    /// All on one day, shown in the header.
    Time,
    /// Within a session spanning several days.
    DayTime,
    Full,
}

impl Times {
    fn format(self) -> &'static str {
        match self {
            Times::Time => "%H:%M",
            Times::DayTime => "%m-%d %H:%M",
            Times::Full => DATE_TIME,
        }
    }

    fn width(self) -> usize {
        match self {
            Times::Time => 5,
            Times::DayTime => 11,
            Times::Full => DATE_TIME_WIDTH,
        }
    }
}

/// A search's matches: one per conversation, newest first.
struct Found {
    exchanges: Vec<Exchange>,
    /// Whether `Options::limit` may have left matches out.
    limited: bool,
}

impl Found {
    /// "12 matches", or "best 200 matches" when there may be more.
    fn summary(&self) -> String {
        let best = if self.limited { "best " } else { "" };
        format!("{best}{}", count(self.exchanges.len(), "match", "matches"))
    }
}

/// A project in a list of projects.
struct ProjectEntry {
    project: String,
    /// As shown: its directory, where that still exists.
    name: String,
    /// When its latest session started or, when searching, its latest match was asked.
    latest: Option<String>,
    /// Its sessions or, when searching, those with matches.
    sessions: usize,
    /// When searching, its matches.
    matches: usize,
    /// When searching, the best `Options::limit` of its matches, newest first.
    hits: Vec<Exchange>,
}

/// Names to show for projects: their directories where these still exist, else
/// what `db::format_project_name` makes of them. Looked up once each.
#[derive(Default)]
struct ProjectNames(RefCell<HashMap<String, String>>);

impl ProjectNames {
    fn get(&self, project: &str) -> String {
        if let Some(name) = self.0.borrow().get(project) {
            return name.clone();
        }
        let name = match import::project_dir(project) {
            Some(dir) => crate::display_dir(&dir),
            None => db::format_project_name(project),
        };
        self.0
            .borrow_mut()
            .insert(project.to_string(), name.clone());
        name
    }
}

/// Whether stdin and stdout are a terminal, as interactive mode needs.
pub fn has_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Run the browser, searching all conversations for `initial_query` first if
/// given, with `status` in the main menu's footer until a key is pressed.
pub fn run(
    conn: &Connection,
    initial_query: Option<String>,
    status: Option<String>,
    opts: &Options,
) -> Result<()> {
    if !has_terminal() {
        bail!("interactive mode needs a terminal; use `{APP} search` in scripts");
    }
    let app = App {
        conn,
        opts,
        screen: Screen::enter()?,
        names: ProjectNames::default(),
    };
    let mut menu = Form::main_menu(opts);
    menu.status = status;
    let mut next = None;
    if let Some(query) = initial_query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
    {
        menu.texts[Field::All as usize] = query;
        next = Some(Field::All);
    }
    loop {
        let field = match next.take() {
            Some(field) => field,
            None => match menu.choose()? {
                FormChoice::Search(index) => Field::ALL[index],
                FormChoice::Back | FormChoice::Quit => return Ok(()),
            },
        };
        let text = &mut menu.texts[field as usize];
        let query = text.trim().to_string();
        let flow = match field {
            Field::All => app.search_all(&query)?,
            Field::Project => app.search_in_a_project(text)?,
            Field::Session => app.search_in_a_session(text)?,
            Field::ProjectSessions => app.project_sessions(&query)?,
        };
        match flow {
            Flow::Back => {}
            Flow::Quit => return Ok(()),
            Flow::Failed(message) => menu.status = Some(message),
        }
    }
}

struct App<'a, 'o> {
    conn: &'a Connection,
    opts: &'a Options<'o>,
    screen: Screen,
    names: ProjectNames,
}

impl App<'_, '_> {
    /// Search all conversations, and browse the matches.
    fn search_all(&self, query: &str) -> Result<Flow> {
        if query.is_empty() {
            return Ok(Flow::Failed("Type what to search for".to_string()));
        }
        let opts = self.opts;
        let scope = Scope::projects(opts.projects, &opts.projects_scope);
        let found = match search(self.conn, opts, query, scope) {
            Ok(found) => found,
            Err(message) => return Ok(Flow::Failed(message)),
        };
        let list = ConversationList::new(
            &found.exchanges,
            quoted(query),
            vec![found.summary(), opts.projects_scope.clone()],
            Times::Full,
            &self.names,
        );
        self.browse(&list)
    }

    /// Choose a project, then search in it as often as the user likes. The
    /// `query` typed in the main menu is searched for as soon as a project is
    /// chosen, and is left as the user last typed it.
    fn search_in_a_project(&self, query: &mut String) -> Result<Flow> {
        let opts = self.opts;
        let projects = self.projects()?;
        if projects.is_empty() {
            return Ok(self.no_sessions(&opts.projects_scope));
        }
        let mut info = vec![count(projects.len(), "project", "projects")];
        if opts.projects.is_some() {
            info.push(opts.projects_scope.clone());
        }
        let list = ProjectList::new(&projects, "Search in a project".to_string(), info, false);
        let mut state = ListState::at(self.current(&projects));
        loop {
            let project = match choose(&list, &mut state)? {
                Choice::Open(index) => &projects[index],
                Choice::Back => return Ok(Flow::Back),
                Choice::Quit => return Ok(Flow::Quit),
            };
            let prompt = Form::prompt(
                project.name.clone(),
                vec![count(project.sessions, "session", "sessions")],
                "project",
            );
            let filter = Some(ProjectFilter::Exact(&project.project));
            let scope = Scope::projects(filter, &project.name);
            let flow = self.search_in(prompt, scope, query, |found| {
                (vec![found.summary(), project.name.clone()], Times::Full)
            })?;
            if state.returned(flow) {
                return Ok(Flow::Quit);
            }
        }
    }

    /// Choose a session, then search in it as often as the user likes, as
    /// `search_in_a_project` does in a project.
    fn search_in_a_session(&self, query: &mut String) -> Result<Flow> {
        let opts = self.opts;
        let sessions = self.sessions_of(opts.sessions)?;
        if sessions.is_empty() {
            return Ok(self.no_sessions(&opts.sessions_scope));
        }
        let info = vec![
            count(sessions.len(), "session", "sessions"),
            opts.sessions_scope.clone(),
        ];
        let list = SessionList::new(
            &sessions,
            "Search in a session".to_string(),
            info,
            &self.names,
        );
        let mut state = ListState::default();
        loop {
            let session = match choose(&list, &mut state)? {
                Choice::Open(index) => &sessions[index],
                Choice::Back => return Ok(Flow::Back),
                Choice::Quit => return Ok(Flow::Quit),
            };
            let id = format!("session {}", short_id(&session.session_id));
            let started = local_time(session.started.as_deref().unwrap_or_default(), DATE_TIME);
            let mut about = vec![id.clone()];
            if list.project_width > 0 {
                about.push(self.names.get(&session.project));
            }
            let mut prompt_info = vec![count(session.questions, "question", "questions")];
            prompt_info.extend(about.iter().cloned());
            let prompt = Form::prompt(started.clone(), prompt_info, "session");
            let scope = Scope {
                project: None,
                session: Some(&session.session_id),
                name: &id,
            };
            let flow = self.search_in(prompt, scope, query, |found| {
                let mut info = vec![found.summary(), started.clone()];
                info.extend(about.iter().cloned());
                (info, session_times(session, &found.exchanges))
            })?;
            if state.returned(flow) {
                return Ok(Flow::Quit);
            }
        }
    }

    /// Search `scope` for `query` right away (unless it's empty), then for
    /// whatever the user types in `prompt`, until they go back or quit; `query`
    /// is left as they last typed it. `header` gives a list of what was found
    /// the info in its header, and how it shows times.
    fn search_in(
        &self,
        mut prompt: Form,
        scope: Scope,
        query: &mut String,
        header: impl Fn(&Found) -> (Vec<String>, Times),
    ) -> Result<Flow> {
        prompt.texts[0] = query.clone();
        let mut ask = query.trim().is_empty();
        loop {
            if ask {
                let choice = prompt.choose()?;
                query.clone_from(&prompt.texts[0]);
                match choice {
                    FormChoice::Search(_) => {}
                    FormChoice::Back => return Ok(Flow::Back),
                    FormChoice::Quit => return Ok(Flow::Quit),
                }
            }
            ask = true;
            let text = query.trim();
            if text.is_empty() {
                prompt.status = Some("Type what to search for".to_string());
                continue;
            }
            let found = match search(self.conn, self.opts, text, scope) {
                Ok(found) => found,
                Err(message) => {
                    prompt.status = Some(message);
                    continue;
                }
            };
            let (info, times) = header(&found);
            let list =
                ConversationList::new(&found.exchanges, quoted(text), info, times, &self.names);
            match self.browse(&list)? {
                Flow::Quit => return Ok(Flow::Quit),
                Flow::Back => {}
                Flow::Failed(message) => prompt.status = Some(message),
            }
        }
    }

    /// Choose a project, then: with a query, one of its matches; without, one
    /// of its sessions and a conversation in it.
    fn project_sessions(&self, query: &str) -> Result<Flow> {
        let opts = self.opts;
        let scope = &opts.projects_scope;
        let (projects, title, mut info) = if query.is_empty() {
            let projects = self.projects()?;
            if projects.is_empty() {
                return Ok(self.no_sessions(scope));
            }
            let info = vec![count(projects.len(), "project", "projects")];
            (projects, "Project sessions".to_string(), info)
        } else {
            let all = Scope::projects(opts.projects, scope);
            let ranked = match matches(self.conn, opts, query, all, usize::MAX) {
                Ok(ranked) => ranked,
                Err(message) => return Ok(Flow::Failed(message)),
            };
            let summary = format!("{} in", count(ranked.len(), "match", "matches"));
            let projects = by_project(ranked, &self.names, opts.limit);
            let summary = format!("{summary} {}", count(projects.len(), "project", "projects"));
            (projects, quoted(query), vec![summary])
        };
        if opts.projects.is_some() {
            info.push(scope.clone());
        }
        let list = ProjectList::new(&projects, title, info, !query.is_empty());
        let mut state = ListState::at(self.current(&projects));
        loop {
            let project = match choose(&list, &mut state)? {
                Choice::Open(index) => &projects[index],
                Choice::Back => return Ok(Flow::Back),
                Choice::Quit => return Ok(Flow::Quit),
            };
            let flow = if query.is_empty() {
                self.sessions(Some(ProjectFilter::Exact(&project.project)), &project.name)?
            } else {
                let list = ConversationList::new(
                    &project.hits,
                    quoted(query),
                    vec![
                        shown_of(project.hits.len(), project.matches),
                        project.name.clone(),
                    ],
                    Times::Full,
                    &self.names,
                );
                self.browse(&list)?
            };
            if state.returned(flow) {
                return Ok(Flow::Quit);
            }
        }
    }

    /// Choose one of the sessions of the projects `project` selects (all
    /// without one), called `scope`, then a conversation in it.
    fn sessions(&self, project: Option<ProjectFilter>, scope: &str) -> Result<Flow> {
        let sessions = self.sessions_of(project)?;
        if sessions.is_empty() {
            return Ok(self.no_sessions(scope));
        }
        let list = SessionList::new(
            &sessions,
            scope.to_string(),
            vec![count(sessions.len(), "session", "sessions")],
            &self.names,
        );
        let mut state = ListState::default();
        loop {
            let info = match choose(&list, &mut state)? {
                Choice::Open(index) => &sessions[index],
                Choice::Back => return Ok(Flow::Back),
                Choice::Quit => return Ok(Flow::Quit),
            };
            let flow = match db::session_exchanges(self.conn, &info.session_id) {
                Err(e) => Flow::Failed(format!("{e:#}")),
                Ok(exchanges) if exchanges.is_empty() => {
                    Flow::Failed("Nothing archived in this session".to_string())
                }
                Ok(exchanges) => {
                    let exchanges = newest_first(exchanges);
                    // Not `info.questions`: threads answered before any
                    // question in them are listed too.
                    let mut header = vec![
                        count(exchanges.len(), "conversation", "conversations"),
                        format!("session {}", short_id(&info.session_id)),
                    ];
                    if list.project_width > 0 {
                        header.push(self.names.get(&info.project));
                    }
                    let conversations = ConversationList::new(
                        &exchanges,
                        local_time(info.started.as_deref().unwrap_or_default(), DATE_TIME),
                        header,
                        session_times(info, &exchanges),
                        &self.names,
                    );
                    self.browse(&conversations)?
                }
            };
            if state.returned(flow) {
                return Ok(Flow::Quit);
            }
        }
    }

    /// Show a list of conversations, opening the chosen ones, until the user
    /// goes back or quits.
    fn browse(&self, list: &ConversationList) -> Result<Flow> {
        let mut state = ListState::default();
        loop {
            match choose(list, &mut state)? {
                Choice::Quit => return Ok(Flow::Quit),
                Choice::Back => return Ok(Flow::Back),
                Choice::Open(index) => {
                    let exchange = &list.exchanges[index];
                    let project = self.names.get(&exchange.project);
                    let opened =
                        open_exchange(self.conn, &self.screen, exchange, &project, self.opts);
                    if let Err(e) = opened {
                        state.status = Some(format!("{e:#}"));
                    }
                }
            }
        }
    }

    /// The projects to list, with the latest session first.
    fn projects(&self) -> Result<Vec<ProjectEntry>> {
        let opts = self.opts;
        let mut projects: Vec<ProjectEntry> = db::projects(
            self.conn,
            opts.projects,
            opts.since.as_deref(),
            opts.until.as_deref(),
        )?
        .into_iter()
        .map(|info| project_entry(info, &self.names))
        .collect();
        projects.sort_by(newest_project_first);
        Ok(projects)
    }

    /// The sessions of the projects `project` selects (all without one) to
    /// list, the newest first.
    fn sessions_of(&self, project: Option<ProjectFilter>) -> Result<Vec<SessionInfo>> {
        let opts = self.opts;
        let mut sessions = db::project_sessions(
            self.conn,
            project,
            opts.since.as_deref(),
            opts.until.as_deref(),
        )?;
        sessions.sort_by(newest_session_first);
        Ok(sessions)
    }

    /// Where the current directory's project is in `projects`, to select it
    /// first; else the top.
    fn current(&self, projects: &[ProjectEntry]) -> usize {
        projects
            .iter()
            .position(|p| Some(p.project.as_str()) == self.opts.current_project)
            .unwrap_or(0)
    }

    /// Why there's nothing to list in `scope`.
    fn no_sessions(&self, scope: &str) -> Flow {
        let when = self.opts.when();
        Flow::Failed(format!("No sessions archived{when} in {scope}"))
    }
}

/// Where to search: the projects a filter selects (all without one), or one
/// session; and what to call that in messages.
#[derive(Clone, Copy)]
struct Scope<'s> {
    project: Option<ProjectFilter<'s>>,
    session: Option<&'s str>,
    name: &'s str,
}

impl<'s> Scope<'s> {
    fn projects(project: Option<ProjectFilter<'s>>, name: &'s str) -> Self {
        Scope {
            project,
            session: None,
            name,
        }
    }
}

/// Search `scope` for the best `Options::limit` matches; on finding nothing or
/// failing, the message to show instead.
fn search(conn: &Connection, opts: &Options, query: &str, scope: Scope) -> Result<Found, String> {
    let hits = matches(conn, opts, query, scope, opts.limit)?;
    Ok(Found {
        limited: hits.len() == opts.limit,
        exchanges: newest_first(hits),
    })
}

/// Up to `limit` matches in `scope`, best first; on finding nothing or
/// failing, the message to show instead.
fn matches(
    conn: &Connection,
    opts: &Options,
    query: &str,
    scope: Scope,
    limit: usize,
) -> Result<Vec<Exchange>, String> {
    let filters = SearchFilters {
        project: scope.project,
        session: scope.session,
        role: opts.role.as_deref(),
        since: opts.since.as_deref(),
        until: opts.until.as_deref(),
    };
    match db::search_exchanges(conn, query, limit, &filters, opts.include_tools) {
        Ok(hits) if hits.is_empty() => Err(format!("No matches for \"{query}\" in {}", scope.name)),
        Ok(hits) => Ok(hits),
        Err(e) => Err(format!("Search failed: {e:#}")),
    }
}

fn project_entry(info: ProjectInfo, names: &ProjectNames) -> ProjectEntry {
    ProjectEntry {
        name: names.get(&info.project),
        project: info.project,
        latest: info.latest,
        sessions: info.sessions,
        matches: 0,
        hits: Vec::new(),
    }
}

/// All of a search's matches (best first) by project, the project with the
/// newest match first, each keeping its best `limit`, newest first.
fn by_project(ranked: Vec<Exchange>, names: &ProjectNames, limit: usize) -> Vec<ProjectEntry> {
    let mut groups: Vec<Vec<Exchange>> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for exchange in ranked {
        let at = *positions
            .entry(exchange.project.clone())
            .or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
        groups[at].push(exchange);
    }
    let mut projects: Vec<ProjectEntry> = groups
        .into_iter()
        .map(|mut hits| {
            let sessions: HashSet<&str> = hits.iter().map(|h| h.session_id.as_str()).collect();
            let sessions = sessions.len();
            let latest = hits
                .iter()
                .filter_map(exchange_time)
                .max_by_key(|time| parse_timestamp(time))
                .map(str::to_string);
            let matches = hits.len();
            hits.truncate(limit);
            ProjectEntry {
                project: hits[0].project.clone(),
                name: names.get(&hits[0].project),
                latest,
                sessions,
                matches,
                hits: newest_first(hits),
            }
        })
        .collect();
    projects.sort_by(newest_project_first);
    projects
}

/// "12 matches", or "best 200 of 1234 matches" when only `shown` of `total` are.
fn shown_of(shown: usize, total: usize) -> String {
    if shown < total {
        format!("best {shown} of {total} matches")
    } else {
        count(total, "match", "matches")
    }
}

impl Form {
    /// The main menu, with a field per kind of search.
    fn main_menu(opts: &Options) -> Self {
        Form {
            title: APP.to_string(),
            info: vec![format!("version {}", env!("CARGO_PKG_VERSION"))],
            intro: INTRO.iter().map(|p| p.to_string()).collect(),
            fields: Field::ALL
                .iter()
                .map(|f| (f.label().to_string(), f.hint(opts)))
                .collect(),
            texts: vec![String::new(); Field::ALL.len()],
            focus: 0,
            status: None,
            esc_clears: true,
            leave: "quit",
        }
    }

    /// Where to type what to search for in the project or session (`what`)
    /// that the header (`title` and `info`) describes.
    fn prompt(title: String, info: Vec<String>, what: &str) -> Self {
        Form {
            title,
            info,
            intro: vec![format!(
                "Type what to look for in this {what} and press Enter. After going \
                 back from the matches, you can search for something else here."
            )],
            fields: vec![(
                format!("Search in this {what}:"),
                "(what to look for)".to_string(),
            )],
            texts: vec![String::new()],
            focus: 0,
            status: None,
            esc_clears: false,
            leave: "back",
        }
    }

    /// Let the user type in the fields until they start a search or leave.
    fn choose(&mut self) -> Result<FormChoice> {
        let fields = self.fields.len();
        loop {
            self.draw()?;
            let key = match read_input()? {
                Input::Key(key) => key,
                Input::Paste(text) => {
                    append_paste(&mut self.texts[self.focus], &text);
                    continue;
                }
                Input::Redraw => continue,
            };
            self.status = None;
            let ctrl = ctrl_key(&key);
            let text = &mut self.texts[self.focus];
            match key.code {
                KeyCode::Char('c' | 'd') if ctrl => return Ok(FormChoice::Quit),
                KeyCode::Esc if self.esc_clears && !text.is_empty() => text.clear(),
                KeyCode::Esc => return Ok(FormChoice::Back),
                KeyCode::Enter => return Ok(FormChoice::Search(self.focus)),
                KeyCode::Up | KeyCode::BackTab => self.focus = (self.focus + fields - 1) % fields,
                KeyCode::Char('p') if ctrl => self.focus = (self.focus + fields - 1) % fields,
                KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1) % fields,
                KeyCode::Char('n') if ctrl => self.focus = (self.focus + 1) % fields,
                _ => edit(text, key),
            }
        }
    }

    /// The header, the introduction as room permits, the fields at the bottom
    /// (as in cscope) and the footer, with the cursor in the focused field.
    fn draw(&self) -> Result<()> {
        let (cols, rows) = terminal::size()?;
        let (cols, rows) = (cols as usize, rows as usize);
        let footer = rows.saturating_sub(1);
        let first_field = footer.saturating_sub(self.fields.len());
        let mut out = io::stdout().lock();
        queue!(out, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
        write!(out, "{}", header_line(&self.title, &self.info, cols))?;

        // Whole paragraphs with blank lines between them, leaving one above
        // the fields.
        let mut row = 2;
        for paragraph in &self.intro {
            let lines = wrap(paragraph, cols.saturating_sub(INDENT));
            if row + lines.len() + 1 > first_field {
                break;
            }
            for line in lines {
                queue!(out, MoveTo(0, row as u16))?;
                write!(out, "{}{DIM}{line}{RESET}", " ".repeat(INDENT))?;
                row += 1;
            }
            row += 1;
        }

        let label_width = self
            .fields
            .iter()
            .map(|(label, _)| display_width(label))
            .max();
        let text_col = (label_width.unwrap_or(0) + 1).min(cols.saturating_sub(MIN_FIELD_WIDTH));
        let room = cols.saturating_sub(text_col + 1);
        let mut cursor = (0, 0);
        for (i, (label, hint)) in self.fields.iter().enumerate() {
            let row = first_field + i;
            if row >= footer {
                break;
            }
            queue!(out, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
            let style = if i == self.focus { BOLD } else { "" };
            let label = fit(label, text_col.saturating_sub(1));
            write!(out, "{style}{label}{RESET}")?;
            let text = tail_to_width(&self.texts[i], room);
            queue!(out, MoveTo(text_col as u16, row as u16))?;
            write!(out, "{text}")?;
            if text.is_empty() {
                write!(out, "{DIM}{}{RESET}", fit(hint, room))?;
            }
            if i == self.focus {
                cursor = (text_col + display_width(text), row);
            }
        }

        queue!(out, MoveTo(0, footer as u16))?;
        match &self.status {
            Some(status) => write!(out, "{BOLD}{}{RESET}", fit(status, cols))?,
            None => write!(out, "{DIM}{}{RESET}", fit(&self.help(), cols))?,
        }
        queue!(out, MoveTo(cursor.0 as u16, cursor.1 as u16), Show)?;
        out.flush()?;
        Ok(())
    }

    /// The keys, for the footer.
    fn help(&self) -> String {
        let esc = if self.esc_clears && !self.texts[self.focus].is_empty() {
            "clear"
        } else {
            self.leave
        };
        let choose = if self.fields.len() > 1 {
            "↑/↓ choose the search · "
        } else {
            ""
        };
        format!("{choose}Enter search · Esc {esc}")
    }
}

/// Whether Ctrl is held for a shortcut, rather than as part of AltGr, which
/// Windows reports as Ctrl+Alt.
fn ctrl_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

/// Apply an editing key to a line of input.
fn edit(input: &mut String, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('u') if ctrl_key(&key) => input.clear(),
        KeyCode::Char('w') if ctrl_key(&key) => {
            let kept = input.trim_end().rfind(' ').map_or(0, |i| i + 1);
            input.truncate(kept);
        }
        // Typed plainly, or with AltGr (Ctrl+Alt).
        KeyCode::Char(c) if ctrl == alt => input.push(c),
        KeyCode::Backspace => {
            input.pop();
        }
        _ => {}
    }
}

/// Add pasted text to the query on one line, so a multi-line paste isn't submitted.
fn append_paste(input: &mut String, text: &str) {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return;
    }
    if !input.is_empty() && !input.ends_with(' ') && text.starts_with(char::is_whitespace) {
        input.push(' ');
    }
    input.extend(words.join(" ").chars().filter(|c| !c.is_control()));
}

/// Where a list is: its selected entry, and a message for the footer.
#[derive(Default)]
struct ListState {
    selected: usize,
    status: Option<String>,
}

impl ListState {
    fn at(selected: usize) -> Self {
        ListState {
            selected,
            status: None,
        }
    }

    /// Back from a level below that left with `flow`: whether to quit. Shows
    /// the message of a level that had nothing to show.
    fn returned(&mut self, flow: Flow) -> bool {
        match flow {
            Flow::Quit => true,
            Flow::Back => false,
            Flow::Failed(message) => {
                self.status = Some(message);
                false
            }
        }
    }
}

/// A list of numbered entries, each `entry_height` lines tall.
trait Listing {
    fn len(&self) -> usize;
    fn entry_height(&self) -> usize;
    /// The header line, ending with `page` ("page 1/3") when there are several.
    fn header(&self, page: Option<String>, cols: usize) -> String;
    /// The lines of entry `index`, numbered `index + 1`.
    fn entry(&self, index: usize, number_width: usize, cols: usize) -> Vec<String>;
}

/// Let the user choose an entry of `list`, go back or quit. The arrow keys
/// select an entry, as does typing its number, and Enter opens it.
fn choose(list: &dyn Listing, state: &mut ListState) -> Result<Choice> {
    let mut typed = String::new();
    loop {
        state.selected = state.selected.min(list.len().saturating_sub(1));
        draw_list(list, state, &typed)?;
        let key = match read_input()? {
            Input::Key(key) => key,
            Input::Paste(_) | Input::Redraw => continue,
        };
        state.status = None;
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl_key(&key) => return Ok(Choice::Quit),
            KeyCode::Char('q') => return Ok(Choice::Quit),
            KeyCode::Esc if typed.is_empty() => return Ok(Choice::Back),
            KeyCode::Esc => typed.clear(),
            KeyCode::Enter if list.len() > 0 => return Ok(Choice::Open(state.selected)),
            code => {
                let size = page_size(list)?;
                state.selected = navigate(code, state.selected, list.len(), size, &mut typed);
            }
        }
    }
}

/// The entry selected after `code` in a list of `len` entries shown `size` at a
/// time, where `selected` was. Digits add to the entry number `typed`, which
/// only ever holds a valid one, and select it; other keys clear it.
fn navigate(code: KeyCode, selected: usize, len: usize, size: usize, typed: &mut String) -> usize {
    let last = len.saturating_sub(1);
    match code {
        KeyCode::Char(d) if d.is_ascii_digit() => {
            typed.push(d);
            match typed.parse::<usize>() {
                Ok(n) if (1..=len).contains(&n) => return n - 1,
                _ => {
                    typed.pop();
                }
            }
            selected
        }
        KeyCode::Backspace => {
            typed.pop();
            // What's left of a valid number is one too.
            typed.parse::<usize>().map_or(selected, |n| n - 1)
        }
        code => {
            typed.clear();
            match code {
                KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
                KeyCode::PageDown | KeyCode::Char(' ' | '+') => {
                    ((selected / size + 1) * size).min(last)
                }
                KeyCode::PageUp | KeyCode::Char('-' | 'b') => {
                    (selected / size).saturating_sub(1) * size
                }
                KeyCode::Home => 0,
                KeyCode::End => last,
                _ => selected,
            }
        }
    }
}

/// The list of projects, all of them or those a search matched.
struct ProjectList<'a> {
    projects: &'a [ProjectEntry],
    title: String,
    info: Vec<String>,
    /// Each project's sessions, or matches when searching.
    counts: Vec<String>,
    count_width: usize,
}

impl<'a> ProjectList<'a> {
    fn new(projects: &'a [ProjectEntry], title: String, info: Vec<String>, searched: bool) -> Self {
        let (counts, count_width) = count_column(projects.iter().map(|p| {
            if searched {
                (p.matches, "match", "matches")
            } else {
                (p.sessions, "session", "sessions")
            }
        }));
        ProjectList {
            projects,
            title,
            info,
            counts,
            count_width,
        }
    }
}

impl Listing for ProjectList<'_> {
    fn len(&self) -> usize {
        self.projects.len()
    }

    fn entry_height(&self) -> usize {
        1
    }

    fn header(&self, page: Option<String>, cols: usize) -> String {
        let mut info = self.info.clone();
        info.extend(page);
        header_line(&self.title, &info, cols)
    }

    /// Number, latest session or match, count and name.
    fn entry(&self, index: usize, number_width: usize, cols: usize) -> Vec<String> {
        let project = &self.projects[index];
        let mut line = number_column(index + 1, number_width);
        let mut columns = Columns::new(cols, number_column_width(number_width));
        if columns.fits(DATE_TIME_WIDTH) {
            let latest = project.latest.as_deref().unwrap_or_default();
            line.push_str(&pad(&local_time(latest, DATE_TIME), DATE_TIME_WIDTH));
            line.push_str("  ");
        }
        if columns.fits(self.count_width) {
            line.push_str(&count_cell(&self.counts[index], self.count_width));
        }
        line.push_str(&fit(&project.name, columns.room()));
        vec![line]
    }
}

/// A list of sessions.
struct SessionList<'a> {
    sessions: &'a [SessionInfo],
    title: String,
    info: Vec<String>,
    /// Each session's project, when they're in several; otherwise empty.
    projects: Vec<String>,
    /// Width of the project column; 0 without one.
    project_width: usize,
    /// Each session's questions.
    counts: Vec<String>,
    count_width: usize,
}

impl<'a> SessionList<'a> {
    fn new(
        sessions: &'a [SessionInfo],
        title: String,
        info: Vec<String>,
        names: &ProjectNames,
    ) -> Self {
        let projects = project_column(sessions.iter().map(|s| s.project.as_str()), names);
        let (counts, count_width) = count_column(
            sessions
                .iter()
                .map(|s| (s.questions, "question", "questions")),
        );
        SessionList {
            sessions,
            title,
            info,
            project_width: column_width(&projects),
            projects,
            counts,
            count_width,
        }
    }
}

impl Listing for SessionList<'_> {
    fn len(&self) -> usize {
        self.sessions.len()
    }

    fn entry_height(&self) -> usize {
        1
    }

    fn header(&self, page: Option<String>, cols: usize) -> String {
        let mut info = self.info.clone();
        info.extend(page);
        header_line(&self.title, &info, cols)
    }

    /// Number, start, project (when there are several), count and first question.
    fn entry(&self, index: usize, number_width: usize, cols: usize) -> Vec<String> {
        let session = &self.sessions[index];
        let mut line = number_column(index + 1, number_width);
        let mut columns = Columns::new(cols, number_column_width(number_width));
        if columns.fits(DATE_TIME_WIDTH) {
            let started = session.started.as_deref().unwrap_or_default();
            line.push_str(&pad(&local_time(started, DATE_TIME), DATE_TIME_WIDTH));
            line.push_str("  ");
        }
        if columns.fits(self.project_width) {
            line.push_str(&pad(&self.projects[index], self.project_width));
            line.push_str("  ");
        }
        if columns.fits(self.count_width) {
            line.push_str(&count_cell(&self.counts[index], self.count_width));
        }
        match &session.first_question {
            Some(question) => line.push_str(&fit(question, columns.room())),
            None => line.push_str(&format!(
                "{DIM}{}{RESET}",
                fit("(no question archived)", columns.room())
            )),
        }
        vec![line]
    }
}

/// A list of conversations: a session's, or those a search matched.
struct ConversationList<'a> {
    /// Newest first.
    exchanges: &'a [Exchange],
    title: String,
    info: Vec<String>,
    times: Times,
    /// Each conversation's project, when they're in several; otherwise empty.
    projects: Vec<String>,
    project_width: usize,
    /// Whether a search found the conversations, so each shows the match.
    searched: bool,
}

impl<'a> ConversationList<'a> {
    fn new(
        exchanges: &'a [Exchange],
        title: String,
        info: Vec<String>,
        times: Times,
        names: &ProjectNames,
    ) -> Self {
        let projects = project_column(exchanges.iter().map(|e| e.project.as_str()), names);
        ConversationList {
            exchanges,
            title,
            info,
            times,
            project_width: column_width(&projects),
            projects,
            searched: exchanges.iter().any(|e| e.matched.is_some()),
        }
    }
}

impl Listing for ConversationList<'_> {
    fn len(&self) -> usize {
        self.exchanges.len()
    }

    /// When searching, the lines around the match follow.
    fn entry_height(&self) -> usize {
        if self.searched {
            1 + CONTEXT_LINES
        } else {
            1
        }
    }

    fn header(&self, page: Option<String>, cols: usize) -> String {
        let mut info = self.info.clone();
        info.extend(page);
        header_line(&self.title, &info, cols)
    }

    fn entry(&self, index: usize, number_width: usize, cols: usize) -> Vec<String> {
        let project = self
            .projects
            .get(index)
            .map(|name| (name.as_str(), self.project_width));
        exchange_lines(
            index + 1,
            number_width,
            &self.exchanges[index],
            self.times,
            project,
            cols,
        )
    }
}

/// Lays out an entry's columns from left to right, keeping room for its text.
struct Columns {
    cols: usize,
    used: usize,
}

impl Columns {
    fn new(cols: usize, used: usize) -> Self {
        Columns { cols, used }
    }

    /// Whether a column `width` wide, and the gap after it, fit with
    /// `MIN_TEXT_WIDTH` columns to spare; if so, takes the room.
    fn fits(&mut self, width: usize) -> bool {
        let fits = width > 0 && self.used + width + 2 + MIN_TEXT_WIDTH <= self.cols;
        if fits {
            self.used += width + 2;
        }
        fits
    }

    /// The columns left for the text.
    fn room(&self) -> usize {
        self.cols.saturating_sub(self.used)
    }
}

/// The names of `projects`, one per entry, for a project column: empty when
/// they're all the same project.
fn project_column<'p>(
    projects: impl Iterator<Item = &'p str> + Clone,
    names: &ProjectNames,
) -> Vec<String> {
    let distinct: HashSet<&str> = projects.clone().collect();
    if distinct.len() > 1 {
        projects.map(|project| names.get(project)).collect()
    } else {
        Vec::new()
    }
}

/// Width of a column of `names`, up to `MAX_PROJECT_WIDTH`.
fn column_width(names: &[String]) -> usize {
    let widest = names.iter().map(|name| display_width(name)).max();
    widest.unwrap_or(0).min(MAX_PROJECT_WIDTH)
}

/// Counts like "12 questions" from (number, singular, plural), with the numbers
/// right-aligned, and the width of their column.
fn count_column<'n>(
    counts: impl Iterator<Item = (usize, &'n str, &'n str)>,
) -> (Vec<String>, usize) {
    let counts: Vec<_> = counts.collect();
    let digits = counts.iter().map(|(n, ..)| n.to_string().len()).max();
    let digits = digits.unwrap_or(0);
    let texts: Vec<String> = counts
        .iter()
        .map(|&(n, one, many)| format!("{n:>digits$} {}", if n == 1 { one } else { many }))
        .collect();
    let width = texts.iter().map(|text| display_width(text)).max();
    (texts, width.unwrap_or(0))
}

/// A count dimmed in its column, and the gap after it.
fn count_cell(count: &str, width: usize) -> String {
    // Not `pad`, which would drop the spaces aligning the numbers.
    format!("{DIM}{count:<width$}{RESET}  ")
}

/// The indented entry number, bold, and the gap after it.
fn number_column(number: usize, number_width: usize) -> String {
    format!(
        "{}{BOLD}{number:>number_width$}{RESET}  ",
        " ".repeat(INDENT)
    )
}

fn number_column_width(number_width: usize) -> usize {
    INDENT + number_width + 2
}

/// The lines listing a conversation: number, time, project (`project`'s name in
/// a column that wide, when given) and question; then, when a search found it,
/// the lines around the match, marked Q: or A: by where it was found. Columns
/// are left out on terminals too narrow for them.
fn exchange_lines(
    number: usize,
    number_width: usize,
    exchange: &Exchange,
    times: Times,
    project: Option<(&str, usize)>,
    cols: usize,
) -> Vec<String> {
    let lead = number_column_width(number_width);
    let mut first = number_column(number, number_width);
    let mut columns = Columns::new(cols, lead);
    if columns.fits(times.width()) {
        let time = exchange
            .timestamp
            .as_deref()
            .map(|ts| local_time(ts, times.format()))
            .unwrap_or_default();
        first.push_str(&pad(&time, times.width()));
        first.push_str("  ");
    }
    if let Some((name, width)) = project {
        if columns.fits(width) {
            first.push_str(&pad(name, width));
            first.push_str("  ");
        }
    }
    let thread = if exchange.agent_id.is_some() {
        "(subagent) "
    } else {
        ""
    };
    match &exchange.question_preview {
        Some(q) => first.push_str(&fit(&format!("{thread}{q}"), columns.room())),
        None => first.push_str(&format!(
            "{DIM}{}{RESET}",
            fit(&format!("{thread}(no question archived)"), columns.room())
        )),
    }
    let mut lines = vec![first];

    if let Some(matched) = &exchange.matched {
        let label = if matched.role == "user" { "Q:" } else { "A:" };
        for (i, line) in matched.lines.iter().enumerate() {
            let label = if i == 0 { label } else { "  " };
            let text = fit(line, cols.saturating_sub(lead + 3));
            lines.push(format!(
                "{}{DIM}{label} {}{RESET}",
                " ".repeat(lead),
                highlight(&text)
            ));
        }
    }
    lines
}

/// A header line: `title` in bold, then the `info` items dimmed.
fn header_line(title: &str, info: &[String], cols: usize) -> String {
    let info = format!("· {}", info.join(" · "));
    // The title may take all but the info's width, or half the line if the info
    // is wider.
    let title = fit(
        title,
        cols.saturating_sub(display_width(&info).min(cols / 2) + 1),
    );
    let info = fit(&info, cols.saturating_sub(display_width(&title) + 1));
    format!("{BOLD}{title}{RESET}{DIM} {info}{RESET}")
}

/// `line` unstyled and in reverse video across the terminal, with a mark in its
/// indent for terminals without reverse video.
fn selected_line(line: &str, cols: usize) -> String {
    let mut text = unstyled(line);
    if text.starts_with(' ') {
        text.replace_range(..1, ">");
    }
    let rest = cols.saturating_sub(display_width(&text));
    format!("{REVERSE}{text}{}{RESET}", " ".repeat(rest))
}

/// `line` without the styles lists use.
fn unstyled(line: &str) -> String {
    line.replace(BOLD, "").replace(DIM, "").replace(RESET, "")
}

fn quoted(query: &str) -> String {
    format!("\"{query}\"")
}

fn short_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// When a conversation was asked, or else when its session started.
fn exchange_time(exchange: &Exchange) -> Option<&str> {
    exchange
        .timestamp
        .as_deref()
        .or(exchange.session_started.as_deref())
}

/// Conversations newest first, those without a time last; among those asked at
/// once, later questions in the thread first.
fn newest_first(mut exchanges: Vec<Exchange>) -> Vec<Exchange> {
    let time = |e: &Exchange| exchange_time(e).and_then(parse_timestamp);
    exchanges.sort_by(|a, b| {
        time(b)
            .cmp(&time(a))
            .then_with(|| a.session_id.cmp(&b.session_id))
            .then_with(|| b.question_id.cmp(&a.question_id))
    });
    exchanges
}

/// Sessions newest first, those without a start time last; ties by id.
fn newest_session_first(a: &SessionInfo, b: &SessionInfo) -> Ordering {
    let time = |s: &SessionInfo| s.started.as_deref().and_then(parse_timestamp);
    time(b)
        .cmp(&time(a))
        .then_with(|| a.session_id.cmp(&b.session_id))
}

/// Projects with the latest session first; ties by name.
fn newest_project_first(a: &ProjectEntry, b: &ProjectEntry) -> Ordering {
    let time = |p: &ProjectEntry| p.latest.as_deref().and_then(parse_timestamp);
    time(b).cmp(&time(a)).then_with(|| a.name.cmp(&b.name))
}

/// Times in a list of a session's conversations: with the date only when they
/// weren't all on the day the session started, which the header shows.
fn session_times(session: &SessionInfo, exchanges: &[Exchange]) -> Times {
    let day = |ts: &str| local_time(ts, "%Y-%m-%d");
    let started = session.started.as_deref().map(day);
    let other_days = exchanges
        .iter()
        .filter_map(|e| e.timestamp.as_deref())
        .any(|ts| Some(day(ts)) != started);
    if other_days {
        Times::DayTime
    } else {
        Times::Time
    }
}

/// Lines available for a list: below the header line and a blank line, above a
/// blank line and the footer.
fn list_lines() -> Result<usize> {
    let rows = terminal::size()?.1 as usize;
    Ok(rows.saturating_sub(4))
}

/// Entries of `list` per page: as many as fit, but at least one.
fn page_size(list: &dyn Listing) -> Result<usize> {
    Ok((list_lines()? / list.entry_height()).max(1))
}

fn page_count(entries: usize, page_size: usize) -> usize {
    entries.div_ceil(page_size).max(1)
}

/// Draw the page of `list` with the selected entry, and a footer showing the
/// status, the entry number being typed, or the keys.
fn draw_list(list: &dyn Listing, state: &ListState, typed: &str) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let (cols, rows) = (cols as usize, rows as usize);
    let size = page_size(list)?;
    let pages = page_count(list.len(), size);
    let page = state.selected / size;
    let mut out = io::stdout().lock();
    queue!(out, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
    let page_info = (pages > 1).then(|| format!("page {}/{pages}", page + 1));
    write!(out, "{}", list.header(page_info, cols))?;

    let number_width = list.len().to_string().len();
    let footer_row = rows.saturating_sub(1);
    let mut row = 2;
    let first = page * size;
    for index in first..list.len().min(first + size) {
        let mut lines = list.entry(index, number_width, cols);
        if index == state.selected {
            lines[0] = selected_line(&lines[0], cols);
        }
        // On a terminal too short for whole entries, as much as fits.
        for (offset, text) in lines.iter().enumerate() {
            if row + offset >= footer_row {
                break;
            }
            queue!(out, MoveTo(0, (row + offset) as u16))?;
            write!(out, "{text}")?;
        }
        row += list.entry_height();
    }
    if list.len() == 0 && footer_row > 2 {
        queue!(out, MoveTo(0, 2))?;
        write!(out, "{DIM}{}{RESET}", fit("Nothing to list", cols))?;
    }

    queue!(out, MoveTo(0, footer_row as u16))?;
    if let Some(status) = &state.status {
        write!(out, "{BOLD}{}{RESET}", fit(status, cols))?;
    } else if !typed.is_empty() {
        let open = format!("Open: {typed}");
        let hint = fit(
            "· Enter to open · Esc to cancel",
            cols.saturating_sub(open.len() + 1),
        );
        write!(out, "{BOLD}{open}{RESET}{DIM} {hint}{RESET}")?;
    } else {
        let mut help = Vec::new();
        match list.len() {
            0 => {}
            1 => help.push("Enter open".to_string()),
            n => help.push(format!("↑/↓ or 1-{n}, Enter open")),
        }
        if pages > 1 {
            help.push("Space/- page".to_string());
        }
        help.push("Esc back · q quit".to_string());
        write!(out, "{DIM}{}{RESET}", fit(&help.join(" · "), cols))?;
    }
    out.flush()?;
    Ok(())
}

/// `text` in lines of at most `width` columns, broken between words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && display_width(&line) + 1 + display_width(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    // A word too long for a line is cut.
    lines.iter().map(|line| fit(line, width)).collect()
}

/// Write the exchange to a temporary Markdown file and open it in the user's editor.
/// `project` is the name to show for its project.
fn open_exchange(
    conn: &Connection,
    screen: &Screen,
    hit: &Exchange,
    project: &str,
    opts: &Options,
) -> Result<()> {
    let messages = db::get_exchange(
        conn,
        &hit.session_id,
        hit.agent_id.as_deref(),
        hit.question_id,
    )?;
    let text = render_exchange(hit, project, &messages, opts.include_tools);

    let short_id: String = hit
        .session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{APP}-{short_id}-"))
        .suffix(".md")
        .tempfile()
        .context("Failed to create a temporary file")?;
    file.write_all(text.as_bytes())?;
    // Close our handle; the file is deleted when `path` is dropped.
    let path = file.into_temp_path();

    screen.suspend()?;
    let edited = run_editor(&path);
    screen.resume()?;
    edited
}

/// Editor used when neither `$VISUAL` nor `$EDITOR` is set.
const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// Run `$VISUAL` or `$EDITOR` on `path` and wait for it to exit.
fn run_editor(path: &Path) -> Result<()> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|e| !e.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDITOR.to_string());
    let mut child = editor_command(&editor, path)
        .spawn()
        .with_context(|| format!("Failed to run editor `{editor}`"))?;
    let status = {
        // Like git: Ctrl-C and Ctrl-\ while the editor runs are meant for the
        // editor. Dying here would also leave the temporary file behind.
        #[cfg(unix)]
        let _ignored = IgnoreInterrupts::new();
        child.wait()
    }
    .with_context(|| format!("Failed to wait for editor `{editor}`"))?;
    if !status.success() {
        bail!("Editor `{editor}` exited with {status}");
    }
    Ok(())
}

/// Like git, run the editor through `sh` so the variable may carry arguments
/// (e.g. `code --wait`) and shell quoting.
#[cfg(unix)]
fn editor_command(editor: &str, path: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(path);
    command
}

#[cfg(not(unix))]
fn editor_command(editor: &str, path: &Path) -> Command {
    let (program, args) = split_editor(editor);
    let mut command = Command::new(program);
    command.args(args).arg(path);
    command
}

/// Without `sh`, the program and arguments in the variable. It's all the program
/// when it names a file (a path with spaces, like `C:\Program Files\...`); a
/// double-quoted program may be followed by arguments; otherwise words are split
/// on whitespace.
#[cfg(any(not(unix), test))]
fn split_editor(editor: &str) -> (String, Vec<String>) {
    let editor = editor.trim();
    if Path::new(editor).is_file() {
        return (editor.to_string(), Vec::new());
    }
    if let Some((program, args)) = editor
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"'))
    {
        let args = args.split_whitespace().map(String::from).collect();
        return (program.to_string(), args);
    }
    let mut words = editor.split_whitespace().map(String::from);
    let program = words.next().unwrap_or_else(|| DEFAULT_EDITOR.to_string());
    (program, words.collect())
}

/// Ignores SIGINT and SIGQUIT in this process (not in children already started)
/// until dropped.
#[cfg(unix)]
struct IgnoreInterrupts {
    previous: [(libc::c_int, libc::sighandler_t); 2],
}

#[cfg(unix)]
impl IgnoreInterrupts {
    fn new() -> Self {
        let previous = [libc::SIGINT, libc::SIGQUIT].map(|signal| {
            // SAFETY: SIG_IGN is a valid disposition; `drop` restores the old one.
            (signal, unsafe { libc::signal(signal, libc::SIG_IGN) })
        });
        Self { previous }
    }
}

#[cfg(unix)]
impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        for (signal, handler) in self.previous {
            if handler != libc::SIG_ERR {
                // SAFETY: reinstates the disposition that `new` replaced.
                unsafe { libc::signal(signal, handler) };
            }
        }
    }
}

/// Format an exchange (as returned by `db::get_exchange`) as Markdown, headed by
/// `project`, the name to show for its project.
fn render_exchange(
    hit: &Exchange,
    project: &str,
    messages: &[SearchResult],
    include_tools: bool,
) -> String {
    let mut out = format!("# {project}");
    if let Some(ts) = &hit.timestamp {
        out.push_str(&format!(" · {}", local_time(ts, "%Y-%m-%d %H:%M")));
    }
    out.push_str(&format!(" · session {}", hit.session_id));
    if let Some(agent_id) = &hit.agent_id {
        out.push_str(&format!(" · subagent {agent_id}"));
    }
    out.push_str("\n\n## Question\n\n");

    let answer = match messages.split_first() {
        Some((question, rest)) if hit.question_id.is_some() && question.role == "user" => {
            out.push_str(question.content.trim_end());
            out.push_str("\n\n");
            rest
        }
        _ => {
            out.push_str(
                "_(none archived: this answer comes before any question in the session)_\n\n",
            );
            messages
        }
    };

    out.push_str("## Answer\n\n");
    let mut any = false;
    // Other user-role rows here are meta messages (compaction summaries,
    // interrupt markers), not part of the answer.
    for message in answer.iter().filter(|m| m.role == "assistant") {
        let content = if include_tools {
            message.content.clone()
        } else {
            crate::strip_tool_lines(&message.content)
        };
        if content.trim().is_empty() {
            continue;
        }
        out.push_str(content.trim_end());
        out.push_str("\n\n");
        any = true;
    }
    if !any {
        out.push_str("_(none archived)_\n");
    }
    out
}

fn read_input() -> Result<Input> {
    loop {
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(Input::Key(key)),
            Event::Paste(text) => return Ok(Input::Paste(text)),
            Event::Resize(..) => return Ok(Input::Redraw),
            _ => {}
        }
    }
}

/// Parse a stored timestamp: ISO 8601 from transcripts, or SQLite's UTC
/// `YYYY-MM-DD HH:MM:SS` for import times.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .map(|time| time.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|time| time.and_utc())
        })
}

/// A stored timestamp in local time, or as stored if it doesn't parse.
fn local_time(ts: &str, format: &str) -> String {
    match parse_timestamp(ts) {
        Some(time) => time.with_timezone(&Local).format(format).to_string(),
        None => ts.to_string(),
    }
}

/// Show matched terms in bold on the otherwise dim snippet line.
fn highlight(snippet: &str) -> String {
    snippet
        .replace(MATCH_START, &format!("{RESET}{BOLD}"))
        .replace(MATCH_END, &format!("{RESET}{DIM}"))
}

fn char_width(c: char) -> usize {
    if c == MATCH_START || c == MATCH_END {
        0
    } else {
        c.width().unwrap_or(0)
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Put `s` on one line (collapsing whitespace, dropping control characters other
/// than match markers) and cut it to at most `max` columns, ending in '…' if cut.
fn fit(s: &str, max: usize) -> String {
    let line: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|&c| !c.is_control() || c == MATCH_START || c == MATCH_END)
        .collect();
    if display_width(&line) <= max {
        return line;
    }
    let mut out = String::new();
    let mut width = 0;
    for c in line.chars() {
        let w = char_width(c);
        if width + w + 1 > max {
            break;
        }
        width += w;
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// `fit` to exactly `width` columns, padding with spaces.
fn pad(s: &str, width: usize) -> String {
    let mut out = fit(s, width);
    let used = display_width(&out);
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// The longest end of `s` that fits in `max` columns.
fn tail_to_width(s: &str, max: usize) -> &str {
    let mut width = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        width += char_width(c);
        if width > max {
            break;
        }
        start = i;
    }
    &s[start..]
}

/// Raw mode on the alternate screen; the terminal is restored on drop and on panic.
struct Screen;

impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // From here on, dropping `screen` restores the terminal.
        let screen = Screen;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            default_hook(info);
        }));
        Ok(screen)
    }

    /// Hand the terminal to another program, such as the editor.
    fn suspend(&self) -> Result<()> {
        restore_terminal()?;
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        // Discard keys typed while the editor was closing, so a stray digit
        // doesn't open another result.
        while event::poll(Duration::ZERO)? {
            event::read()?;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

fn restore_terminal() -> io::Result<()> {
    let screen = execute!(
        io::stdout(),
        DisableBracketedPaste,
        Show,
        LeaveAlternateScreen
    );
    // Leave raw mode even if the escape codes couldn't be written.
    let raw = terminal::disable_raw_mode();
    screen.and(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Match, MessageOrigin};

    fn message(role: &str, content: &str) -> SearchResult {
        SearchResult {
            session_id: "47cf1f2e-aaaa".to_string(),
            project: "-home-user-ghq-github-com-owner-repo".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            timestamp: None,
        }
    }

    fn exchange(question_id: Option<i64>) -> Exchange {
        Exchange {
            session_id: "47cf1f2e-aaaa".to_string(),
            project: "-home-user-ghq-github-com-owner-repo".to_string(),
            session_started: Some("2026-03-13T14:00:00Z".to_string()),
            agent_id: None,
            question_id,
            question_preview: None,
            timestamp: Some("2026-03-13T14:21:48.123Z".to_string()),
            matched: None,
        }
    }

    fn matched(role: &str, lines: &[&str]) -> Option<Match> {
        Some(Match {
            role: role.to_string(),
            lines: lines.iter().map(|line| line.to_string()).collect(),
        })
    }

    fn session(id: &str, project: &str, started: &str, questions: usize) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            project: project.to_string(),
            started: Some(started.to_string()),
            questions,
            first_question: Some(format!("First question of {id}")),
        }
    }

    /// Project names as given, without looking for their directories.
    fn names(pairs: &[(&str, &str)]) -> ProjectNames {
        let names = ProjectNames::default();
        for (project, name) in pairs {
            names
                .0
                .borrow_mut()
                .insert(project.to_string(), name.to_string());
        }
        names
    }

    fn visible_width(line: &str) -> usize {
        display_width(&unstyled(line))
    }

    #[test]
    fn test_navigate_with_arrows_and_pages() {
        let mut typed = String::new();
        let mut at = |code, selected| navigate(code, selected, 25, 10, &mut typed);
        assert_eq!(at(KeyCode::Down, 0), 1);
        assert_eq!(at(KeyCode::Down, 24), 24);
        assert_eq!(at(KeyCode::Up, 3), 2);
        assert_eq!(at(KeyCode::Up, 0), 0);
        // Paging goes to the first entry of the next or previous page.
        assert_eq!(at(KeyCode::PageDown, 3), 10);
        assert_eq!(at(KeyCode::Char(' '), 13), 20);
        assert_eq!(at(KeyCode::PageDown, 21), 24);
        assert_eq!(at(KeyCode::PageUp, 13), 0);
        assert_eq!(at(KeyCode::Char('-'), 24), 10);
        assert_eq!(at(KeyCode::Home, 17), 0);
        assert_eq!(at(KeyCode::End, 2), 24);
        assert_eq!(navigate(KeyCode::Down, 0, 0, 10, &mut String::new()), 0);
    }

    #[test]
    fn test_navigate_by_typed_number() {
        let mut typed = String::new();
        // "2" selects entry 2, "23" entry 23...
        assert_eq!(navigate(KeyCode::Char('2'), 0, 25, 10, &mut typed), 1);
        assert_eq!(navigate(KeyCode::Char('3'), 1, 25, 10, &mut typed), 22);
        assert_eq!(typed, "23");
        // ...and a digit that would make no entry's number is ignored.
        assert_eq!(navigate(KeyCode::Char('4'), 22, 25, 10, &mut typed), 22);
        assert_eq!(typed, "23");
        assert_eq!(navigate(KeyCode::Backspace, 22, 25, 10, &mut typed), 1);
        assert_eq!(typed, "2");
        assert_eq!(navigate(KeyCode::Backspace, 1, 25, 10, &mut typed), 1);
        assert_eq!(typed, "");
        assert_eq!(navigate(KeyCode::Char('0'), 4, 25, 10, &mut typed), 4);
        assert_eq!(typed, "");
        // Moving otherwise forgets the number.
        navigate(KeyCode::Char('1'), 4, 25, 10, &mut typed);
        assert_eq!(navigate(KeyCode::Down, 0, 25, 10, &mut typed), 1);
        assert_eq!(typed, "");
    }

    #[test]
    fn test_page_count() {
        assert_eq!(page_count(0, 10), 1);
        assert_eq!(page_count(10, 10), 1);
        assert_eq!(page_count(11, 10), 2);
        assert_eq!(page_count(3, 1), 3);
    }

    #[test]
    fn test_selected_line_is_reversed_across_the_terminal() {
        let line = format!("  {BOLD}3{RESET}  14:21  {DIM}How?{RESET}");
        assert_eq!(
            selected_line(&line, 20),
            format!("{REVERSE}> 3  14:21  How?    {RESET}")
        );
    }

    #[test]
    fn test_wrap_breaks_between_words() {
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("  spaced   out  ", 40), ["spaced out"]);
        assert_eq!(wrap("unbreakable", 6), ["unbre…"]);
        assert!(wrap("", 10).is_empty());
    }

    #[test]
    fn test_fit_flattens_and_truncates() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("two\n  lines\tand tabs", 40), "two lines and tabs");
        assert_eq!(fit("hello world", 8), "hello w…");
        assert_eq!(fit("hello", 0), "");
        assert_eq!(fit("color \x1b[31mred", 40), "color [31mred");
    }

    #[test]
    fn test_fit_counts_wide_characters_and_ignores_markers() {
        // Each CJK character is two columns wide.
        assert_eq!(fit("日本語テキスト", 7), "日本語…");
        let marked = format!("a {MATCH_START}match{MATCH_END} here");
        assert_eq!(fit(&marked, 12), marked);
        assert_eq!(display_width(&marked), 12);
    }

    #[test]
    fn test_pad_fills_to_width() {
        assert_eq!(pad("repo", 6), "repo  ");
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(pad("a-long-project", 6), "a-lon…");
    }

    #[test]
    fn test_tail_to_width_keeps_the_end() {
        assert_eq!(tail_to_width("hello world", 5), "world");
        assert_eq!(tail_to_width("abc", 10), "abc");
        assert_eq!(tail_to_width("日本語", 5), "本語");
        assert_eq!(tail_to_width("abc", 0), "");
    }

    #[test]
    fn test_highlight_marks_matches_bold() {
        let s = format!("x {MATCH_START}docker{MATCH_END} y");
        assert_eq!(
            highlight(&s),
            format!("x {RESET}{BOLD}docker{RESET}{DIM} y")
        );
    }

    #[test]
    fn test_render_exchange_question_and_answer() {
        let messages = [
            message("user", "How do I fix the build?"),
            message(
                "assistant",
                "Let me check.\n[tool_use: Bash] {\"command\":\"make\"}",
            ),
            message("assistant", "[tool_use: Edit] {\"file\":\"Makefile\"}"),
            message("assistant", "Fixed: add -j1."),
        ];
        let text = render_exchange(&exchange(Some(1)), "owner/repo", &messages, false);
        let asked = local_time("2026-03-13T14:21:48.123Z", "%Y-%m-%d %H:%M");
        assert_eq!(
            text,
            format!(
                "# owner/repo · {asked} · session 47cf1f2e-aaaa\n\n\
                 ## Question\n\nHow do I fix the build?\n\n\
                 ## Answer\n\nLet me check.\n\nFixed: add -j1.\n\n"
            )
        );

        let text = render_exchange(&exchange(Some(1)), "owner/repo", &messages, true);
        assert!(text.contains("[tool_use: Bash]"));
        assert!(text.contains("[tool_use: Edit]"));
    }

    #[test]
    fn test_render_exchange_without_question() {
        let messages = [message("assistant", "Resuming where we left off.")];
        let text = render_exchange(&exchange(None), "owner/repo", &messages, false);
        assert!(text.contains("## Question\n\n_(none archived"));
        assert!(text.contains("## Answer\n\nResuming where we left off.\n"));
    }

    #[test]
    fn test_render_exchange_without_answer() {
        let messages = [message("user", "Anyone there?")];
        let text = render_exchange(&exchange(Some(1)), "owner/repo", &messages, false);
        assert!(text.ends_with("## Answer\n\n_(none archived)_\n"));
    }

    #[test]
    fn test_render_exchange_leaves_out_meta_messages() {
        let messages = [
            message("user", "Migrate the schema."),
            message("assistant", "Starting."),
            message(
                "user",
                "This session is being continued from a previous conversation",
            ),
            message("assistant", "Finished."),
        ];
        let text = render_exchange(&exchange(Some(1)), "owner/repo", &messages, false);
        assert!(text.ends_with("## Answer\n\nStarting.\n\nFinished.\n\n"));
        assert!(!text.contains("being continued"));
    }

    #[test]
    fn test_render_exchange_names_the_subagent() {
        let mut subagent = exchange(Some(1));
        subagent.agent_id = Some("a1b2".to_string());
        let text = render_exchange(&subagent, "owner/repo", &[message("user", "Task")], false);
        assert!(text.lines().next().unwrap().ends_with(" · subagent a1b2"));
    }

    #[test]
    fn test_local_time_falls_back_to_stored_text() {
        assert_eq!(local_time("not a time", "%Y-%m-%d"), "not a time");
        assert_eq!(local_time("2026-03-13T14:21:48Z", "%Y").len(), 4);
    }

    #[test]
    fn test_append_paste_keeps_query_on_one_line() {
        let mut input = String::new();
        append_paste(&mut input, "docker build\nfails at\tstep 3\n");
        assert_eq!(input, "docker build fails at step 3");

        let mut input = "error".to_string();
        append_paste(&mut input, " in\r\nmain");
        assert_eq!(input, "error in main");

        let mut input = "foo".to_string();
        append_paste(&mut input, "bar");
        assert_eq!(input, "foobar");
        append_paste(&mut input, "\n\n");
        assert_eq!(input, "foobar");
    }

    #[test]
    fn test_edit_keys() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let mut input = "docker build".to_string();
        edit(&mut input, key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(input, "docker builds");
        edit(&mut input, key(KeyCode::Backspace, KeyModifiers::NONE));
        edit(&mut input, key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(input, "docker ");
        edit(&mut input, key(KeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(input, "docker ");
        // AltGr, as Windows reports it.
        edit(
            &mut input,
            key(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        );
        assert_eq!(input, "docker @");
        edit(
            &mut input,
            key(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        );
        assert_eq!(input, "docker @u");
        edit(&mut input, key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input, "");
    }

    #[test]
    fn test_exchange_lines_fit_the_terminal() {
        let mut long = exchange(Some(1));
        long.question_preview = Some("How do I fix the build? ".repeat(10));
        let hit = format!("…the {MATCH_START}build{MATCH_END} {}", "x".repeat(300));
        let after = "y".repeat(300);
        long.matched = matched(
            "assistant",
            &["Looking at it.", hit.as_str(), after.as_str()],
        );
        for cols in [120, 80, 50, 30, 12] {
            for times in [Times::Time, Times::DayTime, Times::Full] {
                for project in [None, Some(("~/src/app", 9))] {
                    for line in exchange_lines(7, 2, &long, times, project, cols) {
                        assert!(visible_width(&line) <= cols, "{cols} cols: {line:?}");
                    }
                }
            }
        }

        let time = local_time("2026-03-13T14:21:48.123Z", "%H:%M");
        let lines = exchange_lines(7, 2, &long, Times::Time, None, 120);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with(&format!("  {BOLD} 7{RESET}  {time}  How do I")));
        // The lines around the match, labelled by where it was found.
        assert_eq!(unstyled(&lines[1]), "      A: Looking at it.");
        assert!(unstyled(&lines[2]).starts_with("         …the build xxx"));
        assert!(unstyled(&lines[3]).starts_with("         yyy"));

        let dated = local_time("2026-03-13T14:21:48.123Z", DATE_TIME);
        let lines = exchange_lines(7, 2, &long, Times::Full, Some(("~/src/app", 12)), 120);
        assert!(unstyled(&lines[0]).contains(&format!("  {dated}  ~/src/app     How do I")));
        // Too narrow for the time: just the number and the question.
        let narrow = exchange_lines(7, 2, &long, Times::Time, None, 30);
        assert!(narrow[0].contains(&format!("{RESET}  How do I")));
    }

    #[test]
    fn test_exchange_lines_without_a_search_are_one_line() {
        let mut asked = exchange(Some(1));
        asked.question_preview = Some("Why?".to_string());
        assert_eq!(exchange_lines(1, 1, &asked, Times::Time, None, 80).len(), 1);

        let before_any_question = exchange(None);
        let lines = exchange_lines(1, 1, &before_any_question, Times::Time, None, 80);
        assert!(unstyled(&lines[0]).ends_with("(no question archived)"));
    }

    #[test]
    fn test_exchange_lines_mark_subagent_questions() {
        let mut subagent = exchange(Some(1));
        subagent.agent_id = Some("a1".to_string());
        subagent.question_preview = Some("Investigate the logs".to_string());
        let lines = exchange_lines(1, 1, &subagent, Times::Time, None, 120);
        assert!(lines[0].contains("(subagent) Investigate the logs"));
    }

    #[test]
    fn test_conversation_list_shows_projects_only_when_several() {
        let names = names(&[("-home-me-app", "~/app"), ("-home-me-tool", "~/tool")]);
        let mut a = exchange(Some(1));
        a.project = "-home-me-app".to_string();
        let mut b = exchange(Some(2));
        b.project = "-home-me-tool".to_string();
        b.matched = matched("user", &["the docker build"]);

        let one = [a.clone()];
        let list = ConversationList::new(&one, "t".into(), vec![], Times::Time, &names);
        assert_eq!((list.project_width, list.entry_height()), (0, 1));

        let both = [a, b];
        let list = ConversationList::new(&both, "t".into(), vec![], Times::Full, &names);
        assert_eq!(list.project_width, "~/tool".len());
        assert_eq!(list.entry_height(), 1 + CONTEXT_LINES);
        assert!(unstyled(&list.entry(1, 1, 100)[0]).contains("  ~/tool  "));
        assert_eq!(
            unstyled(&list.header(Some("page 1/2".to_string()), 100)),
            "t · page 1/2"
        );
    }

    #[test]
    fn test_session_list_entries() {
        let sessions = [
            session("aaaa1111-x", "-home-me-app", "2026-03-13T14:00:00Z", 12),
            session("bbbb2222-x", "-home-me-app", "2026-03-12T09:00:00Z", 1),
        ];
        let names = ProjectNames::default();
        let info = vec!["2 sessions".to_string()];
        let list = SessionList::new(&sessions, "~/app".into(), info, &names);
        let started = local_time("2026-03-13T14:00:00Z", DATE_TIME);
        assert_eq!(
            unstyled(&list.entry(0, 1, 100)[0]),
            format!("  1  {started}  12 questions  First question of aaaa1111-x")
        );
        // Counts line up.
        assert!(unstyled(&list.entry(1, 1, 100)[0]).contains("   1 question   First"));
        // Narrower: the count goes first, then the date.
        assert!(!unstyled(&list.entry(0, 1, 50)[0]).contains("questions"));
        assert!(unstyled(&list.entry(0, 1, 50)[0]).contains(&started));
        assert_eq!(
            unstyled(&list.entry(0, 1, 30)[0]),
            "  1  First question of aaaa11…"
        );
        for cols in [100, 60, 40, 30, 12] {
            for index in 0..2 {
                let line = &list.entry(index, 1, cols)[0];
                assert!(visible_width(line) <= cols, "{cols} cols: {line:?}");
            }
        }
        assert_eq!(unstyled(&list.header(None, 80)), "~/app · 2 sessions");
    }

    #[test]
    fn test_session_list_names_projects_only_when_several() {
        let names = names(&[("-home-me-app", "~/app"), ("-home-me-tool", "~/tool")]);
        let one = [session("a", "-home-me-app", "2026-03-13T14:00:00Z", 2)];
        let list = SessionList::new(&one, "t".into(), vec![], &names);
        assert_eq!(list.project_width, 0);

        let mixed = [
            session("a", "-home-me-app", "2026-03-13T14:00:00Z", 2),
            session("b", "-home-me-tool", "2026-03-12T14:00:00Z", 2),
        ];
        let list = SessionList::new(&mixed, "t".into(), vec![], &names);
        assert_eq!(list.project_width, "~/tool".len());
        assert!(unstyled(&list.entry(0, 1, 120)[0]).contains("  ~/app   2 questions"));
    }

    #[test]
    fn test_project_list_entries() {
        let entry = |project: &str, name: &str, latest: &str, sessions| ProjectEntry {
            project: project.to_string(),
            name: name.to_string(),
            latest: Some(latest.to_string()),
            sessions,
            matches: 0,
            hits: Vec::new(),
        };
        let projects = [
            entry("-home-me-app", "~/app", "2026-03-13T14:00:00Z", 12),
            entry("-srv-tool", "/srv/tool", "2026-01-02T09:00:00Z", 1),
        ];
        let list = ProjectList::new(&projects, "Projects".into(), vec![], false);
        let latest = local_time("2026-03-13T14:00:00Z", DATE_TIME);
        assert_eq!(
            unstyled(&list.entry(0, 1, 100)[0]),
            format!("  1  {latest}  12 sessions  ~/app")
        );
        assert!(unstyled(&list.entry(1, 1, 100)[0]).ends_with("   1 session   /srv/tool"));
        for cols in [100, 40, 12] {
            let line = &list.entry(0, 1, cols)[0];
            assert!(visible_width(line) <= cols, "{cols} cols: {line:?}");
        }
    }

    #[test]
    fn test_headers_fit_the_terminal() {
        let long = "x".repeat(200);
        let info = ["12 sessions".to_string(), "page 1/3".to_string()];
        for cols in [120, 80, 40, 20, 5] {
            let header = header_line(&long, &info, cols);
            assert!(visible_width(&header) <= cols, "{cols} cols: {header:?}");
        }
        // A short title is kept whole.
        assert_eq!(
            unstyled(&header_line("~/app", &info, 40)),
            "~/app · 12 sessions · page 1/3"
        );
    }

    #[test]
    fn test_session_times_have_dates_only_when_spanning_days() {
        let entry = session("s", "p", "2026-02-01T12:00:00Z", 2);
        let at = |ts: &str| {
            let mut e = exchange(Some(1));
            e.timestamp = Some(ts.to_string());
            e
        };
        let mut undated = exchange(None);
        undated.timestamp = None;
        let same_day = [at("2026-02-01T12:30:00Z"), undated];
        assert_eq!(session_times(&entry, &same_day), Times::Time);
        let later = [at("2026-02-01T12:30:00Z"), at("2026-02-03T12:00:00Z")];
        assert_eq!(session_times(&entry, &later), Times::DayTime);
    }

    /// An exchange in `session`, which started at `started`, asked at `asked`.
    fn exchange_at(session: &str, started: Option<&str>, asked: Option<&str>, id: i64) -> Exchange {
        let mut exchange = exchange(Some(id));
        exchange.session_id = session.to_string();
        exchange.session_started = started.map(str::to_string);
        exchange.timestamp = asked.map(str::to_string);
        exchange
    }

    fn order(exchanges: &[Exchange]) -> Vec<(String, Option<i64>)> {
        exchanges
            .iter()
            .map(|e| (e.session_id.clone(), e.question_id))
            .collect()
    }

    #[test]
    fn test_conversations_newest_first() {
        let mut before_any_question = exchange_at(
            "new",
            Some("2026-02-01T09:00:00Z"),
            Some("2026-02-01T09:00:00Z"),
            0,
        );
        before_any_question.question_id = None;
        let exchanges = vec![
            exchange_at(
                "old",
                Some("2026-01-01T09:00:00Z"),
                Some("2026-01-01T09:30:00Z"),
                2,
            ),
            exchange_at(
                "new",
                Some("2026-02-01T09:00:00Z"),
                Some("2026-02-01T11:00:00Z"),
                5,
            ),
            exchange_at("undated", None, None, 9),
            before_any_question,
            exchange_at(
                "new",
                Some("2026-02-01T09:00:00Z"),
                Some("2026-02-01T10:00:00Z"),
                4,
            ),
            // Imported without timestamps: the session's import time stands in.
            exchange_at("imported", Some("2026-01-15 08:00:00"), None, 7),
            // Asked at the same time as another: the later question first.
            exchange_at(
                "old",
                Some("2026-01-01T09:00:00Z"),
                Some("2026-01-01T09:30:00Z"),
                3,
            ),
        ];
        let expected = [
            ("new", Some(5)),
            ("new", Some(4)),
            ("new", None),
            ("imported", Some(7)),
            ("old", Some(3)),
            ("old", Some(2)),
            ("undated", Some(9)),
        ]
        .map(|(s, id)| (s.to_string(), id));
        assert_eq!(order(&newest_first(exchanges)), expected);
    }

    #[test]
    fn test_search_and_group_matches() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = db::open_db(tmp.path()).unwrap();
        let rows = [
            ("old", "-home-me-app", "2026-01-01T09:00:00Z"),
            ("new", "-home-me-app", "2026-02-01T09:00:00Z"),
            ("elsewhere", "-home-me-other", "2026-01-15T09:00:00Z"),
        ];
        for (session, project, started) in rows {
            db::upsert_session(&conn, session, project, Some(started)).unwrap();
            for (n, (role, content)) in [
                ("user", "why does the docker build fail?"),
                ("assistant", "The docker cache is stale."),
                ("user", "and the deploy?"),
                ("assistant", "It needs docker too."),
            ]
            .into_iter()
            .enumerate()
            {
                let uuid = format!("{session}-{n}");
                let time = format!("{}0{n}:00Z", &started[..14]);
                let origin = MessageOrigin::default();
                db::insert_message(
                    &conn,
                    session,
                    Some(&uuid),
                    role,
                    content,
                    Some(&time),
                    &origin,
                )
                .unwrap();
            }
        }
        let opts = Options {
            limit: 50,
            projects: None,
            projects_scope: "all projects".to_string(),
            sessions: Some(ProjectFilter::Exact("-home-me-app")),
            sessions_scope: "~/app".to_string(),
            current_project: Some("-home-me-app"),
            role: None,
            since: None,
            until: None,
            include_tools: false,
        };

        let all = Scope::projects(None, "all projects");
        let found = search(&conn, &opts, "docker", all).unwrap();
        assert_eq!(
            (found.summary().as_str(), found.limited),
            ("6 matches", false)
        );
        let sessions: Vec<_> = found
            .exchanges
            .iter()
            .map(|e| e.session_id.as_str())
            .collect();
        assert_eq!(
            sessions,
            ["new", "new", "elsewhere", "elsewhere", "old", "old"]
        );
        let previews: Vec<_> = found.exchanges[..2]
            .iter()
            .map(|h| h.question_preview.as_deref().unwrap())
            .collect();
        assert_eq!(
            previews,
            ["and the deploy?", "why does the docker build fail?"]
        );

        // Grouped, all matches count, but each group keeps only its best `limit`.
        let names = names(&[("-home-me-app", "~/app"), ("-home-me-other", "~/other")]);
        let ranked = matches(&conn, &opts, "docker", all, usize::MAX).unwrap();
        let projects: Vec<_> = by_project(ranked.clone(), &names, 3)
            .into_iter()
            .map(|p| (p.name, p.sessions, p.matches, p.hits.len()))
            .collect();
        assert_eq!(
            projects,
            [
                ("~/app".to_string(), 2, 4, 3),
                ("~/other".to_string(), 1, 2, 2)
            ]
        );
        let app = &by_project(ranked, &names, 50)[0];
        let newest = [("new", Some(7)), ("new", Some(5)), ("old", Some(3))];
        assert_eq!(
            order(&app.hits[..3]),
            newest.map(|(s, id)| (s.to_string(), id))
        );
        assert_eq!(app.latest.as_deref(), Some("2026-02-01T09:02:00Z"));
        assert_eq!(shown_of(3, 4), "best 3 of 4 matches");
        assert_eq!(shown_of(1, 1), "1 match");

        // In one session.
        let old = Scope {
            project: None,
            session: Some("old"),
            name: "session old",
        };
        let found = search(&conn, &opts, "docker", old).unwrap();
        assert_eq!(
            order(&found.exchanges),
            [("old".to_string(), Some(3)), ("old".to_string(), Some(1))]
        );
        let deploy = search(&conn, &opts, "deploy", old).unwrap();
        assert_eq!(order(&deploy.exchanges), [("old".to_string(), Some(3))]);

        let limited = Options { limit: 3, ..opts };
        let found = search(&conn, &limited, "docker", all).unwrap();
        assert_eq!(found.summary(), "best 3 matches");
        assert_eq!(
            search(&conn, &limited, "kubernetes", old).err().as_deref(),
            Some("No matches for \"kubernetes\" in session old")
        );
    }

    #[test]
    fn test_parse_timestamp_reads_both_stored_formats() {
        let iso = parse_timestamp("2026-01-15T08:00:00.123Z").unwrap();
        let sqlite = parse_timestamp("2026-01-15 08:00:00").unwrap();
        assert_eq!(iso.format("%F %T").to_string(), "2026-01-15 08:00:00");
        assert_eq!(sqlite.format("%F %T").to_string(), "2026-01-15 08:00:00");
        assert_eq!(parse_timestamp("yesterday"), None);
    }

    #[test]
    fn test_split_editor() {
        let words = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            split_editor("code --wait"),
            ("code".to_string(), words(&["--wait"]))
        );
        assert_eq!(
            split_editor(r#""C:\Program Files\Editor\ed.exe" -w --new"#),
            (
                r"C:\Program Files\Editor\ed.exe".to_string(),
                words(&["-w", "--new"])
            )
        );
        assert_eq!(split_editor("  "), (DEFAULT_EDITOR.to_string(), Vec::new()));

        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("My Editor");
        std::fs::write(&program, "").unwrap();
        let program = program.to_string_lossy().into_owned();
        assert_eq!(split_editor(&program), (program.clone(), Vec::new()));
    }
}
