#!/bin/bash
set -u

RESOLVER="https://doh-edge.vasie1337.workers.dev/dns-query"

# list of realistic domains to query — reuse what a real browser would hit
DOMAINS=(
  google.com github.com youtube.com cloudflare.com wikipedia.org
  reddit.com twitter.com stackoverflow.com hacker-news.firebaseio.com
  news.ycombinator.com amazon.com netflix.com apple.com microsoft.com
  doc.rust-lang.org docs.python.org mdn.mozilla.org developer.mozilla.org
  archlinux.org debian.org
)

QTYPES_HEX=("\x00\x01" "\x00\x1c" "\x00\x41")  # A, AAAA, HTTPS

# encode a domain as length-prefixed labels + null terminator
encode_qname() {
  local domain="$1"
  local out=""
  local IFS='.'
  for label in $domain; do
    out+=$(printf '\\x%02x' ${#label})
    out+=$(printf '%s' "$label" | xxd -p | sed 's/\(..\)/\\x\1/g' | tr -d '\n')
  done
  out+='\x00'
  echo "$out"
}

build_query() {
  local qname_hex="$1"
  local qtype_hex="$2"
  # header: id=0xAAAA, flags=0x0100 (RD), qdcount=1, anc/ns/ar=0
  printf '\xaa\xaa\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00'
  printf "$qname_hex"
  printf "$qtype_hex\x00\x01"
}

query_once() {
  local domain="$1"
  local qtype_hex="$2"
  local qname_hex=$(encode_qname "$domain")
  build_query "$qname_hex" "$qtype_hex" | \
    curl -sS -o /dev/null \
      -H 'content-type: application/dns-message' \
      -H 'accept: application/dns-message' \
      --data-binary @- \
      "$RESOLVER"
}

echo "[$(date -u +%H:%M:%S)] starting loadgen from $(curl -sS ifconfig.me)"

while true; do
  domain="${DOMAINS[$RANDOM % ${#DOMAINS[@]}]}"
  qtype="${QTYPES_HEX[$RANDOM % ${#QTYPES_HEX[@]}]}"
  query_once "$domain" "$qtype"
  # sleep 0.5–2s, roughly like idle browser traffic
  sleep "$(awk -v min=0.5 -v max=2 'BEGIN{srand(); print min+rand()*(max-min)}')"
done
