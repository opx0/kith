//! wallsync — local-first, peer-to-peer collections shared with the people you trust.

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
#[command(name = "wallsync", version, about = "Local-first, peer-to-peer collections shared with the people you trust")]
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
        /// Carry an address for this Device, for networks where discovery does
        /// not reach. Example: --address tcp://192.168.1.5:22000
        #[arg(long, value_name = "ADDR")]
        address: Option<String>,
    },
    /// Admit a knocking Device.
    Approve { device: Option<String> },
    /// Dismiss a knocking Device.
    Reject { device: Option<String> },
    /// Add content to a Circle's Collection as Items.
    Add { paths: Vec<String> },
    /// List Items, Circles or Members.
    List {
        subject: Option<String>,
        /// Which Circle, when this Device is in more than one.
        #[arg(long, value_name = "NAME")]
        circle: Option<String>,
    },
    /// Report sync state.
    Status {
        /// Which Circle, when this Device is in more than one.
        #[arg(long, value_name = "NAME")]
        circle: Option<String>,
    },
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
        Some(Command::Invite { new, address }) => {
            cmd::membership::invite(new, address.as_deref()).await
        }
        Some(Command::Approve { device }) => cmd::membership::approve(device.as_deref()).await,
        Some(Command::Reject { device }) => cmd::membership::reject(device.as_deref()).await,
        Some(Command::Add { paths }) => {
            if paths.is_empty() {
                eprintln!("wallsync add <paths…> — what should join the Collection?");
                EX_USAGE
            } else {
                cmd::add::run(&paths).await
            }
        }
        Some(Command::List { subject, circle }) => {
            cmd::report::list(subject.as_deref(), circle.as_deref(), cli.json).await
        }
        Some(Command::Status { circle }) => {
            cmd::report::status(circle.as_deref(), cli.json).await
        }
        // Bare `wallsync` opens the TUI.
        None => tui::run().await,
    };
    std::process::exit(code);
}

/// `wallsync init` — mint this Person and bind them to this Device.
///
/// The honesty note is not decoration: wallsync has no recovery authority, and the
/// only moment a Person can be told that before it matters is now.
fn init(name: Option<String>) -> i32 {
    let Some(name) = name else {
        eprintln!("wallsync init <name> — the name your Circles will see");
        return EX_USAGE;
    };

    match identity::create(&name, &jiff::Timestamp::now().to_string()) {
        Ok(id) => {
            println!("You are {} ({})", id.display_name, id.person.short());
            println!();
            println!("This Identity lives only on this Device. wallsync issues no accounts and");
            println!("keeps no registry, so nobody — including wallsync — can restore it. If you");
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

/// `wallsync doctor` — one-shot, stateless, exit-coded.
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
                    println!("      wallsync never rewrites the daemon's config; check its API key");
                    failed = true;
                }
                Err(e) => {
                    println!("FAIL  Sync Engine not reachable: {e}");
                    println!("      wallsync adapts a daemon you run; start it and try again");
                    failed = true;
                }
            }
        }
        Err(_) => {
            println!("FAIL  no Sync Engine configuration found");
            println!("      install and start Syncthing, then run wallsync doctor again");
            failed = true;
        }
    }

    let settings = config::load();
    match &settings.apply_command {
        Some(_) => println!("ok    wallpaper backend: your configured apply command"),
        None => {
            let backends = WallpaperProvider::default().detected();
            if backends.is_empty() {
                println!("warn  no wallpaper backend detected (looked for caelestia, swww, hyprpaper, feh)");
                println!("      Apply will be shown as unavailable until one is installed,");
                println!("      or until you set provider.wallpaper.custom.apply in config.toml");
            } else {
                println!("ok    wallpaper backend: {}", backends.join(", "));
            }
        }
    }

    match identity::load() {
        Ok(Some(id)) => println!("ok    Identity present: {} ({})", id.display_name, id.person.short()),
        Ok(None) => {
            let where_ = identity::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the data directory".into());
            println!("warn  no Identity yet — run wallsync init");
            println!("      it will be written to {where_}");
        }
        Err(e) => {
            println!("FAIL  {e}");
            failed = true;
        }
    }

    if failed { EX_UNAVAILABLE } else { 0 }
}
