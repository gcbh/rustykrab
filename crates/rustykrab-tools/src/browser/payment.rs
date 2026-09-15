//! Entering an approved card and pressing pay, without the model holding
//! either the card or the final say on the amount.
//!
//! The browser half of the payment path (the approval itself is
//! `rustykrab_store::PaymentRequestStore`). Three rules live here:
//!
//! 1. **Where a card may go.** Into a card field whose top-level page is the
//!    approved merchant origin, in a frame that is either that origin or one
//!    of a short fixed list of payment providers — and every frame between
//!    must be one of those too, so a provider frame nested inside a
//!    third-party ad frame does not qualify. The field must look like the
//!    card field the agent named: a number goes into a number box, never
//!    into a "notes" box that happens to have a ref.
//! 2. **What the model gets back.** Constant status strings. No value, no
//!    length, nothing derived from the card.
//! 3. **When pay may be pressed.** Only after the page's own total, read at
//!    the moment of pressing, is within the approved amount in the approved
//!    currency. No total found is a refusal, not a pass.

use regex::Regex;
use rustykrab_store::{CardDetails, Money};
use std::sync::OnceLock;
use zeroize::Zeroizing;

/// Frame origins that may receive card fields on a merchant's behalf.
///
/// Fixed in code on purpose: adding a provider is a reviewed change, not
/// something a page, a config file or the model can extend. These are the
/// hosted-field iframes of widely used processors (Stripe Elements,
/// Braintree Hosted Fields, Adyen Secured Fields, Square Web Payments,
/// Cybersource Microform, Spreedly iFrame). A checkout whose card fields
/// live anywhere else is refused, and the agent tells the user to pay.
pub(crate) const PAYMENT_FRAME_ORIGINS: &[&str] = &[
    "https://js.stripe.com",
    "https://assets.braintreegateway.com",
    "https://checkoutshopper-live.adyen.com",
    "https://checkoutshopper-live-us.adyen.com",
    "https://checkoutshopper-live-au.adyen.com",
    "https://checkoutshopper-live-apse.adyen.com",
    "https://checkoutshopper-live-in.adyen.com",
    "https://web.squarecdn.com",
    "https://flex.cybersource.com",
    "https://core.spreedly.com",
];

/// Which part of the card a field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaymentField {
    Number,
    /// Month and year in one box.
    Expiry,
    ExpMonth,
    ExpYear,
    Cvc,
    Name,
    PostalCode,
}

impl PaymentField {
    pub(crate) const ALL: &'static [&'static str] = &[
        "number",
        "expiry",
        "exp_month",
        "exp_year",
        "cvc",
        "name",
        "postal_code",
    ];

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "number" => Self::Number,
            "expiry" => Self::Expiry,
            "exp_month" => Self::ExpMonth,
            "exp_year" => Self::ExpYear,
            "cvc" => Self::Cvc,
            "name" => Self::Name,
            "postal_code" => Self::PostalCode,
            _ => return None,
        })
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Expiry => "expiry",
            Self::ExpMonth => "exp_month",
            Self::ExpYear => "exp_year",
            Self::Cvc => "cvc",
            Self::Name => "name",
            Self::PostalCode => "postal_code",
        }
    }

    /// The JSON object handed to the page script for this field — and only
    /// this field. Filling the name must not put the security code into the
    /// page's JavaScript context, even as an unused argument.
    ///
    /// `None` when the card has no value for it (a postal code the user
    /// left blank).
    pub(crate) fn page_value(&self, card: &CardDetails) -> Option<Zeroizing<String>> {
        let month = format!("{:02}", card.exp_month());
        let year = card.exp_year().to_string();
        let value = match self {
            Self::Number => serde_json::json!({ "v": card.number() }),
            Self::Cvc => serde_json::json!({ "v": card.security_code() }),
            Self::Name => serde_json::json!({ "v": card.holder() }),
            Self::PostalCode => serde_json::json!({ "v": card.postal_code()? }),
            Self::Expiry => serde_json::json!({ "m": month, "y": year }),
            Self::ExpMonth => serde_json::json!({ "m": month }),
            Self::ExpYear => serde_json::json!({ "y": year }),
        };
        Some(Zeroizing::new(value.to_string()))
    }
}

