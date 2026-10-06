//! `rustykrab questions`, `rustykrab answer` and `rustykrab judgment`: the
//! question router's and standing judgment's terminal surface (plan
//! `docs/plans/control-layer-and-worker-fleet.md`, sections 7 and 14).
//!
//! Like `rustykrab work`, a thin client of the running daemon's REST API
//! (`/api/questions`, `/api/judgment`): nothing here opens the store or
//! builds a controller, so an answer is the same command the phone sends,
//! and it resumes the item that asked.

use std::path::Path;
use std::time::Duration;

use reqwest::Url;
use serde_json::{json, Value};

use rustykrab_control::handle::{AnswerReply, JudgmentView};
use rustykrab_gateway::question_routes::QuestionList;
use rustykrab_store::JudgmentRow;

const USAGE: &str = "\
usage:
  rustykrab questions [--all]            questions waiting on you (--all: every one)
  rustykrab answer <question> <answer>   answer one, by its id or first characters;
                                         the item that asked resumes
  rustykrab judgment [list]              the standing judgment in force
  rustykrab judgment grant <words>       grant it in ordinary language, e.g.
                                         \"Ask me before paying for anything.\"
  rustykrab judgment revoke <id>         revoke a grant

Talks to the daemon at RUSTYKRAB_GATEWAY_URL (default http://127.0.0.1:3000).";

/// Entry point from `main`: `verb` is the subcommand, `args` the words after.
pub async fn run(data_dir: &Path, verb: &str, args: &[String]) -> anyhow::Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if words
        .first()
        .is_some_and(|w| matches!(*w, "help" | "-h" | "--help"))
    {
        println!("{USAGE}");
        return Ok(());
    }
    let client = Client::connect(data_dir).await?;
    let out = match (verb, words.as_slice()) {
        ("questions", []) => questions(&client, false).await?,
        ("questions", ["--all"]) => questions(&client, true).await?,
        ("answer", [question, answer @ ..]) if !answer.is_empty() => {
            let reply: AnswerReply = client
                .post(
                    &format!("/api/questions/{question}/answer"),
                    json!({ "answer": answer.join(" ") }),
                )
                .await?;
            let mut line = format!(
                "Answered {}: {}",
                short(&reply.question.id),
                reply.question.answer.unwrap_or_default()
            );
            if !reply.resumed.is_empty() {
                line.push_str(&format!(
                    "; resumed {}",
                    reply
                        .resumed
                        .iter()
                        .map(|i| short(i))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !reply.released.is_empty() {
                line.push_str(&format!("; {} item(s) released", reply.released.len()));
            }
            if !reply.cancelled.is_empty() {
                line.push_str(&format!("; {} item(s) cancelled", reply.cancelled.len()));
            }
            format!("{line}\n")
        }
        ("judgment", []) | ("judgment", ["list"]) => {
            let view: JudgmentView = client.get("/api/judgment").await?;
            judgment(&view)
        }
        ("judgment", ["grant", words @ ..]) if !words.is_empty() => {
            let row: JudgmentRow = client
                .post("/api/judgment", json!({ "text": words.join(" ") }))
                .await?;
            grant(&row)
        }
        ("judgment", ["revoke", id]) => {
            let reply: Value = client
                .post(&format!("/api/judgment/{id}/revoke"), json!({}))
                .await?;
            if reply["revoked"] == true {
                format!("Revoked {id}.\n")
            } else {
                format!("{id} was already revoked.\n")
            }
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    print!("{out}");
    Ok(())
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

async fn questions(client: &Client, all: bool) -> anyhow::Result<String> {
    let path = if all {
        "/api/questions".to_string()
    } else {
        "/api/questions?waiting=true".to_string()
    };
    let list: QuestionList = client.get(&path).await?;
    if list.questions.is_empty() {
        return Ok("Nothing is waiting on you.\n".to_string());
    }
    let mut out = String::new();
    for q in &list.questions {
        out.push_str(&format!(
            "{}  {:<14} {:<11} item #{}  {}\n",
            short(&q.id),
            q.class.as_str(),
            q.status.as_str(),
            short(&q.item),
            q.text
        ));
        if !q.options.is_empty() {
            out.push_str(&format!("          options: {}\n", q.options.join(" | ")));
        }
        if let Some(answer) = &q.answer {
            out.push_str(&format!(
                "          answered: {answer} (by {})\n",
                q.answered_by.as_deref().unwrap_or("?")
            ));
        }
    }
    Ok(out)
}

fn judgment(view: &JudgmentView) -> String {
    let mut out = String::from("In force:\n");
    for rule in &view.rules {
        out.push_str(&format!("  - {rule}\n"));
    }
    if view.grants.is_empty() {
        out.push_str("No grants: the baseline alone.\n");
    }
    for g in &view.grants {
        out.push_str(&format!("\n{}  \"{}\"\n", short(&g.id), g.text));
        for c in &g.checks {
            out.push_str(&format!("    {}\n", c.describe()));
        }
    }
    out
}

fn grant(row: &JudgmentRow) -> String {
    let mut out = format!("Granted {}. It compiled to:\n", row.id);
    for c in &row.checks {
        out.push_str(&format!("  - {}\n", c.describe()));
    }
    if row.checks.is_empty() {
        out.push_str("  nothing: no sentence matched a rule this build knows\n");
    }
    for s in &row.unrecognised {
        out.push_str(&format!("Not understood, so not in force: \"{s}\"\n"));
    }
    out
}

struct Client {
    http: reqwest::Client,
    base: Url,
}

impl Client {
    async fn connect(data_dir: &Path) -> anyhow::Result<Client> {
        let (base, http) = crate::daemon_client::connect(data_dir, Duration::from_secs(60)).await?;
        Ok(Client { http, base })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let url = self.base.join(path)?;
        Self::read(self.http.get(url).send().await?).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> anyhow::Result<T> {
        let url = self.base.join(path)?;
        Self::read(self.http.post(url).json(&body).send().await?).await
    }

    async fn read<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> anyhow::Result<T> {
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        let body: Value = response.json().await.unwrap_or(Value::Null);
        anyhow::bail!(
            "{status}: {}",
            body["message"].as_str().unwrap_or("the daemon refused")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rustykrab_core::questions::JudgmentCheck;

    #[test]
    fn a_grant_says_what_it_compiled_to_and_what_it_did_not() {
        let row = JudgmentRow {
            id: "j1".into(),
            scope: "all".into(),
            text: "Ask me before paying. Be nice.".into(),
            checks: vec![JudgmentCheck::ConsentFor {
                resource: "payment".into(),
            }],
            policy: Default::default(),
            unrecognised: vec!["Be nice".into()],
            granted_by: None,
            granted_at: Utc::now(),
            revoked_at: None,
        };
        let out = grant(&row);
        assert!(out.contains("ask before anything writes payment"), "{out}");
        assert!(
            out.contains("Not understood, so not in force: \"Be nice\""),
            "{out}"
        );
    }
}
