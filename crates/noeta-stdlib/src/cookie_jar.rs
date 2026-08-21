//! The client-side **cookie jar** (RFC 6265): what a configured `Client` remembers between
//! requests.
//!
//! [`crate::cookie::Cookie`] is the value a *server* builds and sends. This is the other half —
//! the storage that turns "the server set a session cookie on the login response" into "the next
//! request carries it". Without it, a program authenticating against any cookie-based API has to
//! scrape `Set-Cookie` by hand, re-parse the attributes, and decide for itself which requests the
//! cookie belongs on, which is exactly the decision this file exists to make once.
//!
//! Three rules do most of the work, and each is a rule about *not* sending something:
//!
//! - **Domain.** A cookie without a `Domain=` attribute is **host-only**: it goes back to the
//!   exact host that set it and nowhere else. One with `Domain=example.com` goes to that host and
//!   its subdomains — but only if the setting host is itself inside that domain, so
//!   `evil.test` cannot set a cookie for `bank.test`.
//! - **Path.** A cookie's path must be a prefix of the request path at a segment boundary, so a
//!   cookie scoped to `/admin` never rides a request to `/adminfoo`.
//! - **Secure.** A `Secure` cookie is withheld from a plain-`http` request entirely, which is what
//!   makes the flag mean anything.
//!
//! The jar deliberately does **not** consult `SameSite`. That attribute answers "is this request
//! coming from another site", and a programmatic client has no site — there is no document, no
//! origin initiating navigation, nothing for `Lax` to be lax about. It is parsed and kept so a
//! program can read it back, and it filters nothing.
//!
//! Nor is there a public-suffix list: `Domain=com` is rejected only by the "the setting host must
//! be inside the domain" rule, which stops the case that matters here (one host setting a cookie
//! for another) without shipping a list that goes stale.

use std::collections::BTreeMap;

use crate::cookie::{Cookie, SameSite};

/// A cookie in the jar, plus the bookkeeping RFC 6265 needs and the `Cookie` value itself does not
/// carry: where it came from, when it dies, and when it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCookie {
    /// The cookie as the server described it.
    pub cookie: Cookie,
    /// The domain this cookie matches against — the `Domain=` attribute if there was one,
    /// otherwise the host that set it. Always lowercase, never leading-dotted.
    pub domain: String,
    /// Whether the match is **exact**. True when the server sent no `Domain=`, which is the
    /// stricter and far more common case.
    pub host_only: bool,
    /// The path the cookie is scoped to — the `Path=` attribute, or the default path derived from
    /// the request that set it.
    pub path: String,
    /// Absolute expiry in unix milliseconds, or `None` for a **session** cookie: one with neither
    /// `Max-Age` nor `Expires`, which lives as long as the jar does.
    pub expires_ms: Option<u64>,
    /// When it was first stored. RFC 6265 §5.4 orders equal-length paths by this.
    pub created_ms: u64,
    /// Insertion order, the final tie-break. A deterministic clock makes equal `created_ms` the
    /// normal case rather than a rarity, so without this the send order would depend on a sort's
    /// stability rather than on a rule.
    seq: u64,
}

impl StoredCookie {
    /// Whether this cookie is dead at `now_ms`. A session cookie never is.
    pub fn expired_at(&self, now_ms: u64) -> bool {
        self.expires_ms.is_some_and(|at| at <= now_ms)
    }
}