/// Status strings [`FILL_PAYMENT_FIELD`] can return, and what each means for
/// the model. Anything else is treated as an interrupted assignment.
pub(crate) fn fill_refusal(status: &str) -> Option<&'static str> {
    Some(match status {
        "origin_mismatch" => {
            "that field's page is not the approved merchant site; take a fresh snapshot of the checkout"
        }
        "frame_not_allowed" => {
            "that card field is inside a frame that is neither the merchant nor a recognised payment provider; do not enter the card, tell the user this checkout needs them to pay directly"
        }
        "unsupported_field" => "that element is not an enabled text input or select",
        "role_mismatch" => {
            "that field does not look like the card field you named; check the snapshot label and pick the matching field"
        }
        "option_not_found" => "that select has no option matching the card's expiry",
        "detached" => "that element is no longer on the page; take a fresh snapshot",
        _ => return None,
    })
}

/// Checks a field against the approved merchant, then assigns the one value
/// it is owed.
///
/// Called as `(FILL_PAYMENT_FIELD)(el, merchantOrigin, providerOrigins,
/// role, value)` with `el` the element and `value` the object from
/// [`PaymentField::page_value`]. Returns a status string and nothing else.
///
/// `location.ancestorOrigins` is how a frame learns its ancestors' origins
/// without being able to script them: `view.top.location.origin` throws in a
/// cross-origin provider frame, which is exactly where card fields live.
pub(crate) const FILL_PAYMENT_FIELD: &str = r#"function(el, merchant, providers, role, value) {
    if (!el || !el.isConnected || !el.ownerDocument || !el.ownerDocument.defaultView) return 'detached';
    var view = el.ownerDocument.defaultView;
    var own, chain;
    try {
        own = view.location.origin;
        chain = Array.prototype.slice.call(view.location.ancestorOrigins || []);
    } catch (_) { return 'origin_mismatch'; }
    var top = chain.length ? chain[chain.length - 1] : own;
    if (top !== merchant) return 'origin_mismatch';
    var trusted = function(o) { return o === merchant || providers.indexOf(o) >= 0; };
    if (!trusted(own)) return 'frame_not_allowed';
    for (var i = 0; i < chain.length - 1; i++) { if (!trusted(chain[i])) return 'frame_not_allowed'; }

    var tag = el.tagName;
    if ((tag !== 'INPUT' && tag !== 'SELECT') || el.disabled || el.readOnly) return 'unsupported_field';
    var kind = (el.type || '').toLowerCase();
    if (tag === 'INPUT' && ['text', 'tel', 'number', 'password', 'email', 'search', ''].indexOf(kind) < 0) return 'unsupported_field';

    var labels = '';
    try {
        if (el.labels) labels = Array.prototype.map.call(el.labels, function(l) { return l.textContent || ''; }).join(' ');
    } catch (_) {}
    var tokens = (el.getAttribute('autocomplete') || '').toLowerCase().split(/\s+/);
    var has = function(t) { return tokens.indexOf(t) >= 0; };
    var hint = [el.id, el.name, el.getAttribute('aria-label'), el.getAttribute('placeholder'),
        el.getAttribute('data-elements-stable-field-name'), labels].join(' ').toLowerCase();
    var detected = '';
    if (has('cc-number')) detected = 'number';
    else if (has('cc-csc')) detected = 'cvc';
    else if (has('cc-exp')) detected = 'expiry';
    else if (has('cc-exp-month')) detected = 'exp_month';
    else if (has('cc-exp-year')) detected = 'exp_year';
    else if (has('cc-name')) detected = 'name';
    else if (has('postal-code')) detected = 'postal_code';
    else if (/card[\s_-]*(number|no\b|num)|cardnumber|\bcc[\s_-]*(number|num)\b/.test(hint)) detected = 'number';
    else if (/\bcvv2?\b|\bcvc2?\b|\bcsc\b|security[\s_-]*code|card[\s_-]*(verification|code)/.test(hint)) detected = 'cvc';
    else if (/\bmm\s*[\/-]\s*yy/.test(hint)) detected = 'expiry';
    else if (/month|\bmm\b/.test(hint) && !/year|\byy/.test(hint)) detected = 'exp_month';
    else if (/year|\byy(yy)?\b/.test(hint) && !/month|\bmm\b/.test(hint)) detected = 'exp_year';
    else if (/expir|\bexp\b|valid[\s_-]*(thru|through)/.test(hint)) detected = 'expiry';
    else if (/name[\s_-]*on[\s_-]*(the[\s_-]*)?card|card[\s_-]*holder|cardholder|holder[\s_-]*name/.test(hint)) detected = 'name';
    else if (/postal|\bzip\b|zip[\s_-]*code|postcode/.test(hint)) detected = 'postal_code';
    if (detected !== role) return 'role_mismatch';
    if (tag === 'SELECT' && role !== 'exp_month' && role !== 'exp_year') return 'unsupported_field';

    var max = el.maxLength > 0 ? el.maxLength : 0;
    var text = value.v;
    if (role === 'expiry') {
        var four = /yyyy/.test(hint) || max >= 7;
        text = max === 4 ? value.m + value.y.slice(2) : value.m + '/' + (four ? value.y : value.y.slice(2));
    } else if (role === 'exp_month') {
        text = value.m;
    } else if (role === 'exp_year') {
        text = (max === 2 || (/\byy\b/.test(hint) && !/yyyy/.test(hint))) ? value.y.slice(2) : value.y;
    }

    if (tag === 'SELECT') {
        var wanted = parseInt(role === 'exp_month' ? value.m : value.y, 10);
        var names = ['january', 'february', 'march', 'april', 'may', 'june', 'july',
            'august', 'september', 'october', 'november', 'december'];
        var index = -1;
        for (var j = 0; j < el.options.length && index < 0; j++) {
            var o = el.options[j];
            if (o.disabled) continue;
            var raw = String(o.value).trim(), label = (o.text || '').trim().toLowerCase();
            var ov = parseInt(raw, 10), on = parseInt(label, 10);
            var hit;
            if (role === 'exp_month') {
                hit = ov === wanted || on === wanted
                    || (isNaN(on) && label.length >= 3 && names[wanted - 1].indexOf(label.slice(0, 3)) === 0);
            } else {
                hit = ov === wanted || on === wanted
                    || (raw.length === 2 && ov === wanted % 100)
                    || (label.length === 2 && on === wanted % 100);
            }
            if (hit) index = j;
        }
        if (index < 0) return 'option_not_found';
        el.selectedIndex = index;
    } else {
        var setter = Object.getOwnPropertyDescriptor(view.HTMLInputElement.prototype, 'value').set;
        setter.call(el, text);
    }
    el.dispatchEvent(new view.Event('input', { bubbles: true }));
    el.dispatchEvent(new view.Event('change', { bubbles: true }));
    return 'filled';
}"#;

