//! Standing judgment (plan section 7): the user's delegation in ordinary
//! language, compiled to a checklist the controller evaluates.
//!
//! [`compile`] reads a grant sentence by sentence against a small, fixed
//! grammar and returns the [`JudgmentCheck`]s it understood, the sentences
//! it did not (reported back, never guessed at), and the grant in the
//! projects crate's [`JudgmentPolicy`] shape (statement, delegated scopes,
//! reserved decisions), which is how the planning plan models delegation.
//! The same words always compile to the same checks, and every check reads
//! back as one line ([`JudgmentCheck::describe`]), so the compiled form is
//! deterministic and inspectable.
//!
//! [`Judgment`] folds the active grants, oldest first, over the controller's
//! baseline [`ApprovalPolicy`]: what needs consent, what is delegated, the
//! plan thresholds, and which question topics are decided alone or always
//! asked. A later grant overrides an earlier one on the same point.

use std::collections::BTreeSet;

use rustykrab_core::questions::JudgmentCheck;
use rustykrab_projects::JudgmentPolicy;
use rustykrab_store::JudgmentRow;

use crate::graph::ApprovalPolicy;

/// The resource a payment writes, as a side effect the policy can name.
pub const PAYMENT: &str = "payment";
/// The resource a message to someone other than the user writes.
pub const THIRD_PARTY_MESSAGE: &str = "message:third_party";

/// The baseline before any grant: plans over ten items or a million tokens,
/// and payments and messages to third parties, need the user. The plan
/// leaves the default thresholds for review (section 17); these are the
/// conservative placeholders.
pub fn baseline() -> ApprovalPolicy {
    ApprovalPolicy {
        id: Some("baseline".to_string()),
        max_items: Some(10),
        max_total_tokens: Some(1_000_000),
        consent_resources: [PAYMENT, THIRD_PARTY_MESSAGE]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ..ApprovalPolicy::default()
    }
}

/// What a grant compiled to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub checks: Vec<JudgmentCheck>,
    /// The grant as the planning plan models delegation.
    pub policy: JudgmentPolicy,
    /// Sentences that compiled to no check.
    pub unrecognised: Vec<String>,
}

/// Compile a grant's words (section 7). Deterministic: no model is called.
pub fn compile(text: &str) -> Compiled {
    let mut checks: Vec<JudgmentCheck> = Vec::new();
    let mut unrecognised = Vec::new();
    for sentence in sentences(text) {
        let found = compile_sentence(&sentence);
        if found.is_empty() {
            unrecognised.push(sentence);
        }
        for c in found {
            if !checks.contains(&c) {
                checks.push(c);
            }
        }
    }
    let mut policy = JudgmentPolicy {
        statement: text.trim().to_string(),
        ..JudgmentPolicy::default()
    };
    for c in &checks {
        match c {
            JudgmentCheck::ConsentFor { .. } | JudgmentCheck::ReserveOn { .. } => {
                policy.reserved_decisions.push(c.describe())
            }
            _ => policy.delegated_scopes.push(c.describe()),
        }
    }
    Compiled {
        checks,
        policy,
        unrecognised,
    }
}

fn sentences(text: &str) -> Vec<String> {
    text.split(['.', ';', '!', '\n'])
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Lowercase words, digits kept, punctuation dropped, `k` suffixes kept
/// with their number (`50k`).
fn words(sentence: &str) -> Vec<String> {
    sentence
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.replace('\'', ""))
        .filter(|w| !w.is_empty())
        .collect()
}

fn has(ws: &[String], any: &[&str]) -> bool {
    ws.iter().any(|w| any.contains(&w.as_str()))
}

/// `a b` appears as consecutive words.
fn has_pair(ws: &[String], a: &str, b: &str) -> bool {
    ws.windows(2).any(|p| p[0] == a && p[1] == b)
}

const PAY: &[&str] = &[
    "pay",
    "paying",
    "pays",
    "payment",
    "payments",
    "spend",
    "spending",
    "buy",
    "buying",
    "purchase",
    "purchases",
    "purchasing",
    "charge",
    "charging",
];

const MESSAGE: &[&str] = &[
    "message",
    "messages",
    "messaging",
    "email",
    "emails",
    "emailing",
    "text",
    "texting",
    "contact",
    "contacting",
    "invite",
    "inviting",
    "invitations",
    "invites",
];

/// Verbs that write a resource named after them: "changing my calendar".
const WRITE: &[&str] = &[
    "write",
    "writing",
    "change",
    "changing",
    "edit",
    "editing",
    "touch",
    "touching",
    "update",
    "updating",
    "use",
    "using",
    "book",
    "booking",
    "manage",
    "managing",
    "modify",
    "modifying",
    "delete",
    "deleting",
];