/// A client's cookie storage. Shared by every client derived from the one that created it, which
/// is what makes `client.cookies().bearer(token)` keep the jar rather than start a new one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    entries: Vec<StoredCookie>,
    next_seq: u64,
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> CookieJar {
        CookieJar::default()
    }

    /// How many cookies are held, expired ones included (they are dropped as they are passed over,
    /// not on a timer).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the jar holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Take in every `Set-Cookie` on a response served from `url`.
    ///
    /// A header the jar refuses — an unparseable one, or a `Domain=` the setting host is not
    /// inside — is skipped rather than fatal. A response carrying one bad cookie and three good
    /// ones must still leave the three, and a request is not the place to fail over a header the
    /// program never asked about.
    pub fn store_response(&mut self, url: &str, headers: &[(String, String)], now_ms: u64) {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("set-cookie") {
                self.store(url, value, now_ms);
            }
        }
    }

    /// Take in one `Set-Cookie` header value received from `url`. Returns whether it was stored.
    ///
    /// A cookie that arrives already expired — `Max-Age=0`, or an `Expires` in the past, which is
    /// how every server deletes one — **removes** the matching entry instead of being stored. That
    /// is the same operation from the jar's side, and reporting it as "not stored" is accurate.
    pub fn store(&mut self, url: &str, set_cookie: &str, now_ms: u64) -> bool {
        let Some(parsed) = parse_set_cookie(set_cookie) else {
            return false;
        };
        let request_host = noeta_ext_abi::uri::host_of(url).to_ascii_lowercase();
        let (domain, host_only) = match &parsed.domain {
            Some(domain) => {
                // The rule that stops one host setting a cookie for another: the domain must
                // cover the host that is setting it.
                if !domain_matches(&request_host, domain) {
                    return false;
                }
                (domain.clone(), false)
            }
            None => (request_host.clone(), true),
        };
        if domain.is_empty() {
            return false;
        }
        let path = match &parsed.path {
            Some(path) if path.starts_with('/') => path.clone(),
            _ => default_path(noeta_ext_abi::uri::path_of(url)),
        };
        let expires_ms = parsed.expiry_ms(now_ms);

        // §5.3 step 11: a cookie replaces the one it shares a (name, domain, host-only, path) key
        // with, and inherits its creation time — so refreshing a session cookie does not shuffle
        // it to the back of the send order.
        let existing = self.entries.iter().position(|e| {
            e.cookie.name == parsed.cookie.name
                && e.domain == domain
                && e.host_only == host_only
                && e.path == path
        });
        if expires_ms.is_some_and(|at| at <= now_ms) {
            if let Some(index) = existing {
                self.entries.remove(index);
            }
            return false;
        }
        let (created_ms, seq) = match existing {
            Some(index) => {
                let previous = self.entries.remove(index);
                (previous.created_ms, previous.seq)
            }
            None => {
                self.next_seq += 1;
                (now_ms, self.next_seq)
            }
        };
        self.entries.push(StoredCookie {
            cookie: parsed.cookie,
            domain,
            host_only,
            path,
            expires_ms,
            created_ms,
            seq,
        });
        true
    }

    /// Seed the jar with a cookie the program built, as if `url` had set it. The door a client
    /// uses to start already authenticated — with a session id from a config file, say — instead
    /// of having to perform a login it already did.
    pub fn insert(&mut self, url: &str, cookie: Cookie, now_ms: u64) {
        self.store(url, &cookie.to_header(), now_ms);
    }

    /// The `Cookie:` request-header value for `url`, or `None` when nothing matches.
    ///
    /// Order is RFC 6265 §5.4: longer paths first, then oldest first. Servers are not supposed to
    /// depend on it, and enough of them do that sending an arbitrary order is a bug waiting to be
    /// blamed on something else.
    pub fn header_for(&self, url: &str, now_ms: u64) -> Option<String> {
        let matched = self.matching(url, now_ms);
        if matched.is_empty() {
            return None;
        }
        Some(
            matched
                .iter()
                .map(|e| format!("{}={}", e.cookie.name, e.cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Every live cookie that would be sent to `url`, in send order.
    pub fn matching(&self, url: &str, now_ms: u64) -> Vec<&StoredCookie> {
        let host = noeta_ext_abi::uri::host_of(url).to_ascii_lowercase();
        let path = noeta_ext_abi::uri::path_of(url);
        let secure = noeta_ext_abi::uri::is_secure(url);
        let mut matched: Vec<&StoredCookie> = self
            .entries
            .iter()
            .filter(|entry| !entry.expired_at(now_ms))
            .filter(|entry| match entry.host_only {
                true => entry.domain == host,
                false => domain_matches(&host, &entry.domain),
            })
            .filter(|entry| path_matches(path, &entry.path))
            .filter(|entry| secure || !entry.cookie.secure)
            .collect();
        matched.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created_ms.cmp(&b.created_ms))
                .then(a.seq.cmp(&b.seq))
        });
        matched
    }

    /// Every live cookie the jar holds, in a stable order that does not depend on a request:
    /// domain, then path, then name. What a program reads to see what it has picked up.
    pub fn snapshot(&self, now_ms: u64) -> Vec<StoredCookie> {
        let mut live: Vec<StoredCookie> = self
            .entries
            .iter()
            .filter(|entry| !entry.expired_at(now_ms))
            .cloned()
            .collect();
        live.sort_by(|a, b| {
            a.domain
                .cmp(&b.domain)
                .then(a.path.cmp(&b.path))
                .then(a.cookie.name.cmp(&b.cookie.name))
        });
        live
    }
}