/// Reads what pressing a pay control would agree to: the control's own
/// label and the page's visible text, from a document on the merchant
/// origin. Called with `this` bound to the control.
///
/// The text comes back to Rust for [`check_total`] and goes no further —
/// nothing here is returned to the model.
pub(crate) const PAY_TARGET: &str = r#"function(merchant) {
    var el = this;
    if (!el.isConnected || !el.ownerDocument || !el.ownerDocument.defaultView) return { status: 'detached' };
    var view = el.ownerDocument.defaultView;
    try {
        if (view.location.origin !== merchant) return { status: 'origin_mismatch' };
        var chain = Array.prototype.slice.call(view.location.ancestorOrigins || []);
        for (var i = 0; i < chain.length; i++) { if (chain[i] !== merchant) return { status: 'origin_mismatch' }; }
    } catch (_) { return { status: 'origin_mismatch' }; }
    var control = (el.closest && el.closest('button, input, a, [role="button"]')) || el;
    if (control.disabled) return { status: 'disabled' };
    var label = [control.innerText, control.tagName === 'INPUT' ? control.value : '',
        control.getAttribute('aria-label')].filter(Boolean).join(' ');
    var doc = view.top.document;
    var text = (doc.body && doc.body.innerText) || '';
    return { status: 'ready', label: String(label).slice(0, 500), text: String(text).slice(0, 200000) };
}"#;

