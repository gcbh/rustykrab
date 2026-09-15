//! One purchase the agent may pay for, and the card that pays for it.
//!
//! A payment is not a credential. A password is a long-lived value the
//! agent types into one site whenever it signs in; a card handed over for a
//! purchase is consent to *one* charge, at *one* merchant, for *at most*
//! one amount. So it gets its own record rather than a row in
//! `credential_requests`, and the record carries the terms the user agreed
//! to — merchant, exact origin, amount, currency — not a field list.
//!
//! ## Where the card lives
//!
//! Not in the database, and not in the keychain. The card the user types on
//! the approval page goes into [`CardVault`], an in-memory map keyed by the
//! request it pays for, and is gone the moment any of these happen:
//!
//! - the agent claims the press ([`PaymentRequestStore::claim_for_pay`]),
//! - [`CARD_TTL`] passes,
//! - the conversation files a newer payment request (superseded),
//! - the daemon restarts.
//!
//! The row keeps only what an audit needs — the terms, who approved, when,
//! and the last four digits — so "what did the agent pay for" stays
//! answerable after the card is long gone.
//!
//! The approval (the row) and the card (the vault entry) are deliberately
//! separate. A card the user typed this minute is one source for the vault
//! slot; a saved card chosen from a wallet could be another, without the
//! approval record changing shape.
//!
//! ## Why pressing pay is a claim, not a read
//!
//! An audit of the concurrent path found a silent double charge. Tool calls
//! in one model turn run concurrently (`MAX_CONCURRENT_TOOL_CALLS`), and the
//! old `pay` read the card with [`PaymentRequestStore::authorized_for`],
//! which hands back a *clone*: two `pay` calls on the same approved request
//! both got a card, both pressed, and the second `mark_used` updated zero
//! rows and discarded the count. Nothing in the store stood between the user
//! and two charges — only the model's habit of calling `pay` once a turn.
//!
//! So the press is now *claimed* before it happens.
//! [`PaymentRequestStore::claim_for_pay`] moves the row
//! `authorized → paying` in one conditional `UPDATE` whose rows-affected
//! count must be exactly 1, and only the winner of that row is handed the
//! card — taken out of the vault, not copied. Three things ride on that one
//! statement:
//!
//! - **Single spend.** A second claim on the same request sees a row that is
//!   no longer `authorized`.
//! - **A global lock.** The statement also refuses if *any* row is `paying`.
//!   One press is in flight at a time across the whole daemon; a checkout is
//!   a serial act and two at once is far more likely a runaway loop than two
//!   genuine purchases.
//! - **A throttle.** It refuses while any row was marked `used` inside the
//!   cooldown ([`DEFAULT_PAY_COOLDOWN`], operator-tunable), so a model that
//!   retries across turns cannot spend approval after approval in seconds.
//!
//! `paying` is not a state anything may sit in forever: a daemon killed
//! mid-press would otherwise hold the global lock for good. A claim older
//! than [`STALE_CLAIM_MS`] is swept to `used` — never back to `authorized`,
//! because the press may well have reached the merchant and the card is the
//! user's, not ours to spend on a guess. The one route back to `authorized`
//! is [`PaymentRequestStore::release_claim`], for a press the browser can
//! prove never left the process (`failed_before_press`).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, Utc};
use rusqlite::params;
use rustykrab_core::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::credential_request::{hash_token, RequestNotifier};

/// How long an approved card stays usable.
///
/// Long enough to take a fresh snapshot, fill four fields and press pay;
/// short enough that an approval the agent never acts on does not leave a
/// card sitting in memory for the rest of the day.
pub const CARD_TTL: Duration = Duration::from_secs(15 * 60);

/// How long an unanswered payment request stays answerable.
///
/// A checkout session rarely outlives an hour, so an approval arriving
/// later than that would authorise a cart that no longer exists.
pub const PENDING_TTL_MS: i64 = 60 * 60 * 1000;

/// How long a claimed press may stand before it is assumed abandoned.
///
/// Generous next to a click — a slow checkout, a page that hangs on submit,
/// a browser restart — and short enough that a daemon killed mid-press does
/// not hold the global pay lock until someone notices. A claim this old is
/// swept to `used`, never back to `authorized`: whatever happened, it may
/// have been a charge.
pub const STALE_CLAIM_MS: i64 = 2 * 60 * 1000;

/// How long after one press the next claim is refused, across every
/// conversation.
///
/// Not a limit on what the user may buy — each purchase is separately
/// approved — but on how fast an agent can act on approvals it already
/// holds. Long enough that a retry loop is caught by a human before it can
/// run, short enough not to obstruct someone paying for two things in a row.
/// `Duration::ZERO` disables it.
pub const DEFAULT_PAY_COOLDOWN: Duration = Duration::from_secs(30);

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn storage(e: impl fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

// ── money ────────────────────────────────────────────────────────────

/// An amount in a currency's minor units: cents for USD, yen for JPY.
///
/// Integer minor units, never a float, so "is the page total above what
/// the user approved" is an exact comparison rather than a rounding bet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Money {
    minor: i64,
    currency: String,
}

impl Money {
    /// Digits after the decimal point for an ISO 4217 code.
    ///
    /// Two unless listed. The lists cover the currencies in current use
    /// whose exponent is not two; getting one wrong would misstate an
    /// amount by a factor of a hundred, so they are spelled out.
    pub fn exponent(currency: &str) -> u32 {
        match currency {
            "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF"
            | "UGX" | "UYI" | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
            "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
            _ => 2,
        }
    }

    /// A positive amount already in minor units.
    pub fn from_minor(minor: i64, currency: &str) -> Result<Self, Error> {
        let currency = normalize_currency(currency)?;
        if minor <= 0 {
            return Err(Error::Storage("a payment amount must be above zero".into()));
        }
        Ok(Self { minor, currency })
    }

    /// Parse a decimal amount as a person writes it: `46`, `46.5`,
    /// `1,234.50`. A currency symbol is not accepted here — the currency is
    /// its own argument, so there is exactly one place it can come from.
    pub fn parse(amount: &str, currency: &str) -> Result<Self, Error> {
        let currency = normalize_currency(currency)?;
        let exponent = Self::exponent(&currency) as usize;
        let cleaned: String = amount.trim().chars().filter(|c| *c != ',').collect();
        let invalid = || {
            Error::Storage(format!(
                "'{amount}' is not an amount in {currency} (expected e.g. 46.00)"
            ))
        };
        let (whole, fraction) = match cleaned.split_once('.') {
            Some((w, f)) => (w, f),
            None => (cleaned.as_str(), ""),
        };
        if whole.is_empty()
            || !whole.chars().all(|c| c.is_ascii_digit())
            || !fraction.chars().all(|c| c.is_ascii_digit())
            || fraction.len() > exponent
        {
            return Err(invalid());
        }
        let mut digits = format!("{whole}{fraction}");
        digits.extend(std::iter::repeat_n('0', exponent - fraction.len()));
        let minor: i64 = digits.parse().map_err(|_| invalid())?;
        Self::from_minor(minor, &currency)
    }

    pub fn minor(&self) -> i64 {
        self.minor
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }
}

impl fmt::Display for Money {
    /// `USD 46.00`. The code rather than a symbol: `$` alone does not say
    /// which dollar, and this string is what the user approves.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exponent = Self::exponent(&self.currency);
        if exponent == 0 {
            return write!(f, "{} {}", self.currency, self.minor);
        }
        let scale = 10i64.pow(exponent);
        write!(
            f,
            "{} {}.{:0width$}",
            self.currency,
            self.minor / scale,
            self.minor % scale,
            width = exponent as usize
        )
    }
}

fn normalize_currency(raw: &str) -> Result<String, Error> {
    let code = raw.trim().to_ascii_uppercase();
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(Error::Storage(format!(
            "'{raw}' is not an ISO 4217 currency code (expected e.g. USD)"
        )));
    }
    Ok(code)
}

// ── card ─────────────────────────────────────────────────────────────

/// Why a card typed on the approval page was not accepted.
///
/// Messages are written for the person holding the phone, and never echo
/// what they typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardError {
    Number,
    Expiry,
    Expired,
    SecurityCode,
    Holder,
    PostalCode,
}

