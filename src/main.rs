//! email-archiver — read-only email archive.
//!
//! Storage in S3 (one bucket per user), index in Postgres, IMAPS to clients.
//! See ARCHIVE-PLAN.md for the design and phasing.

use anyhow::{Context, Result};
use email_archiver::config::Config;
use email_archiver::{check, db, diagnose, gmail, ingest, secrets, server, web};
use email_archiver::{connect_db, EXPECTED_DATABASE};

const USAGE: &str = "\
email-archiver

USAGE:
    email-archiver migrate
        Show the current schema. Migrations run automatically on every
        command, so this is a report rather than a required step.

    email-archiver add-user <email> <bucket> [display name]
        Register an archive user and the S3 bucket they own. The email address
        IS the login -- one thing to remember instead of two, and already
        unique without having to invent usernames.

    email-archiver rename-user <old email> <new email>
        Change a user's primary address. Aliases, password, bucket and archived
        mail are unaffected; every foreign key uses the numeric id.

    email-archiver alias <existing email> <new alias>
        Add another address the user may log in with. Works everywhere the
        primary does -- IMAP, the web client, and every command below that
        names a user.

    email-archiver remove-alias <alias>
        Remove an alias. Refuses to remove a user's primary address; use
        rename-user for that.

    email-archiver add-account <email> <address> <label> <imap|gmail>
        Register a source mailbox feeding that user's archive. <label> becomes
        the IMAP namespace prefix, e.g. work -> work/INBOX.

    email-archiver generate-key
        Print a fresh encryption key for config.toml. Changing this key makes
        every stored source password unreadable, so generate it once.

    email-archiver set-password <email>
        Set an archive user's password (Argon2id), used by both IMAP and the web
        client. Prompts twice, without echoing. Until this is set the account
        cannot be logged into at all.

        Passing the password as an argument still works but is discouraged: the
        shell rewrites !, $, backticks and quotes first, so the stored hash may
        cover a string you cannot retype.

    email-archiver set-source <address> <host> <username> [--insecure-tls]
        Store generic IMAP credentials for a source mailbox, encrypted. Prompts
        for the password. --insecure-tls accepts any certificate for THIS
        source only.

    email-archiver set-google <domain> <service-account.json>
        Store a Google Workspace service account key for a whole domain,
        encrypted. One key covers every mailbox in the domain, so Workspace
        accounts need no set-source. The file is only read once; afterwards the
        key lives in the database.

    email-archiver remove-google <domain>
        Forget a domain's service account key.

    email-archiver sources
        List every configured source: Workspace domains, and each account with
        how it authenticates. Flags accounts registered but not yet usable.

    email-archiver check <email> [--deep]
        Verify Postgres and S3 agree for one user. Samples 5 blobs by default;
        --deep reads and hash-checks every message body.

    email-archiver rebuild-manifests <email>
        Regenerate all S3 manifests for a user from the database. Use when
        manifests are missing, or were written under an older key scheme.

    email-archiver serve [bind] [--allow-plaintext]
        Read-only IMAP server. Default 127.0.0.1:1143.
        Serves TLS when tls.cert_path and tls.key_path are set. Loopback may be
        plaintext; any other address needs TLS, or --allow-plaintext to
        override for local testing. IMAP LOGIN sends the password in the clear,
        so that override puts real passwords on the network.

    email-archiver serve-web [bind] [--allow-plaintext] [--assets <dir>]
        Read-only web client and HTTP API. Default 127.0.0.1:8000, plaintext.
        Loopback is always plaintext, whatever tls.* says -- a certificate for
        the public hostname cannot validate for 127.0.0.1, so TLS there could
        not work anyway. Any other address needs TLS (phase 7) or
        --allow-plaintext. --assets serves the built frontend; without it only
        the API responds.

    email-archiver backfill-headers <email>
        Cache header blocks for messages archived before that column existed.
        Purely an optimisation; safe to interrupt and resume.

    email-archiver diagnose <address> <folder>
        Explain a folder's completeness gap: which absent source UIDs are
        duplicates of content already archived, and which are genuinely missing.

    email-archiver ingest <address>
        Pull mail from one source mailbox. Resumable: re-running continues
        from where it stopped.

Configuration is read from $EMAIL_ARCHIVER_CONFIG, else /etc/email-archiver/config.toml.
Generate it with: cd terraform && terraform output -raw archiver_config > config.toml
";