const FILLER: &[&str] = &[
    "my", "the", "a", "an", "to", "on", "in", "of", "for", "me", "any", "anything", "things",
    "stuff", "yourself", "without", "asking", "ask", "first", "please", "you", "your", "about",
    "before", "and", "or", "it", "them", "all", "always", "never",
];

fn asks_first(ws: &[String]) -> bool {
    (has(ws, &["ask", "check", "confirm"]) && has(ws, &["before", "first"]))
        || has_pair(ws, "always", "ask")
        || has_pair(ws, "need", "approval")
        || has_pair(ws, "my", "approval")
}

fn delegates(ws: &[String]) -> bool {
    has_pair(ws, "you", "may")
        || has_pair(ws, "you", "can")
        || has_pair(ws, "feel", "free")
        || has_pair(ws, "go", "ahead")
        || has_pair(ws, "without", "asking")
        || has_pair(ws, "no", "need")
        || has_pair(ws, "dont", "ask")
        || has_pair(ws, "do", "not")
}

/// The resource a sentence's verb writes, if it names one.
fn resource_of(ws: &[String]) -> Option<String> {
    if has(ws, PAY) {
        return Some(PAYMENT.to_string());
    }
    let messages_people = has(ws, MESSAGE)
        || (has(ws, &["send", "sending"])
            && has(
                ws,
                &[
                    "people", "anyone", "anybody", "others", "someone", "friends",
                ],
            ));
    if messages_people {
        return Some(THIRD_PARTY_MESSAGE.to_string());
    }
    let at = ws.iter().position(|w| WRITE.contains(&w.as_str()))?;
    ws[at + 1..]
        .iter()
        .find(|w| !FILLER.contains(&w.as_str()) && !WRITE.contains(&w.as_str()))
        .map(|w| w.trim_end_matches('s').to_string())
        .filter(|w| !w.is_empty())
}

/// The number in a sentence, with a `k` suffix read as thousands.
fn number_in(ws: &[String]) -> Option<u64> {
    ws.iter().find_map(|w| {
        if let Some(n) = w.strip_suffix('k') {
            return n.parse::<u64>().ok().map(|n| n * 1_000);
        }
        w.parse::<u64>().ok()
    })
}

/// The topic after `after`, fillers dropped: "about restaurants" gives
/// `restaurants`.
fn topic_after(ws: &[String], after: &[&str]) -> Option<String> {
    let at = ws.iter().position(|w| after.contains(&w.as_str()))?;
    let topic: Vec<&str> = ws[at + 1..]
        .iter()
        .map(String::as_str)
        .filter(|w| !FILLER.contains(w) && !after.contains(w))
        .collect();
    (!topic.is_empty()).then(|| topic.join(" "))
}

fn compile_sentence(sentence: &str) -> Vec<JudgmentCheck> {
    let ws = words(sentence);
    let mut out = Vec::new();

    // Plan thresholds: a number with items or tokens.
    if let Some(n) = number_in(&ws) {
        if has(&ws, &["token", "tokens"]) {
            out.push(JudgmentCheck::PlanTokensAtMost { tokens: n });
            return out;
        }
        if has(&ws, &["item", "items", "step", "steps", "task", "tasks"]) {
            out.push(JudgmentCheck::PlanItemsAtMost {
                items: u32::try_from(n).unwrap_or(u32::MAX),
            });
            return out;
        }
    }

    // Deciding alone, and always asking, about a topic.
    let decides_alone = has(&ws, &["judgment", "judgement"])
        || ((has(&ws, &["decide", "choose", "pick"]) && has(&ws, &["yourself", "may", "can"]))
            && !asks_first(&ws));
    if decides_alone {
        if let Some(topic) = topic_after(&ws, &["on", "about", "for", "decide", "choose", "pick"]) {
            out.push(JudgmentCheck::DecideOn { topic });
            return out;
        }
    }
    let never_decides = has_pair(&ws, "never", "decide") || has_pair(&ws, "never", "choose");
    if never_decides {
        if let Some(topic) = topic_after(&ws, &["decide", "choose"]) {
            out.push(JudgmentCheck::ReserveOn { topic });
            return out;
        }
    }

    // Resources: consent, or delegation.
    if let Some(resource) = resource_of(&ws) {
        if asks_first(&ws) && !has_pair(&ws, "without", "asking") {
            out.push(JudgmentCheck::ConsentFor { resource });
        } else if delegates(&ws) {
            out.push(JudgmentCheck::Delegate { resource });
        }
        return out;
    }

    if has_pair(&ws, "always", "ask") {
        if let Some(topic) = topic_after(&ws, &["about", "before", "on"]) {
            out.push(JudgmentCheck::ReserveOn { topic });
        }
    }
    out
}

