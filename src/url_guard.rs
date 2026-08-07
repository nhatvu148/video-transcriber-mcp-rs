//! Refusing caller-supplied URLs that point back into our own network.
//!
//! Every entry point here eventually hands a URL to `yt-dlp`, which will fetch
//! essentially anything. Without a check, whoever can reach the endpoint can
//! make this process request loopback, RFC1918 space, link-local (where cloud
//! metadata lives) or a private-network name.
//!
//! # What this does not stop
//!
//! yt-dlp resolves DNS again and follows redirects on its own, so an
//! attacker-controlled public URL answering `302 Location: http://169.254.169.254/`
//! walks straight past everything here, with no race to win. Pinning the
//! resolved IP does not help and is not available anyway: yt-dlp has no
//! `--resolve` equivalent, and even with one it would constrain only the first
//! hop while redirects to CDN hosts are ordinary and necessary.
//!
//! The host allowlist is what closes that, by removing the attacker-controlled
//! entry point — but it can only be used where the set of acceptable sites is
//! genuinely bounded. Where it isn't, the remaining options are `yt-dlp
//! --proxy` through a filtering forward proxy, or egress rules at the network
//! layer. Both sit outside this process.
//!
//! # Local use is not a threat model
//!
//! A stdio MCP server transcribing `http://localhost:8000/lecture.mp4` from the
//! user's own machine is legitimate, and there is no attacker in that picture —
//! the caller already has a shell. So the guard is applied by callers that are
//! remotely reachable, not unconditionally.


/// Allow any host, leaving only the internal-address checks.
///
/// For entry points whose acceptable sites genuinely cannot be enumerated —
/// the transcription tool advertises 1000+ platforms. Closes the direct cases
/// and nothing more; see the module docs for what that leaves open.
pub const ANY_HOST: &str = "*";

/// True when an address belongs to the deployment's own network rather than the
/// public internet.
///
/// Covers loopback, RFC1918 private space, link-local (which is where cloud
/// metadata services live), carrier-grade NAT, and the IPv6 equivalents —
/// including v4-mapped v6, since `::ffff:127.0.0.1` is otherwise a trivial
/// bypass of a v4-only check.
pub fn is_internal_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                // 100.64.0.0/10 — carrier-grade NAT, used internally by some hosts
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // Several v6 forms embed a v4 address. Each is a way to smuggle an
            // internal v4 target past a check that only understands v6.
            let embedded_v4 = v6
                .to_ipv4_mapped()
                // 6to4: 2002:AABB:CCDD::/48 carries v4 AA.BB.CC.DD
                .or_else(|| {
                    (seg[0] == 0x2002).then(|| {
                        std::net::Ipv4Addr::new(
                            (seg[1] >> 8) as u8,
                            seg[1] as u8,
                            (seg[2] >> 8) as u8,
                            seg[2] as u8,
                        )
                    })
                })
                // Teredo: 2001:0000:/32, client v4 in the last two groups,
                // stored bitwise-complemented.
                .or_else(|| {
                    (seg[0] == 0x2001 && seg[1] == 0).then(|| {
                        let a = !seg[6];
                        let b = !seg[7];
                        std::net::Ipv4Addr::new((a >> 8) as u8, a as u8, (b >> 8) as u8, b as u8)
                    })
                })
                // IPv4-compatible `::a.b.c.d` (deprecated, and missed by
                // to_ipv4_mapped). Modern stacks do not auto-route these, so
                // this is completeness rather than a live hole — but leaving
                // one member of the family out invites someone to assume the
                // others are missing too.
                .or_else(|| {
                    (seg[0..6] == [0, 0, 0, 0, 0, 0] && (seg[6] != 0 || seg[7] > 1)).then(|| {
                        std::net::Ipv4Addr::new(
                            (seg[6] >> 8) as u8,
                            seg[6] as u8,
                            (seg[7] >> 8) as u8,
                            seg[7] as u8,
                        )
                    })
                })
                // NAT64 well-known prefix 64:ff9b::/96 — v4 in the low 32 bits,
                // so `64:ff9b::a9fe:a9fe` is 169.254.169.254 anywhere a NAT64
                // gateway is present. A documented way past filters that stop
                // at the more familiar embeddings above.
                .or_else(|| {
                    (seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0]).then(|| {
                        std::net::Ipv4Addr::new(
                            (seg[6] >> 8) as u8,
                            seg[6] as u8,
                            (seg[7] >> 8) as u8,
                            seg[7] as u8,
                        )
                    })
                });

            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local
                || (seg[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfe80
                || embedded_v4.is_some_and(|v4| is_internal_ip(IpAddr::V4(v4)))
        }
    }
}

