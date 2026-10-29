# Block page fixtures

One file per observed response. The classifier tests in
`crates/kage-core/src/classifier/fixture_tests.rs` run the shipped signatures
(`policies/block-signatures.yaml`) against every file here and fail when

- a fixture is not classified with its expected outcome, vendor and type;
- a vendor or block kind named in spec §12 has no fixture;
- a rule in the signature file decides no fixture;
- a fixture is missing from the table below, or the table and the file
  disagree on whether it is synthetic.

## Every fixture here is synthetic

No page in this directory was recorded from a live site. Recording a real
block page means provoking a block on a real target, which the house rules
forbid outside the field validation campaign (plan M13), so each page was
written by hand from the vendor's public documentation and the publicly known
markup of its block pages. Hosts, ids, tokens and reference numbers are
invented; no fixture contains personal data.

Two are less certain than the rest and say so in their `source` field:
`akamai-bot-manager-challenge.yaml` (script names vary by deployment) and
`f5-tspd-challenge.yaml` (spec §12 calls the F5/Shape rule a heuristic).

When M13 records real pages, add them here with `synthetic: false`, sanitize
them (replace hosts, ids, tokens and any personal data), and keep the
synthetic one only if it covers a variant the recording does not.

## File format

```yaml
synthetic: true            # false for a sanitized recording
source: what the page is modelled on, or where and when it was recorded
observation:               # what one attempt observed
  render: http | browser   # browser: `body` is the rendered DOM
  status: 403
  url: https://shop.example/products/42
  headers: [{ name, value }]
  cookies: [{ name, value }]
  browser: { script_hosts, iframe_hosts, interstitial_text,
             interstitial_detected, interstitial_resolved }
  body: |
    <html>...</html>
expect:
  outcome: challenge_failed
  vendor: cloudflare       # required unless the outcome is ok or challenge_passed
  type: managed_challenge  # required with vendor
  rule: rule-id            # optional: the rule that must decide
  vendors: [cloudflare]    # optional: the exact list of vendors seen
```

## Fixtures

| File | Origin | Expected |
|---|---|---|
| `cloudflare-managed-challenge.yaml` | synthetic | `challenge_failed`, cloudflare / managed_challenge (cf-mitigated header) |
| `cloudflare-just-a-moment-503.yaml` | synthetic | `challenge_failed`, cloudflare / managed_challenge (503 "Just a moment") |
| `cloudflare-turnstile-wall.yaml` | synthetic | `captcha`, cloudflare / turnstile |
| `cloudflare-waf-block-1020.yaml` | synthetic | `forbidden`, cloudflare / waf_block |
| `cloudflare-rate-limit-1015.yaml` | synthetic | `rate_limited`, cloudflare / rate_limit_1015 |
| `cloudflare-challenge-passed.yaml` | synthetic | `challenge_passed` (interstitial resolved, cf_clearance) |
| `akamai-access-denied.yaml` | synthetic | `forbidden`, akamai / access_denied |
| `akamai-bot-manager-challenge.yaml` | synthetic | `challenge_failed`, akamai / bot_manager_challenge |
| `akamai-reference-error.yaml` | synthetic, modelled on the field campaign (M13) | `forbidden`, akamai / access_denied (a branded denial with `x-reference-error`) |
| `datadome-captcha.yaml` | synthetic | `captcha`, datadome / captcha |
| `datadome-captcha-json.yaml` | synthetic | `captcha`, datadome / captcha (JSON answer to an XHR) |
| `datadome-interstitial.yaml` | synthetic | `challenge_failed`, datadome / interstitial |
| `datadome-hard-block.yaml` | synthetic | `forbidden`, datadome / hard_block |
| `human-press-and-hold.yaml` | synthetic | `captcha`, human / press_and_hold |
| `human-block-json.yaml` | synthetic | `captcha`, human / press_and_hold (JSON answer to an XHR) |
| `kasada-challenge.yaml` | synthetic | `challenge_failed`, kasada / kpsdk_challenge (x-kpsdk headers, ips.js) |
| `kasada-challenge-p-js.yaml` | synthetic | `challenge_failed`, kasada / kpsdk_challenge (p.js, no headers) |
| `imperva-incident.yaml` | synthetic | `forbidden`, imperva / incident |
| `imperva-incident-status-200.yaml` | synthetic | `forbidden`, imperva / incident (served with 200) |
| `aws-waf-challenge.yaml` | synthetic | `challenge_failed`, aws_waf / challenge |
| `aws-waf-captcha.yaml` | synthetic | `captcha`, aws_waf / captcha |
| `aws-waf-challenge-page.yaml` | synthetic | `challenge_failed`, aws_waf / challenge (header stripped) |
| `f5-request-rejected.yaml` | synthetic | `forbidden`, f5_shape / request_rejected |
| `f5-tspd-challenge.yaml` | synthetic | `challenge_failed`, f5_shape / js_challenge |
| `recaptcha-wall.yaml` | synthetic | `captcha`, recaptcha / recaptcha |
| `hcaptcha-wall.yaml` | synthetic | `captcha`, hcaptcha / hcaptcha |
| `generic-403.yaml` | synthetic | `forbidden`, generic / http_403 |
| `generic-429.yaml` | synthetic | `rate_limited`, generic / http_429 |
| `generic-503.yaml` | synthetic | `provider_error`, generic / server_error |
| `soft-empty-body.yaml` | synthetic | `soft_block`, generic / empty_body |
| `soft-js-required.yaml` | synthetic | `soft_block`, generic / js_required |
| `soft-blank-page.yaml` | synthetic | `soft_block`, generic / blank_page |
| `soft-login-wall.yaml` | synthetic | `soft_block`, generic / login_wall |
| `soft-consent-wall.yaml` | synthetic | `soft_block`, generic / consent_wall |

Negative cases: good responses that carry vendor markers or block-like words
and must stay `ok`.

| File | Origin | What it guards against |
|---|---|---|
| `ok-cloudflare-site.yaml` | synthetic | `__cf_bm`, `cf-ray` and Cloudflare scripts on a normal page |
| `ok-akamai-site.yaml` | synthetic | `_abck`, `bm_sz`, `ak_bmsc` on a normal page |
| `ok-datadome-site.yaml` | synthetic | the `datadome` cookie and the tag script on a normal page |
| `ok-human-site.yaml` | synthetic | `_px` cookies and the sensor snippet on a normal page |
| `ok-imperva-site.yaml` | synthetic | Imperva cookies and its injected script, no incident id |
| `ok-page-with-recaptcha-badge.yaml` | synthetic | a reCAPTCHA v3 badge and a login form on a full article |
| `ok-consent-banner.yaml` | synthetic | a cookie banner over a full article |
| `ok-spa-rendered.yaml` | synthetic | a rendered single-page app that keeps its noscript notice |
| `ok-json-api.yaml` | synthetic | a short JSON body |
| `ok-short-page.yaml` | synthetic | a short HTML page with no marker |
| `ok-404.yaml` | synthetic | a 404, which is returned as it is |