impl CardError {
    pub fn message(&self) -> &'static str {
        match self {
            CardError::Number => "That card number doesn't look right.",
            CardError::Expiry => "Enter the expiry as MM/YY.",
            CardError::Expired => "That card has expired.",
            CardError::SecurityCode => "The security code should be 3 or 4 digits.",
            CardError::Holder => "Enter the name on the card.",
            CardError::PostalCode => "That postal code doesn't look right.",
        }
    }
}

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// A validated payment card, held only in memory.
///
/// Every sensitive part is `Zeroizing`, so the bytes are overwritten when
/// the last copy drops. `Debug` is hand-written: a card that reached a
/// `tracing` field or a panic message through a derived impl would put the
/// number in a log file, which is precisely the leak this type exists to
/// prevent.
#[derive(Clone)]
pub struct CardDetails {
    number: Zeroizing<String>,
    exp_month: u8,
    exp_year: u16,
    security_code: Zeroizing<String>,
    holder: Zeroizing<String>,
    postal_code: Option<Zeroizing<String>>,
}

impl fmt::Debug for CardDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CardDetails")
            .field("last4", &self.last4())
            .field("number", &"<redacted>")
            .field("security_code", &"<redacted>")
            .field("holder", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl CardDetails {
    /// Validate what the user typed, against today's date.
    pub fn new(
        number: &str,
        expiry: &str,
        security_code: &str,
        holder: &str,
        postal_code: Option<&str>,
    ) -> Result<Self, CardError> {
        let today = Utc::now();
        Self::new_at(
            number,
            expiry,
            security_code,
            holder,
            postal_code,
            today.year(),
            today.month(),
        )
    }

    /// [`Self::new`] with the current month supplied, so expiry is testable.
    pub fn new_at(
        number: &str,
        expiry: &str,
        security_code: &str,
        holder: &str,
        postal_code: Option<&str>,
        this_year: i32,
        this_month: u32,
    ) -> Result<Self, CardError> {
        // Spaces and dashes are how card numbers are printed and how phone
        // autofill often inserts them; anything else is a typo.
        let digits: Zeroizing<String> =
            Zeroizing::new(number.chars().filter(|c| !matches!(c, ' ' | '-')).collect());
        if !(12..=19).contains(&digits.len())
            || !digits.chars().all(|c| c.is_ascii_digit())
            || !luhn_valid(&digits)
        {
            return Err(CardError::Number);
        }

        let (exp_month, exp_year) = parse_expiry(expiry).ok_or(CardError::Expiry)?;
        if (i32::from(exp_year), exp_month as u32) < (this_year, this_month) {
            return Err(CardError::Expired);
        }

        let code = security_code.trim();
        if !(3..=4).contains(&code.len()) || !code.chars().all(|c| c.is_ascii_digit()) {
            return Err(CardError::SecurityCode);
        }

        let holder = holder.trim();
        if holder.is_empty() || holder.chars().count() > 100 {
            return Err(CardError::Holder);
        }

        let postal_code = match postal_code.map(str::trim).filter(|p| !p.is_empty()) {
            None => None,
            Some(p)
                if p.len() <= 12
                    && p.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-') =>
            {
                Some(Zeroizing::new(p.to_string()))
            }
            Some(_) => return Err(CardError::PostalCode),
        };

        Ok(Self {
            number: digits,
            exp_month,
            exp_year,
            security_code: Zeroizing::new(code.to_string()),
            holder: Zeroizing::new(holder.to_string()),
            postal_code,
        })
    }

    /// Digits only, no separators.
    pub fn number(&self) -> &str {
        &self.number
    }

    /// 1–12.
    pub fn exp_month(&self) -> u8 {
        self.exp_month
    }

    /// Four digits.
    pub fn exp_year(&self) -> u16 {
        self.exp_year
    }

    pub fn security_code(&self) -> &str {
        &self.security_code
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    pub fn postal_code(&self) -> Option<&str> {
        self.postal_code.as_deref().map(String::as_str)
    }

    /// The last four digits: what a receipt shows, and all the audit row
    /// keeps.
    pub fn last4(&self) -> String {
        self.number[self.number.len() - 4..].to_string()
    }
}

fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    for (i, c) in digits.chars().rev().enumerate() {
        let mut d = c.to_digit(10).unwrap_or(0);
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum.is_multiple_of(10)
}

/// `MM/YY`, `MM / YY`, `MM/YYYY`, `MMYY` or `MM-YY`.
fn parse_expiry(raw: &str) -> Option<(u8, u16)> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let separators_ok = raw
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '/' | ' ' | '-'));
    if !separators_ok {
        return None;
    }
    let (month, year) = match digits.len() {
        4 => (&digits[..2], format!("20{}", &digits[2..])),
        6 => (&digits[..2], digits[2..].to_string()),
        // `3/28`: a single-digit month is common when typed by hand.
        3 => (&digits[..1], format!("20{}", &digits[1..])),
        5 => (&digits[..1], digits[1..].to_string()),
        _ => return None,
    };
    let month: u8 = month.parse().ok()?;
    let year: u16 = year.parse().ok()?;
    ((1..=12).contains(&month) && year >= 2000).then_some((month, year))
}

// ── vault ────────────────────────────────────────────────────────────

struct Held {
    card: CardDetails,
    expires: Instant,
}

/// Approved cards, keyed by the payment request each one may pay.
///
/// In memory on purpose, like `PendingLinks`: a card that does not survive
/// a restart is a purchase the user approves again, which is the right
/// outcome — the alternative is a card number on disk.
#[derive(Clone, Default)]
pub(crate) struct CardVault {
    inner: Arc<Mutex<HashMap<String, Held>>>,
}

impl CardVault {
    fn put(&self, request_id: &str, card: CardDetails, ttl: Duration) {
        self.lock().insert(
            request_id.to_string(),
            Held {
                card,
                expires: Instant::now() + ttl,
            },
        );
    }

    /// The card for a request, if it is still live. An expired entry is
    /// removed on the way past rather than by a timer.
    fn get(&self, request_id: &str) -> Option<CardDetails> {
        let mut inner = self.lock();
        match inner.get(request_id) {
            Some(held) if held.expires > Instant::now() => Some(held.card.clone()),
            Some(_) => {
                inner.remove(request_id);
                None
            }
            None => None,
        }
    }

    /// The card for a request, removed on the way out, with the deadline it
    /// was holding.
    ///
    /// The taking is the point: a claimed press is the one press, so the
    /// winner of the row gets the only copy and nothing is left for a
    /// concurrent caller to find. The deadline comes back so a press that
    /// provably never happened can put the card back on its *original*
    /// clock ([`PaymentRequestStore::release_claim`]) — restoring a fresh
    /// [`CARD_TTL`] would let a loop of claim-and-release keep a card in
    /// memory indefinitely.
    fn take(&self, request_id: &str) -> Option<(CardDetails, Instant)> {
        match self.lock().remove(request_id) {
            Some(held) if held.expires > Instant::now() => Some((held.card, held.expires)),
            _ => None,
        }
    }

    fn put_until(&self, request_id: &str, card: CardDetails, expires: Instant) {
        self.lock()
            .insert(request_id.to_string(), Held { card, expires });
    }

    fn remove(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── requests ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentStatus {
    /// Filed; the user has not answered.
    Pending,
    /// The user supplied a card and approved the terms. The card is in the
    /// vault until claimed or expired.
    Authorized,
    /// A press is claimed and in flight. The card has left the vault, and
    /// no other request may be claimed while this one stands. Reached only
    /// through [`PaymentRequestStore::claim_for_pay`], and left only for
    /// `used` (pressed, or the claim went stale) or back to `authorized`
    /// through [`PaymentRequestStore::release_claim`], for a press that
    /// provably never reached the page.
    Paying,
    /// The agent pressed pay. Terminal, and the card is gone.
    Used,
    /// The user said no.
    Declined,
    Expired,
    /// The conversation filed a newer payment request.
    Superseded,
}

impl PaymentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaymentStatus::Pending => "pending",
            PaymentStatus::Authorized => "authorized",
            PaymentStatus::Paying => "paying",
            PaymentStatus::Used => "used",
            PaymentStatus::Declined => "declined",
            PaymentStatus::Expired => "expired",
            PaymentStatus::Superseded => "superseded",
        }
    }

    fn parse(raw: &str) -> Result<Self, Error> {
        Ok(match raw {
            "pending" => PaymentStatus::Pending,
            "authorized" => PaymentStatus::Authorized,
            "paying" => PaymentStatus::Paying,
            "used" => PaymentStatus::Used,
            "declined" => PaymentStatus::Declined,
            "expired" => PaymentStatus::Expired,
            "superseded" => PaymentStatus::Superseded,
            other => return Err(Error::Storage(format!("unknown payment status '{other}'"))),
        })
    }
}

