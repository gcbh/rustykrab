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
//! - the agent presses pay ([`PaymentRequestStore::mark_used`]),
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
    /// vault until used or expired.
    Authorized,
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

#[derive(Clone)]
pub struct PaymentRequestStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
    vault: CardVault,
    notifier: Option<Arc<dyn RequestNotifier>>,
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
        }
    }

    /// Attach whatever resumes the conversation once a payment is approved.
    pub fn with_notifier(mut self, notifier: Arc<dyn RequestNotifier>) -> Self {
        self.notifier = Some(notifier);
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
    pub async fn authorized_for(
        &self,
        conversation_id: Uuid,
        origin: &str,
    ) -> Result<AuthorizedPayment, Error> {
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

    /// The agent pressed pay. The approval is spent and the card erased,
    /// whatever the checkout then does: a second press after an uncertain
    /// outcome could charge twice, and the user can approve again.
    pub async fn mark_used(&self, id: &str) -> Result<(), Error> {
        self.vault.remove(id);
        let row_id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests SET status = 'used', used_at = ?2
                  WHERE id = ?1 AND status = 'authorized'",
                params![row_id, now_ms()],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn expire(&self, id: &str) -> Result<(), Error> {
        self.vault.remove(id);
        let row_id = id.to_string();
        crate::with_conn(&self.conn, move |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'expired', decided_at = COALESCE(decided_at, ?2),
                        link_token_hash = NULL, link_expires_at = NULL
                  WHERE id = ?1 AND status IN ('pending', 'authorized')",
                params![row_id, now_ms()],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    /// Mark unanswered requests past [`PENDING_TTL_MS`] expired.
    pub async fn sweep_expired(&self) -> Result<usize, Error> {
        crate::with_conn(&self.conn, |conn| {
            conn.execute(
                "UPDATE payment_requests
                    SET status = 'expired', decided_at = ?2,
                        link_token_hash = NULL, link_expires_at = NULL
                  WHERE status = 'pending' AND created_at <= ?1",
                params![now_ms() - PENDING_TTL_MS, now_ms()],
            )
            .map_err(storage)
        })
        .await
    }
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

        payments.mark_used(&id).await.unwrap();

        assert!(matches!(
            payments.authorized_for(conv, ORIGIN).await.unwrap(),
            AuthorizedPayment::None
        ));
        assert!(store.card_vault.get(&id).is_none());
        assert_eq!(payments.get(&id).await.unwrap().status, PaymentStatus::Used);
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
