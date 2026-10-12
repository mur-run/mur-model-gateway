#!/usr/bin/env bash
# Aggregate the gateway's `compress summary` / `compress engine built` log
# lines into the numbers that decide whether wire compression is worth
# keeping: fired share, skip distribution, token vs byte savings, cold starts.
#
#   scripts/compress-summary-report.sh [proxy.log]
set -euo pipefail

LOG="${1:-$HOME/Library/Logs/mur-model-gateway/proxy.log}"
strip() { sed 's/\x1b\[[0-9;]*m//g' "$LOG"; }

echo "== cold starts (expect 1 per process start) =="
strip | grep -E "compress engine built|listening addr" | sed -E 's/ mur_model_gateway(::compress)?: / /' | cut -c1-120 || true

echo
echo "== compress summary totals =="
strip | grep "compress summary" | sed -E 's/.*compress summary //' \
| awk '
  {
    for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] }
    n++
    blocks += v["blocks"]; hit += v["hit"]; miss += v["miss"]; fired += v["fired"]
    small += v["small"]; marker += v["marker"]; exempt += v["exempt"]
    if (v["stale"] == "true") stale++
    before += v["before"]; after += v["after"]
    tb += v["tokens_before"]; ta += v["tokens_after"]
    ms += v["took_ms"]; if (v["took_ms"] + 0 > maxms) maxms = v["took_ms"] + 0
    if (v["fired"] + 0 > 0) fired_req++
  }
  function pct(a, b) { return b ? sprintf("%.1f%%", 100 * a / b) : "n/a" }
  END {
    printf "requests          %d (with any fired: %d)\n", n, fired_req
    printf "blocks judged     %d  hit=%d miss=%d  (hit rate %s)\n", blocks, hit, miss, pct(hit, hit + miss)
    printf "fired             %d  (%s of judged, %s of all blocks)\n", fired, pct(fired, hit + miss), pct(fired, blocks + exempt)
    printf "skipped           small=%d marker=%d exempt=%d  (requests with stale collapse: %d)\n", small, marker, exempt, stale
    printf "bytes             %d -> %d  saved %s\n", before, after, pct(before - after, before)
    printf "tokens (judged)   %d -> %d  saved %s\n", tb, ta, pct(tb - ta, tb)
    printf "time              avg %.0f ms  max %d ms\n", n ? ms / n : 0, maxms
  }'

echo
echo "== whole-body token savings (compress body tokens lines) =="
strip | grep "compress body tokens" | sed -E 's/.*compress body tokens //' \
| awk '
  {
    for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] }
    n++; b += v["body_tokens_before"]; a += v["body_tokens_after"]
  }
  END {
    if (!n) { print "none logged"; exit }
    printf "requests          %d\n", n
    printf "body tokens       %d -> %d  saved %.2f%%\n", b, a, b ? 100 * (b - a) / b : 0
  }'