/// What the agent asks the user to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTerms {
    /// The merchant as the user knows it, e.g. "Steamship Authority".
    pub merchant: String,
    /// The exact web origin the card may be entered on, e.g.
    /// `https://www.steamshipauthority.com`. Callers canonicalise it; the
    /// store only refuses things that are plainly not an origin.
    pub origin: String,
    /// The most the user agrees to pay.
    pub amount: Money,
    /// What is being bought, in the agent's words.
    pub description: Option<String>,
}

/// A payment request as recorded. Never carries the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRequest {
    pub id: String,
    pub conversation_id: Option<String>,
    pub merchant: String,
    pub origin: String,
    pub amount: Money,
    pub description: Option<String>,
    pub status: PaymentStatus,
    pub created_at: i64,
    /// Set once authorised.
    pub card_last4: Option<String>,
}

/// The outcome of asking "may this conversation pay on this site now?".
#[derive(Debug)]
pub enum AuthorizedPayment {
    /// Nothing approved and live for this conversation.
    None,
    /// An approval exists, but for a different site. Carried so the agent
    /// can be told where it is allowed to pay rather than just "no".
    OriginMismatch { approved_origin: String },
    Ready {
        request: Box<PaymentRequest>,
        card: CardDetails,
    },
}

// ── claiming a press ─────────────────────────────────────────────────

/// The card, out of the vault, on the clock it was already keeping.
///
/// Holding one is holding the only copy: [`CardVault::take`] removed it.
/// Dropping it erases the card (every field is `Zeroizing`) and the
/// approval is never usable again — which is the right outcome for a press
/// whose result is unknown. Handing it back to
/// [`PaymentRequestStore::release_claim`] is the only way to undo that, and
/// only a caller who can prove the press never happened may do it.
pub struct ClaimedCard {
    card: CardDetails,
    /// The vault deadline this card was under, so a release restores its
    /// remaining life rather than a fresh [`CARD_TTL`].
    expires: Instant,
}

impl fmt::Debug for ClaimedCard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimedCard")
            .field("card", &self.card)
            .finish_non_exhaustive()
    }
}

impl ClaimedCard {
    pub fn card(&self) -> &CardDetails {
        &self.card
    }
}

/// A won claim: the row is `paying`, and this is the card it may press with.
#[derive(Debug)]
pub struct PayClaim {
    pub request: Box<PaymentRequest>,
    pub card: ClaimedCard,
}

/// Why a press was not claimed.
///
/// Typed rather than a string, because the browser turns each kind into a
/// different `retry_safe` and the model is told a different thing to do
/// next. Every message is written for the model: it says what happened and
/// what its next move is, since "no" on its own is an invitation to press
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayRefusal {
    /// The row is not `authorized`. `Used` and `Paying` are the interesting
    /// cases — this approval is already spent or already claimed — and
    /// `Expired` is what a claim whose card had left the vault becomes.
    NotAuthorized { status: PaymentStatus },
    /// Some *other* request is mid-press. One checkout at a time.
    AnotherPaymentInFlight,
    /// Something was paid too recently.
    Cooldown { remaining: Duration },
}

impl PayRefusal {
    /// What to tell the model. Never suggests a way around the refusal.
    pub fn message(&self) -> String {
        match self {
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Used,
            } => "This purchase has already been paid for. Do not press pay again and do not \
                  press any other button on this checkout — a second press could charge the \
                  user twice. Take a snapshot to read what the page says, and tell the user \
                  what happened."
                .into(),
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Paying,
            } => "A press for this same purchase is already in flight. Do not press pay again \
                  and do not press any other button. Wait 5 seconds, then call pay once more; \
                  if it is refused again, take a snapshot and tell the user what the page shows."
                .into(),
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Expired,
            } => "The user's approval for this purchase has run out, so there is no card to \
                  pay with. Do not press pay or any other button. File a new payment_request \
                  with this checkout's url, merchant and total, tell the user in one sentence, \
                  and stop until they approve."
                .into(),
            PayRefusal::NotAuthorized { status } => format!(
                "This purchase is '{}', not approved, so it cannot be paid. Do not press pay \
                 or any other button on this checkout. Tell the user, and file a new \
                 payment_request only if they still want it.",
                status.as_str()
            ),
            PayRefusal::AnotherPaymentInFlight => {
                "Another purchase is being paid for right now, and only one payment may be in \
                 flight at a time. Do not press pay again and do not press any other button. \
                 Wait 5 seconds, then call pay once more; if it is refused again, stop and tell \
                 the user."
                    .into()
            }
            PayRefusal::Cooldown { remaining } => format!(
                "Something was paid too recently: payments are throttled, and this one is \
                 refused for another {} seconds. Do not press pay again and do not press any \
                 other button. Wait {} seconds, then call pay once more; if it is refused \
                 again, stop and tell the user.",
                remaining.as_secs() + 1,
                remaining.as_secs() + 1,
            ),
        }
    }

    /// Whether calling `pay` again could succeed without another approval.
    ///
    /// False for anything that needs the user: a spent, expired or declined
    /// approval is not coming back on a retry.
    pub fn retry_safe(&self) -> bool {
        match self {
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Paying,
            } => true,
            PayRefusal::NotAuthorized { .. } => false,
            PayRefusal::AnotherPaymentInFlight | PayRefusal::Cooldown { .. } => true,
        }
    }

    /// A short tag for logs and tool output.
    pub fn kind(&self) -> &'static str {
        match self {
            PayRefusal::NotAuthorized { .. } => "not_authorized",
            PayRefusal::AnotherPaymentInFlight => "another_payment_in_flight",
            PayRefusal::Cooldown { .. } => "cooldown",
        }
    }
}

impl fmt::Display for PayRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

#[derive(Clone)]
pub struct PaymentRequestStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
    vault: CardVault,
    notifier: Option<Arc<dyn RequestNotifier>>,
    /// How long after one press the next claim is refused. See
    /// [`DEFAULT_PAY_COOLDOWN`].
    pay_cooldown: Duration,
}

const COLUMNS: &str = "id, conversation_id, merchant, origin, amount_minor, currency, \
                       description, status, created_at, card_last4";

fn row_to_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<(PaymentRow, String)> {
    Ok((
        PaymentRow {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            merchant: row.get(2)?,
            origin: row.get(3)?,
            amount_minor: row.get(4)?,
            currency: row.get(5)?,
            description: row.get(6)?,
            created_at: row.get(8)?,
            card_last4: row.get(9)?,
        },
        row.get(7)?,
    ))
}

struct PaymentRow {
    id: String,
    conversation_id: Option<String>,
    merchant: String,
    origin: String,
    amount_minor: i64,
    currency: String,
    description: Option<String>,
    created_at: i64,
    card_last4: Option<String>,
}

impl PaymentRow {
    fn into_request(self, status: &str) -> Result<PaymentRequest, Error> {
        Ok(PaymentRequest {
            id: self.id,
            conversation_id: self.conversation_id,
            merchant: self.merchant,
            origin: self.origin,
            amount: Money::from_minor(self.amount_minor, &self.currency)?,
            description: self.description,
            status: PaymentStatus::parse(status)?,
            created_at: self.created_at,
            card_last4: self.card_last4,
        })
    }
}

/// Plainly an origin: scheme, host, optional port, and nothing after.
fn looks_like_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    rest.is_some_and(|host| {
        !host.is_empty()
            && !host
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@'))
    })
}

