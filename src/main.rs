//! kith — local-first, peer-to-peer collections shared with the people you trust.

mod cmd;
mod config;
mod domain;
mod engine;
mod hash;
mod identity;
mod invite;
mod provider;
mod store;
mod tui;

use clap::{Parser, Subcommand};

use engine::syncthing::SyncthingEngine;
use engine::{SyncEngine, SyncError};
use provider::wallpaper::WallpaperProvider;

// Exit codes follow sysexits so the whole binary speaks one dialect.
const EX_USAGE: i32 = 64;
const EX_UNAVAILABLE: i32 = 69;
const EX_CONFIG: i32 = 78;

#[derive(Parser)]
#[command(name = "kith", version, about = "Local-first, peer-to-peer collections shared with the people you trust")]
struct Cli {
    /// Emit a machine-readable envelope instead of prose.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

/// The v0.1 verb list, closed. A capability that is not here is not in v0.1.
#[derive(Subcommand)]
enum Command {
    /// Create this Person's Identity on this Device.
    Init { name: Option<String> },
    /// Create a Circle, optionally adopting an existing synced directory.
    Create {
        name: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        adopt: bool,
    },
    /// Join a Circle with an Invite code.
    Join { code: String },
    /// Print a time-bounded Invite code.
    Invite {
        #[arg(long)]
        new: bool,
    },
    /// Admit a knocking Device.
    Approve { device: Option<String> },
    /// Dismiss a knocking Device.
    Reject { device: Option<String> },
    /// Add content to a Circle's Collection as Items.
    Add { paths: Vec<String> },
    /// List Items, Circles or Members.
    List { subject: Option<String> },
    /// Report sync state.
    Status,
    /// Check that this Device is set up correctly.
    Doctor,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Every verb returns its own process exit code (cli-tui.md §2). This
    // function's whole job is to pick the one that runs — it decides nothing
    // about the domain, which is why there is no logic between here and the
    // command modules.
    let code = match cli.command {
        Some(Command::Doctor) => doctor().await,
        Some(Command::Init { name }) => init(name),
        Some(Command::Create { name, path, adopt }) => {
            cmd::create::run(&name, path.as_deref(), adopt).await
        }
        Some(Command::Join { code }) => cmd::membership::join(&code).await,
        Some(Command::Invite { new }) => cmd::membership::invite(new).await,
        Some(Command::Approve { device }) => cmd::membership::approve(device.as_deref()).await,
        Some(Command::Reject { device }) => cmd::membership::reject(device.as_deref()).await,
        Some(Command::Add { paths }) => {
            if paths.is_empty() {
                eprintln!("kith add <paths…> — what should join the Collection?");
                EX_USAGE
            } else {
                cmd::add::run(&paths).await
            }
        }
        Some(Command::List { subject }) => cmd::report::list(subject.as_deref(), cli.json).await,
        Some(Command::Status) => cmd::report::status(cli.json).await,
        // Bare `kith` opens the TUI.
        None => tui::run().await,
    };
    std::process::exit(code);
}

/// `kith init` — mint this Person and bind them to this Device.
///
/// The honesty note is not decoration: kith has no recovery authority, and the
/// only moment a Person can be told that before it matters is now.
fn init(name: Option<String>) -> i32 {
    let Some(name) = name else {
        eprintln!("kith init <name> — the name your Circles will see");
        return EX_USAGE;
    };

    match identity::create(&name, &jiff::Timestamp::now().to_string()) {
        Ok(id) => {
            println!("You are {} ({})", id.display_name, id.person.short());
            println!();
            println!("This Identity lives only on this Device. kith issues no accounts and");
            println!("keeps no registry, so nobody — including kith — can restore it. If you");
            println!("lose this Device, you lose this Person and start again as someone new.");
            0
        }
        Err(identity::IdentityError::AlreadyExists(p)) => {
            eprintln!("an Identity already exists at {}", p.display());
            eprintln!("v0.1 has no rename; replacing it would orphan every Item it has added");
            EX_CONFIG
        }
        Err(identity::IdentityError::NameRequired) => {
            eprintln!("a display name is required");
            EX_USAGE
        }
        Err(e) => {
            eprintln!("{e}");
            EX_CONFIG
        }
    }
}

/// `kith doctor` — one-shot, stateless, exit-coded.
///
/// It asks whether *this Device* is set up correctly. Whether a *Circle* is
/// healthy right now is the Health screen's question, and that is v0.2.
/// Warnings never fail the run; only a genuine fault does.
async fn doctor() -> i32 {
    let mut failed = false;

    match SyncthingEngine::discover() {
        Ok(creds) => {
            println!("ok    credentials discovered from {}", creds.source.display());
            let engine = SyncthingEngine::new(creds);
            match engine.health().await {
                Ok(h) => println!("ok    Sync Engine reachable, version {}", h.version),
                Err(SyncError::Unauthorized) => {
                    println!("FAIL  the Sync Engine rejected our credentials");
                    println!("      kith never rewrites the daemon's config; check its API key");
                    failed = true;
                }
                Err(e) => {
                    println!("FAIL  Sync Engine not reachable: {e}");
                    println!("      kith adapts a daemon you run; start it and try again");
                    failed = true;
                }
            }
        }
        Err(_) => {
            println!("FAIL  no Sync Engine configuration found");
            println!("      install and start Syncthing, then run kith doctor again");
            failed = true;
        }
    }

    let backends = WallpaperProvider::default().detected();
    if backends.is_empty() {
        println!("warn  no wallpaper backend detected (looked for swww, hyprpaper, feh)");
        println!("      Apply will be shown as unavailable until one is installed");
    } else {
        println!("ok    wallpaper backend: {}", backends.join(", "));
    }

    match identity::load() {
        Ok(Some(id)) => println!("ok    Identity present: {} ({})", id.display_name, id.person.short()),
        Ok(None) => {
            let where_ = identity::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the data directory".into());
            println!("warn  no Identity yet — run kith init");
            println!("      it will be written to {where_}");
        }
        Err(e) => {
            println!("FAIL  {e}");
            failed = true;
        }
    }

    if failed { EX_UNAVAILABLE } else { 0 }
}
