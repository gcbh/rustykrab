//! The work surface on a phone (plan `docs/plans/control-layer-and-worker-fleet.md`,
//! section 14.2): the Telegram commands that mirror the CLI subset a phone
//! needs, and the buttons on the controller's notices.
//!
//! | Command | Does |
//! |---|---|
//! | `/work` | open work, one roll-up line per root |
//! | `/work <id>` | the tree under an item, to depth 2 |
//! | `/approve <id>` | release a plan's held items (`work approve`) |
//! | `/reject <id> [reason]` | cancel them, with cascade (`work reject`) |
//! | `/cancel <id>` | cancel an item and its open subtree (`work cancel`) |
//! | `/answer <question> <answer>` | answer a question; the item resumes |
//! | `/questions` | the questions waiting on you |
//!
//! Ids may be given whole or by their first characters (`#abcd1234`, as
//! the notices print them). A command handled here never enters a
//! conversation: an answer resumes the work item that asked.
//!
//! [`notice_backend`] puts approve and reject buttons under a plan preview,
//! and a question's options under it, on channels that support buttons
//! (Telegram); every notice also carries the commands, so a channel without
//! buttons loses nothing. A credential question's page link (plan section
//! 7) follows its notice as its own message on the same channel, from the
//! controller's in-memory [`CredentialLinks`]; it is never in the outbox.
//!
//! [`refresh_credentials`] keeps the catalog's [`StoredCredentials`] in
//! step with the credential registry, so `on_credential` triggers fire and
//! parked items wake for a credential stored by any path.

use std::sync::Arc;

use async_trait::async_trait;
use rustykrab_channels::{ButtonRow, CommandHook, TelegramChannel};
use rustykrab_control::controller::{CredentialLinks, StoredCredentials};
use rustykrab_control::handle::ControlHandle;
use rustykrab_core::work::{Status, WorkItem};
use rustykrab_store::{QuestionFilter, Store, WorkFilter};
use rustykrab_tools::MessageBackend;

/// Roots `/work` lists before it says how many more there are.
const ROOTS_MAX: usize = 15;
/// Lines `/work <id>` prints.
const TREE_MAX: usize = 25;
/// Options a question offers as buttons.
const OPTION_BUTTONS: usize = 4;