/// Rejects a caller-supplied URL that could reach our own network.
///
/// `fetch_audio` hands this URL to yt-dlp, which will fetch essentially
/// anything. Without this check an authenticated caller can make the engine
/// request loopback, private-range, link-local (cloud metadata) or Fly 6PN
/// `.internal` addresses — and because the handler returned the raw error
/// string, it worked as an oracle even when no audio came back.
///
/// # Why the allowlist is the control that matters
///
/// The IP checks below only describe where the *submitted* hostname resolves.
/// yt-dlp then fetches independently: it re-resolves DNS and follows HTTP
/// redirects. So a public, attacker-controlled URL that answers `302` with
/// `Location: http://169.254.169.254/...` walks straight past every IP check
/// here, with no race to win. Redirects are far easier to exploit than the DNS
/// rebinding case, and validating redirect hops ourselves does not help — an
/// attacker serves a benign response to our probe and the redirect to yt-dlp.
///
/// The host allowlist is what actually closes this, because it removes the
/// attacker-controlled entry point. The IP checks remain as defence in depth
/// and as the whole control when the allowlist is disabled.
///
/// `FETCH_AUDIO_ALLOWED_HOSTS` — comma-separated suffixes, defaults to the
/// platforms Free mode serves. Set it to `*` to allow any host, which restores
/// the redirect exposure and should only be paired with egress filtering at the
/// network layer.
/// Defaults to the platforms the product names in its own UI — the hint under
/// the URL input reads "YouTube, Vimeo, TikTok, Twitter, Twitch, Coursera, and
/// 1000+ more" (`web/index.html`, `extension/sidepanel.html`), and it sits
/// above the mode picker, so a Free-mode user reads it as applying to them.
///
/// The list therefore covers every platform named there. It cannot cover
/// "1000+ more" — that is the trade this allowlist makes, and it is why an
/// unlisted host is refused with "use Fast mode" rather than a generic error.
/// If Free mode should genuinely accept anything, that is `*` plus the egress
/// filtering tracked in the follow-up issue, not a longer list.
pub const DEFAULT_ALLOWED_HOSTS: &str =
    "youtube.com,youtu.be,vimeo.com,tiktok.com,twitter.com,x.com,twitch.tv,coursera.org";

