/// Who or what initiated this navigation.
///
/// Carried through the entire navigation chain so the policy can distinguish
/// user agency from document agency, even across server redirects.
#[derive(Debug, Clone)]
pub enum NavigationInitiator {
    /// The user typed in the address bar, clicked a link, or pressed a
    /// keyboard shortcut (Reload, Enter in URL bar).
    UserGesture,
    /// Document logic: a `navigate` action fired by a timer tick, a
    /// network-response batch, or a computed binding — anything that did not
    /// originate from a direct user interaction.
    DocumentLogic,
    /// A server redirect of a prior navigation.  Wraps the *original*
    /// initiator so the gesture gate (N3) can look through the redirect chain:
    /// a user-gesture navigation that redirects cross-origin is still user
    /// agency → allowed.
    RedirectOf(Box<NavigationInitiator>),
    /// A Back/Forward history step (button click or `Alt+Left`/`Alt+Right`).
    /// The user clicking Back/Forward is itself a user gesture, so this
    /// carries agency exactly like [`Self::UserGesture`] under N3 — the
    /// distinct variant exists so callers (`window::history`) can tell a
    /// history restoration apart from a fresh navigation without adding a
    /// second, ungated navigation path.
    HistoryStep,
}

impl NavigationInitiator {
    /// Wraps `self` as the initiator of a server redirect of the navigation it
    /// describes.
    ///
    /// Redirect chains are *collapsed* rather than nested: the wrapped value is
    /// always the chain's root initiator, so `RedirectOf` is never more than
    /// one level deep however many hops a server strings together. This is not
    /// a cosmetic simplification — [`has_user_agency`] answers by unwinding to
    /// the root, so collapsing preserves the verdict exactly while keeping the
    /// recursion depth constant instead of proportional to attacker-chosen hop
    /// count.
    ///
    /// The whole point of carrying the value at all is that the wrapped
    /// initiator must be the *real* one. Synthesising a
    /// [`NavigationInitiator::UserGesture`] here — because "a redirect of a
    /// navigation is probably a navigation the user asked for" — would let any
    /// server turn a document-logic, same-origin request into a
    /// gesture-authorised cross-origin navigation with a single `Location`
    /// header, which is precisely the N3 block this type exists to enforce.
    #[must_use]
    pub fn redirect_of(self) -> Self {
        match self {
            Self::RedirectOf(inner) => Self::RedirectOf(inner),
            root => Self::RedirectOf(Box::new(root)),
        }
    }
}

/// The policy verdict on a proposed navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationVerdict {
    /// Navigation is permitted; carries the resolved URL.
    Allow(String),
    /// Navigation is blocked; carries a human-readable reason.
    Block(&'static str),
}

