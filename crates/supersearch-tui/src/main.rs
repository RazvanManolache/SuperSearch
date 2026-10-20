//! `supersearch-tui`: full-terminal SuperSearch with the same search
//! engine as the desktop app and CLI (tabs, every filter, sorting,
//! paging, preview, row actions, export, saved searches, languages).

mod app;
mod ui;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use supersearch_config::{default_config_path, AppConfig};

use app::App;

/// Terminal UI for SuperSearch: tabs, filters, results, and preview.
#[derive(Debug, Parser)]
#[command(name = "supersearch-tui", version)]
struct Cli {
    /// Starting search folder (repeatable, `;`-separated like the GUI).
    #[arg(long)]
    folder: Vec<PathBuf>,
    /// Index database to search instead of live folders.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Config file (defaults to the shared SuperSearch config).
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let config = AppConfig::load_from(&config_path).unwrap_or_default();

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("supersearch-tui needs an interactive terminal (stdin and stdout must be TTYs)");
        std::process::exit(2);
    }

    enable_raw_mode()?;
    let _restore = RestoreTerminal;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let roots = cli
        .folder
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let mut app = App::new(config, config_path.clone(), None);
    if !roots.is_empty() {
        app.tab_mut().roots = roots;
    }
    if let Some(db) = cli.db {
        app.tab_mut().db_path = db.display().to_string();
        app.tab_mut().use_indexed = true;
    }
    app.focus_field(app::Field::Name);

    let result = run_loop(&mut terminal, &mut app);

    if result.is_ok() {
        // Remember folders + db for the desktop app and CLI, like the
        // CLI's `record_usage` after a search.
        let roots = app::parse_roots(&app.tab().roots);
        let db = {
            let p = app.tab().db_path.trim();
            if p.is_empty() {
                None
            } else {
                Some(PathBuf::from(p))
            }
        };
        app.config.record_usage(&roots, db.as_ref());
        app.persist_config();
    }
    result
}

/// Restore terminal state on normal exit, I/O errors, and unwinding.
struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        app.poll_worker();
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.on_key(key) {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }
}