/// A parsed `Set-Cookie`: the cookie plus the two attributes that decide how long it lives, which
/// [`Cookie`] does not model because a *server* building one only ever writes `Max-Age`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCookie {
    pub cookie: Cookie,
    /// The `Domain=` attribute, lowercased with any leading `.` removed, or `None` for host-only.
    pub domain: Option<String>,
    /// The `Path=` attribute as written, or `None` to derive the default path.
    pub path: Option<String>,
    /// The `Max-Age=` attribute in seconds. Wins over `expires_unix_ms` when both are present
    /// (RFC 6265 §5.3), because it is unaffected by a clock skew between client and server.
    pub max_age: Option<i64>,
    /// The `Expires=` attribute as absolute unix milliseconds, when it parsed.
    pub expires_unix_ms: Option<u64>,
}

impl SetCookie {
    /// When this cookie dies, in unix milliseconds — `None` for a session cookie.
    pub fn expiry_ms(&self, now_ms: u64) -> Option<u64> {
        if let Some(max_age) = self.max_age {
            // A non-positive `Max-Age` is the deletion form: already expired, whatever `now` is.
            return Some(match max_age > 0 {
                true => now_ms.saturating_add((max_age as u64).saturating_mul(1000)),
                false => 0,
            });
        }
        self.expires_unix_ms
    }
}

/// Parse a `Set-Cookie` header value (RFC 6265 §5.2). `None` when there is no `name=value` pair at
/// all, or when the name or value carries a byte a cookie may not.
pub fn parse_set_cookie(header: &str) -> Option<SetCookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?;
    let (name, value) = pair.split_once('=')?;
    // `Cookie::new` is the validation, so a header carrying a control character or a stray `;`
    // cannot enter the jar and then be replayed into a request we build.
    let mut cookie = Cookie::new(name.trim(), value.trim()).ok()?;
    // A server's cookie is not the safe-defaults one `Cookie::new` builds: `HttpOnly` and
    // `SameSite` are whatever the header said, and the defaults are what a header that says
    // nothing means.
    cookie.http_only = false;
    cookie.same_site = SameSite::Lax;

    let mut parsed = SetCookie {
        cookie,
        domain: None,
        path: None,
        max_age: None,
        expires_unix_ms: None,
    };
    for attribute in parts {
        let (key, value) = match attribute.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (attribute.trim(), ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" => {
                // A leading dot is legacy syntax for "and subdomains", which is what a `Domain=`
                // attribute means anyway; §5.2.3 says to ignore it.
                let domain = value.trim_start_matches('.').to_ascii_lowercase();
                if !domain.is_empty() {
                    parsed.domain = Some(domain.clone());
                    parsed.cookie.domain = Some(domain);
                }
            }
            "path" => {
                parsed.path = Some(value.to_string());
                parsed.cookie.path = Some(value.to_string());
            }
            "max-age" => {
                if let Ok(seconds) = value.parse::<i64>() {
                    parsed.max_age = Some(seconds);
                    parsed.cookie.max_age = Some(seconds);
                }
            }
            "expires" => parsed.expires_unix_ms = parse_cookie_date(value),
            "secure" => parsed.cookie.secure = true,
            "httponly" => parsed.cookie.http_only = true,
            "samesite" => {
                if let Ok(same_site) = SameSite::parse(value) {
                    parsed.cookie.same_site = same_site;
                }
            }
            // An unknown attribute is ignored, per §5.2 — servers send extensions, and a jar that
            // refused a cookie over one would drop cookies for reasons the program cannot see.
            _ => {}
        }
    }
    Some(parsed)
}