/// Whether a control would plausibly submit the checkout. Used only while a
/// card is entered, to route that press through `pay`. `this` is the
/// control.
pub(crate) const SUBMIT_LIKE: &str = r#"function() {
    var el = (this.closest && this.closest('button, input, a, [role="button"]')) || this;
    var tag = el.tagName, type = (el.getAttribute('type') || '').toLowerCase();
    if (tag === 'BUTTON' && (type === '' || type === 'submit') && el.form) return true;
    if (tag === 'INPUT' && (type === 'submit' || type === 'image')) return true;
    var label = [el.innerText, tag === 'INPUT' ? el.value : '', el.getAttribute('aria-label')]
        .filter(Boolean).join(' ').toLowerCase();
    return /\b(pay|purchase|place\s+(my\s+)?order|complete\s+(my\s+)?(order|purchase|booking|payment)|confirm\s+(and\s+pay|order|payment|purchase|booking)|buy(\s+now)?|book\s+now|submit\s+(order|payment))\b/.test(label);
}"#;

/// A JS literal for a list of origins.
pub(crate) fn origins_literal(origins: &[String]) -> String {
    serde_json::to_string(origins).unwrap_or_else(|_| "[]".into())
}

// ── totals ───────────────────────────────────────────────────────────

/// Why pay was not pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TotalRefusal {
    /// Nothing on the page reads as a total or a pay amount.
    NotFound,
    /// An amount is shown in a currency other than the approved one.
    OtherCurrency(String),
    /// The page asks for more than the user approved.
    AboveApproved(Money),
}

impl TotalRefusal {
    pub(crate) fn explain(&self, approved: &Money) -> String {
        match self {
            TotalRefusal::NotFound => format!(
                "Could not find the order total on the page to check against the approved {approved}. \
                 Do not pay. If the total is on a later step, go to that step and try pay again; \
                 otherwise tell the user."
            ),
            TotalRefusal::OtherCurrency(seen) => format!(
                "The page shows an amount in a different currency ({seen}) from the approved {approved}. \
                 Do not pay; tell the user."
            ),
            TotalRefusal::AboveApproved(found) => format!(
                "The page total {found} is above what the user approved ({approved}). Do not pay. \
                 If the new total is right, file a new payment_request for it."
            ),
        }
    }
}

fn money_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            (?P<pre>US\$|CA\$|C\$|A\$|AU\$|NZ\$|HK\$|S\$|R\$|[$€£¥₹₩]|\b[A-Z]{3}\b)?
            [\ \u{00A0}]?
            (?P<num>\d{1,3}(?:[,.\u{00A0}\u{202F}\ ]\d{3})+(?:[.,]\d{1,3})?|\d+(?:[.,]\d{1,3})?)
            (?:[\ \u{00A0}]?(?P<post>[€£¥₹₩]|\b[A-Z]{3}\b))?",
        )
        .expect("static regex")
    })
}

/// ISO codes read as a currency marker when written next to a number. A
/// list rather than "any three capitals", so `TAX 5` is not a currency.
const KNOWN_CODES: &[&str] = &[
    "AED", "AUD", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP", "HKD", "HUF", "IDR",
    "ILS", "INR", "ISK", "JPY", "KRW", "MXN", "MYR", "NOK", "NZD", "PHP", "PLN", "SAR", "SEK",
    "SGD", "THB", "TRY", "TWD", "USD", "ZAR",
];

/// Which currencies a marker could mean. `$` alone is any dollar.
fn marker_currencies(marker: &str) -> &'static [&'static str] {
    match marker {
        "US$" => &["USD"],
        "CA$" | "C$" => &["CAD"],
        "A$" | "AU$" => &["AUD"],
        "NZ$" => &["NZD"],
        "HK$" => &["HKD"],
        "S$" => &["SGD"],
        "R$" => &["BRL"],
        "$" => &["USD", "CAD", "AUD", "NZD", "HKD", "SGD", "MXN", "TWD"],
        "€" => &["EUR"],
        "£" => &["GBP"],
        "¥" => &["JPY", "CNY"],
        "₹" => &["INR"],
        "₩" => &["KRW"],
        code => KNOWN_CODES
            .iter()
            .find(|c| **c == code)
            .map(std::slice::from_ref)
            .unwrap_or(&[]),
    }
}

