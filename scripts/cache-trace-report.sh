#!/usr/bin/env bash
# Aggregate the gateway's `cache trace` log lines into the numbers that size
# the prompt-cache fixes: cold-start buckets, avoidable write tokens (B3 and
# the sequential investigation) and B1's tools-breakpoint target, per model.
# Every definition is the spec's (docs/specs/2026-10-13-cache-trace-design.md,
# "Cold start, avoidable, buckets"); change it there first.
#
#   scripts/cache-trace-report.sh [proxy.log]
#   MUR_REPORT_CLIENT=claude-cli scripts/cache-trace-report.sh   # one client only
#
# Estimates (`prefix_est`, `tools_est`) scale `creation` by canonical byte
# share and are labelled est; the raw `creation` beside them is the bound.
set -euo pipefail

LOG="${1:-$HOME/Library/Logs/mur-model-gateway/proxy.log}"
strip() {
  sed 's/\x1b\[[0-9;]*m//g' "$LOG" \
  | { grep "cache trace " || true; } \
  | if [ -n "${MUR_REPORT_CLIENT:-}" ]; then grep -E "client=\"?${MUR_REPORT_CLIENT}\"?( |$)" || true; else cat; fi \
  | sed -E 's/.*cache trace //'
}

strip | awk '
function num(x) { return x == "-" ? 0 : x + 0 }
function rate(t) { return t == "1h" ? 2 : 1.25 }
function ttl_ms(t) { return t == "1h" ? 3600000 : 300000 }
function pct(a, b) { return b ? sprintf("%.1f%%", 100 * a / b) : "n/a" }
function median(m, n,   i, j, t, arr) {
  for (i = 1; i <= n; i++) arr[i] = tbv[m, i]
  for (i = 2; i <= n; i++) { t = arr[i]; j = i - 1; while (j >= 1 && arr[j] > t) { arr[j + 1] = arr[j]; j-- } arr[j + 1] = t }
  return arr[int((n + 1) / 2)]
}
{
  delete v
  fb = 0
  for (i = 1; i <= NF; i++) { eq = index($i, "="); if (eq) v[substr($i, 1, eq - 1)] = substr($i, eq + 1) }
  m = v["model"]; gsub(/"/, "", m); if (m == "") m = "-"
  c = v["client"]; gsub(/"/, "", c); if (c == "") c = "-"
  if (!(m in seen_model)) { seen_model[m] = 1; mlist[++mn] = m }
  mc[m SUBSEP c] = 1
  lines[m]++; lines_c[m, c]++
  if (v["streaming"] != "true") nonstream[m]++
  if (v["status"] !~ /^2/) bad_status[m]++
  tb = num(v["tools_bytes"]); tbn[m]++; tbv[m, tbn[m]] = tb
  if (v["read"] == "-") { no_usage[m]++; next }
  usage[m]++; usage_c[m, c]++
  creation = num(v["creation"]); rd = num(v["read"])
  if (v["creation_5m"] != "-") { c5 = num(v["creation_5m"]); c1 = num(v["creation_1h"]) }
  else { fallback[m]++; fb = 1; if (v["ttl"] == "1h") { c1 = creation; c5 = 0 } else { c5 = creation; c1 = 0 } }
  cr[m] += creation; cr5[m] += c5; cr1[m] += c1; cr_c[m, c] += creation
  wr[m] += c5 * 1.25 + c1 * 2
  ttl = v["ttl"]; tmax = v["ttl_max"]; b1 = (tmax == "-") ? "5m" : tmax
  # sanity
  nt = split(v["bp_ttls"], bt, ",")
  if (v["bp_ttls"] != "-" && bt[nt] != ttl) s_ttl[m]++
  if (v["bp_ttls"] == "-" && ttl != "-") s_ttl[m]++
  seen5 = 0; for (k = 1; k <= nt; k++) { if (bt[k] == "5m") seen5 = 1; else if (bt[k] == "1h" && seen5) s_order[m]++ }
  span = v["span_bytes"]; pb = num(v["prefix_bytes"])
  if (span != "-" && pb > num(span)) s_span[m]++
  nb = split(v["bp_bytes"], bb, ",")
  if (v["bp_bytes"] != "-") { for (k = 2; k <= nb; k++) if (bb[k] + 0 < bb[k - 1] + 0) s_bb[m]++; if (bb[nb] + 0 != pb) s_bb[m]++ }
  if (v["creation_5m"] != "-" && ((ttl == "1h" && c1 == 0 && creation > 0) || (tmax == "5m" && c1 > 0))) s_split[m]++
  if (v["diverged"] ~ /@\?$/ || v["diverged"] ~ /\+$/) s_div[m]++
  if ((v["prefix_seen_ago_ms"] == "-" && num(v["inflight_same_prefix"]) > 0) || (v["tools_seen_ago_ms"] == "-" && num(v["inflight_same_tools"]) > 0)) s_inv[m]++
  ptl = v["prefix_ttl_left_ms"]; psa = v["prefix_seen_ago_ms"]; ip = num(v["inflight_same_prefix"])
  if (ttl != "-" && ip == 0 && psa != "-" && num(psa) < ttl_ms(ttl) && (ptl == "-" || num(ptl) < 0)) s_seen[m]++
  # cold start: read=0 and creation>0
  if (!(rd == 0 && creation > 0)) next
  cold[m]++; cold_c[m, c]++
  sp = num(span)
  pest = sp ? creation * pb / sp : 0
  test = sp ? creation * tb / sp : 0
  if (ttl == "-") wp = pest * rate(b1)
  else { w = 0; prev = 0; for (k = 1; k <= nb; k++) { w += (bb[k] - prev) * rate(bt[k]); prev = bb[k] }; wp = sp ? creation / sp * w : 0 }
  wt = test * rate(b1)
  split(v["bp"], bps, ","); tseg = substr(bps[1], 7); mseg = substr(bps[3], 6)
  if (ttl == "-") { b = "no_prefix_bp"; sub_ = (mseg == "none") ? "no_cache_control" : (mseg == "auto" ? "top_level_only" : "msgs_only"); subn[m, sub_]++ }
  else if (ptl == "-" && ip == 0) b = "first"
  else if ((ptl == "-" || num(ptl) < 0) && ip > 0) b = "concurrent"
  else if (num(ptl) >= 0) b = "sequential"
  else b = "expired"
  bn[m, b]++; bcr[m, b] += creation; bpe[m, b] += pest; bw[m, b] += wp; if (fb) bfb[m, b]++
  if (tseg == "none" && num(v["tools_count"]) > 0 && v["tools_ttl_left_ms"] != "-" && num(v["tools_ttl_left_ms"]) >= 0) {
    b1n[m]++; b1cr[m] += creation; b1te[m] += test; b1w[m] += wt; b1b[m, b]++
    if (b == "concurrent" || b == "sequential") { ovn[m]++; ovte[m] += test }
  }
}
END {
  if (!mn) { print "none logged"; exit }
  for (i = 2; i <= mn; i++) { t = mlist[i]; j = i - 1; while (j >= 1 && mlist[j] > t) { mlist[j + 1] = mlist[j]; j-- } mlist[j + 1] = t }
  nbk = split("first concurrent sequential expired no_prefix_bp", bk, " ")
  for (mi = 1; mi <= mn; mi++) {
    m = mlist[mi]
    printf "== model %s ==\n", m
    printf "lines             %d  with usage %d  cold starts %d  non-streaming %d (%s)\n", lines[m], usage[m], cold[m], nonstream[m], pct(nonstream[m], lines[m])
    printf "writes (raw)      creation %d  5m %d  1h %d  weighted %.1f  fallback %d\n", cr[m], cr5[m], cr1[m], wr[m], fallback[m]
    for (k in mc) { split(k, p, SUBSEP); if (p[1] != m) continue; c = p[2]
      printf "  client %-12s lines=%d  usage=%d  cold=%d  creation=%d\n", c, lines_c[m, c], usage_c[m, c], cold_c[m, c], cr_c[m, c] }
    printf "buckets           prefix_est = creation * prefix_bytes / span_bytes, weighted at the prefix breakpoint TTL\n"
    for (bi = 1; bi <= nbk; bi++) { b = bk[bi]
      printf "  %-14s n=%d  creation=%d  prefix_est=%.1f  weighted=%.1f  fallback=%d  (%s of weighted writes)\n", b, bn[m, b], bcr[m, b], bpe[m, b], bw[m, b], bfb[m, b], pct(bw[m, b], wr[m])
      if (b == "no_prefix_bp") printf "    sub-classes   no_cache_control=%d  top_level_only=%d  msgs_only=%d\n", subn[m, "no_cache_control"], subn[m, "top_level_only"], subn[m, "msgs_only"] }
    an = bn[m, "concurrent"] + bn[m, "sequential"]; acr = bcr[m, "concurrent"] + bcr[m, "sequential"]
    ape = bpe[m, "concurrent"] + bpe[m, "sequential"]; aw = bw[m, "concurrent"] + bw[m, "sequential"]
    printf "avoidable (B3)    n=%d  creation=%d  prefix_est=%.1f  weighted=%.1f  (%s of weighted writes)\n", an, acr, ape, aw, pct(aw, wr[m])
    printf "B1 tools bp       n=%d  creation=%d  tools_est=%.1f  weighted@b1_ttl=%.1f  overlap with B3: n=%d tools_est=%.1f\n", b1n[m], b1cr[m], b1te[m], b1w[m], ovn[m], ovte[m]
    printf "  by bucket       first=%d  concurrent=%d  sequential=%d  expired=%d  no_prefix_bp=%d\n", b1b[m, "first"], b1b[m, "concurrent"], b1b[m, "sequential"], b1b[m, "expired"], b1b[m, "no_prefix_bp"]
    mn_tb = tbv[m, 1]; for (k = 2; k <= tbn[m]; k++) if (tbv[m, k] < mn_tb) mn_tb = tbv[m, k]
    printf "  tools_bytes     min=%d  median=%d\n", mn_tb, median(m, tbn[m])
    printf "sanity            ttl!=last(bp_ttls)=%d  ttl_order=%d  prefix>span=%d  bp_bytes=%d  split_disagree=%d  status!=2xx=%d  read=-:%d  diverged?+=%d  invariant=%d  seen_vs_ttl_left=%d\n", s_ttl[m], s_order[m], s_span[m], s_bb[m], s_split[m], bad_status[m], no_usage[m], s_div[m], s_inv[m], s_seen[m]
    print ""
  }
}'
