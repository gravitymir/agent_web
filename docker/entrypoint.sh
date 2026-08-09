#!/bin/sh
# Guest container entrypoint.
#
# Runs as root to install egress firewall rules, then drops to the unprivileged
# 'guest' user to run the app. SSRF hardening: the container may reach the public
# internet (needed for the model API + WebFetch/WebSearch) but NOT private /
# internal ranges — the host, the LAN, and cloud metadata (169.254.169.254).
#
# Loopback (127.0.0.0/8) is left open so Docker's embedded DNS (127.0.0.11)
# keeps resolving. Requires --cap-add NET_ADMIN. Set CWI_EGRESS=off to skip.

set -eu

if [ "${CWI_EGRESS:-on}" != "off" ]; then
  # Allow DNS and loopback FIRST (rules are matched top-to-bottom). Docker's
  # embedded resolver (127.0.0.11) forwards to a private upstream, so blindly
  # rejecting private ranges would break all name resolution.
  iptables -A OUTPUT -o lo -j ACCEPT
  iptables -A OUTPUT -p udp --dport 53 -j ACCEPT
  iptables -A OUTPUT -p tcp --dport 53 -j ACCEPT
  # Then reject private / internal ranges (host, LAN, cloud metadata).
  for net in \
    10.0.0.0/8 \
    172.16.0.0/12 \
    192.168.0.0/16 \
    169.254.0.0/16 \
    100.64.0.0/10 \
    ; do
    iptables -A OUTPUT -d "$net" -j REJECT
  done
  # IPv6 unique-local + link-local (best-effort; IPv6 is usually off here).
  for net6 in fc00::/7 fe80::/10 ; do
    ip6tables -A OUTPUT -d "$net6" -j REJECT 2>/dev/null || true
  done
fi

exec gosu guest /app/agent_web