#[tokio::main]
async fn main() -> Result<()> {
    // Both `ring` and `aws-lc-rs` end up in the dependency graph (sqlx and the
    // AWS SDK each pull one), so rustls refuses to guess which to use and
    // panics at the first TLS handshake. Choose explicitly, once, before any
    // connection is made. Failure here means a provider was already installed,
    // which is harmless.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handled before the config is loaded. The config now REQUIRES an
    // encryption key, so needing a working config in order to generate that key
    // would deadlock first-time setup.
    if args.first().map(String::as_str) == Some("generate-key") {
        println!("{}", secrets::SecretKey::generate()?);
        eprintln!("\nAdd to config.toml as: encryption_key = \"...\"");
        eprintln!("Keep it. Losing it makes every stored source password unreadable.");
        return Ok(());
    }

    // Usage is answered BEFORE the config is read. Printing help should not
    // require a working configuration -- and gern-shell reads this output to
    // build its own help for the `email` verb, on machines and at moments where
    // no config need exist.
    match args.first().map(String::as_str) {
        None | Some("help") | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            return Ok(());
        }
        _ => {}
    }

    let config = Config::load()?;

    match args.first().map(String::as_str) {
        Some("migrate") => migrate(&config).await,

        Some("add-user") => {
            let (login, bucket) = (arg(&args, 1, "login")?, arg(&args, 2, "bucket")?);
            let display = args.get(3).cloned().unwrap_or_else(|| login.clone());
            let pool = connect_db(&config).await?;
            let id = db::create_user(&pool, &login, &bucket, &display).await?;
            println!("user {login} (id {id}) -> bucket {bucket}");
            Ok(())
        }

        Some("rename-user") => {
            let old = arg(&args, 1, "old email")?;
            let new = arg(&args, 2, "new email")?;
            let pool = connect_db(&config).await?;
            db::rename_user(&pool, &old, &new).await?;
            println!("{old} is now {new}");
            eprintln!("  Their password, bucket and archived mail are unchanged.");
            Ok(())
        }

        Some("alias") => {
            let existing = arg(&args, 1, "existing email")?;
            let alias = arg(&args, 2, "new alias")?;
            let pool = connect_db(&config).await?;
            let user_id = db::add_alias(&pool, &existing, &alias).await?;
            println!("{alias} is now an alias for {existing}");
            for (login, primary) in db::logins_for(&pool, user_id).await? {
                println!("  {login}{}", if primary { "  (primary)" } else { "" });
            }
            Ok(())
        }

        Some("remove-alias") => {
            let alias = arg(&args, 1, "alias")?;
            let pool = connect_db(&config).await?;
            db::remove_alias(&pool, &alias).await?;
            println!("{alias} is no longer a login");
            Ok(())
        }

        Some("add-account") => {
            let login = arg(&args, 1, "login")?;
            let address = arg(&args, 2, "address")?;
            let label = arg(&args, 3, "label")?;
            let provider = arg(&args, 4, "provider (imap|gmail)")?;
            let pool = connect_db(&config).await?;
            let id = db::create_account(&pool, &login, &address, &label, &provider).await?;
            println!("account {address} (id {id}) -> user {login}, namespace {label}/");
            Ok(())
        }

        Some("set-password") => {
            let login = arg(&args, 1, "login")?;
            let password = match args.get(2) {
                Some(given) => {
                    warn_argv_password();
                    given.clone()
                }
                None => prompt_new_password()?,
            };
            let pool = connect_db(&config).await?;
            db::set_user_password(&pool, &login, &password).await?;
            println!("password set for {login}");
            Ok(())
        }

        Some("set-source") => {
            let address = arg(&args, 1, "address")?;
            let host = arg(&args, 2, "host")?;
            let username = arg(&args, 3, "username")?;
            let password = match args.get(4).filter(|a| !a.starts_with("--")) {
                Some(given) => {
                    warn_argv_password();
                    given.clone()
                }
                None => prompt_password("Source mailbox password: ")?,
            };
            let insecure = args.iter().any(|a| a == "--insecure-tls");
            let pool = connect_db(&config).await?;
            db::set_source(
                &pool,
                &config.key()?,
                &address,
                &host,
                993,
                &username,
                &password,
                insecure,
            )
            .await?;
            println!("source credentials stored for {address} (encrypted)");
            if insecure {
                eprintln!(
                    "  WARNING: certificate verification disabled for {host} — encrypted, \
                     but the server is NOT authenticated"
                );
            }
            Ok(())
        }

        Some("set-google") => {
            let domain = arg(&args, 1, "domain")?;
            let path = arg(&args, 2, "path to the service account JSON key")?;
            let account = gmail::ServiceAccount::load(&path)?;
            let json = std::fs::read_to_string(&path)?;
            let pool = connect_db(&config).await?;
            db::set_google_domain(&pool, &config.key()?, &domain, &account.client_email, &json)
                .await?;
            println!(
                "{domain} -> {} (key stored, encrypted)",
                account.client_email
            );
            eprintln!(
                "  The key is now in the database; the file at {path} is no longer needed                  by ingest and can be removed."
            );
            Ok(())
        }

        Some("remove-google") => {
            let domain = arg(&args, 1, "domain")?;
            let pool = connect_db(&config).await?;
            db::remove_google_domain(&pool, &domain).await?;
            println!("{domain} removed; its mailboxes can no longer be ingested");
            Ok(())
        }

        Some("sources") => {
            let pool = connect_db(&config).await?;
            let accounts = db::all_sources(&pool).await?;
            let domains = db::google_domains(&pool).await?;

            if accounts.is_empty() && domains.is_empty() {
                println!("no sources configured");
                return Ok(());
            }

            if !domains.is_empty() {
                println!("Google Workspace domains:");
                for (domain, client_email) in &domains {
                    println!("  {domain:28} {client_email}");
                }
            }

            if !accounts.is_empty() {
                println!("Accounts:");
                for (address, label, provider, host, configured) in &accounts {
                    let how = match (provider.as_str(), configured) {
                        ("gmail", _) => "google (domain key)".to_string(),
                        (_, true) => host.clone().unwrap_or_default(),
                        // A generic IMAP account with no host or password is
                        // registered but cannot be ingested yet.
                        (_, false) => "NO CREDENTIALS -- set-source".to_string(),
                    };
                    println!("  {address:32} {label:12} {how}");
                }
            }
            Ok(())
        }

        Some("check") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            let deep = args.iter().any(|a| a == "--deep");
            check::run(&config, &pool, &login, deep).await
        }

        Some("rebuild-manifests") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            check::rebuild_manifests(&config, &pool, &login).await
        }

        Some("serve") => {
            let bind = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:1143".to_string());
            let allow_plaintext = args.iter().any(|a| a == "--allow-plaintext");
            let pool = connect_db(&config).await?;
            server::run(&std::sync::Arc::new(config), &pool, &bind, allow_plaintext).await
        }

        Some("serve-web") => {
            let bind = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:8000".to_string());
            let allow_plaintext = args.iter().any(|a| a == "--allow-plaintext");
            let assets = args
                .iter()
                .position(|a| a == "--assets")
                .and_then(|i| args.get(i + 1))
                .map(std::path::PathBuf::from);
            let pool = connect_db(&config).await?;
            web::run(
                &std::sync::Arc::new(config),
                &pool,
                &bind,
                allow_plaintext,
                assets,
            )
            .await
        }

        Some("backfill-headers") => {
            let login = arg(&args, 1, "login")?;
            let pool = connect_db(&config).await?;
            check::backfill_headers(&config, &pool, &login).await
        }

        Some("diagnose") => {
            let address = arg(&args, 1, "address")?;
            let folder = arg(&args, 2, "folder")?;
            let pool = connect_db(&config).await?;
            diagnose::run(&config, &pool, &address, &folder).await
        }

        Some("ingest") => {
            let address = arg(&args, 1, "address")?;
            let pool = connect_db(&config).await?;
            ingest::run(&config, &pool, &address).await
        }

        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