impl PaymentRequestStore {
    pub(crate) fn new(conn: Arc<Mutex<rusqlite::Connection>>, vault: CardVault) -> Self {
        Self {
            conn,
            vault,
            notifier: None,
            pay_cooldown: DEFAULT_PAY_COOLDOWN,
        }
    }

    /// Attach whatever resumes the conversation once a payment is approved.
    pub fn with_notifier(mut self, notifier: Arc<dyn RequestNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// How long after one press the next claim is refused.
    /// `Duration::ZERO` disables the throttle.
    pub fn with_pay_cooldown(mut self, cooldown: Duration) -> Self {
        self.pay_cooldown = cooldown;
        self
    }

    /// Ask the user to approve a purchase.
    ///
    /// A conversation has at most one purchase in flight: filing a new one
    /// supersedes any earlier request it left pending or authorised, and
    /// drops that request's card. An agent that re-files because the cart
    /// total changed must not leave the old approval usable.
    pub async fn file(
        &self,
        terms: PaymentTerms,
        conversation_id: Option<Uuid>,
    ) -> Result<String, Error> {
        let merchant = terms.merchant.trim().to_string();
        if merchant.is_empty() || merchant.chars().count() > 200 {
            return Err(Error::Storage(
                "a payment request needs the merchant's name".into(),
            ));
        }
        if !looks_like_origin(&terms.origin) {
            return Err(Error::Storage(format!(
                "'{}' is not a web origin (expected e.g. https://shop.example.com)",
                terms.origin
            )));
        }
        let description = terms
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());

        let id = Uuid::new_v4().to_string();
        let conversation = conversation_id.map(|c| c.to_string());
        let row_id = id.clone();
        let superseded = crate::with_conn(&self.conn, move |conn| {
            let mut superseded = Vec::new();
            if let Some(conv) = &conversation {
                let mut stmt = conn
                    .prepare(
                        "SELECT id FROM payment_requests
                          WHERE conversation_id = ?1
                            AND status IN ('pending', 'authorized')",
                    )
                    .map_err(storage)?;
                superseded = stmt
                    .query_map(params![conv], |row| row.get::<_, String>(0))
                    .map_err(storage)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(storage)?;
                conn.execute(
                    "UPDATE payment_requests
                        SET status = 'superseded', decided_at = ?2,
                            link_token_hash = NULL, link_expires_at = NULL
                      WHERE conversation_id = ?1
                        AND status IN ('pending', 'authorized')",
                    params![conv, now_ms()],
                )
                .map_err(storage)?;
            }
            conn.execute(
                "INSERT INTO payment_requests
                    (id, conversation_id, merchant, origin, amount_minor, currency,
                     description, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
                params![
                    row_id,
                    conversation,
                    merchant,
                    terms.origin,
                    terms.amount.minor(),
                    terms.amount.currency(),
                    description,
                    now_ms()
                ],
            )
            .map_err(storage)?;
            Ok(superseded)
        })
        .await?;
        for old in superseded {
            self.vault.remove(&old);
        }
        Ok(id)
    }

    /// Mint the one-time link that opens the approval page.
    ///
    /// Same shape as a credential link: the token is returned once, only
    /// its hash is stored, and re-issuing kills the previous link.
    pub async fn issue_link(&self, id: &str, ttl: Duration) -> Result<String, Error> {
        let token = {
            use rand::Rng;
            let mut raw = [0u8; 32];
            rand::rng().fill(&mut raw);
            hex::encode(raw)
        };
        let hash = hash_token(&token);
        let expires = Utc::now()
            .checked_add_signed(chrono::Duration::from_std(ttl).unwrap_or_default())
            .map(|t| t.timestamp_millis())
            .unwrap_or(i64::MAX);
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let n = conn
                .execute(
                    "UPDATE payment_requests
                        SET link_token_hash = ?1, link_expires_at = ?2
                      WHERE id = ?3 AND status = 'pending'",
                    params![hash, expires, id],
                )
                .map_err(storage)?;
            if n == 0 {
                return Err(Error::NotFound(format!("pending payment request {id}")));
            }
            Ok(())
        })
        .await?;
        Ok(token)
    }

    /// The pending request a link token opens, or `None` for a token that
    /// is unknown, expired, already answered or superseded — deliberately
    /// indistinguishable, as with credential links.
    pub async fn find_by_link(&self, token: &str) -> Result<Option<PaymentRequest>, Error> {
        self.sweep_expired().await?;
        let hash = hash_token(token);
        let now = now_ms();
        crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM payment_requests
                      WHERE link_token_hash = ?1
                        AND status = 'pending'
                        AND link_expires_at > ?2"
                ))
                .map_err(storage)?;
            let mut rows = stmt
                .query_map(params![hash, now], row_to_request)
                .map_err(storage)?;
            match rows.next() {
                Some(row) => {
                    let (row, status) = row.map_err(storage)?;
                    Ok(Some(row.into_request(&status)?))
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Record a request by id.
    pub async fn get(&self, id: &str) -> Result<PaymentRequest, Error> {
        let id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            let (row, status) = conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM payment_requests WHERE id = ?1"),
                    params![id],
                    row_to_request,
                )
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        Error::NotFound(format!("payment request '{id}'"))
                    }
                    other => storage(other),
                })?;
            row.into_request(&status)
        })
        .await
    }

    /// The user approved the terms and supplied a card.
    ///
    /// The card goes to the vault, the row records who approved and the
    /// last four digits, and the link dies. Only then is the conversation
    /// woken: a wake that raced ahead of the vault write would resume a
    /// turn that finds nothing to pay with.
    pub async fn authorize(
        &self,
        id: &str,
        card: CardDetails,
        decided_by: &str,
    ) -> Result<(), Error> {
        self.authorize_for(id, card, decided_by, CARD_TTL).await
    }

    async fn authorize_for(
        &self,
        id: &str,
        card: CardDetails,
        decided_by: &str,
        ttl: Duration,
    ) -> Result<(), Error> {
        let last4 = card.last4();
        self.vault.put(id, card, ttl);

        let row_id = id.to_string();
        let by = decided_by.to_string();
        let updated = crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'authorized', decided_at = ?2, decided_by = ?3,
                        card_last4 = ?4, link_token_hash = NULL, link_expires_at = NULL
                  WHERE id = ?1 AND status = 'pending' AND created_at > ?5",
                params![row_id, now_ms(), by, last4, now_ms() - PENDING_TTL_MS],
            )
            .map_err(storage)
        })
        .await;

        match updated {
            Ok(1) => {}
            Ok(_) => {
                self.vault.remove(id);
                return Err(Error::AlreadyExists(format!(
                    "payment request {id} is no longer waiting for approval"
                )));
            }
            Err(e) => {
                self.vault.remove(id);
                return Err(e);
            }
        }

        if let Some(notifier) = &self.notifier {
            match self.get(id).await {
                Ok(request) => {
                    notifier.payment_authorized(request.conversation_id.as_deref(), &request)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "approved payment could not be reloaded to wake its turn")
                }
            }
        }
        Ok(())
    }

    /// The user refused. Nothing to wipe but the link.
    pub async fn decline(&self, id: &str, decided_by: &str) -> Result<(), Error> {
        let row_id = id.to_string();
        let by = decided_by.to_string();
        let n = crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'declined', decided_at = ?2, decided_by = ?3,
                        link_token_hash = NULL, link_expires_at = NULL
                  WHERE id = ?1 AND status = 'pending'",
                params![row_id, now_ms(), by],
            )
            .map_err(storage)
        })
        .await?;
        if n == 0 {
            return Err(Error::AlreadyExists(format!(
                "payment request {id} is no longer waiting for approval"
            )));
        }
        self.vault.remove(id);
        Ok(())
    }

    /// Whether `conversation_id` may pay on `origin` right now, and with
    /// what.
    ///
    /// An authorised row whose card is no longer in the vault — a restart,
    /// or [`CARD_TTL`] passing — is marked expired here, so the answer and
    /// the record agree.
    ///
    /// Only `authorized` rows are considered, so a request whose press is
    /// already claimed (`paying`) or spent (`used`) answers "nothing here"
    /// — which is what stops `fill_payment` typing a card into a checkout
    /// that is already being paid for.
    pub async fn authorized_for(
        &self,
        conversation_id: Uuid,
        origin: &str,
    ) -> Result<AuthorizedPayment, Error> {
        // A claim abandoned by a crashed daemon would otherwise leave its
        // row `paying` for good, hiding an approval that is in fact spent.
        self.sweep_stale_claims().await?;
        let conv = conversation_id.to_string();
        let rows = crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM payment_requests
                      WHERE conversation_id = ?1 AND status = 'authorized'
                      ORDER BY created_at DESC"
                ))
                .map_err(storage)?;
            let rows = stmt
                .query_map(params![conv], row_to_request)
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            rows.into_iter()
                .map(|(row, status)| row.into_request(&status))
                .collect::<Result<Vec<_>, _>>()
        })
        .await?;

        let mut mismatch = None;
        for request in rows {
            let Some(card) = self.vault.get(&request.id) else {
                self.expire(&request.id).await?;
                continue;
            };
            if request.origin == origin {
                return Ok(AuthorizedPayment::Ready {
                    request: Box::new(request),
                    card,
                });
            }
            mismatch.get_or_insert(request.origin);
        }
        Ok(match mismatch {
            Some(approved_origin) => AuthorizedPayment::OriginMismatch { approved_origin },
            None => AuthorizedPayment::None,
        })
    }

    /// Claim the one press this approval allows.
    ///
    /// The whole single-spend guarantee is the rows-affected count of one
    /// `UPDATE`. It moves `authorized → paying` only if this row is still
    /// `authorized`, no *other* row is mid-press, and nothing was paid
    /// inside the cooldown — so two concurrent callers cannot both leave
    /// with a card, and the loser is told which of the three it lost to.
    /// See the module docs for the double charge that put it here.
    ///
    /// The card is *taken* out of the vault, not copied: whoever wins the
    /// row gets the only copy, and the loser gets neither row nor card.
    /// A won row with no card left (the [`CARD_TTL`] ran out, or the daemon
    /// restarted since the approval) is marked `expired` rather than handed
    /// back — the approval is unusable, and leaving it `paying` would hold
    /// the global lock for a press that can never happen.
    ///
    /// `Err` is a database failure. A refusal is an ordinary answer.
    pub async fn claim_for_pay(&self, id: &str) -> Result<Result<PayClaim, PayRefusal>, Error> {
        // Before judging "another payment is in flight", make sure the
        // in-flight one is real and not a crash from ten minutes ago.
        self.sweep_stale_claims().await?;

        let row_id = id.to_string();
        let now = now_ms();
        let cooldown_ms = i64::try_from(self.pay_cooldown.as_millis()).unwrap_or(i64::MAX);
        let claimed = crate::with_conn(&self.conn, move |conn| {
            // One statement, one connection, one lock: the diagnosis below
            // runs against the same state the UPDATE just saw.
            let rows = conn
                .execute(
                    "UPDATE payment_requests
                        SET status = 'paying', claimed_at = ?2
                      WHERE id = ?1 AND status = 'authorized'
                        AND NOT EXISTS (SELECT 1 FROM payment_requests
                                         WHERE status = 'paying' AND id <> ?1)
                        AND NOT EXISTS (SELECT 1 FROM payment_requests
                                         WHERE status = 'used' AND used_at > ?2 - ?3)",
                    params![row_id, now, cooldown_ms],
                )
                .map_err(storage)?;
            match rows {
                1 => {
                    let (row, status) = conn
                        .query_row(
                            &format!("SELECT {COLUMNS} FROM payment_requests WHERE id = ?1"),
                            params![row_id],
                            row_to_request,
                        )
                        .map_err(storage)?;
                    Ok(Ok(row.into_request(&status)?))
                }
                0 => Ok(Err(diagnose_refusal(conn, &row_id, now, cooldown_ms)?)),
                other => Err(Error::Storage(format!(
                    "claiming payment request {row_id} touched {other} rows; id is the \
                     primary key, so the schema is not the one this code was written for"
                ))),
            }
        })
        .await?;

        let request = match claimed {
            Ok(request) => request,
            Err(refusal) => return Ok(Err(refusal)),
        };
        match self.vault.take(id) {
            Some((card, expires)) => Ok(Ok(PayClaim {
                request: Box::new(request),
                card: ClaimedCard { card, expires },
            })),
            None => {
                tracing::info!(
                    request = %id,
                    "pay claim won a row whose card had already left the vault; expiring it"
                );
                self.expire(id).await?;
                Ok(Err(PayRefusal::NotAuthorized {
                    status: PaymentStatus::Expired,
                }))
            }
        }
    }

    /// The claimed press happened. The approval is spent whatever the
    /// checkout then does: a second press after an uncertain outcome could
    /// charge twice, and the user can approve again.
    ///
    /// Only a `paying` row may be marked used, so this cannot quietly spend
    /// an approval nobody claimed — the zero-rows case the audit found is
    /// now an error rather than a shrug.
    pub async fn mark_used(&self, id: &str) -> Result<(), Error> {
        let row_id = id.to_string();
        let updated = crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests SET status = 'used', used_at = ?2
                  WHERE id = ?1 AND status = 'paying'",
                params![row_id, now_ms()],
            )
            .map_err(storage)
        })
        .await?;
        if updated == 0 {
            // Deliberately before touching the vault: a refused mark_used
            // must leave the approval exactly as it found it, card
            // included, or a stray call would quietly disarm a live one.
            return Err(Error::AlreadyExists(format!(
                "payment request {id} is not mid-press, so it cannot be recorded as paid \
                 (claim it with claim_for_pay first)"
            )));
        }
        // The claim already took the card; this is belt and braces for a
        // caller that reached `paying` another way.
        self.vault.remove(id);
        Ok(())
    }

    /// Give back a claim for a press that provably never reached the page.
    ///
    /// The only route from `paying` back to `authorized`, and it exists for
    /// exactly one case: the browser knows the click failed before any
    /// input left the process (`actions::failed_before_press`). Anything
    /// less certain must stay spent.
    ///
    /// The card goes back on the clock it was already keeping, not a fresh
    /// [`CARD_TTL`] — otherwise a claim/release loop would keep a card in
    /// memory long past the fifteen minutes the user agreed to.
    pub async fn release_claim(&self, id: &str, card: ClaimedCard) -> Result<(), Error> {
        let row_id = id.to_string();
        let released = crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests SET status = 'authorized', claimed_at = NULL
                  WHERE id = ?1 AND status = 'paying'",
                params![row_id],
            )
            .map_err(storage)
        })
        .await?;
        if released == 0 {
            // `card` drops here, and with it the only copy: a row that is
            // not `paying` must not get its card back.
            return Err(Error::AlreadyExists(format!(
                "payment request {id} is not mid-press, so there is no claim to release"
            )));
        }
        self.vault.put_until(id, card.card, card.expires);
        Ok(())
    }

    /// Retire claims older than [`STALE_CLAIM_MS`].
    ///
    /// To `used`, never back to `authorized`. A claim this old means the
    /// daemon died between the claim and the press, and from here there is
    /// no way to know whether the merchant was charged — so the approval is
    /// treated as spent and the user asked again if they still want it. The
    /// alternative, releasing it, risks the second charge this whole
    /// mechanism exists to prevent.
    ///
    /// `used_at` is set to the claim time rather than now: that is when the
    /// press, if there was one, happened, and it keeps a lock stuck for an
    /// hour from imposing a fresh cooldown the moment it is cleared.
    async fn sweep_stale_claims(&self) -> Result<usize, Error> {
        let cutoff = now_ms() - STALE_CLAIM_MS;
        let stale = crate::with_conn(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM payment_requests
                      WHERE status = 'paying' AND COALESCE(claimed_at, 0) <= ?1",
                )
                .map_err(storage)?;
            let ids: Vec<String> = stmt
                .query_map(params![cutoff], |row| row.get::<_, String>(0))
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            drop(stmt);
            if !ids.is_empty() {
                conn.execute(
                    "UPDATE payment_requests
                        SET status = 'used', used_at = COALESCE(used_at, claimed_at, ?2)
                      WHERE status = 'paying' AND COALESCE(claimed_at, 0) <= ?1",
                    params![cutoff, now_ms()],
                )
                .map_err(storage)?;
            }
            Ok(ids)
        })
        .await?;
        for id in &stale {
            self.vault.remove(id);
        }
        if !stale.is_empty() {
            tracing::warn!(
                count = stale.len(),
                "payment claims were abandoned mid-press and are recorded as spent; \
                 the daemon may have stopped between claiming and pressing"
            );
        }
        Ok(stale.len())
    }

    async fn expire(&self, id: &str) -> Result<(), Error> {
        self.vault.remove(id);
        let row_id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'expired', decided_at = COALESCE(decided_at, ?2),
                        claimed_at = NULL,
                        link_token_hash = NULL, link_expires_at = NULL
                  WHERE id = ?1 AND status IN ('pending', 'authorized', 'paying')",
                params![row_id, now_ms()],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// Mark unanswered requests past [`PENDING_TTL_MS`] expired, and retire
    /// claims past [`STALE_CLAIM_MS`]. Returns how many rows moved.
    pub async fn sweep_expired(&self) -> Result<usize, Error> {
        let stale = self.sweep_stale_claims().await?;
        let expired = crate::with_conn(&self.conn, |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'expired', decided_at = ?2,
                        link_token_hash = NULL, link_expires_at = NULL
                  WHERE status = 'pending' AND created_at <= ?1",
                params![now_ms() - PENDING_TTL_MS, now_ms()],
            )
            .map_err(storage)
        })
        .await?;
        Ok(stale + expired)
    }
}