/// A number as written, in `exponent` minor-unit digits, or `None` when it
/// cannot be one in that currency.
///
/// With both `,` and `.` present the last is the decimal point. With one,
/// three digits after it is a thousands separator unless the currency has
/// three decimals; otherwise it is the decimal point.
fn minor_units(raw: &str, exponent: u32) -> Option<i64> {
    let compact: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{00A0}' | '\u{202F}'))
        .collect();
    let last_comma = compact.rfind(',');
    let last_dot = compact.rfind('.');
    let decimal_at = match (last_comma, last_dot) {
        (Some(c), Some(d)) => Some(c.max(d)),
        (Some(p), None) | (None, Some(p)) => {
            let sep = compact.as_bytes()[p] as char;
            let count = compact.matches(sep).count();
            let after = compact.len() - p - 1;
            if count > 1 || (after == 3 && exponent != 3) {
                None
            } else {
                Some(p)
            }
        }
        (None, None) => None,
    };
    let (whole, fraction) = match decimal_at {
        Some(p) => (&compact[..p], &compact[p + 1..]),
        None => (compact.as_str(), ""),
    };
    let whole: String = whole.chars().filter(char::is_ascii_digit).collect();
    if whole.is_empty() || fraction.len() > exponent as usize {
        return None;
    }
    let mut digits = whole;
    digits.push_str(fraction);
    digits.extend(std::iter::repeat_n('0', exponent as usize - fraction.len()));
    digits.parse().ok()
}

enum Amount {
    Approved(i64),
    Other(String),
}

/// Every amount in `text` that carries a currency marker. A bare number —
/// "2 passengers", "Sep 17" — is not an amount.
fn amounts(text: &str, approved: &Money) -> Vec<Amount> {
    let exponent = Money::exponent(approved.currency());
    money_token()
        .captures_iter(text)
        .filter_map(|caps| {
            let marker = caps
                .name("pre")
                .or_else(|| caps.name("post"))
                .map(|m| m.as_str())?;
            let currencies = marker_currencies(marker);
            if currencies.is_empty() {
                return None;
            }
            let num = caps.name("num")?.as_str();
            if !currencies.contains(&approved.currency()) {
                return Some(Amount::Other(format!("{marker}{num}")));
            }
            minor_units(num, exponent).map(Amount::Approved)
        })
        .collect()
}

fn total_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(total|amount\s+due|balance\s+due|due\s+today|you\s+pay|to\s+pay)\b")
            .expect("static regex")
    })
}