/// Read a password without echoing it.
///
/// **Never take a password from argv.** The shell rewrites `!`, `$`, backticks,
/// quotes and backslashes before this program is even started, so a password
/// containing any of them is stored as a hash of something the user cannot
/// retype -- which presents as "the password stopped working" with no way to
/// tell it from a forgotten one. It also lands in shell history and in the
/// process list, where other users can read it.
///
/// stdin passes through untouched, so this is both safer and more correct.
fn prompt_password(prompt: &str) -> Result<String> {
    use std::io::IsTerminal;

    // rpassword talks to the CONSOLE, not to stdin, so a redirect or a pipe
    // never reaches it and the process waits for a keystroke that will never
    // come. A deploy script calling this would hang indefinitely rather than
    // fail, so piped input is handled explicitly instead.
    let password = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt).context("reading password")?
    } else {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading password from stdin")?;
        // Only the line ending is stripped. Trailing spaces can be part of a
        // password, and silently trimming them would store a hash of something
        // other than what was supplied.
        line.trim_end_matches(['\r', '\n']).to_string()
    };

    anyhow::ensure!(!password.is_empty(), "password was empty");
    Ok(password)
}

/// Prompt twice, so a typo becomes a retry rather than a lockout.
///
/// Worth the second prompt precisely because nothing echoes: a mistyped
/// password is otherwise stored happily and only discovered at the next login,
/// long after the typo is recoverable from memory.
fn prompt_new_password() -> Result<String> {
    use std::io::IsTerminal;

    let first = prompt_password("New password: ")?;
    // Piped input gets one line, not two: asking a script to repeat itself
    // would just make it fail.
    if !std::io::stdin().is_terminal() {
        return Ok(first);
    }
    let again = prompt_password("Repeat password: ")?;
    anyhow::ensure!(
        first == again,
        "passwords did not match; nothing was changed"
    );
    Ok(first)
}

fn warn_argv_password() {
    eprintln!(
        "WARNING: password given on the command line. Your shell may have
         altered it (!, $, backticks, quotes), it is now in your shell history,
         and it was visible in the process list. Omit it to be prompted instead."
    );
}

fn arg(args: &[String], index: usize, name: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .with_context(|| format!("missing argument <{name}>\n\n{USAGE}"))
}

/// Report the schema. Migrations have already run in `connect_db`; this exists
/// to show what is there, not to be the thing that applies them.
async fn migrate(config: &Config) -> Result<()> {
    println!("loaded {config:?}");
    let pool = connect_db(config).await?;
    let database = EXPECTED_DATABASE;

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await?;

    println!("migrations applied to database {database:?}. tables:");
    for t in &tables {
        println!("  {t}");
    }

    pool.close().await;
    Ok(())
}