/// Why a claim that touched no rows was refused, read from the same
/// connection the `UPDATE` ran on.
///
/// The order matters: the row's own status is the most specific answer, and
/// only once it is `authorized` — so this request itself is fine — is the
/// refusal about something else on the machine.
fn diagnose_refusal(
    conn: &rusqlite::Connection,
    id: &str,
    now: i64,
    cooldown_ms: i64,
) -> Result<PayRefusal, Error> {
    let status: String = conn
        .query_row(
            "SELECT status FROM payment_requests WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                Error::NotFound(format!("payment request '{id}'"))
            }
            other => storage(other),
        })?;
    let status = PaymentStatus::parse(&status)?;
    if status != PaymentStatus::Authorized {
        return Ok(PayRefusal::NotAuthorized { status });
    }

    let in_flight: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM payment_requests
                            WHERE status = 'paying' AND id <> ?1)",
            params![id],
            |row| row.get(0),
        )
        .map_err(storage)?;
    if in_flight {
        return Ok(PayRefusal::AnotherPaymentInFlight);
    }

    let last_used: Option<i64> = conn
        .query_row(
            "SELECT MAX(used_at) FROM payment_requests WHERE status = 'used'",
            [],
            |row| row.get(0),
        )
        .map_err(storage)?;
    if let Some(used_at) = last_used.filter(|used_at| *used_at > now - cooldown_ms) {
        let remaining = (used_at + cooldown_ms - now).max(0);
        return Ok(PayRefusal::Cooldown {
            remaining: Duration::from_millis(remaining as u64),
        });
    }

    // The row is authorised and nothing blocks it now, so the claim lost a
    // race to something that has since resolved — another claim that was
    // released, or a sweep. Retryable, and "wait, then try once" is the
    // right instruction for it.
    Ok(PayRefusal::AnotherPaymentInFlight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    /// A public test number (Visa, passes Luhn). Never a real card.
    const TEST_CARD: &str = "4242 4242 4242 4242";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), vec![7u8; 32])
            .expect("open")
            .with_credential_backend(Arc::new(crate::credential_backend::MemoryBackend::new()));
        (dir, store)
    }

    fn card() -> CardDetails {
        CardDetails::new_at(
            TEST_CARD,
            "12/31",
            "123",
            "Ada Lovelace",
            Some("02554"),
            2026,
            9,
        )
        .expect("valid test card")
    }

    fn terms(origin: &str) -> PaymentTerms {
        PaymentTerms {
            merchant: "Steamship Authority".into(),
            origin: origin.into(),
            amount: Money::parse("46.00", "usd").unwrap(),
            description: Some("2 passengers, Hyannis to Nantucket".into()),
        }
    }

    const ORIGIN: &str = "https://www.steamshipauthority.com";

    #[test]
    fn money_parses_what_people_write_and_refuses_the_rest() {
        assert_eq!(Money::parse("46", "USD").unwrap().minor(), 4600);
        assert_eq!(Money::parse("46.5", "usd").unwrap().minor(), 4650);
        assert_eq!(Money::parse("1,234.50", "USD").unwrap().minor(), 123_450);
        assert_eq!(Money::parse("4600", "JPY").unwrap().minor(), 4600);
        assert_eq!(Money::parse("1.234", "KWD").unwrap().minor(), 1234);
        for bad in ["", "0", "0.00", "-5", "46.005", "$46", "46.00 USD", "4 6"] {
            assert!(
                Money::parse(bad, "USD").is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(
            Money::parse("46.5", "JPY").is_err(),
            "yen have no minor unit"
        );
        assert!(Money::parse("46", "dollars").is_err());
        assert_eq!(Money::parse("46", "USD").unwrap().to_string(), "USD 46.00");
        assert_eq!(Money::parse("4600", "JPY").unwrap().to_string(), "JPY 4600");
        assert_eq!(Money::from_minor(5, "USD").unwrap().to_string(), "USD 0.05");
    }

    #[test]
    fn a_card_is_validated_before_it_is_held() {
        assert!(card().number() == "4242424242424242");
        assert_eq!(card().last4(), "4242");
        assert_eq!((card().exp_month(), card().exp_year()), (12, 2031));

        let at = |n: &str, e: &str, c: &str, h: &str| {
            CardDetails::new_at(n, e, c, h, None, 2026, 9).map(|_| ())
        };
        assert_eq!(
            at("4242 4242 4242 4241", "12/31", "123", "A"),
            Err(CardError::Number)
        );
        assert_eq!(
            at("4242abcd42424242", "12/31", "123", "A"),
            Err(CardError::Number)
        );
        assert_eq!(at(TEST_CARD, "13/31", "123", "A"), Err(CardError::Expiry));
        assert_eq!(at(TEST_CARD, "08/26", "123", "A"), Err(CardError::Expired));
        assert_eq!(
            at(TEST_CARD, "09/26", "123", "A"),
            Ok(()),
            "valid through its month"
        );
        assert_eq!(
            at(TEST_CARD, "12/31", "12", "A"),
            Err(CardError::SecurityCode)
        );
        assert_eq!(at(TEST_CARD, "12/31", "123", "  "), Err(CardError::Holder));
        for expiry in ["1231", "12 / 31", "12/2031", "12-31"] {
            assert_eq!(at(TEST_CARD, expiry, "1234", "A"), Ok(()), "{expiry}");
        }
    }

    #[test]
    fn debug_output_never_contains_the_card() {
        let rendered = format!("{:?}", card());
        assert!(!rendered.contains("4242424242424242"), "{rendered}");
        assert!(!rendered.contains("123\""), "{rendered}");
        assert!(!rendered.contains("Lovelace"), "{rendered}");
        assert!(rendered.contains("4242"), "last four are fine to show");
    }

    #[tokio::test]
    async fn an_approved_payment_is_usable_only_on_its_origin_in_its_conversation() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();

        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));

        payments
            .authorize(&id, card(), "me@example.com")
            .await
            .unwrap();

        match payments.authorized_for(conv, ORIGIN).await.unwrap() {
            AuthorizedPayment::Ready { request, card } => {
                assert_eq!(request.id, id);
                assert_eq!(request.status, PaymentStatus::Authorized);
                assert_eq!(request.card_last4.as_deref(), Some("4242"));
                assert_eq!(card.security_code(), "123");
            }
            other => panic!("expected a ready payment, got {other:?}"),
        }
        match payments
            .authorized_for(conv, "https://steamshipauthority.com.evil.example")
            .await
            .unwrap()
        {
            AuthorizedPayment::OriginMismatch { approved_origin } => {
                assert_eq!(approved_origin, ORIGIN)
            }
            other => panic!("expected an origin mismatch, got {other:?}"),
        }
        assert!(matches!(
            payments
                .authorized_for(Uuid::new_v4(), ORIGIN)
                .await
                .unwrap(),
            AuthorizedPayment::None
        ));
    }

    #[tokio::test]
    async fn pressing_pay_spends_the_approval_and_erases_the_card() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments.authorize(&id, card(), "me").await.unwrap();

        let claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        assert_eq!(claim.request.id, id);
        assert_eq!(claim.card.card().security_code(), "123");
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Paying,
            "the row is the lock while the press is in flight"
        );
        assert!(
            store.card_vault.get(&id).is_none(),
            "the claim takes the card rather than copying it"
        );

        payments.mark_used(&id).await.unwrap();

        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
        assert!(store.card_vault.get(&id).is_none());
        assert_eq!(payments.get(&id).await.unwrap().status, PaymentStatus::Used);
    }

    /// The defect this whole mechanism exists for: two `pay` tool calls in
    /// one model turn, running concurrently, both getting a card and both
    /// pressing. Exactly one may leave with a card.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_claims_on_one_approval_produce_exactly_one_press() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments.authorize(&id, card(), "me").await.unwrap();

        let (left, right) = {
            let (a, b) = (payments.clone(), payments.clone());
            let (id_a, id_b) = (id.clone(), id.clone());
            tokio::join!(
                tokio::spawn(async move { a.claim_for_pay(&id_a).await.unwrap() }),
                tokio::spawn(async move { b.claim_for_pay(&id_b).await.unwrap() }),
            )
        };
        let outcomes = [left.unwrap(), right.unwrap()];
        let won: Vec<_> = outcomes.iter().filter(|o| o.is_ok()).collect();
        assert_eq!(won.len(), 1, "exactly one claim may win: {outcomes:?}");
        assert_eq!(
            won[0].as_ref().map(|c| c.card.card().last4()).unwrap(),
            "4242"
        );

        let refusal = outcomes
            .iter()
            .find_map(|o| o.as_ref().err())
            .expect("one refusal");
        assert!(
            matches!(
                refusal,
                PayRefusal::AnotherPaymentInFlight
                    | PayRefusal::NotAuthorized {
                        status: PaymentStatus::Paying
                    }
            ),
            "the loser must be told why: {refusal:?}"
        );
        assert!(
            store.card_vault.get(&id).is_none(),
            "the loser must not leave a card behind either"
        );
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Paying
        );
    }

    #[tokio::test]
    async fn only_a_claimed_press_can_be_recorded_as_paid() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let id = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        payments.authorize(&id, card(), "me").await.unwrap();

        assert!(
            payments.mark_used(&id).await.is_err(),
            "an approval nobody claimed was never pressed"
        );
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Authorized,
            "and the refused mark_used must not have moved it"
        );

        let _claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        payments.mark_used(&id).await.unwrap();
        assert!(
            payments.mark_used(&id).await.is_err(),
            "the second press is the double charge; it must not pass silently"
        );
    }

    /// Two approvals, two conversations. The throttle is global: it is
    /// about how fast the agent is spending, not about one purchase.
    #[tokio::test]
    async fn a_second_purchase_waits_out_the_cooldown() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let first = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        let second = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        payments.authorize(&first, card(), "me").await.unwrap();
        payments.authorize(&second, card(), "me").await.unwrap();

        let _claim = payments
            .claim_for_pay(&first)
            .await
            .unwrap()
            .expect("claimed");
        payments.mark_used(&first).await.unwrap();

        match payments.claim_for_pay(&second).await.unwrap() {
            Err(PayRefusal::Cooldown { remaining }) => {
                assert!(
                    remaining <= DEFAULT_PAY_COOLDOWN && remaining > Duration::from_secs(25),
                    "{remaining:?}"
                );
            }
            other => panic!("expected a cooldown refusal, got {other:?}"),
        }
        assert_eq!(
            payments.get(&second).await.unwrap().status,
            PaymentStatus::Authorized,
            "a refused claim leaves the approval alone"
        );

        let unthrottled = payments.clone().with_pay_cooldown(Duration::ZERO);
        assert!(
            unthrottled.claim_for_pay(&second).await.unwrap().is_ok(),
            "zero disables the throttle"
        );
    }

    /// A daemon killed between claiming and pressing must not hold the
    /// global lock for the rest of the day.
    #[tokio::test]
    async fn an_abandoned_claim_is_recorded_as_spent_and_stops_blocking() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let abandoned = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        let next = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        payments.authorize(&abandoned, card(), "me").await.unwrap();
        payments.authorize(&next, card(), "me").await.unwrap();
        let claim = payments
            .claim_for_pay(&abandoned)
            .await
            .unwrap()
            .expect("claimed");

        assert!(
            matches!(
                payments.claim_for_pay(&next).await.unwrap(),
                Err(PayRefusal::AnotherPaymentInFlight)
            ),
            "one press at a time while the claim is fresh"
        );

        // Three minutes ago, past STALE_CLAIM_MS.
        let stale_id = abandoned.clone();
        crate::with_conn(&store.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests SET claimed_at = ?1 WHERE id = ?2",
                params![now_ms() - 3 * 60 * 1000, stale_id],
            )
            .map_err(storage)
        })
        .await
        .unwrap();

        assert_eq!(payments.sweep_expired().await.unwrap(), 1);
        assert_eq!(
            payments.get(&abandoned).await.unwrap().status,
            PaymentStatus::Used,
            "never back to authorized: the press may have reached the merchant"
        );
        assert!(store.card_vault.get(&abandoned).is_none());
        assert!(
            payments
                .release_claim(&abandoned, claim.card)
                .await
                .is_err(),
            "the swept claim is no longer releasable"
        );
        assert!(
            payments.claim_for_pay(&next).await.unwrap().is_ok(),
            "and the lock is free again"
        );
    }

    #[tokio::test]
    async fn a_press_that_never_reached_the_page_gives_the_approval_back() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments
            .authorize_for(&id, card(), "me", Duration::from_secs(120))
            .await
            .unwrap();

        let claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        let deadline = claim.card.expires;
        payments.release_claim(&id, claim.card).await.unwrap();

        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Authorized
        );
        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::Ready { .. }
        ));
        let (_, restored) = store.card_vault.take(&id).expect("card is back");
        assert_eq!(
            restored, deadline,
            "the card keeps its own clock; a release must not extend the TTL"
        );
        store
            .card_vault
            .put_until(&id, card(), restored.max(Instant::now()));

        let claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        payments.mark_used(&id).await.unwrap();
        assert!(
            payments.release_claim(&id, claim.card).await.is_err(),
            "a spent approval cannot be released back to authorized"
        );
        assert!(store.card_vault.get(&id).is_none());
    }

    /// A card whose TTL ran out between approval and press. The row must
    /// not sit in `paying` holding the global lock for a press that can
    /// never happen.
    #[tokio::test]
    async fn a_claim_with_no_card_left_expires_the_approval_instead_of_holding_the_lock() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let id = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        payments
            .authorize_for(&id, card(), "me", Duration::from_millis(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        match payments.claim_for_pay(&id).await.unwrap() {
            Err(refusal @ PayRefusal::NotAuthorized { status }) => {
                assert_eq!(status, PaymentStatus::Expired);
                assert!(!refusal.retry_safe());
            }
            other => panic!("expected an expired refusal, got {other:?}"),
        }
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Expired
        );

        let other = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        payments.authorize(&other, card(), "me").await.unwrap();
        assert!(
            payments.claim_for_pay(&other).await.unwrap().is_ok(),
            "nothing is left holding the lock"
        );
    }

    /// `fill_payment` asks this question before typing anything into a
    /// checkout, so "claimed or spent" must read as "nothing to fill here".
    /// Once a press is claimed the card has left the vault; a lookup that
    /// still answered `Ready` would be offering a card that is gone.
    #[tokio::test]
    async fn a_claimed_or_spent_request_offers_nothing_to_fill() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments.authorize(&id, card(), "me").await.unwrap();

        let claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Paying
        );
        assert!(
            matches!(
                payments.authorized_for(conv, ORIGIN).await.unwrap(),
                AuthorizedPayment::None
            ),
            "a press in flight is not something to fill a card for"
        );

        drop(claim);
        payments.mark_used(&id).await.unwrap();
        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
    }

    /// Every refusal has to tell the model what to do next; "no" alone is
    /// an invitation to press something else on the checkout.
    #[test]
    fn every_refusal_says_what_to_do_next() {
        for refusal in [
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Used,
            },
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Paying,
            },
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Expired,
            },
            PayRefusal::NotAuthorized {
                status: PaymentStatus::Declined,
            },
            PayRefusal::AnotherPaymentInFlight,
            PayRefusal::Cooldown {
                remaining: Duration::from_secs(12),
            },
        ] {
            let message = refusal.message();
            assert!(
                message.contains("other button"),
                "{:?} must forbid pressing something else: {message}",
                refusal
            );
            assert!(
                message.contains("Wait")
                    || message.contains("Tell the user")
                    || message.contains("tell the user"),
                "{:?} must say what comes next: {message}",
                refusal
            );
            assert!(!refusal.kind().is_empty());
        }
        assert!(PayRefusal::AnotherPaymentInFlight.retry_safe());
        assert!(!PayRefusal::NotAuthorized {
            status: PaymentStatus::Used
        }
        .retry_safe());
    }

    #[tokio::test]
    async fn a_card_past_its_ttl_is_gone_and_the_record_says_so() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments
            .authorize_for(&id, card(), "me", Duration::from_millis(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Expired
        );
    }

    /// A restart empties the vault. The row must not keep claiming an
    /// approval nothing can act on.
    #[tokio::test]
    async fn a_restart_forgets_the_card_and_expires_the_approval() {
        let (dir, store) = store();
        let conv = Uuid::new_v4();
        let id = {
            let payments = store.payment_requests();
            let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
            payments.authorize(&id, card(), "me").await.unwrap();
            id
        };
        drop(store);

        let reopened = Store::open(dir.path(), vec![7u8; 32])
            .unwrap()
            .with_credential_backend(Arc::new(crate::credential_backend::MemoryBackend::new()));
        let payments = reopened.payment_requests();
        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Expired
        );
    }

    #[tokio::test]
    async fn a_new_request_supersedes_the_old_one_and_drops_its_card() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let first = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments.authorize(&first, card(), "me").await.unwrap();

        let second = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();

        assert_eq!(
            payments.get(&first).await.unwrap().status,
            PaymentStatus::Superseded
        );
        assert!(store.card_vault.get(&first).is_none());
        assert_eq!(
            payments.get(&second).await.unwrap().status,
            PaymentStatus::Pending
        );
        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
    }

    #[tokio::test]
    async fn a_link_opens_its_request_once() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let id = payments
            .file(terms(ORIGIN), Some(Uuid::new_v4()))
            .await
            .unwrap();
        let token = payments
            .issue_link(&id, Duration::from_secs(900))
            .await
            .unwrap();

        let found = payments.find_by_link(&token).await.unwrap().expect("opens");
        assert_eq!(found.id, id);
        assert_eq!(found.amount.to_string(), "USD 46.00");
        assert!(payments
            .find_by_link(&"0".repeat(64))
            .await
            .unwrap()
            .is_none());

        payments.authorize(&id, card(), "me").await.unwrap();
        assert!(
            payments.find_by_link(&token).await.unwrap().is_none(),
            "an answered link must be dead"
        );
        assert!(
            payments.authorize(&id, card(), "me").await.is_err(),
            "an approval cannot be given twice"
        );
    }

    #[tokio::test]
    async fn an_expired_link_or_a_stale_request_cannot_be_approved() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let id = payments.file(terms(ORIGIN), None).await.unwrap();
        let token = payments.issue_link(&id, Duration::ZERO).await.unwrap();
        assert!(payments.find_by_link(&token).await.unwrap().is_none());

        let stale = payments.file(terms(ORIGIN), None).await.unwrap();
        let stale_id = stale.clone();
        crate::with_conn(&store.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests SET created_at = ?1 WHERE id = ?2",
                params![now_ms() - PENDING_TTL_MS - 1, stale_id],
            )
            .map_err(storage)
        })
        .await
        .unwrap();
        assert!(payments.authorize(&stale, card(), "me").await.is_err());
        assert!(
            store.card_vault.get(&stale).is_none(),
            "a refused approval must not leave its card behind"
        );
    }

    #[tokio::test]
    async fn declining_burns_the_link_and_holds_nothing() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        let id = payments.file(terms(ORIGIN), None).await.unwrap();
        let token = payments
            .issue_link(&id, Duration::from_secs(900))
            .await
            .unwrap();
        payments.decline(&id, "me").await.unwrap();
        assert!(payments.find_by_link(&token).await.unwrap().is_none());
        assert_eq!(
            payments.get(&id).await.unwrap().status,
            PaymentStatus::Declined
        );
        assert!(payments.authorize(&id, card(), "me").await.is_err());
    }

    #[tokio::test]
    async fn terms_that_are_not_terms_are_refused() {
        let (_dir, store) = store();
        let payments = store.payment_requests();
        for origin in [
            "steamshipauthority.com",
            "https://shop.example.com/checkout",
            "https://",
            "https://user@shop.example.com",
        ] {
            assert!(
                payments.file(terms(origin), None).await.is_err(),
                "{origin} is not an origin"
            );
        }
        let mut nameless = terms(ORIGIN);
        nameless.merchant = "  ".into();
        assert!(payments.file(nameless, None).await.is_err());
    }

    /// The card must not be in the database files however it got there.
    /// Reads the WAL too: a fresh write may not have reached `store.db`.
    #[tokio::test]
    async fn the_card_is_nowhere_on_disk() {
        let (dir, store) = store();
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        payments.authorize(&id, card(), "me").await.unwrap();
        // Through the whole press, not just the approval: claiming and
        // marking used both write the row, and neither may carry the card
        // along with them.
        let claim = payments.claim_for_pay(&id).await.unwrap().expect("claimed");
        payments.mark_used(&id).await.unwrap();
        drop(claim);

        let mut on_disk = String::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                on_disk.push_str(&String::from_utf8_lossy(&std::fs::read(path).unwrap()));
            }
        }
        assert!(
            on_disk.contains("Steamship Authority"),
            "nothing was persisted to assert against"
        );
        for leaked in ["4242424242424242", "Ada Lovelace", "02554"] {
            assert!(!on_disk.contains(leaked), "'{leaked}' was written to disk");
        }
    }

    #[derive(Debug, Default)]
    struct Recorder {
        woken: Mutex<Vec<(Option<String>, String)>>,
    }

    impl RequestNotifier for Recorder {
        fn request_filed(&self, _credential_name: &str, _action: &str) {}

        fn payment_authorized(&self, conversation_id: Option<&str>, request: &PaymentRequest) {
            self.woken.lock().unwrap().push((
                conversation_id.map(str::to_string),
                request.merchant.clone(),
            ));
        }
    }

    #[tokio::test]
    async fn approving_wakes_the_conversation_that_asked_and_nothing_else_does() {
        let recorder = Arc::new(Recorder::default());
        let (_dir, store) = store();
        let store = store.with_request_notifier(recorder.clone());
        let payments = store.payment_requests();
        let conv = Uuid::new_v4();

        let declined = payments.file(terms(ORIGIN), None).await.unwrap();
        payments.decline(&declined, "me").await.unwrap();
        let id = payments.file(terms(ORIGIN), Some(conv)).await.unwrap();
        let _ = payments.authorize(&declined, card(), "me").await;
        assert!(recorder.woken.lock().unwrap().is_empty());

        payments.authorize(&id, card(), "me").await.unwrap();
        let woken = recorder.woken.lock().unwrap();
        assert_eq!(
            *woken,
            vec![(Some(conv.to_string()), "Steamship Authority".to_string())]
        );
    }
}