fn short(id: &str) -> String {
    format!("#{}", id.chars().take(8).collect::<String>())
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn phrase(status: Status) -> String {
    match status.reason() {
        Some(r) => format!("{} ({r})", status.name()),
        None => status.name().to_string(),
    }
}

/// The work commands over the controller and its store.
pub struct WorkCommands {
    control: Arc<dyn ControlHandle>,
    store: Store,
    /// The actor commands are recorded under.
    actor: String,
}

impl WorkCommands {
    pub fn new(control: Arc<dyn ControlHandle>, store: Store, actor: &str) -> Self {
        WorkCommands {
            control,
            store,
            actor: actor.to_string(),
        }
    }

    /// An item by its id or a unique prefix of it.
    async fn resolve(&self, raw: &str) -> Result<WorkItem, String> {
        let id = raw.trim().trim_start_matches('#');
        if id.is_empty() {
            return Err("name an item: its id, or the #id a message printed".into());
        }
        if let Ok(Some(item)) = self.store.work_get(id).await {
            return Ok(item);
        }
        let all = self
            .store
            .work_list(&WorkFilter {
                include_closed: true,
                ..WorkFilter::default()
            })
            .await
            .map_err(|e| format!("could not read work items: {e}"))?;
        let mut matches: Vec<WorkItem> = all.into_iter().filter(|i| i.id.starts_with(id)).collect();
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(format!("no work item {}", short(id))),
            n => Err(format!(
                "{} names {n} items; give more of the id",
                short(id)
            )),
        }
    }

    async fn list(&self) -> String {
        let open = match self.store.work_list(&WorkFilter::default()).await {
            Ok(items) => items,
            Err(e) => return format!("Could not read work items: {e}"),
        };
        let roots: Vec<&WorkItem> = open.iter().filter(|i| i.parent.is_none()).collect();
        if roots.is_empty() {
            return "No open work.".to_string();
        }
        let mut lines = vec![format!("Open work ({}):", roots.len())];
        for root in roots.iter().take(ROOTS_MAX) {
            let mut line = format!(
                "{} {} [{}] {}",
                short(&root.id),
                clip(&root.title, 60),
                root.kind.as_str(),
                phrase(root.status)
            );
            if let Ok(graph) = self.control.graph(&root.id).await {
                if let Some(node) = graph.nodes.first().filter(|n| n.children_total > 0) {
                    line.push_str(&format!(
                        ", {} of {} done",
                        node.children_done, node.children_total
                    ));
                }
            }
            lines.push(line);
        }
        if roots.len() > ROOTS_MAX {
            lines.push(format!("and {} more", roots.len() - ROOTS_MAX));
        }
        lines.push("/work <id> shows one.".to_string());
        lines.join("\n")
    }

    async fn show(&self, raw: &str) -> String {
        let item = match self.resolve(raw).await {
            Ok(item) => item,
            Err(e) => return e,
        };
        let graph = match self.control.graph(&item.id).await {
            Ok(g) => g,
            Err(e) => return format!("Could not read {}: {e}", short(&item.id)),
        };
        let mut lines = Vec::new();
        for node in graph.nodes.iter().filter(|n| n.depth <= 2).take(TREE_MAX) {
            let indent = "  ".repeat(node.depth as usize);
            let mut line = format!(
                "{indent}{} {} [{}] {}",
                short(&node.item.id),
                clip(&node.item.title, 60),
                node.item.kind.as_str(),
                phrase(node.rollup.unwrap_or(node.item.status))
            );
            if node.children_total > 0 {
                line.push_str(&format!(
                    ", {} of {} done",
                    node.children_done, node.children_total
                ));
            }
            if let Some(origin) = &node.item.status_origin {
                line.push_str(&format!(" <- {}", short(origin)));
            }
            lines.push(line);
        }
        if graph.nodes.len() > TREE_MAX {
            lines.push(format!("and {} more", graph.nodes.len() - TREE_MAX));
        }
        lines.join("\n")
    }

    async fn approve(&self, raw: &str) -> String {
        let root = match self.resolve(raw).await {
            Ok(item) => item,
            Err(e) => return e,
        };
        match self.control.approve(&root.id, &self.actor).await {
            Ok(released) if released.is_empty() => {
                format!("Nothing under {} is waiting for approval.", short(&root.id))
            }
            Ok(released) => format!(
                "Approved {}: {} item(s) released.",
                short(&root.id),
                released.len()
            ),
            Err(e) => format!("Could not approve {}: {e}", short(&root.id)),
        }
    }

    async fn reject(&self, raw: &str, reason: Option<String>) -> String {
        let root = match self.resolve(raw).await {
            Ok(item) => item,
            Err(e) => return e,
        };
        match self.control.reject(&root.id, reason, &self.actor).await {
            Ok(cancelled) if cancelled.is_empty() => {
                format!("Nothing under {} is waiting for approval.", short(&root.id))
            }
            Ok(cancelled) => format!(
                "Rejected {}: {} held item(s) cancelled.",
                short(&root.id),
                cancelled.len()
            ),
            Err(e) => format!("Could not reject {}: {e}", short(&root.id)),
        }
    }

    async fn cancel(&self, raw: &str) -> String {
        let item = match self.resolve(raw).await {
            Ok(item) => item,
            Err(e) => return e,
        };
        let before = self.control.graph(&item.id).await.ok();
        match self.control.cancel(&item.id, None, &self.actor).await {
            Ok(cancelled) => {
                let finished: Vec<String> = before
                    .map(|g| {
                        g.nodes
                            .iter()
                            .filter(|n| {
                                n.item.status.is_closed() && !cancelled.contains(&n.item.id)
                            })
                            .map(|n| format!("{} {}", short(&n.item.id), phrase(n.item.status)))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut reply = format!(
                    "Cancelled {} item(s) under {}.",
                    cancelled.len(),
                    short(&item.id)
                );
                if !finished.is_empty() {
                    reply.push_str(&format!(" Already finished: {}.", finished.join(", ")));
                }
                reply
            }
            Err(e) => format!("Could not cancel {}: {e}", short(&item.id)),
        }
    }

    async fn answer(&self, question: &str, answer: &str) -> String {
        if question.is_empty() || answer.trim().is_empty() {
            return "Usage: /answer <question> <your answer>".to_string();
        }
        match self.control.answer(question, answer, &self.actor).await {
            Ok(reply) => {
                let mut text = format!(
                    "Answered question {}.",
                    reply.question.id.chars().take(8).collect::<String>()
                );
                if !reply.resumed.is_empty() {
                    text.push_str(&format!(
                        " {} resumes.",
                        reply
                            .resumed
                            .iter()
                            .map(|i| short(i))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !reply.released.is_empty() {
                    text.push_str(&format!(" {} item(s) released.", reply.released.len()));
                }
                if !reply.cancelled.is_empty() {
                    text.push_str(&format!(" {} item(s) cancelled.", reply.cancelled.len()));
                }
                text
            }
            Err(e) => format!("Could not answer {question}: {e}"),
        }
    }

    async fn judgment(&self) -> String {
        match self.control.judgment().await {
            Ok(view) => {
                let mut lines = vec!["In force:".to_string()];
                lines.extend(view.rules.iter().map(|r| format!("- {r}")));
                for g in &view.grants {
                    lines.push(format!(
                        "{} \"{}\"",
                        g.id.chars().take(8).collect::<String>(),
                        clip(&g.text, 120)
                    ));
                }
                lines.push("/grant <words> adds to it; /revoke <id> takes a grant back.".into());
                lines.join("\n")
            }
            Err(e) => format!("Could not read standing judgment: {e}"),
        }
    }

    async fn grant(&self, words: &str) -> String {
        if words.is_empty() {
            return "Usage: /grant <what may be decided without asking>".to_string();
        }
        match self.control.grant_judgment(words, "", &self.actor).await {
            Ok(row) => {
                let mut lines = vec![format!(
                    "Granted {}. In force now:",
                    row.id.chars().take(8).collect::<String>()
                )];
                lines.extend(row.checks.iter().map(|c| format!("- {}", c.describe())));
                if row.checks.is_empty() {
                    lines.push("- nothing: no sentence matched a rule I know".into());
                }
                for s in &row.unrecognised {
                    lines.push(format!("Not understood, so not in force: \"{s}\""));
                }
                lines.join("\n")
            }
            Err(e) => format!("Could not grant it: {e}"),
        }
    }

    async fn revoke(&self, raw: &str) -> String {
        let prefix = raw.trim().trim_start_matches('#');
        let grants = match self.control.judgment().await {
            Ok(view) => view.grants,
            Err(e) => return format!("Could not read standing judgment: {e}"),
        };
        let matches: Vec<&rustykrab_store::JudgmentRow> = grants
            .iter()
            .filter(|g| !prefix.is_empty() && g.id.starts_with(prefix))
            .collect();
        let [grant] = matches.as_slice() else {
            return format!("No single grant starts with {prefix}; /judgment lists them.");
        };
        match self.control.revoke_judgment(&grant.id, &self.actor).await {
            Ok(true) => format!("Revoked \"{}\".", clip(&grant.text, 120)),
            Ok(false) => "It was already revoked.".to_string(),
            Err(e) => format!("Could not revoke it: {e}"),
        }
    }

    async fn questions(&self) -> String {
        let waiting = match self
            .store
            .questions_list(&QuestionFilter {
                status: Some(rustykrab_core::questions::QuestionStatus::Open),
                ..QuestionFilter::default()
            })
            .await
        {
            Ok(q) => q,
            Err(e) => return format!("Could not read questions: {e}"),
        };
        if waiting.is_empty() {
            return "Nothing is waiting on you.".to_string();
        }
        let mut lines = vec!["Waiting on your answer:".to_string()];
        for q in waiting.iter().rev().take(ROOTS_MAX) {
            lines.push(format!(
                "{} ({}): {}",
                q.id.chars().take(8).collect::<String>(),
                short(&q.item),
                clip(&q.text, 160)
            ));
        }
        lines.push("/answer <question> <your answer>".to_string());
        lines.join("\n")
    }
}

#[async_trait]
impl CommandHook for WorkCommands {
    async fn handle(&self, text: &str, _chat_id: i64, _thread_id: i64) -> Option<String> {
        let mut words = text.split_whitespace();
        let command = words.next()?;
        // `/approve@botname` in a group.
        let command = command.split('@').next().unwrap_or(command);
        let first = words.next().unwrap_or("");
        let rest: String = words.collect::<Vec<_>>().join(" ");
        let reply = match command {
            "/work" if first.is_empty() => self.list().await,
            "/work" => self.show(first).await,
            "/approve" => self.approve(first).await,
            "/reject" => {
                let reason = Some(rest.clone()).filter(|r| !r.trim().is_empty());
                self.reject(first, reason).await
            }
            "/cancel" => self.cancel(first).await,
            "/answer" => self.answer(first, &rest).await,
            "/questions" => self.questions().await,
            "/judgment" => self.judgment().await,
            "/grant" => {
                let words = format!("{first} {rest}");
                self.grant(words.trim()).await
            }
            "/revoke" => self.revoke(first).await,
            _ => return None,
        };
        Some(reply)
    }

    fn help(&self) -> Vec<String> {
        vec![
            "/work: open work; /work <id>: one item's tree".to_string(),
            "/approve <id>, /reject <id> [reason]: answer a plan's approval".to_string(),
            "/cancel <id>: cancel an item and what is open under it".to_string(),
            "/questions, /answer <question> <answer>: questions waiting on you".to_string(),
            "/judgment, /grant <words>, /revoke <id>: standing judgment".to_string(),
        ]
    }
}

/// The `planner` worker (plan section 6.1): a local worker on the planner
/// definition, on the same model and tools as the general worker, which the
/// controller leases planning items to and nothing else.
pub fn planner_worker(
    definition: rustykrab_core::AgentDefinition,
    provider: Arc<dyn rustykrab_core::model::ModelProvider>,
    tools: Vec<Arc<dyn rustykrab_core::Tool>>,
    backend: Arc<dyn rustykrab_tools::WorkBackend>,
    transcripts: Arc<dyn rustykrab_agent::RunTranscripts>,
    slot: Arc<tokio::sync::Semaphore>,
) -> Arc<dyn rustykrab_control::worker::Worker> {
    Arc::new(
        rustykrab_agent::LocalWorker::new(
            "planner",
            definition,
            provider,
            tools,
            Arc::new(rustykrab_agent::ProcessSandbox::new()),
            backend,
        )
        .with_transcripts(transcripts)
        .with_slot(slot),
    )
}

/// The buttons a notice's commands stand for: approve and reject under a
/// plan preview, a question's options under it.
pub fn notice_buttons(body: &str) -> Vec<ButtonRow> {
    let mut rows: Vec<ButtonRow> = Vec::new();
    let mut question: Option<String> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("Reply /approve ") {
            if let Some(root) = rest.split_whitespace().next() {
                rows.push(vec![
                    ("Approve".to_string(), format!("approve:{root}")),
                    ("Reject".to_string(), format!("reject:{root}")),
                ]);
            }
        } else if let Some(rest) = line.strip_prefix("Question ") {
            question = rest.split_whitespace().next().map(str::to_string);
        } else if let (Some(options), Some(q)) = (line.strip_prefix("Options: "), &question) {
            let row: ButtonRow = options
                .trim_end_matches('.')
                .split("  ")
                .filter_map(|o| {
                    let (n, label) = o.trim().split_once(") ")?;
                    n.parse::<usize>().ok()?;
                    Some((clip(label, 30), format!("answer:{q}:{n}")))
                })
                .take(OPTION_BUTTONS)
                .collect();
            if !row.is_empty() {
                rows.push(row);
            }
        } else if line.starts_with("Answer with /answer ") && line.contains(" yes") {
            if let Some(q) = &question {
                rows.push(vec![
                    ("Yes".to_string(), format!("answer:{q}:yes")),
                    ("No".to_string(), format!("answer:{q}:no")),
                ]);
            }
        }
    }
    rows
}

/// Sends the controller's notices: on Telegram with buttons when a notice
/// carries commands a button can stand for, and through `inner` otherwise;
/// then any credential link its questions carry, one message each.
struct NoticeBackend {
    inner: Arc<dyn MessageBackend>,
    telegram: Option<Arc<TelegramChannel>>,
    links: Option<CredentialLinks>,
}

#[async_trait]
impl MessageBackend for NoticeBackend {
    async fn send_message(
        &self,
        channel: &str,
        text: &str,
        chat_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> rustykrab_core::Result<serde_json::Value> {
        let buttons = notice_buttons(text);
        let chat = chat_id.and_then(|c| c.parse::<i64>().ok());
        let sent = if let (Some(tg), Some(chat), false, "telegram") =
            (&self.telegram, chat, buttons.is_empty(), channel)
        {
            let thread = thread_id.and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
            tg.send_text_with_buttons(chat, text, thread, &buttons)
                .await
                .map_err(|e| rustykrab_core::Error::ToolExecution(e.to_string().into()))?;
            serde_json::json!({ "delivered": "telegram", "chat_id": chat })
        } else {
            self.inner
                .send_message(channel, text, chat_id, thread_id)
                .await?
        };
        // Only once the notice is out, so the user reads why before they
        // are handed the form. Taken, so a link goes once; a failed send
        // is logged without the link.
        if let Some(links) = &self.links {
            for link in links.take_for_notice(text) {
                if let Err(e) = self
                    .inner
                    .send_message(channel, &link, chat_id, thread_id)
                    .await
                {
                    tracing::warn!(channel, error = %e, "credential link not delivered");
                }
            }
        }
        Ok(sent)
    }
}

/// The notice sender: [`notice_buttons`] on Telegram, `inner` for the rest,
/// and the credential links `links` holds after the notice asking for them.
pub fn notice_backend(
    inner: Arc<dyn MessageBackend>,
    telegram: Option<Arc<TelegramChannel>>,
    links: Option<CredentialLinks>,
) -> Arc<dyn MessageBackend> {
    Arc::new(NoticeBackend {
        inner,
        telegram,
        links,
    })
}

/// Keep `stored` in step with the credential registry every `every_secs`
/// (names only; no value is read).
pub async fn refresh_credentials(store: Store, stored: Arc<StoredCredentials>, every_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(every_secs.max(1)));
    loop {
        interval.tick().await;
        if let Err(e) = stored.refresh(&store.secrets()).await {
            tracing::debug!(error = %e, "could not read the credential registry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_gets_approve_and_reject_and_a_question_its_options() {
        let body = "\"Plan\" (#abc): blocked (needs_consent); 0 of 3 done.\n\
                    Needs your approval (x): 2 held.\n\
                    Reply /approve 1234-root or /reject 1234-root [reason].\n\
                    Asked: Which florist?\n\
                    Question q1w2e3r4 is for \"Flowers\" (#fff).\n\
                    Options: 1) Petals  2) Stems.\n\
                    Answer with /answer q1w2e3r4 <your answer> (or the option number).";
        let rows = notice_buttons(body);
        assert_eq!(
            rows[0],
            vec![
                ("Approve".to_string(), "approve:1234-root".to_string()),
                ("Reject".to_string(), "reject:1234-root".to_string()),
            ]
        );
        assert_eq!(
            rows[1],
            vec![
                ("Petals".to_string(), "answer:q1w2e3r4:1".to_string()),
                ("Stems".to_string(), "answer:q1w2e3r4:2".to_string()),
            ]
        );
        for row in &rows {
            for (_, data) in row {
                assert!(data.len() <= 64, "Telegram caps callback data at 64 bytes");
                assert!(rustykrab_channels::telegram::command_for_button(data).is_some());
            }
        }
        assert!(notice_buttons("\"Done\" (#a): done.").is_empty());
    }

    #[tokio::test]
    async fn the_commands_read_approve_and_answer_through_the_controller() {
        use rustykrab_control::controller::{Controller, ControllerConfig};
        use rustykrab_core::work::{ItemRef, PlanOutcome, WorkItemDraft, WorkPlan};
        use rustykrab_tools::work_backend::Provenance;

        let dir = std::env::temp_dir().join(format!("rk-work-channel-{}", uuid::Uuid::new_v4()));
        let store = Store::open(&dir, vec![9u8; 32]).unwrap();
        let config = ControllerConfig {
            approval: rustykrab_control::questions::baseline(),
            ..ControllerConfig::default()
        };
        let control: Arc<dyn ControlHandle> =
            Arc::new(Controller::new(store.clone(), vec![], config));
        let commands = WorkCommands::new(control.clone(), store.clone(), "user:telegram");
        assert_eq!(
            commands.handle("/work", 1, 0).await.as_deref(),
            Some("No open work.")
        );
        assert!(commands.handle("hello", 1, 0).await.is_none());
        assert!(commands.handle("/unknown", 1, 0).await.is_none());

        let draft = |tmp: &str, title: &str| WorkItemDraft {
            tmp: Some(tmp.into()),
            title: title.into(),
            objective: "o".into(),
            done_when: "d".into(),
            ..WorkItemDraft::default()
        };
        let mut invite = draft("m", "Invite the friends");
        invite.parent = Some(ItemRef::Tmp { tmp: "P".into() });
        invite.writable_resources = vec!["message:third_party".into()];
        let outcome = control
            .file_plan(
                WorkPlan {
                    root: ItemRef::Tmp { tmp: "P".into() },
                    items: vec![draft("P", "Plan the birthday"), invite],
                    edges: vec![],
                    rationale: "r".into(),
                },
                Provenance::default(),
                rustykrab_control::graph::FilingSource::Planner,
            )
            .await
            .unwrap();
        let PlanOutcome::Accepted(accepted) = outcome else {
            panic!("rejected");
        };
        let root: String = accepted.root.chars().take(8).collect();
        let listed = commands.handle("/work", 1, 0).await.unwrap();
        assert!(listed.contains("Plan the birthday"), "{listed}");
        let tree = commands
            .handle(&format!("/work #{root}"), 1, 0)
            .await
            .unwrap();
        assert!(tree.contains("Invite the friends"), "{tree}");
        let asked = commands.handle("/questions", 1, 0).await.unwrap();
        assert!(asked.contains("Approve the plan"), "{asked}");
        let approved = commands
            .handle(&format!("/approve {root}"), 1, 0)
            .await
            .unwrap();
        assert!(approved.contains("1 item(s) released"), "{approved}");
        let granted = commands
            .handle("/grant Ask me before paying for anything.", 1, 0)
            .await
            .unwrap();
        assert!(granted.contains("payment"), "{granted}");
        let view = commands.handle("/judgment", 1, 0).await.unwrap();
        assert!(view.contains("In force"), "{view}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_consent_gets_yes_and_no() {
        let body = "Asked: May I pay?\nQuestion abcd1234 is for \"Pay\" (#1).\n\
                    Answer with /answer abcd1234 yes, or /answer abcd1234 no.";
        assert_eq!(
            notice_buttons(body),
            vec![vec![
                ("Yes".to_string(), "answer:abcd1234:yes".to_string()),
                ("No".to_string(), "answer:abcd1234:no".to_string()),
            ]]
        );
    }

    /// Records what the channel was asked to send.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(String, String, Option<String>)>>);

    #[async_trait]
    impl MessageBackend for Recorder {
        async fn send_message(
            &self,
            channel: &str,
            text: &str,
            chat_id: Option<&str>,
            _thread_id: Option<&str>,
        ) -> rustykrab_core::Result<serde_json::Value> {
            self.0.lock().unwrap().push((
                channel.to_string(),
                text.to_string(),
                chat_id.map(str::to_string),
            ));
            Ok(serde_json::json!({ "delivered": channel }))
        }
    }

    #[tokio::test]
    async fn a_credential_link_follows_its_notice_on_the_same_channel_once() {
        let recorder = Arc::new(Recorder::default());
        let links = CredentialLinks::with_base(Some("https://krab.test".to_string()));
        links.hold(
            "abcd1234-0000-0000-0000-000000000000",
            "https://krab.test/c/token".to_string(),
        );
        let backend = notice_backend(recorder.clone(), None, Some(links.clone()));
        let body = "Asked: \"Pay\" (#1) needs the credential `bank login`\n\
                    Question abcd1234 is for \"Pay\" (#1).\n\
                    Store it on the credential page: a one-time link follows this message.";
        backend
            .send_message("signal", body, Some("+15550100"), None)
            .await
            .unwrap();
        backend
            .send_message("signal", body, Some("+15550100"), None)
            .await
            .unwrap();
        let sent = recorder.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[0].1, body);
        assert_eq!(
            sent[1],
            (
                "signal".to_string(),
                "https://krab.test/c/token".to_string(),
                Some("+15550100".to_string())
            )
        );
        assert_eq!(sent[2].1, body, "the link went once");
        assert_eq!(links.waiting(), 0);
    }
}