/// Decide whether pressing a pay control stays within the approval.
///
/// Considers amounts on the control itself ("Pay $46.00") and on lines of
/// the page that name a total — or the short line right after one, since
/// checkouts often render the label and the figure as separate blocks. The
/// largest of those must not exceed `approved`. Returns the amount checked.
///
/// Deliberately conservative. A page showing a struck-through original
/// total above the discounted one is refused; the agent can file a new
/// request at the higher figure, and a wrongly refused payment costs a tap
/// where a wrongly allowed one costs money.
pub(crate) fn check_total(
    label: &str,
    page_text: &str,
    approved: &Money,
) -> Result<Money, TotalRefusal> {
    let mut found = amounts(label, approved);
    let lines: Vec<&str> = page_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for (i, line) in lines.iter().enumerate() {
        if !total_line().is_match(line) {
            continue;
        }
        let here = amounts(line, approved);
        if here.is_empty() {
            if let Some(next) = lines.get(i + 1).filter(|n| n.chars().count() <= 40) {
                found.extend(amounts(next, approved));
            }
        } else {
            found.extend(here);
        }
    }

    let mut largest: Option<i64> = None;
    for amount in found {
        match amount {
            Amount::Other(seen) => return Err(TotalRefusal::OtherCurrency(seen)),
            Amount::Approved(minor) => largest = Some(largest.map_or(minor, |l| l.max(minor))),
        }
    }
    let largest = largest.ok_or(TotalRefusal::NotFound)?;
    let checked =
        Money::from_minor(largest, approved.currency()).map_err(|_| TotalRefusal::NotFound)?;
    if largest > approved.minor() {
        return Err(TotalRefusal::AboveApproved(checked));
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(amount: &str) -> Money {
        Money::parse(amount, "USD").unwrap()
    }

    #[test]
    fn a_total_within_the_approval_passes_and_reports_what_was_checked() {
        let page = "Your trip\n2 passengers\nSubtotal $42.00\nFees $4.00\nTotal $46.00";
        assert_eq!(
            check_total("Pay now", page, &usd("46.00")),
            Ok(usd("46.00"))
        );
        assert_eq!(check_total("Pay now", page, &usd("50")), Ok(usd("46.00")));
    }

    #[test]
    fn a_total_above_the_approval_is_refused() {
        let page = "Order total: $52.50";
        assert_eq!(
            check_total("Place order", page, &usd("46.00")),
            Err(TotalRefusal::AboveApproved(usd("52.50")))
        );
    }

    #[test]
    fn the_amount_on_the_button_counts() {
        assert_eq!(
            check_total("Pay US$60.00", "Checkout", &usd("46.00")),
            Err(TotalRefusal::AboveApproved(usd("60.00")))
        );
        assert_eq!(
            check_total("Pay USD 46.00", "Checkout", &usd("46.00")),
            Ok(usd("46.00"))
        );
    }

    #[test]
    fn a_label_and_its_figure_on_separate_lines_are_read_together() {
        let page = "Total\n$46.00\nCard number";
        assert_eq!(check_total("Pay", page, &usd("46")), Ok(usd("46.00")));
        let page = "Total\n$146.00";
        assert!(check_total("Pay", page, &usd("46")).is_err());
    }

    #[test]
    fn no_total_is_a_refusal_not_a_pass() {
        assert_eq!(
            check_total(
                "Continue",
                "Card number\nExpiry\n2 passengers, Sep 17",
                &usd("46")
            ),
            Err(TotalRefusal::NotFound)
        );
        assert_eq!(
            check_total("Pay", "Total: 46", &usd("46")),
            Err(TotalRefusal::NotFound),
            "a bare number is not an amount"
        );
    }

    #[test]
    fn another_currency_is_refused() {
        assert_eq!(
            check_total("Pay", "Total €46.00", &usd("46")),
            Err(TotalRefusal::OtherCurrency("€46.00".into()))
        );
        let eur = Money::parse("46", "EUR").unwrap();
        assert_eq!(check_total("Pay", "Total 46,00 €", &eur), Ok(eur.clone()));
        assert_eq!(
            check_total("Pay", "Gesamtsumme 1.046,00 EUR\nTotal 1.046,00 EUR", &eur),
            Err(TotalRefusal::AboveApproved(
                Money::parse("1046", "EUR").unwrap()
            ))
        );
    }

    #[test]
    fn separators_are_read_the_way_the_page_means_them() {
        assert_eq!(minor_units("1,234.56", 2), Some(123_456));
        assert_eq!(minor_units("1.234,56", 2), Some(123_456));
        assert_eq!(minor_units("1,234", 2), Some(123_400));
        assert_eq!(minor_units("46.5", 2), Some(4650));
        assert_eq!(minor_units("12 345,00", 2), Some(1_234_500));
        assert_eq!(minor_units("4,600", 0), Some(4600));
        assert_eq!(minor_units("1.234", 3), Some(1234));
        assert_eq!(
            minor_units("46.001", 2),
            Some(4_600_100),
            "three digits is thousands"
        );
        assert_eq!(minor_units("4.60", 0), None, "yen have no minor unit");
    }

    #[test]
    fn a_dollar_sign_is_any_dollar_and_a_known_code_is_exact() {
        let cad = Money::parse("46", "CAD").unwrap();
        assert_eq!(check_total("Pay $46.00", "", &cad), Ok(cad.clone()));
        assert!(matches!(
            check_total("Pay US$46.00", "", &cad),
            Err(TotalRefusal::OtherCurrency(_))
        ));
        assert_eq!(
            check_total("Pay", "TAX 5\nTotal $46.00", &usd("46")),
            Ok(usd("46.00")),
            "TAX is not a currency code"
        );
    }

    #[test]
    fn each_field_hands_the_page_only_its_own_value() {
        let card = CardDetails::new_at(
            "4242424242424242",
            "03/31",
            "987",
            "Ada Lovelace",
            None,
            2026,
            9,
        )
        .unwrap();
        let name = PaymentField::Name.page_value(&card).unwrap();
        assert!(name.contains("Ada Lovelace"));
        assert!(!name.contains("987") && !name.contains("4242"));
        let expiry = PaymentField::Expiry.page_value(&card).unwrap();
        assert_eq!(expiry.as_str(), r#"{"m":"03","y":"2031"}"#);
        assert!(
            PaymentField::PostalCode.page_value(&card).is_none(),
            "no postal code was given, so there is nothing to enter"
        );
        for raw in PaymentField::ALL {
            assert_eq!(PaymentField::parse(raw).unwrap().as_str(), *raw);
        }
    }
}