/// The active grants folded over the baseline: what the controller
/// evaluates at acceptance (6.1) and when it routes a question (7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgment {
    /// The approval triggers after every grant.
    pub approval: ApprovalPolicy,
    /// `(topic, policy id)`: questions decided by policy.
    pub decide_on: Vec<(String, String)>,
    /// `(topic, policy id)`: questions always asked.
    pub reserve_on: Vec<(String, String)>,
    /// `(resource, policy id)`: resources the user delegated.
    pub delegated: Vec<(String, String)>,
    /// Every grant that contributed, oldest first.
    pub sources: Vec<String>,
}

impl Judgment {
    /// No grants: the baseline alone.
    pub fn new(base: ApprovalPolicy) -> Judgment {
        Judgment {
            approval: base,
            decide_on: Vec::new(),
            reserve_on: Vec::new(),
            delegated: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// Fold `grants` (oldest first; revoked ones are skipped) over `base`.
    pub fn from_grants(base: ApprovalPolicy, grants: &[JudgmentRow]) -> Judgment {
        let mut j = Judgment::new(base);
        for grant in grants.iter().filter(|g| g.revoked_at.is_none()) {
            for check in &grant.checks {
                j.apply(check, &grant.id);
            }
            if !grant.checks.is_empty() {
                j.sources.push(grant.id.clone());
                j.approval.id = Some(grant.id.clone());
            }
        }
        j
    }

    fn apply(&mut self, check: &JudgmentCheck, policy: &str) {
        let tag = |s: &str| (s.to_string(), policy.to_string());
        match check {
            JudgmentCheck::PlanItemsAtMost { items } => self.approval.max_items = Some(*items),
            JudgmentCheck::PlanTokensAtMost { tokens } => {
                self.approval.max_total_tokens = Some(*tokens)
            }
            JudgmentCheck::ConsentFor { resource } => {
                self.delegated.retain(|(r, _)| r != resource);
                self.approval.consent_resources.insert(resource.clone());
            }
            JudgmentCheck::Delegate { resource } => {
                let covered: BTreeSet<String> = self
                    .approval
                    .consent_resources
                    .iter()
                    .filter(|r| {
                        *r == resource
                            || r.starts_with(&format!("{resource}:"))
                            || resource.starts_with(&format!("{r}:"))
                    })
                    .cloned()
                    .collect();
                for r in covered {
                    self.approval.consent_resources.remove(&r);
                }
                self.delegated.push(tag(resource));
            }
            JudgmentCheck::DecideOn { topic } => {
                self.reserve_on.retain(|(t, _)| t != topic);
                self.decide_on.push(tag(topic));
            }
            JudgmentCheck::ReserveOn { topic } => {
                self.decide_on.retain(|(t, _)| t != topic);
                self.reserve_on.push(tag(topic));
            }
        }
    }

    /// The grant that reserves a question's topic for the user.
    pub fn reserves(&self, text: &str) -> Option<&str> {
        self.reserve_on
            .iter()
            .rev()
            .find(|(t, _)| mentions(text, t))
            .map(|(_, p)| p.as_str())
    }

    /// The topic and grant that let policy decide a question.
    pub fn decides(&self, text: &str) -> Option<(&str, &str)> {
        self.decide_on
            .iter()
            .rev()
            .find(|(t, _)| mentions(text, t))
            .map(|(t, p)| (t.as_str(), p.as_str()))
    }

    /// The grant that delegated a resource a consent question names.
    pub fn delegates_in(&self, text: &str) -> Option<(&str, &str)> {
        self.delegated
            .iter()
            .rev()
            .find(|(r, _)| {
                let word = r.rsplit(':').next().unwrap_or(r).replace('_', " ");
                mentions(text, &word)
            })
            .map(|(r, p)| (r.as_str(), p.as_str()))
    }

    /// Every rule in force, one line each, for `judgment list` and the plan
    /// preview's "the policy that required approval".
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(n) = self.approval.max_items {
            out.push(format!("plans of more than {n} items need approval"));
        }
        if let Some(n) = self.approval.max_total_tokens {
            out.push(format!("plans budgeted over {n} tokens need approval"));
        }
        for r in &self.approval.consent_resources {
            out.push(format!("anything that writes {r} needs approval"));
        }
        for (r, _) in &self.delegated {
            out.push(format!("writing {r} is delegated"));
        }
        for (t, _) in &self.decide_on {
            out.push(format!("questions about {t} are decided by policy"));
        }
        for (t, _) in &self.reserve_on {
            out.push(format!("questions about {t} always go to you"));
        }
        out
    }
}

/// Whether `text` mentions every word of `topic`, each by its stem (a
/// trailing `s` dropped), so "restaurants" matches "Which restaurant?".
pub(crate) fn mentions(text: &str, topic: &str) -> bool {
    let hay: Vec<String> = words(text)
        .into_iter()
        .map(|w| w.trim_end_matches('s').to_string())
        .collect();
    let needles: Vec<String> = words(topic)
        .into_iter()
        .map(|w| w.trim_end_matches('s').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    !needles.is_empty() && needles.iter().all(|n| hay.iter().any(|h| h == n))
}