/// The **default path** of a cookie set by a request to `request_path` (RFC 6265 §5.1.4): the
/// directory the request was in. `/a/b/c` gives `/a/b`, and `/a` gives `/`.
///
/// Not "the request path", which is the mistake that looks right: a cookie set by `GET /login`
/// would then be scoped to exactly `/login` and would never be sent anywhere useful.
pub fn default_path(request_path: &str) -> String {
    if !request_path.starts_with('/') {
        return "/".to_string();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => request_path[..index].to_string(),
    }
}

/// Whether `host` is covered by cookie domain `domain` (RFC 6265 §5.1.3).
///
/// Either they are the same, or `host` is a subdomain of `domain`. The `.`-boundary check is the
/// whole rule: without it `notexample.com` would match `example.com`.
pub fn domain_matches(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    // An IP address is never a subdomain of anything, so a `Domain=` attribute cannot apply to
    // one. Cheap structural test: a bare IPv4 has only digits and dots.
    let is_ip = host.starts_with('[')
        || (!host.is_empty() && host.chars().all(|c| c.is_ascii_digit() || c == '.'));
    !is_ip && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// Whether a request to `request_path` is covered by cookie path `cookie_path` (RFC 6265 §5.1.4).
///
/// A prefix match at a **segment boundary**: `/admin` covers `/admin` and `/admin/users`, and does
/// not cover `/adminfoo`.
pub fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    cookie_path.ends_with('/') || request_path.as_bytes()[cookie_path.len()] == b'/'
}

/// The month names an `Expires` value may use, in order.
const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Parse an `Expires` value into absolute unix milliseconds, by RFC 6265 §5.1.1.
///
/// The cookie spec deliberately does **not** use a date *format*; it uses a recovery algorithm
/// that pulls a time, a day, a month and a year out of whatever the server sent, in any order.
/// That is not leniency for its own sake — the three date formats HTTP allows are mutually
/// incompatible (`Sun, 06 Nov 1994 08:49:37 GMT`, `Sunday, 06-Nov-94 08:49:37 GMT`,
/// `Sun Nov  6 08:49:37 1994`), all three appear in the wild, and a client that understood only
/// the modern one would silently treat a legacy expiry as a session cookie.
///
/// An unparseable value yields `None`, which makes the cookie a session cookie rather than
/// dropping it — the same choice the browsers made.
pub fn parse_cookie_date(value: &str) -> Option<u64> {
    let mut time: Option<(u32, u32, u32)> = None;
    let mut day: Option<u32> = None;
    let mut month: Option<u32> = None;
    let mut year: Option<i32> = None;

    for token in value.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':')) {
        if token.is_empty() {
            continue;
        }
        if time.is_none()
            && let Some(parsed) = parse_time(token)
        {
            time = Some(parsed);
            continue;
        }
        if day.is_none()
            && let Some(parsed) = leading_number(token, 2)
        {
            day = Some(parsed as u32);
            continue;
        }
        if month.is_none() {
            let head = token.get(..3).unwrap_or_default().to_ascii_lowercase();
            if let Some(index) = MONTHS.iter().position(|m| *m == head) {
                month = Some(index as u32 + 1);
                continue;
            }
        }
        if year.is_none()
            && let Some(parsed) = leading_number(token, 4)
        {
            year = Some(parsed);
            continue;
        }
    }

    let (hour, minute, second) = time?;
    let day = day?;
    let month = month?;
    // §5.1.1: a two-digit year is 1970..=1999 or 2000..=2069 — the same window every cookie
    // implementation uses, and the reason a `Expires=..-94 ..` value from 1994 still works.
    let year = match year? {
        y @ 70..=99 => y + 1900,
        y @ 0..=69 => y + 2000,
        y => y,
    };
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days.checked_mul(86_400)? + (hour * 3600 + minute * 60 + second) as i64;
    // A date before the epoch is in the past by any reckoning, which is what a deletion header
    // means — clamp rather than fail, so `Expires=Thu, 01 Jan 1970 00:00:00 GMT` deletes.
    Some((seconds.max(0) as u64).saturating_mul(1000))
}

