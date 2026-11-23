//! `vikram link` — serve the remote-access API, and mint the tokens that reach it.
//!
//! Two halves with nothing between them: `serve` boots the runtime and listens, and `token` only
//! reads and writes `$VIKRAM_HOME/link/tokens.json`. Minting a token never starts a runtime, and
//! serving never prints a secret.

use clap::Subcommand;
use std::path::PathBuf;
use vikram_link::time::{iso8601, now_ms, parse_duration, remaining_label};
use vikram_link::token::{link_dir, max_temp_ms};
use vikram_link::{AddRequest, Lifetime, Server, ServerConfig, TokenStore, TokenSummary};
use vikram_runtime::config::Config;

#[derive(Debug, Subcommand)]
pub enum LinkAction {
    /// Serve the link API for linked devices: HTTP for questions, a WebSocket for events.
    ///
    /// Binds loopback by default. A bind other machines can reach is refused unless
    /// VIKRAM_LINK_TOKEN is set — that value is the bootstrap owner token, and it is how the first
    /// real token gets minted over the network.
    Serve {
        /// Address to bind. Loopback unless VIKRAM_LINK_TOKEN is set.
        #[arg(long, default_value = "127.0.0.1:7717", value_name = "ADDR")]
        bind: String,
        /// TLS certificate chain, PEM. Needs --tls-key.
        #[arg(long, value_name = "PEM", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// TLS private key, PEM. Needs --tls-cert.
        #[arg(long, value_name = "PEM", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
    },
    /// Mint, list and revoke link tokens.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Mint a token for one linked device. The secret is printed once, here, and never stored.
    ///
    /// Choose exactly one lifetime: `--for <duration>` is temporary, `--permanent` has no expiry
    /// and is revoked by hand. Durations are a whole number and a unit: 90s, 30m, 2h, 7d.
    #[command(
        after_help = "Tiers:\n  admin        everything, including tokens and the audit log; one admin owns the runtime\n  main         the root assistant: every session, turns, approvals\n  bots         every bot; never the root session\n  bot:<name>   exactly one bot\n  team         every team; never the root session\n  team:<name>  exactly one team\n  read         read-only mirror: events, no submit, no approvals\n\nExamples:\n  vikram link token add laptop --tier admin --for 30m\n  vikram link token add phone --tier read --permanent\n  vikram link token add buildbot --tier bot:alpha --for 7d"
    )]
    Add {
        /// A name to recognise the device by.
        name: String,
        /// admin, main, bots, bot:<name>, team, team:<name> or read.
        #[arg(long, value_name = "TIER")]
        tier: String,
        /// Temporary: how long it lives (90s, 30m, 2h, 7d).
        #[arg(long = "for", value_name = "DURATION")]
        span: Option<String>,
        /// Permanent: no expiry, revoked by hand.
        #[arg(long)]
        permanent: bool,
        /// Make this the owner token (admin only; moves ownership from any previous owner).
        #[arg(long)]
        owner: bool,
    },
    /// Revoke a token, or prune the expired ones.
    Rm {
        /// The token to revoke.
        name: Option<String>,
        /// Remove every token whose expiry has passed.
        #[arg(long)]
        expired: bool,
    },
    /// List tokens: name, tier, created, expiry, last seen, state. Never a secret.
    List,
}

pub fn run(config: Config, action: LinkAction) -> Result<(), String> {
    match action {
        LinkAction::Serve {
            bind,
            tls_cert,
            tls_key,
        } => serve(config, bind, tls_cert, tls_key),
        LinkAction::Token { action } => token(action),
    }
}

