//! Auth spike: prove the end-to-end Office 365 OAuth + Graph round-trip.
//!
//! `cargo run -p auth-spike`. First run opens the browser to sign in; tokens
//! are cached in the OS keychain and silently refreshed afterwards.

use anyhow::Context;
use wattmail_application::inbox_preview;
use wattmail_infrastructure::{AuthService, GraphClient, OAuthConfig};

// Public client identifiers — NOT secrets. Safe to commit / ship in the binary.
const CLIENT_ID: &str = "60d6101b-3d8a-4a09-8718-ad90c0d88f13";
const TENANT_ID: &str = "652459b1-612f-4586-b424-a0069d51cc32";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = OAuthConfig::office365(TENANT_ID, CLIENT_ID);

    let auth = AuthService::new(config).context("initialising auth service")?;
    let access_token = auth
        .access_token()
        .await
        .context("acquiring an access token")?;

    let provider = GraphClient::new(access_token);
    let preview = inbox_preview(&provider, 10)
        .await
        .context("fetching inbox preview")?;

    println!(
        "\nSigned in as {} <{}>\n",
        preview.user.display_name, preview.user.email
    );
    println!("Most recent {} message(s):", preview.messages.len());
    for (i, m) in preview.messages.iter().enumerate() {
        let flag = if m.is_read { ' ' } else { '•' };
        println!("{:>2}. {} [{}] {}", i + 1, flag, m.received, m.subject);
        println!("        from {}", m.from);
    }
    println!();

    Ok(())
}