/// Parse an `hh:mm:ss` token, ignoring any trailing non-digits.
fn parse_time(token: &str) -> Option<(u32, u32, u32)> {
    let mut fields = token.splitn(3, ':');
    let hour = leading_number(fields.next()?, 2)?;
    let minute = leading_number(fields.next()?, 2)?;
    let second = leading_number(fields.next()?, 2)?;
    Some((hour as u32, minute as u32, second as u32))
}

/// The leading run of at most `max` ASCII digits in `token`, or `None` when it does not start with
/// one. Trailing non-digits are ignored, which is what lets `06-Nov-94` yield `6` from its first
/// token and what §5.1.1's productions describe.
fn leading_number(token: &str, max: usize) -> Option<i32> {
    let digits: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > max {
        return None;
    }
    digits.parse().ok()
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's `days_from_civil`).
///
/// Written out rather than pulled from a date library because this crate's date library is a
/// dependency of one module, and a cookie expiry is a scalar comparison against a clock read —
/// nothing here needs calendars, zones or leap seconds.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = year.div_euclid(400) as i64;
    let year_of_era = year.rem_euclid(400) as i64;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Group `entries` by domain for a display or debug rendering — the jar's contents as a map.
pub fn by_domain(entries: &[StoredCookie]) -> BTreeMap<&str, Vec<&StoredCookie>> {
    let mut grouped: BTreeMap<&str, Vec<&StoredCookie>> = BTreeMap::new();
    for entry in entries {
        grouped
            .entry(entry.domain.as_str())
            .or_default()
            .push(entry);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed wall-clock instant to measure expiries against: 2023-11-14T22:13:20Z.
    const NOW: u64 = 1_700_000_000_000;

    fn jar_with(url: &str, headers: &[&str]) -> CookieJar {
        let mut jar = CookieJar::new();
        for header in headers {
            jar.store(url, header, NOW);
        }
        jar
    }

    #[test]
    fn a_cookie_goes_back_to_the_host_that_set_it_and_nowhere_else() {
        // No `Domain=` means host-only, which is the default and the strict case.
        let jar = jar_with("https://api.test/login", &["sid=abc"]);
        assert_eq!(
            jar.header_for("https://api.test/orders", NOW).as_deref(),
            Some("sid=abc")
        );
        for elsewhere in [
            "https://other.test/orders",
            "https://sub.api.test/orders",
            "https://notapi.test/orders",
        ] {
            assert_eq!(
                jar.header_for(elsewhere, NOW),
                None,
                "elsewhere={elsewhere}"
            );
        }
    }

    #[test]
    fn a_domain_cookie_reaches_subdomains_at_a_dot_boundary() {
        let jar = jar_with("https://api.test/login", &["sid=abc; Domain=api.test"]);
        for host in [
            "https://api.test/x",
            "https://a.api.test/x",
            "https://a.b.api.test/x",
        ] {
            assert!(jar.header_for(host, NOW).is_some(), "host={host}");
        }
        // The `.`-boundary check is the whole rule: without it this would match.
        assert_eq!(jar.header_for("https://notapi.test/x", NOW), None);
    }

    #[test]
    fn a_host_cannot_set_a_cookie_for_a_domain_it_is_not_in() {
        // The attack this rule exists for. `evil.test` naming `bank.test` would otherwise put a
        // cookie in the jar that later rides a request to the bank.
        let mut jar = CookieJar::new();
        assert!(!jar.store("https://evil.test/x", "sid=planted; Domain=bank.test", NOW));
        assert!(jar.is_empty());
        assert_eq!(jar.header_for("https://bank.test/account", NOW), None);
    }

    #[test]
    fn a_path_scoped_cookie_matches_only_at_a_segment_boundary() {
        let jar = jar_with("https://x.test/login", &["sid=abc; Path=/admin"]);
        assert!(jar.header_for("https://x.test/admin", NOW).is_some());
        assert!(jar.header_for("https://x.test/admin/users", NOW).is_some());
        // `/adminfoo` starts with `/admin` and is a different place entirely.
        assert_eq!(jar.header_for("https://x.test/adminfoo", NOW), None);
        assert_eq!(jar.header_for("https://x.test/other", NOW), None);
    }

    #[test]
    fn the_default_path_is_the_directory_the_request_was_in() {
        // Not the request path: a cookie set by `GET /login` scoped to exactly `/login` would
        // never be sent anywhere useful.
        assert_eq!(default_path("/login"), "/");
        assert_eq!(default_path("/a/b/c"), "/a/b");
        assert_eq!(default_path("/a/"), "/a");
        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path("relative"), "/");

        let jar = jar_with("https://x.test/api/v2/login", &["sid=abc"]);
        assert!(
            jar.header_for("https://x.test/api/v2/orders", NOW)
                .is_some()
        );
        assert_eq!(jar.header_for("https://x.test/api/v3/orders", NOW), None);
    }

    #[test]
    fn a_secure_cookie_is_withheld_from_a_plain_http_request() {
        let jar = jar_with("https://x.test/login", &["sid=abc; Secure", "theme=dark"]);
        assert_eq!(
            jar.header_for("https://x.test/a", NOW).as_deref(),
            Some("sid=abc; theme=dark")
        );
        assert_eq!(
            jar.header_for("http://x.test/a", NOW).as_deref(),
            Some("theme=dark"),
            "the flag means nothing if the cookie rides a plain-http request anyway"
        );
    }

    #[test]
    fn max_age_expires_a_cookie_and_zero_deletes_it() {
        let mut jar = jar_with("https://x.test/", &["sid=abc; Max-Age=60"]);
        assert!(jar.header_for("https://x.test/a", NOW).is_some());
        assert!(
            jar.header_for("https://x.test/a", NOW + 61_000).is_none(),
            "a minute later it is gone"
        );
        // `Max-Age=0` is how a server deletes a cookie: it removes the entry rather than adding
        // an already-dead one.
        assert!(!jar.store("https://x.test/", "sid=; Max-Age=0", NOW));
        assert!(jar.is_empty());
    }

    #[test]
    fn max_age_wins_over_expires() {
        // RFC 6265 §5.3: `Max-Age` is relative and therefore immune to a clock skew between the
        // server and us, which is exactly why it takes precedence.
        let jar = jar_with(
            "https://x.test/",
            &["sid=abc; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:00 GMT"],
        );
        assert!(
            jar.header_for("https://x.test/a", NOW).is_some(),
            "the epoch `Expires` would have deleted it if it had won"
        );
    }

    #[test]
    fn an_expires_in_the_past_deletes_and_one_in_the_future_stores() {
        let mut jar = jar_with("https://x.test/", &["sid=abc"]);
        assert!(jar.header_for("https://x.test/a", NOW).is_some());
        // The deletion header every server sends.
        assert!(!jar.store(
            "https://x.test/",
            "sid=; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            NOW
        ));
        assert!(jar.is_empty());

        jar.store(
            "https://x.test/",
            "sid=abc; Expires=Wed, 14 Nov 2040 22:13:20 GMT",
            NOW,
        );
        assert!(jar.header_for("https://x.test/a", NOW).is_some());
    }

    #[test]
    fn every_http_date_format_a_server_might_send_parses() {
        // RFC 9110 defines three and requires a recipient accept all three; a client that
        // understood only the modern one would silently treat a legacy expiry as a session
        // cookie, and the cookie would outlive its deletion.
        let expected = 784_111_777_000u64; // 1994-11-06T08:49:37Z
        for value in [
            "Sun, 06 Nov 1994 08:49:37 GMT",  // IMF-fixdate
            "Sunday, 06-Nov-94 08:49:37 GMT", // RFC 850
            "Sun Nov  6 08:49:37 1994",       // asctime
        ] {
            assert_eq!(parse_cookie_date(value), Some(expected), "value={value:?}");
        }
        // A two-digit year lands in the 1970..=2069 window §5.1.1 defines.
        assert_eq!(
            parse_cookie_date("Sun, 06-Nov-69 08:49:37 GMT"),
            parse_cookie_date("Sun, 06 Nov 2069 08:49:37 GMT")
        );
        // Unparseable is `None` — a session cookie, not a dropped one.
        assert_eq!(parse_cookie_date("tomorrow"), None);
        assert_eq!(parse_cookie_date(""), None);
        assert_eq!(parse_cookie_date("Sun, 06 Nov 1994"), None, "no time");
        assert_eq!(parse_cookie_date("Sun, 32 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse_cookie_date("Sun, 06 Nov 1994 25:49:37 GMT"), None);
    }

    #[test]
    fn a_session_cookie_has_no_expiry_at_all() {
        let jar = jar_with("https://x.test/", &["sid=abc"]);
        assert_eq!(jar.snapshot(NOW)[0].expires_ms, None);
        assert!(
            jar.header_for("https://x.test/a", NOW + 86_400_000 * 365)
                .is_some(),
            "it lives as long as the jar does"
        );
    }

    #[test]
    fn a_cookie_replaces_its_namesake_and_keeps_its_place_in_the_order() {
        let mut jar = CookieJar::new();
        jar.store("https://x.test/", "sid=first", NOW);
        jar.store("https://x.test/", "other=v", NOW);
        jar.store("https://x.test/", "sid=second", NOW + 5_000);
        assert_eq!(jar.len(), 2, "same name, domain and path — one entry");
        assert_eq!(
            jar.header_for("https://x.test/a", NOW + 5_000).as_deref(),
            Some("sid=second; other=v"),
            "refreshing a session cookie must not shuffle it behind the ones set after it"
        );
    }

    #[test]
    fn the_same_name_at_a_different_path_is_a_different_cookie() {
        let mut jar = CookieJar::new();
        jar.store("https://x.test/", "sid=root; Path=/", NOW);
        jar.store("https://x.test/", "sid=admin; Path=/admin", NOW);
        assert_eq!(jar.len(), 2);
        // §5.4: longer path first. Servers are not supposed to depend on the order and enough of
        // them do that an arbitrary one is a bug waiting to be blamed on something else.
        assert_eq!(
            jar.header_for("https://x.test/admin/x", NOW).as_deref(),
            Some("sid=admin; sid=root")
        );
        assert_eq!(
            jar.header_for("https://x.test/other", NOW).as_deref(),
            Some("sid=root")
        );
    }

    #[test]
    fn equal_path_lengths_order_by_age_and_then_by_arrival() {
        // A deterministic clock makes an equal `created_ms` the normal case, so without the
        // arrival tie-break the send order would depend on a sort's stability rather than a rule.
        let mut jar = CookieJar::new();
        for name in ["c", "a", "b"] {
            jar.store("https://x.test/", &format!("{name}=1"), NOW);
        }
        assert_eq!(
            jar.header_for("https://x.test/a", NOW).as_deref(),
            Some("c=1; a=1; b=1")
        );
    }

    #[test]
    fn every_attribute_is_read_off_a_set_cookie_header() {
        let parsed = parse_set_cookie(
            "sid=abc; Path=/admin; Domain=.Example.COM; Max-Age=600; Secure; HttpOnly; \
             SameSite=Strict; Priority=High",
        )
        .expect("a well-formed header");
        assert_eq!(parsed.cookie.name, "sid");
        assert_eq!(parsed.cookie.value, "abc");
        assert_eq!(parsed.path.as_deref(), Some("/admin"));
        // A leading dot is legacy syntax for what `Domain=` already means (§5.2.3).
        assert_eq!(parsed.domain.as_deref(), Some("example.com"));
        assert_eq!(parsed.max_age, Some(600));
        assert!(parsed.cookie.secure);
        assert!(parsed.cookie.http_only);
        assert_eq!(parsed.cookie.same_site, SameSite::Strict);
    }

    #[test]
    fn a_header_with_no_pair_or_an_illegal_byte_is_refused() {
        assert_eq!(parse_set_cookie("nonsense"), None);
        assert_eq!(parse_set_cookie(""), None);
        // A control character in the value would let the cookie split a header when replayed into
        // a request we build; `Cookie::new` is what refuses it, and it is on this path for
        // exactly that reason.
        assert_eq!(parse_set_cookie("sid=a\rb"), None);
        assert_eq!(parse_set_cookie("si d=abc"), None, "a space is not a token");
    }

    #[test]
    fn a_bad_cookie_does_not_cost_the_response_its_good_ones() {
        let mut jar = CookieJar::new();
        jar.store_response(
            "https://x.test/",
            &[
                ("Set-Cookie".to_string(), "a=1".to_string()),
                ("Set-Cookie".to_string(), "garbage".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
                ("Content-Type".to_string(), "text/html".to_string()),
            ],
            NOW,
        );
        assert_eq!(
            jar.header_for("https://x.test/a", NOW).as_deref(),
            Some("a=1; b=2")
        );
    }

    #[test]
    fn same_site_is_parsed_and_filters_nothing() {
        // A programmatic client has no site: there is no document, no origin initiating a
        // navigation, nothing for `Lax` to be lax about. Withholding on it would silently break
        // every cookie a browser-oriented server marks `Strict`.
        let jar = jar_with("https://x.test/", &["sid=abc; SameSite=Strict"]);
        assert_eq!(jar.snapshot(NOW)[0].cookie.same_site, SameSite::Strict);
        assert!(jar.header_for("https://x.test/a", NOW).is_some());
    }

    #[test]
    fn a_seeded_cookie_behaves_like_one_the_server_set() {
        // The door a client uses to start already authenticated instead of performing a login it
        // already did.
        let mut jar = CookieJar::new();
        let cookie = crate::cookie::Cookie::new("sid", "from-config").expect("valid");
        jar.insert("https://x.test/", cookie, NOW);
        assert_eq!(
            jar.header_for("https://x.test/a", NOW).as_deref(),
            Some("sid=from-config")
        );
    }

    #[test]
    fn the_snapshot_is_stable_and_hides_the_dead() {
        let mut jar = CookieJar::new();
        jar.store("https://b.test/", "z=1", NOW);
        jar.store("https://a.test/", "y=1", NOW);
        jar.store("https://a.test/", "x=1; Max-Age=1", NOW);
        let snapshot = jar.snapshot(NOW);
        let names: Vec<&str> = snapshot.iter().map(|e| e.cookie.name.as_str()).collect();
        assert_eq!(names, ["x", "y", "z"], "domain, then path, then name");
        assert_eq!(
            jar.snapshot(NOW + 2_000).len(),
            2,
            "the expired one is gone"
        );
    }

    #[test]
    fn an_ip_address_host_is_never_a_subdomain() {
        assert!(domain_matches("192.168.1.1", "192.168.1.1"));
        assert!(!domain_matches("192.168.1.1", "1.1"));
        assert!(!domain_matches("[::1]", "1]"));
        assert!(domain_matches("a.example.com", "example.com"));
        assert!(!domain_matches("aexample.com", "example.com"));
    }

    #[test]
    fn grouping_by_domain_keeps_a_stable_reading_order() {
        let mut jar = CookieJar::new();
        jar.store("https://b.test/", "z=1", NOW);
        jar.store("https://a.test/", "y=1", NOW);
        let snapshot = jar.snapshot(NOW);
        let grouped = by_domain(&snapshot);
        assert_eq!(
            grouped.keys().copied().collect::<Vec<_>>(),
            vec!["a.test", "b.test"]
        );
    }
}
