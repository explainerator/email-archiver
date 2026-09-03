//! Makes cargo rebuild when a migration is added or edited.
//!
//! `sqlx::migrate!` reads `migrations/` at COMPILE time and bakes the contents
//! into the binary. Cargo does not know that, so adding a `.sql` file changes
//! nothing it tracks: the next build is a no-op and the resulting binary has no
//! idea the migration exists.
//!
//! The failure that causes is quiet and convincing. `email-archiver migrate`
//! runs, applies the migrations it was built with, reports success, and lists
//! the tables — while the new migration has not run at all. It cost two rounds
//! of confused debugging when row-level security appeared not to switch on, and
//! it had already cost one earlier in this project's history.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
