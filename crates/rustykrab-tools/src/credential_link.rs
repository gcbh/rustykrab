//! Minting the one-time link a user opens to hand over a credential.
//!
//! Shared by every path that files a request. `credential_request` had
//! this inline and `gmail` had nothing, which meant the tool that files
//! most of the requests in practice — measured at 25/25 against
//! `browser`'s 1/9 — was also the one that could only ever say "open the
//! app". On Telegram or Signal there is no app in the loop, so that reply
//! is a dead end.

use std::time::Duration;

use rustykrab_store::CredentialRequestStore;

/// How long a credential link stays good.
///
/// Long enough to walk to a phone and generate an app password, short
/// enough that a link left in a chat log is usually already dead.
pub const LINK_TTL: Duration = Duration::from_secs(15 * 60);

/// A one-time URL for `request_id`, or `None` when no public base URL is
/// configured.
///
/// The token is minted here and returned exactly once, to be put in the
/// message the user receives; only its hash is stored. Failure to mint is
/// not failure to ask — the request is already filed and answerable in
/// the app, so this logs and returns `None` rather than propagating.
pub async fn mint_link(requests: &CredentialRequestStore, request_id: &str) -> Option<String> {
    let base = std::env::var("RUSTYKRAB_PUBLIC_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())?;

    match requests.issue_link(request_id, LINK_TTL).await {
        Ok(token) => Some(format!("{base}/c/{token}")),
        Err(e) => {
            tracing::warn!(error = %e, "could not mint a credential link");
            None
        }
    }
}

/// What to tell the model to do next, given a link or the lack of one.
///
/// Kept next to the minting so the two cannot drift: the difference
/// between these branches is exactly whether `mint_link` returned
/// something, and every caller needs both.
///
/// Both branches say to stop. An agent that files a request and then
/// keeps working will either invent a value or report failure, and both
/// are worse than waiting.
pub fn next_step(link: Option<&str>, service: &str) -> String {
    match link {
        Some(url) => format!(
            "Give the user this link and nothing else about the credential: {url} \
             — say it opens a secure form for their {service} details, that it \
             works once and expires in {} minutes. Do not ask for the value in \
             chat. Stop this task until they answer.",
            LINK_TTL.as_secs() / 60
        ),
        None => format!(
            "Tell the user you have asked for their {service} details and that a \
             prompt is waiting in the Apollo app. Do not ask for the value in \
             chat. Stop this task until they answer."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_branch_carries_the_url_and_its_lifetime() {
        let s = next_step(Some("https://mac.example.ts.net/c/abc"), "Gmail");
        assert!(s.contains("https://mac.example.ts.net/c/abc"));
        assert!(
            s.contains("15 minutes"),
            "the user needs to know it expires"
        );
        assert!(s.contains("Gmail"));
    }

    #[test]
    fn both_branches_tell_the_agent_to_stop() {
        // An agent that carries on after asking is the failure mode this
        // whole flow exists to avoid; neither branch may omit it.
        assert!(next_step(Some("https://x/c/t"), "Gmail").contains("Stop this task"));
        assert!(next_step(None, "Gmail").contains("Stop this task"));
    }

    #[test]
    fn neither_branch_invites_asking_in_chat() {
        assert!(
            next_step(Some("https://x/c/t"), "Gmail").contains("Do not ask for the value in chat")
        );
        assert!(next_step(None, "Gmail").contains("Do not ask for the value in chat"));
    }
}