pub fn allowed_hosts_config() -> String {
    std::env::var("FETCH_AUDIO_ALLOWED_HOSTS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.to_string())
}

/// Pure so tests can pass a config directly. Reading the environment inside
/// made every test that varied it share process-global state, which is a CI
/// flake waiting to happen under parallel execution.
pub fn host_is_allowed_by(host: &str, configured: &str) -> bool {
    if configured.trim() == "*" {
        return true;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    configured
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        // Suffix match on a label boundary, so `evil-youtube.com` and
        // `youtube.com.attacker.net` do not pass as `youtube.com`.
        .any(|allowed| host == allowed || host.ends_with(&format!(".{allowed}")))
}

pub async fn reject_internal_url(raw: &str, allowed: &str) -> Result<(), &'static str> {
    const BAD_URL: &str = "a valid http(s) URL is required";
    const UNREACHABLE: &str = "that host is not reachable";
    const NOT_SUPPORTED: &str = "that site isn't supported in Free mode — use Fast mode";

    let parsed = url::Url::parse(raw).map_err(|_| BAD_URL)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(BAD_URL);
    }
    // A public video URL never needs credentials, and they are a standard way
    // to confuse a downstream fetcher about which host it is really talking to.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BAD_URL);
    }

    let host = parsed.host_str().ok_or(BAD_URL)?;
    // Fly's private network resolves `*.internal`. Checked by name because it
    // may not resolve at all from every context, which would otherwise look
    // like a lookup failure rather than a refusal.
    let lower = host.to_ascii_lowercase();
    if lower == "internal" || lower.ends_with(".internal") || lower.ends_with(".local") {
        return Err(UNREACHABLE);
    }

    // The control that survives redirects. Checked before the IP work because
    // it is both cheaper and stronger.
    if !host_is_allowed_by(host, allowed) {
        return Err(NOT_SUPPORTED);
    }

    // `Url::host_str` returns IPv6 literals bracketed (`[::1]`), and
    // `Ipv6Addr::from_str` rejects brackets — so passing it through unmodified
    // sent every v6 literal down the DNS path, where it failed to resolve. That
    // failed closed, but it meant the v6 branch of `is_internal_ip` was never
    // reached from a real request, and legitimate public v6 literals were
    // refused as unreachable.
    let host_for_lookup = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    // Check *every* answer: a name with one public and one private record would
    // otherwise pass on the strength of the public one.
    //
    // Bounded, because `lookup_host` otherwise waits out the OS resolver's full
    // retry window — a hostname delegated to a blackholed nameserver would pin
    // a request handler for that long, for free, from any account.
    //
    // Partial by construction: `lookup_host` runs `getaddrinfo` on tokio's
    // blocking pool, and cancelling the future does not cancel that thread. So
    // this frees the *handler* at 5s while the blocking task runs on. Enough
    // concurrent blackholed lookups could still saturate the blocking pool.
    // Not closed here because the `/api/*` governor already bounds request rate
    // per IP (see `main.rs`), and closing it properly means replacing
    // `getaddrinfo` with a cancellable resolver such as `hickory-resolver` —
    // a dependency worth adding only if this stops being theoretical.
    const DNS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let port = parsed.port_or_known_default().unwrap_or(443);
    let mut resolved = tokio::time::timeout(
        DNS_TIMEOUT,
        tokio::net::lookup_host((host_for_lookup, port)),
    )
    .await
    // Timed out, and separately: resolution failed. Both mean we could not
    // establish the address is safe, and both must refuse rather than proceed.
    .map_err(|_| UNREACHABLE)?
    .map_err(|_| UNREACHABLE)?
    .peekable();
    if resolved.peek().is_none() {
        return Err(UNREACHABLE);
    }
    for addr in resolved {
        if is_internal_ip(addr.ip()) {
            return Err(UNREACHABLE);
        }
    }
    Ok(())
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;
    use std::net::IpAddr;


    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test ip")
    }

    #[test]
    fn loopback_private_and_link_local_are_internal() {
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            // The one that matters most: cloud metadata lives here.
            "169.254.169.254",
            "100.64.0.1", // carrier-grade NAT
            "0.0.0.0",
        ] {
            assert!(is_internal_ip(ip(s)), "{s} must be treated as internal");
        }
    }

    #[test]
    fn ipv6_internal_forms_are_caught_including_v4_mapped() {
        for s in [
            "::1",
            "fc00::1",
            "fe80::1",
            // ::ffff:127.0.0.1 is the classic bypass of a v4-only check.
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            // NAT64 well-known prefix — 169.254.169.254 reached via a gateway.
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            // IPv4-compatible ::a.b.c.d — the deprecated form.
            "::7f00:1",      // 127.0.0.1
            "::a9fe:a9fe",   // 169.254.169.254
        ] {
            assert!(is_internal_ip(ip(s)), "{s} must be treated as internal");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for s in ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"] {
            assert!(!is_internal_ip(ip(s)), "{s} must be allowed");
        }
    }

    // IP literals throughout: lookup_host resolves them without touching DNS,
    // so these stay deterministic in CI.
    #[tokio::test]
    async fn internal_destinations_are_refused() {
        for u in [
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://10.0.0.5:8080/api/jobs",
        ] {
            assert!(
                reject_internal_url(u, "*").await.is_err(),
                "{u} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn non_http_schemes_and_credentials_are_refused() {
        for u in [
            "ftp://8.8.8.8/x",
            "file:///etc/passwd",
            "gopher://8.8.8.8/",
            // Credentials are never needed for a public video and are a
            // standard way to confuse a downstream fetcher about the host.
            "http://user:pass@8.8.8.8/x",
            "not a url at all",
        ] {
            assert!(
                reject_internal_url(u, "*").await.is_err(),
                "{u} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn fly_private_networking_names_are_refused_without_resolving() {
        for u in [
            "http://whisgram.internal/",
            "http://SOMEAPP.INTERNAL/x",
            "http://printer.local/",
        ] {
            assert!(
                reject_internal_url(u, "*").await.is_err(),
                "{u} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn an_allowed_public_host_passes_every_check() {
        // IP literal + a config naming it, so the positive path is exercised
        // end-to-end without depending on DNS in CI.
        assert!(reject_internal_url("https://8.8.8.8/video.mp4", "8.8.8.8").await.is_ok());
    }

    #[tokio::test]
    async fn the_refusal_message_does_not_describe_the_network() {
        // The caller should learn "no", not what is reachable from in here.
        let msg = reject_internal_url("http://169.254.169.254/", "*").await.unwrap_err();
        assert!(!msg.contains("169.254"), "must not echo the address: {msg}");
        assert!(!msg.to_lowercase().contains("private"), "must not hint at topology: {msg}");
    }

    #[test]
    fn allowlist_matches_on_a_label_boundary() {
        let cfg = DEFAULT_ALLOWED_HOSTS;
        for good in [
            "youtube.com", "www.youtube.com", "m.youtube.com", "youtu.be",
            // Every platform the UI names must work, or Free mode refuses
            // something the page told the user it supports.
            "vimeo.com", "www.tiktok.com", "twitter.com", "x.com",
            "www.twitch.tv", "www.coursera.org",
        ] {
            assert!(host_is_allowed_by(good, cfg), "{good} should be allowed");
        }
        // Naive suffix matching lets every one of these through.
        for bad in [
            "evil-youtube.com",
            "youtube.com.attacker.net",
            "notyoutube.com",
            "attacker.net",
            "169.254.169.254",
        ] {
            assert!(!host_is_allowed_by(bad, cfg), "{bad} must NOT be allowed");
        }
    }

    #[test]
    fn allowlist_is_configurable_and_star_disables_it() {
        assert!(host_is_allowed_by("videos.example.org", "example.org"));
        assert!(
            !host_is_allowed_by("youtube.com", "example.org"),
            "the default set must not leak through a custom config"
        );
        assert!(host_is_allowed_by("anything.at.all", "*"));
    }

    #[tokio::test]
    async fn an_unlisted_host_is_refused_even_though_it_is_public() {
        // The control that survives redirects: an attacker-controlled public
        // host never reaches yt-dlp, so it cannot 302 to metadata.
        assert!(
            reject_internal_url("https://8.8.8.8/vid.mp4", DEFAULT_ALLOWED_HOSTS)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ipv6_literals_reach_the_ip_check_instead_of_failing_dns() {
        // host_str() returns these bracketed; unstripped they fell through to
        // DNS and errored, so the v6 branch was never exercised from a real
        // request and public v6 literals were wrongly refused.
        for u in [
            "http://[::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://[fe80::1]/x",
            "http://[2002:a9fe:a9fe::1]/x",  // 6to4 wrapping 169.254.169.254
        ] {
            assert!(reject_internal_url(u, "*").await.is_err(), "{u} must be refused");
        }
        // ...and a public v6 literal is now reachable rather than "unreachable".
        assert!(
            reject_internal_url("http://[2001:4860:4860::8888]/v.mp4", "*")
                .await
                .is_ok(),
            "a public IPv6 literal must be allowed"
        );
    }

    #[tokio::test]
    async fn the_jobs_path_uses_any_host_but_still_blocks_internal() {
        // Fast mode advertises 1000+ platforms, so it cannot use the allowlist.
        // ANY_HOST keeps arbitrary public hosts working while still refusing
        // addresses that resolve into our own network.
        assert!(reject_internal_url("https://8.8.8.8/v.mp4", ANY_HOST).await.is_ok());
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8080/api/jobs",
            "http://whisgram.internal/",
        ] {
            assert!(
                reject_internal_url(u, ANY_HOST).await.is_err(),
                "{u} must be refused even on the any-host path"
            );
        }
    }
}
