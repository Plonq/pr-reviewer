//! Read-only discussion export for checking a busy real PR:
//! cargo run -p github --example discussion -- zed-industries/zed#13433
use github::{gh_cli::GhCliClient, GitHubClient};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args()
        .nth(1)
        .ok_or("expected owner/repo#123 or a PR URL")?;
    let client = GhCliClient::new();
    let loc = client.resolve_pr(&arg)?;
    let discussion = client.pr_discussion(&loc)?;
    eprintln!(
        "{} main comments/reviews, {} threads, {} thread comments, {} resolved threads",
        discussion.comments.len(),
        discussion.threads.len(),
        discussion
            .threads
            .iter()
            .map(|t| t.comments.len())
            .sum::<usize>(),
        discussion.threads.iter().filter(|t| t.is_resolved).count()
    );
    println!("{}", serde_json::to_string_pretty(&discussion)?);
    Ok(())
}