/// Extracts the domain from a `mizu://` URL, or `None` for other schemes.
///
/// Uses the same strict boundary as `MizuUri::parse`: scans for '/', '?',
/// or '#' so query strings cannot bleed into the domain token.
fn mizu_domain(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("mizu://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Returns `true` when `host` is a host token `mizu://` actually admits: no
/// userinfo, no explicit port, and none of the WHATWG "forbidden host code
/// points" that `url::Host` (opaque-host parsing, since `mizu` is a
/// non-special scheme) rejects.
///
/// This mirrors the policy `MizuUri::parse` enforces on the way to the socket
/// (see `core::uri`), and mirrors it *by hand* rather than by calling that
/// parser. Two reasons. `check_navigation` is documented as pure and
/// allocation-free, and is the one policy function `formal/` models; and
/// `MizuUri::parse` reaches `url::Url::parse`, whose IDNA/`zerovec` codepaths
/// break Kani's codegen for the whole crate the moment they become reachable
/// from a harness (`core::uri` documents the incompatibility). What must stay
/// true is the *agreement*: a token this function accepts must be one
/// `MizuUri::parse` would also accept, or the choke point and the fetcher
/// would disagree about what the origin is. Widening either side without the
/// other is the bug to watch for.
///
/// Rejecting here is fail-closed twice over: such a URL could never be fetched
/// anyway (`MizuUri::parse` refuses it), so the only thing the old behaviour
/// bought was the chance for a spoof-shaped host — `trusted.com@evil.com`, or
/// `trusted.com\evil.com` (a backslash reads as a path separator to some
/// consumers and as a plain host character to others) — to be carried further
/// into the browser as if it were an origin.
fn is_wellformed_mizu_host(host: &str) -> bool {
    if host.is_empty() || host.contains('@') {
        return false;
    }
    // An IPv6 literal is bracketed and is full of colons; a colon anywhere
    // else is a port. `[::1]:80` is caught by the trailing-text check.
    let unbracketed = match host.strip_prefix('[') {
        Some(rest) => match rest.strip_suffix(']') {
            Some(inner) if !inner.is_empty() => inner,
            _ => return false,
        },
        None => {
            if host.contains(':') {
                return false;
            }
            host
        }
    };
    // WHATWG forbidden host code points (opaque-host parsing), minus `:`
    // which is handled above so a bracketed IPv6 literal's internal colons
    // are not rejected here.
    !unbracketed.bytes().any(|b| {
        matches!(
            b,
            0x00..=0x20 | b'#' | b'/' | b'<' | b'>' | b'?' | b'\\' | b'^' | b'|' | 0x7f
        )
    })
}

/// Returns `true` when the root initiator (unwinding `RedirectOf` chains)
/// is a [`NavigationInitiator::UserGesture`].
fn has_user_agency(initiator: &NavigationInitiator) -> bool {
    match initiator {
        NavigationInitiator::UserGesture => true,
        NavigationInitiator::DocumentLogic => false,
        NavigationInitiator::RedirectOf(inner) => has_user_agency(inner),
        NavigationInitiator::HistoryStep => true,
    }
}

/// Returns `true` when `current_url` and `target` are both `mizu://` URLs with
/// the same domain, compared ASCII-case-insensitively.
///
/// # Case
///
/// The comparison has to be case-insensitive, and for the same reason
/// `security::network::is_local_host`'s does: `mizu://` is a non-special URL
/// scheme, so the `url` crate treats its host as an opaque host and does not
/// lowercase it. Every *other* part of this runtime already treats the two
/// spellings as one thing — DNS is case-insensitive, and
/// `ValidatedDomain::from_raw` lowercases before hashing, so `mizu://Example.com`
/// and `mizu://example.com` share one encrypted store, one keyring entry and
/// one quota ledger entry. A case-sensitive check here would call that single
/// origin two origins: same data, two navigation identities. Agreeing with the
/// storage identity is what keeps "same origin" one concept.
///
/// A malformed host (userinfo or an explicit port) is never same-origin with
/// anything, including itself — see [`is_wellformed_mizu_host`].
fn is_same_origin(current_url: &str, target: &str) -> bool {
    match (mizu_domain(current_url), mizu_domain(target)) {
        (Some(a), Some(b)) => {
            is_wellformed_mizu_host(a) && is_wellformed_mizu_host(b) && a.eq_ignore_ascii_case(b)
        }
        _ => false,
    }
}

/// Single policy entry point for all navigation decisions.
///
/// This is a **pure function** — no I/O, no side effects — so it can be
/// verified with property-based testing or formal methods.
///
/// # Scheme rules (N4)
///
/// | Target scheme | Verdict |
/// |---|---|
/// | `mizu://` | Apply origin + gesture checks (N3) |
/// | `file://` from `file://` origin | `Allow` (sandbox enforced by caller) |
/// | `file://` from `mizu://` origin | `Block` (remote → local) |
/// | `http://`, `https://` | `Block` (not a Mizu scheme) |
/// | bare hostname (no `://`) | Normalise to `mizu://` and re-check |
/// | anything else | `Block` |
///
/// # Host rules
///
/// | `mizu://` host | Verdict |
/// |---|---|
/// | empty (`mizu:///path`) | `Block` |
/// | carries userinfo or an explicit port | `Block` |
/// | anything else | Apply the origin rules below |
///
/// # Origin rules (N3)
///
/// Hosts are compared ASCII-case-insensitively, matching how the storage
/// identity and DNS already treat them — see [`is_same_origin`].
///
/// | Same origin? | User gesture? | Verdict |
/// |---|---|---|
/// | Yes | any | `Allow` |
/// | No | Yes | `Allow` |
/// | No | No | `Block` |
///
/// # File-sandbox rules
///
/// `file://` → `file://` navigation is allowed at this level; the caller
/// (`navigate_to_url`) is responsible for the sandbox containment check
/// because it requires I/O (`canonicalize`).  The policy here only blocks
/// the *scheme transition* (`mizu://` → `file://`).
///
/// # Fail-secure
///
/// Any parse ambiguity or unrecognised scheme results in `Block`.
pub fn check_navigation(
    current_url: &str,
    target: &str,
    initiator: &NavigationInitiator,
) -> NavigationVerdict {
    // Empty target is always a block.
    if target.is_empty() {
        return NavigationVerdict::Block("empty navigation target");
    }

    // --- Normalise bare hostname/path to mizu:// ---
    let normalised: String;
    let effective_target = if !target.contains("://") {
        // file:// origin with a relative path: this is a local file navigation.
        if current_url.starts_with("file://") {
            // Relative paths within file:// are allowed at the policy level;
            // sandbox enforcement is the caller's responsibility.
            return NavigationVerdict::Allow(target.to_owned());
        }
        normalised = format!("mizu://{target}");
        normalised.as_str()
    } else {
        target
    };

    // --- Scheme gate (N4) ---
    if effective_target.starts_with("http://") || effective_target.starts_with("https://") {
        return NavigationVerdict::Block("http(s):// is not a navigable Mizu scheme");
    }

    // file:// target
    if effective_target.starts_with("file://") {
        if current_url.starts_with("mizu://") {
            return NavigationVerdict::Block(
                "remote document may not navigate to file:// resource",
            );
        }
        if current_url.starts_with("file://") {
            // file→file: sandbox enforced by caller.
            return NavigationVerdict::Allow(effective_target.to_owned());
        }
        // Unknown origin scheme → file: block.
        return NavigationVerdict::Block(
            "navigation to file:// from unknown origin scheme blocked",
        );
    }

    // mizu:// target
    if effective_target.starts_with("mizu://") {
        // Validate that the domain is non-empty.
        let Some(target_host) = mizu_domain(effective_target) else {
            return NavigationVerdict::Block("mizu:// URL has an empty domain");
        };
        // …and that it is a host `mizu://` admits at all. `trusted.com@evil.com`
        // reads as `trusted.com` to a human and resolves to `evil.com`; an
        // explicit port is not part of the scheme. Neither could ever be
        // fetched (`MizuUri::parse` refuses both), so blocking here costs
        // nothing and keeps a spoofable string from being carried any further
        // into the browser as if it were an origin.
        if !is_wellformed_mizu_host(target_host) {
            return NavigationVerdict::Block(
                "mizu:// URL must not carry credentials or an explicit port",
            );
        }

        // N3: origin + gesture check.
        if is_same_origin(current_url, effective_target) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        // Cross-origin: requires user agency.
        if has_user_agency(initiator) {
            return NavigationVerdict::Allow(effective_target.to_owned());
        }

        return NavigationVerdict::Block("cross-origin navigation without user gesture blocked");
    }

    // Any other scheme — fail secure.
    NavigationVerdict::Block("unrecognised scheme; navigation blocked")
}

// No Kani harness here, despite `check_navigation` being the most obvious
// candidate on paper (pure, no I/O — see `SECURITY-INVARIANTS.md` §8.3/§8.4
// for the full coverage table and rationale). An attempted harness over
// bounded symbolic `current_url`/`target` strings plus a depth-capped
// `RedirectOf` chain did not converge in a usable time budget under CBMC:
// symbolic-string comparison across four scheme prefixes is exactly the
// shape bounded model checking handles worst. N2/N3/N4 are covered by the
// unit tests below and, at the design level, by `formal/`'s Lean
// development. Revisit with a different string-modelling strategy.

#[cfg(test)]
mod tests;