fn serve(
    config: Config,
    bind: String,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
) -> Result<(), String> {
    // Bound before the runtime boots: an address that cannot be taken must fail without a runtime
    // being started, and without anything having been written to the store.
    let server = Server::bind(ServerConfig {
        bind,
        tls_cert,
        tls_key,
        env_token: std::env::var("VIKRAM_LINK_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty()),
    })?;
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let runtime = tokio
        .block_on(vikram_runtime::Runtime::start(config))
        .map_err(|e| e.to_string())?;
    let handle = runtime.handle();

    let now = now_ms();
    let store = TokenStore::at_home()?;
    let live = store
        .summaries(now)
        .iter()
        .filter(|row| row.state(now) == "active")
        .count();
    println!(
        "vikram link: serving {}://{}",
        server.scheme(),
        server.local_addr()
    );
    println!("  tokens: {live} active in {}", store.path().display());
    println!("  audit:  {}", link_dir().join("audit.jsonl").display());
    println!(
        "  from another machine: ssh -N -L 7717:127.0.0.1:7717 <this host>, then use http://127.0.0.1:7717"
    );
    println!(
        "  endpoints: GET /health, GET /sessions, POST /sessions/{{id}}/submit, GET /events (WebSocket)"
    );

    server.serve(handle, tokio.handle().clone())
}

fn token(action: TokenAction) -> Result<(), String> {
    match action {
        TokenAction::Add {
            name,
            tier,
            span,
            permanent,
            owner,
        } => add(name, tier, span, permanent, owner),
        TokenAction::Rm { name, expired } => rm(name, expired),
        TokenAction::List => list(),
    }
}

fn add(
    name: String,
    tier: String,
    span: Option<String>,
    permanent: bool,
    owner: bool,
) -> Result<(), String> {
    let lifetime = choose_lifetime(span, permanent)?;
    let ceiling = max_temp_ms()?;
    let now = now_ms();
    let mut store = TokenStore::at_home()?;
    let minted = store.add(
        AddRequest {
            name,
            tier,
            lifetime,
            owner,
        },
        now,
        ceiling,
    )?;
    let life = match minted.expires_ms {
        Some(expires) => format!("{}, at {}", remaining_label(now, expires), iso8601(expires)),
        None => "permanent".to_owned(),
    };
    println!("minted `{}` — tier {}, {life}", minted.name, minted.tier);
    println!("  store: {}", store.path().display());
    println!();
    println!("secret (shown once — copy it now, it cannot be recovered):");
    println!("  {}", minted.secret);
    println!();
    println!("send it as `Authorization: Bearer <secret>`, or as `?token=<secret>` on /events.");
    Ok(())
}

/// The lifetime rule, in one place: exactly one of the two flags, and the duration is checked here
/// so a typo fails before anything is written.
fn choose_lifetime(span: Option<String>, permanent: bool) -> Result<Lifetime, String> {
    match (span, permanent) {
        (Some(_), true) => Err(
            "give either --for <duration> or --permanent, not both — a token cannot be temporary and permanent"
                .into(),
        ),
        (Some(span), false) => {
            parse_duration(&span)?;
            Ok(Lifetime::For(span))
        }
        (None, true) => Ok(Lifetime::Permanent),
        (None, false) => Err(
            "choose how long this token lives: --for <duration> (for example `--for 30m`) or --permanent"
                .into(),
        ),
    }
}

fn rm(name: Option<String>, expired: bool) -> Result<(), String> {
    let now = now_ms();
    let mut store = TokenStore::at_home()?;
    match (name, expired) {
        (Some(_), true) => Err("give a name or --expired, not both".into()),
        (None, false) => Err(
            "name the token to revoke (`vikram link token rm <name>`), or pass --expired to prune the expired ones"
                .into(),
        ),
        (None, true) => {
            let removed = store.remove_expired(now)?;
            if removed.is_empty() {
                println!("no expired tokens");
            } else {
                println!(
                    "removed {} expired token{}: {}",
                    removed.len(),
                    if removed.len() == 1 { "" } else { "s" },
                    removed.join(", ")
                );
            }
            Ok(())
        }
        (Some(name), false) => {
            if store.revoke(&name)? {
                println!("revoked `{name}`");
                Ok(())
            } else {
                Err(format!("no token named `{name}`"))
            }
        }
    }
}

fn list() -> Result<(), String> {
    let now = now_ms();
    let store = TokenStore::at_home()?;
    let rows = store.summaries(now);
    if rows.is_empty() {
        println!(
            "no link tokens — mint one with `vikram link token add laptop --tier admin --for 30m`"
        );
        return Ok(());
    }
    print!("{}", render_list(&rows, now));
    Ok(())
}

/// The table `token list` prints. It is built from [`TokenSummary`], which carries no secret, hash
/// or salt: there is nothing here that could print one.
fn render_list(rows: &[TokenSummary], now: u64) -> String {
    let mut table: Vec<[String; 6]> = Vec::with_capacity(rows.len());
    for row in rows {
        let expires = match row.expires_ms {
            Some(expires) => remaining_label(now, expires),
            None => "permanent".to_owned(),
        };
        let mut state = row.state(now).to_owned();
        if row.owner {
            state.push_str(" (owner)");
        }
        table.push([
            row.name.clone(),
            row.tier.to_string(),
            iso8601(row.created_ms),
            expires,
            row.last_seen_ms
                .map(iso8601)
                .unwrap_or_else(|| "never".to_owned()),
            state,
        ]);
    }
    let headers = ["NAME", "TIER", "CREATED", "EXPIRES", "LAST SEEN", "STATE"];
    let mut widths = headers.map(|header| header.len());
    for row in &table {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |cells: [&str; 6], widths: [usize; 6]| {
        let mut line = String::new();
        for (index, cell) in cells.iter().enumerate() {
            line.push_str(cell);
            if index + 1 < cells.len() {
                line.push_str(&" ".repeat(widths[index] - cell.chars().count() + 2));
            }
        }
        line.push('\n');
        line
    };
    out.push_str(&line(headers, widths));
    for row in &table {
        out.push_str(&line(
            [&row[0], &row[1], &row[2], &row[3], &row[4], &row[5]],
            widths,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_link::tier::Tier;

    fn summary(name: &str, tier: &str, expires_ms: Option<u64>, revoked: bool) -> TokenSummary {
        TokenSummary {
            name: name.into(),
            tier: Tier::parse(tier).unwrap(),
            created_ms: 0,
            expires_ms,
            last_seen_ms: None,
            revoked,
            owner: false,
        }
    }

    #[test]
    fn neither_flag_is_refused_and_says_what_to_choose() {
        let err = choose_lifetime(None, false).unwrap_err();
        assert!(err.contains("--for"), "{err}");
        assert!(err.contains("--permanent"), "{err}");
    }

    #[test]
    fn both_flags_are_refused() {
        let err = choose_lifetime(Some("30m".into()), true).unwrap_err();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn a_bad_duration_is_refused_before_anything_is_written() {
        let err = choose_lifetime(Some("7w".into()), false).unwrap_err();
        assert!(err.contains("`90s`, `30m`, `2h` or `7d`"), "{err}");
        assert_eq!(
            choose_lifetime(Some("30m".into()), false).unwrap(),
            Lifetime::For("30m".into())
        );
        assert_eq!(choose_lifetime(None, true).unwrap(), Lifetime::Permanent);
    }

    #[test]
    fn the_table_says_permanent_expired_and_never_a_secret() {
        let rows = vec![
            summary("laptop", "admin", None, false),
            summary("old", "bot:alpha", Some(1), false),
            summary("gone", "read", None, true),
        ];
        let table = render_list(&rows, 2_000);
        assert!(table.starts_with("NAME"), "{table}");
        assert!(table.contains("permanent"), "{table}");
        assert!(table.contains("expired"), "{table}");
        assert!(table.contains("revoked"), "{table}");
        assert!(
            table.contains("bot:alpha"),
            "the tier is printed by its canonical name"
        );
        assert!(!table.contains("vlink_"), "no secret can be printed");
        assert!(!table.contains("hash"));
    }

    #[test]
    fn an_expiring_token_says_how_long_it_has_left() {
        let now = 1_000_000;
        let rows = vec![summary(
            "laptop",
            "admin",
            Some(now + 30 * 60 * 1_000),
            false,
        )];
        let table = render_list(&rows, now);
        assert!(table.contains("in 30m"), "{table}");
    }
}
