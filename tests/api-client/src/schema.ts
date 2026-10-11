/**
 * The types of the Kage API, generated from crates/kage-api/openapi.json
 * by openapi-typescript. Do not edit: run `just openapi`.
 */

export interface paths {
    "/healthz": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Liveness probe: answers as long as the process is running. It checks no
         *     dependency; readiness is a separate endpoint (spec §16.1).
         */
        get: operations["healthz"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/metrics": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Prometheus metrics (spec §20): Kage's own series, the personas' states
         *     and self-test scores, and the sidecars' series merged in.
         */
        get: operations["metrics"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/readyz": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Readiness probe: the database answers, at least one persona is ready
         *     behind a usable proxy, and at least one sidecar answers (spec §16.1).
         *     `503` otherwise, and from the moment the server is told to stop.
         */
        get: operations["readyz"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/audit": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Read the audit trail, newest first: every admin call through the API
         *     and every change an operator made at the command line.
         */
        get: operations["list_audit"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/keys": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** List the API keys. */
        get: operations["list_keys"];
        put?: never;
        /** Make an API key. Its token is in the answer and nowhere else. */
        post: operations["create_key"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/keys/{id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        post?: never;
        /**
         * Disable an API key. It stops authenticating at once; its row and its
         *     audit trail stay. Disabling a disabled key changes nothing.
         */
        delete: operations["revoke_key"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/keys/{id}/rotate": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Give an API key a new secret. The old token stops working at once; the
         *     new one is in the answer and nowhere else.
         */
        post: operations["rotate_key"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/personas": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** List the personas. */
        get: operations["list_personas"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/personas/{id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** One persona. */
        get: operations["get_persona"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/personas/{id}/regenerate": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Replace a persona with a new one behind the same proxy. The new
         *     persona inherits nothing.
         */
        post: operations["regenerate_persona"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/personas/{id}/retire": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Retire a persona: its sessions end and its http cookies are deleted at
         *     once, its profile is archived and later deleted, and its proxy gets a
         *     new persona at the manager's next sweep. Retiring a retired persona
         *     changes nothing.
         */
        post: operations["retire_persona"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/personas/{id}/selftest": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Run the self-tests of a persona, in the server.
         * @description The http path is tested when the persona has one and an echo is
         *     configured; the browser path in the persona's own context, through the
         *     scheduler's gate and the forwarder. A failed test cools the persona
         *     until one passes; a passed one ends a cooling that waited for it. This
         *     may take a minute or two.
         */
        post: operations["selftest_persona"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/policies/reload": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Read the policy files again, now. An invalid file changes nothing: the
         *     policies in force stay, and the answer says what is wrong.
         */
        post: operations["reload_policies"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/pool": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** The pool and the bandwidth budget at a glance. */
        get: operations["get_pool"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/proxies": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** List the proxies. */
        get: operations["list_proxies"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/proxies/{id}/health": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /** Probe a proxy now: reachability, latency, the exit address, a leak. */
        post: operations["check_proxy"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/admin/proxies/{id}/replace": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Ask the provider to replace a proxy, inside the replacement budget.
         *     The proxy's persona is retired when the proxy is. A refusal by the
         *     budget or by the provider is an `outcome`, not an error.
         */
        post: operations["replace_proxy"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/artifacts/{id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * What was kept of a page, streamed.
         * @description The media type is the stored one. A text body that is kept compressed
         *     is sent compressed (`Content-Encoding: gzip`) to a client that takes
         *     it, and unpacked for one that does not.
         */
        get: operations["get_artifact"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/fetch": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Fetch one page.
         * @description The answer is `200` with the result whenever a fetch was made, whatever
         *     the site did: a block is `outcome`, with `block` saying who and how. A
         *     request that cannot be made as asked is a problem document with a
         *     stable `code`. A fetch that takes longer than the server's wall time
         *     (180 s unless configured) is answered `202`; `GET /v1/results/{id}`
         *     has its result when it ends.
         */
        post: operations["fetch"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/policies/domains/{domain}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * What Kage would do with a request for a host.
         * @description The policy is resolved as a request's would be: the host's own entry,
         *     then its parent domains', then the defaults. Nothing is fetched and no
         *     name is looked up.
         */
        get: operations["preview_policy"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/results/{id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * The result of a fetch, by the request's id.
         * @description `200` with the result once the fetch has ended; `202` while it runs.
         *     What only the synchronous answer has (`pacing`, `behavior`, `console`,
         *     `side_effects`) is null here.
         */
        get: operations["get_result"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/sessions": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Open a session, or return the open one.
         * @description `201` for a new session, `200` for one that is open under the id. The
         *     persona is the ready one with the fewest open sessions among those the
         *     hints and the domain's policy allow.
         */
        post: operations["open_session"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/v1/sessions/{id}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Look at a session. Looking is not using: the session's idle time does
         *     not start again.
         */
        get: operations["get_session"];
        put?: never;
        post?: never;
        /** End a session. Its persona goes on; the id is free again. */
        delete: operations["end_session"];
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
}
export type webhooks = Record<string, never>;
export interface components {
    schemas: {
        /** @description Any JSON value. */
        AnyJson: unknown;
        /** @description One try at a request by one persona. */
        AttemptView: {
            /**
             * Format: int32
             * @description 1 for the first attempt.
             */
            n: number;
            /** @description How it ended. */
            outcome: components["schemas"]["OutcomeField"];
            /** @description The persona that made it, as in `persona.id`. */
            persona: string;
            /**
             * @description Why it ended as it did when the site did not say, for example
             *     `proxy_failed`.
             */
            reason?: string | null;
            /** @description The path it took. */
            render: components["schemas"]["RenderTaken"];
            /** @description The seed its behavior and pauses were drawn from. Admin keys only. */
            seed?: string | null;
            /**
             * Format: int32
             * @description The status of the response, when one arrived.
             */
            status?: number | null;
            /**
             * Format: int64
             * @description How long it took, in milliseconds.
             */
            total_ms: number;
        };
        /** @description A page of the audit trail. */
        AuditPage: {
            /** @description The events, newest first. */
            events: components["schemas"]["AuditView"][];
            /**
             * Format: uuid
             * @description Pass it as `cursor` for the events before these; null at the end.
             */
            next_cursor?: string | null;
        };
        /** @description One entry of the audit trail. */
        AuditView: {
            /** @description What was done, for example `admin.persona.retire`. */
            action: string;
            /** @description Who did it: `api_key`, `cli` or `system`. */
            actor: string;
            /**
             * Format: uuid
             * @description The key, when the actor was one.
             */
            api_key_id?: string | null;
            /**
             * Format: date-time
             * @description When it happened.
             */
            at: string;
            /** @description What came of it. Never a secret. */
            details: components["schemas"]["AnyJson"];
            /**
             * Format: uuid
             * @description The event's id; ids sort by time.
             */
            id: string;
            /** @description `success`, `failure` or `denied`. */
            outcome: string;
            /** @description What it was done to. */
            target?: string | null;
        };
        /**
         * @description What the persona does on a browser page, where the request wants less
         *     than the domain policy's default. What is left out is the policy's.
         *     Ignored, and reported as ignored, when the policy sets
         *     `behavior_locked`.
         *
         *     What it costs: with the policy's default a page takes about 35 s
         *     (median), which is about 100 pages an hour per persona. `scroll_depth:
         *     0.3, dwell_ms: 3000` is about 11 s a page and still looks like someone
         *     skimming a listing; `scroll_depth: 0, dwell_ms: 3000` is about 5 s.
         *     There is no bare load: the pointer always moves, and the dwell never
         *     falls below the policy's `min_dwell` (2 s unless a domain says
         *     otherwise).
         */
        BehaviorField: {
            /**
             * Format: int64
             * @description The median of the time spent on the page after it loaded, in
             *     milliseconds. Each attempt's dwell is drawn around it and is never
             *     shorter than the policy's `min_dwell`. Default: the persona's own
             *     human dwell (a median of about 6 s).
             * @example 3000
             */
            dwell_ms?: number | null;
            referer_strategy?: components["schemas"]["RefererStrategyField"] | null;
            /**
             * Format: double
             * @description How far down the page to scroll, from 0 to 1; 0 for no scrolling.
             *     Default: drawn between 0.6 and 0.9. Scrolling with its reading
             *     pauses is most of a page's time.
             * @example 0.3
             */
            scroll_depth?: number | null;
        };
        /**
         * @description Whose the behavior on the page was.
         * @enum {string}
         */
        BehaviorSource: "policy" | "request";
        /** @description What the persona did on the page of the last browser attempt. */
        BehaviorView: {
            /** @description Whose it was. */
            applied: components["schemas"]["BehaviorSource"];
            /**
             * Format: int64
             * @description The time spent on the page after it loaded, in milliseconds.
             */
            dwell_ms?: number | null;
            /**
             * Format: int64
             * @description Movements of the pointer.
             */
            moves?: number | null;
            /**
             * Format: int64
             * @description Notches of the wheel.
             */
            notches?: number | null;
            /** @description How the page was arrived at. */
            referer_strategy: components["schemas"]["RefererStrategyField"];
            /**
             * @description Whether the request asked for behavior that the policy's lock
             *     overrode.
             */
            request_ignored: boolean;
            /**
             * Format: double
             * @description How far the page was scrolled; null for no scrolling.
             */
            scroll_depth?: number | null;
            /** @description The seed all of it was drawn from. Admin keys only. */
            seed?: string | null;
        };
        /** @description Who blocked, and how (spec §12). */
        BlockView: {
            /**
             * Format: float
             * @description How sure the signature is, from 0 to 1.
             */
            confidence: number;
            /** @description The markers that matched. */
            evidence: string[];
            /** @description The kind of block, for example `managed_challenge`. */
            type: string;
            /** @description The vendor, for example `cloudflare`. */
            vendor: string;
        };
        /** @description What readiness was found to be. */
        Checks: {
            /** @description The database answers. */
            database: boolean;
            /**
             * @description At least one persona is ready (active, warmed up) behind a proxy
             *     that can carry traffic.
             */
            persona: boolean;
            /** @description At least one browser sidecar answers. */
            sidecar: boolean;
        };
        /**
         * @description The operating system a persona claims.
         * @enum {string}
         */
        ClaimedOsField: "windows" | "macos" | "linux";
        /** @description One entry of the browser's log. */
        ConsoleView: {
            /** @description `log`, `info`, `warning`, `error` or `debug`. */
            level: string;
            /** @description Where the entry comes from. */
            source: string;
            /** @description The text. */
            text: string;
            /** @description The URL it names, if any. */
            url?: string | null;
        };
        /**
         * @description A browser engine.
         * @enum {string}
         */
        EngineField: "chromium" | "firefox";
        /**
         * @description The stable codes of the API's errors (spec §16.1). The name is
         *     what a problem document carries as `code`.
         * @enum {string}
         */
        ErrorCode: "missing_key" | "invalid_key" | "key_disabled" | "scope_missing" | "rate_limited" | "concurrency_limited" | "budget_exhausted" | "invalid_request" | "validation_failed" | "invalid_url" | "ssrf_denied" | "header_denied" | "header_invalid" | "header_not_placeable" | "render_include_conflict" | "render_kind_conflict" | "render_needs_http" | "request_kind_needs_referer" | "preflight_required" | "timeout_too_short" | "not_available_yet" | "invalid_pacing" | "invalid_session_id" | "session_not_found" | "session_persona_conflict" | "no_matching_persona" | "not_found" | "method_not_allowed" | "result_not_found" | "artifact_not_found" | "persona_not_found" | "proxy_not_found" | "key_not_found" | "conflict" | "payload_too_large" | "uri_too_long" | "unsupported_media_type" | "request_timeout" | "unavailable" | "internal";
        /** @description The answer to a fetch that is not finished: `202`. */
        FetchAccepted: {
            /**
             * Format: uuid
             * @description The request's id.
             */
            id: string;
            /**
             * Format: uuid
             * @description The job that carries the fetch on. Null until milestone M9: the
             *     fetch runs on by itself and its result is stored.
             */
            job_id?: string | null;
            /**
             * @description Where the result will be: `GET` it until it is `finished`.
             * @example /v1/results/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e
             */
            result: string;
            /** @description Always `running`. */
            state: components["schemas"]["ResultState"];
        };
        /**
         * @description One fetch (spec §16.4).
         * @example {
         *       "behavior": {
         *         "dwell_ms": 3000,
         *         "scroll_depth": 0.3
         *       },
         *       "include": [
         *         "dom",
         *         "headers",
         *         "xhr"
         *       ],
         *       "persona": {
         *         "countries": [
         *           "DE",
         *           "AT"
         *         ],
         *         "engines": [
         *           "chromium"
         *         ]
         *       },
         *       "render": "auto",
         *       "timeout_ms": 60000,
         *       "url": "https://example.org/listing?page=2",
         *       "wait": {
         *         "until": "network_idle"
         *       }
         *     }
         */
        FetchRequest: {
            behavior?: components["schemas"]["BehaviorField"] | null;
            /**
             * @description The body of a `POST`, as text; needs `render: http` and a
             *     `Content-Type` header.
             */
            body?: string | null;
            /**
             * Format: int64
             * @description How long a cached result may be served for, in seconds. The cache
             *     arrives with milestone M11; until then only 0 is accepted.
             */
            cache_ttl_s?: number;
            extract?: components["schemas"]["FreeForm"] | null;
            /**
             * @description Headers of the request's own; at most 32, and only with
             *     `render: http`. A browser sends its own headers and cannot be given
             *     others without a page noticing. Headers that are the persona's
             *     identity or the transport's are refused by name (`User-Agent`,
             *     `Accept-Language`, `Accept-Encoding`, `Cookie`, `Origin`, `Sec-*`,
             *     `Proxy-*`, `Host` and others). A `Referer` names the page the
             *     request comes from.
             */
            headers?: {
                [key: string]: string;
            };
            /**
             * @description The parts of the result to return and to keep. Default: `dom` and
             *     `headers`, or `raw` and `headers` with `render: http`.
             */
            include?: components["schemas"]["Include"][] | null;
            /**
             * Format: int64
             * @description The largest body and document kept, in bytes; a longer one is cut.
             */
            max_body_bytes?: number | null;
            /**
             * Format: int32
             * @description Retries after the first attempt, each by another persona. Default:
             *     the policy's (2).
             */
            max_retries?: number | null;
            /**
             * @description `GET`, `HEAD` or `POST`. Anything but `GET` needs `render: http`.
             * @example GET
             */
            method?: string;
            pacing?: components["schemas"]["PacingField"] | null;
            pdf?: components["schemas"]["PdfField"] | null;
            persona?: components["schemas"]["PersonaHints"] | null;
            /** @description The path. */
            render?: components["schemas"]["RenderField"];
            /**
             * @description What the request is on the http path; anything but `navigation`
             *     needs `render: http` and a `Referer` header naming the page that
             *     makes the request.
             */
            request_kind?: components["schemas"]["RequestKindField"];
            /** @description What to do about robots.txt. */
            robots?: components["schemas"]["RobotsField"];
            screenshot?: components["schemas"]["ScreenshotField"] | null;
            /**
             * @description An open session of this key: the request is made by the session's
             *     persona and by no other. Needs the `sessions` scope. A fetch never
             *     opens a session; `POST /v1/sessions` does.
             * @example proj-a-42
             */
            session_id?: string | null;
            /**
             * Format: int64
             * @description The longest one attempt may take, in milliseconds: at least 15000
             *     when a browser may make it (`browser` and `auto`), at least 1000
             *     with `http`, at most 300000. Default: 60000 in a browser, 30000 on
             *     the http path.
             */
            timeout_ms?: number | null;
            /**
             * @description The page: an `http` or `https` URL of at most 8192 bytes. Hosts
             *     that name no public site are refused (`ssrf_denied`).
             * @example https://example.org/listing?page=2
             */
            url: string;
            wait?: components["schemas"]["WaitField"] | null;
            xhr?: components["schemas"]["XhrField"] | null;
        };
        /** @description The result of a fetch (spec §16.5). */
        FetchResult: {
            /**
             * @description What was stored of the page, by kind (`dom_html`, `raw_body`,
             *     `screenshot`, `pdf`): ids for `GET /v1/artifacts/{id}`.
             */
            artifacts: {
                [key: string]: string;
            };
            /** @description Every attempt, in order. */
            attempts: components["schemas"]["AttemptView"][];
            behavior?: components["schemas"]["BehaviorView"] | null;
            block?: components["schemas"]["BlockView"] | null;
            /** @description Always false until milestone M11. */
            cached: boolean;
            /**
             * @description The browser's log; with `include: ["console"]`, in the answer of
             *     `POST /v1/fetch` only.
             */
            console?: components["schemas"]["ConsoleView"][] | null;
            /**
             * Format: date-time
             * @description When the result was stored.
             */
            created_at: string;
            /**
             * @description The document as the browser had it after the page settled; with
             *     `include: ["dom"]`. Null on the http path, which has no DOM: the
             *     document is `raw_body` there.
             */
            dom_html?: string | null;
            /** @description Whether the request moved from the http path to a browser. */
            escalated: boolean;
            extract?: components["schemas"]["FreeForm"] | null;
            /** @description The URL after redirects. */
            final_url?: string | null;
            /** @description The headers of the main response; with `include: ["headers"]`. */
            headers?: components["schemas"]["HeaderView"][] | null;
            /**
             * Format: uuid
             * @description The request's id, as in `X-Request-Id`; `GET /v1/results/{id}`
             *     returns this document again.
             */
            id: string;
            /**
             * @description How the request ended. A block is an outcome, not an HTTP error:
             *     the status of this answer is 200 whenever a fetch was made.
             */
            outcome: components["schemas"]["OutcomeField"];
            pacing?: components["schemas"]["PacingView"] | null;
            persona?: components["schemas"]["PersonaView"] | null;
            /**
             * @description The body of the main response as text; with `include: ["raw"]`,
             *     and on the http path with `include: ["dom"]` too. Null when the
             *     body is not text: `artifacts.raw_body` has it then.
             */
            raw_body?: string | null;
            /**
             * @description Why it ended without the site's verdict, as a stable name:
             *     `no_persona`, `domain_paused`, `egress_unsafe`,
             *     `bandwidth_exhausted`, `proxy_failed`, `timeout` and others.
             */
            reason?: string | null;
            /** @description Every URL visited before `final_url`, in order. */
            redirects: string[];
            /**
             * Format: date-time
             * @description When asking again is worth it, when Kage knows: the end of a
             *     domain's pause or of a persona's cooldown.
             */
            retry_at?: string | null;
            robots?: components["schemas"]["FreeForm"] | null;
            /** @description The session the request was made in. */
            session_id?: string | null;
            /**
             * @description Events a capture fired in the page (`resize` for a full-page
             *     screenshot on chromium, `beforeprint` and `afterprint` for a PDF).
             */
            side_effects: string[];
            /**
             * @description Always `finished` here; a fetch still under way is a
             *     [`FetchAccepted`].
             */
            state: components["schemas"]["ResultState"];
            /**
             * Format: int32
             * @description The status of the last response.
             */
            status?: number | null;
            /** @description Time per phase of the last attempt. */
            timing: components["schemas"]["TimingView"];
            xhr?: components["schemas"]["XhrView"] | null;
        };
        /** @description A JSON object with any members; what they are is said where it is used. */
        FreeForm: {
            [key: string]: unknown;
        };
        /** @description One response header. A list keeps the order and repeated names. */
        HeaderView: {
            /** @description The name as received. */
            name: string;
            /**
             * @description The value. Of a `Set-Cookie` only the cookie's name and attributes:
             *     its value reads `[redacted]` unless the operator exposes it.
             */
            value: string;
        };
        /**
         * @description A part of the result a request asks for.
         * @enum {string}
         */
        Include: "dom" | "raw" | "headers" | "extract" | "xhr" | "screenshot" | "pdf" | "console";
        /** @description A key with its token, which is shown this once. */
        KeyCreated: {
            /** @description The key. */
            key: components["schemas"]["KeyView"];
            /**
             * @description The token: `Authorization: Bearer <token>`. Kage keeps only a hash
             *     of its secret and cannot show it again.
             * @example kage_0199c1f26b7a7c3e9d412f8a5b6c7d8e_k3J...
             */
            token: string;
        };
        /** @description The keys. */
        KeyList: {
            /** @description The keys, oldest first, disabled ones included. */
            keys: components["schemas"]["KeyView"][];
        };
        /** @description What a new key is to be. */
        KeyRequest: {
            /**
             * Format: int32
             * @description Pages per day (UTC); no budget when left out.
             */
            daily_page_budget?: number | null;
            /**
             * Format: int32
             * @description Requests in flight at once; 4 when left out.
             */
            max_concurrent?: number | null;
            /**
             * @description A name for humans: 1 to 100 characters.
             * @example project-a
             */
            name: string;
            /**
             * Format: int32
             * @description Requests per minute; 60 when left out.
             */
            requests_per_minute?: number | null;
            /** @description What the key may do; at least one scope. */
            scopes: components["schemas"]["ScopeField"][];
        };
        /** @description An API key. Its secret is not here: only its hash is stored. */
        KeyView: {
            /** @description Whether it may authenticate requests. */
            active: boolean;
            /**
             * Format: date-time
             * @description When it was made.
             */
            created_at: string;
            /**
             * Format: int32
             * @description Pages per day (UTC); null for no budget.
             */
            daily_page_budget?: number | null;
            /**
             * Format: date-time
             * @description When it was disabled.
             */
            disabled_at?: string | null;
            /**
             * Format: uuid
             * @description The key's id; the `<keyid>` of its token, with hyphens.
             */
            id: string;
            /**
             * Format: date-time
             * @description When it last authenticated a request, to the minute.
             */
            last_used_at?: string | null;
            /**
             * Format: int32
             * @description Requests in flight at once.
             */
            max_concurrent: number;
            /** @description Its name. */
            name: string;
            /**
             * Format: int32
             * @description Requests per minute.
             */
            requests_per_minute: number;
            /** @description What it may do. */
            scopes: components["schemas"]["ScopeField"][];
        };
        /** @description Body of a liveness response. */
        Liveness: {
            /** @description Current liveness status. */
            status: components["schemas"]["LivenessStatus"];
        };
        /**
         * @description Whether the process can answer requests.
         * @enum {string}
         */
        LivenessStatus: "ok";
        /**
         * @description How an attempt or a request ended (spec §2).
         * @enum {string}
         */
        OutcomeField: "ok" | "challenge_passed" | "challenge_failed" | "captcha" | "rate_limited" | "forbidden" | "soft_block" | "provider_error" | "timeout" | "ssrf_denied" | "robots_denied";
        /** @description The pacing a request asks for (decision D9). Give `profile` or `custom`. */
        PacingField: {
            custom?: components["schemas"]["FreeForm"] | null;
            /**
             * @description The name of a profile of `policies/pacing-profiles.yaml`.
             * @example human
             */
            profile?: string | null;
        };
        /** @description The pacing that applied (spec §11). */
        PacingView: {
            /** @description The profile's name; `custom` for one sent with the request. */
            applied: string;
            /**
             * @description Whether the request asked for pacing that the policy's lock
             *     overrode.
             */
            request_ignored: boolean;
            /**
             * Format: int64
             * @description How long the request waited for personas and their slots.
             */
            waited_ms: number;
        };
        /**
         * @description A paper size.
         * @enum {string}
         */
        PaperFormatField: "a4" | "letter";
        /**
         * @description How a PDF is printed. Printing fires `beforeprint` and `afterprint` in
         *     the page, as printing by hand does; the result names both.
         */
        PdfField: {
            /** @description The paper size. */
            format?: components["schemas"]["PaperFormatField"];
            /** @description Landscape orientation. */
            landscape?: boolean;
            /** @description Print background graphics. */
            print_background?: boolean;
        };
        /** @description A persona, as an operator sees it. */
        PersonaAdminView: {
            /**
             * Format: int32
             * @description The major version of its browser.
             */
            browser_major: number;
            /** @description Whether its browser is open now. */
            browser_open: boolean;
            /** @description The operating system it claims. */
            claimed_os: components["schemas"]["ClaimedOsField"];
            /** @description Why it cools: `proxy_backoff`, `selftest_failed` or `geo_changed`. */
            cooling_reason?: string | null;
            /** @description The country its address is in. */
            country?: string | null;
            /**
             * Format: date-time
             * @description When it was generated.
             */
            created_at: string;
            /** @description Its engine. */
            engine: components["schemas"]["EngineField"];
            fingerprint?: components["schemas"]["FreeForm"] | null;
            /**
             * @description The emulation profile of its http path; null for a browser-only
             *     persona.
             */
            http_profile?: string | null;
            /**
             * Format: uuid
             * @description The persona's id.
             */
            id: string;
            /** @description Its languages, most preferred first. */
            languages: string[];
            /** @description Its locale. */
            locale: string;
            /**
             * Format: uuid
             * @description The proxy it lives behind.
             */
            proxy_id: string;
            /** @description Whether real work may use it: active and warmed up. */
            ready: boolean;
            /** @description Why it was retired. */
            retire_reason?: string | null;
            /**
             * Format: date-time
             * @description When it was retired.
             */
            retired_at?: string | null;
            /** @description Its screen: `width`, `height`, `dpr`, `color_depth`. */
            screen: components["schemas"]["FreeForm"];
            /** @description `active`, `cooling` or `retired`. */
            state: string;
            /** @description Its time zone. */
            timezone: string;
            /** @description What its browser sends as `User-Agent`. */
            user_agent: string;
            /**
             * Format: date-time
             * @description When it was warmed up.
             */
            warmed_at?: string | null;
            /**
             * Format: int32
             * @description Warm-up visits made.
             */
            warmup_visits: number;
        };
        /** @description Which personas a request would have. */
        PersonaHints: {
            /**
             * @description Countries the request would rather be fetched from, as two-letter
             *     codes. A preference, not a filter: the result says where the
             *     persona was. Against a session's persona it is a condition.
             * @example [
             *       "DE",
             *       "AT"
             *     ]
             */
            countries?: string[];
            /** @description The engines the request accepts; empty for any the policy allows. */
            engines?: components["schemas"]["EngineField"][];
        };
        /** @description The personas. */
        PersonaList: {
            /** @description The personas, oldest first. */
            personas: components["schemas"]["PersonaAdminView"][];
        };
        /** @description The persona that made a request, as its caller may know it. */
        PersonaView: {
            /** @description The operating system it claims. */
            claimed_os: components["schemas"]["ClaimedOsField"];
            /**
             * @description The country its address is in.
             * @example DE
             */
            country?: string | null;
            /** @description Its engine. */
            engine: components["schemas"]["EngineField"];
            /**
             * @description For a key without the `admin` scope a handle (`p_` and 16
             *     hexadecimal characters) that is the same for this key and this
             *     persona every time and means nothing to another key; for an admin
             *     key the persona's id.
             * @example p_3f9a6c0d12e4b87a
             */
            id: string;
        };
        /** @description What is in force after a reload. */
        PoliciesView: {
            /** @description Domains with an entry of their own. */
            domains: number;
            /** @description Pacing profiles. */
            pacing_profiles: number;
            /** @description Block signature rules. */
            signature_rules: number;
        };
        /** @description The policy in force for a host. */
        PolicyPreview: {
            /**
             * @description The path a request with `render: auto` takes first: `http` when the
             *     policy says so, else `browser`.
             * @example browser
             */
            first_render: string;
            /**
             * @description The pacing profile the policy names, in the format of
             *     `policies/pacing-profiles.yaml`.
             */
            pacing_profile: components["schemas"]["FreeForm"];
            /**
             * Format: date-time
             * @description Until when the host is paused because too many requests to it were
             *     blocked; null when it is not. A request made before then ends as
             *     `provider_error` with reason `domain_paused`.
             */
            paused_until?: string | null;
            /**
             * @description Every field of the policy as it resolves for the host: `host`,
             *     `matched` (the entry that applies), `inherits`, `render`,
             *     `countries`, `engines`, `pacing`, `pacing_locked`, `robots`,
             *     `referer_strategy`, `active_hours`, `min_dwell`, `behavior_locked`,
             *     `retries`, `breaker_threshold`, `breaker_pause`, `cooldowns`.
             */
            policy: components["schemas"]["FreeForm"];
        };
        /** @description The pool at a glance. */
        PoolView: {
            bandwidth?: components["schemas"]["FreeForm"] | null;
            /**
             * @description The personas: how many per state and per engine, how many are
             *     ready, and how many browsers are open.
             */
            personas: components["schemas"]["FreeForm"];
            /**
             * @description The proxies: by state, how many are usable, whether a leak is on
             *     record, and the replacement budget of the current term.
             */
            proxies: components["schemas"]["FreeForm"];
        };
        /** @description A problem document (RFC 9457). */
        Problem: components["schemas"]["FreeForm"] & {
            /** @description The stable code clients branch on. */
            code: components["schemas"]["ErrorCode"];
            /** @description What happened this time. */
            detail: string;
            /**
             * Format: uuid
             * @description The id of the request, as in `X-Request-Id`.
             */
            request_id?: string | null;
            /**
             * Format: int32
             * @description The HTTP status.
             */
            status: number;
            /** @description What the code means; the same for every occurrence. */
            title: string;
            /** @description Identifies the kind of problem: `urn:kage:problem:<code>`. */
            type: string;
        };
        /**
         * @description A proxy, as an operator sees it. Its user name and password are never
         *     returned.
         */
        ProxyAdminView: {
            /**
             * Format: date-time
             * @description Until when it is left alone.
             */
            backoff_until?: string | null;
            /** @description The city the provider states. */
            city?: string | null;
            /**
             * Format: int32
             * @description Failed probes in a row.
             */
            consecutive_failures: number;
            /** @description The country the provider states. */
            country?: string | null;
            /**
             * Format: date-time
             * @description When it joined the pool.
             */
            created_at: string;
            /**
             * Format: date-time
             * @description Since when it fails without a break.
             */
            failing_since?: string | null;
            /** @description Its address. */
            host: string;
            /**
             * Format: uuid
             * @description The proxy's id.
             */
            id: string;
            last_health?: components["schemas"]["FreeForm"] | null;
            /**
             * Format: int32
             * @description Its port.
             */
            port: number;
            /** @description `webshare` or `static`. */
            provider: string;
            /** @description Why a replacement was requested, when one was. */
            replacement_reason?: string | null;
            /**
             * Format: date-time
             * @description When it left.
             */
            retired_at?: string | null;
            /** @description `new`, `healthy`, `degraded`, `backoff`, `replacing` or `retired`. */
            state: string;
        };
        /** @description The proxies. */
        ProxyList: {
            /** @description The proxies, oldest first. */
            proxies: components["schemas"]["ProxyAdminView"][];
        };
        /** @description Body of a readiness response. */
        ReadyReport: {
            /** @description What was checked. */
            checks: components["schemas"]["Checks"];
            /** @description Whether Kage can fetch. */
            status: components["schemas"]["ReadyStatus"];
        };
        /**
         * @description Whether Kage can fetch.
         * @enum {string}
         */
        ReadyStatus: "ready" | "not_ready" | "stopping";
        /**
         * @description How a page is arrived at.
         * @enum {string}
         */
        RefererStrategyField: "direct" | "none" | "search_engine";
        /**
         * @description The path a request asks for.
         * @enum {string}
         */
        RenderField: "auto" | "http" | "browser";
        /**
         * @description The path an attempt took.
         * @enum {string}
         */
        RenderTaken: "http" | "browser";
        /** @description What became of a request to replace a proxy. */
        ReplacementView: {
            /** @description What the outcome means here. */
            detail: string;
            /**
             * @description `requested`, `budget_exhausted`, `quota_exhausted`,
             *     `country_unavailable`, `unsupported`, `not_eligible` or `failed`.
             */
            outcome: string;
        };
        /**
         * @description What a request is on the http path.
         * @enum {string}
         */
        RequestKindField: "navigation" | "fetch" | "xhr";
        /**
         * @description Whether a fetch has finished.
         * @enum {string}
         */
        ResultState: "running" | "finished";
        /**
         * @description What to do about robots.txt. Fetching and enforcing it arrive with
         *     milestone M11; until then no verdict is reported and `enforce` is
         *     refused.
         * @enum {string}
         */
        RobotsField: "policy" | "report" | "enforce" | "ignore";
        /**
         * @description What an API key may do.
         * @enum {string}
         */
        ScopeField: "fetch" | "jobs" | "crawl" | "sessions" | "admin";
        /** @description How a screenshot is taken. */
        ScreenshotField: {
            /** @description The image format. */
            format?: components["schemas"]["ScreenshotFormatField"];
            /**
             * @description False, the default: the visible viewport, which no page can
             *     observe. True: the whole document, which fires `resize` in the page
             *     on chromium; the result names that side effect.
             */
            full_page?: boolean;
            /**
             * Format: int32
             * @description Quality for `jpeg` and `webp`, 1 to 100.
             */
            quality?: number | null;
        };
        /**
         * @description An image format.
         * @enum {string}
         */
        ScreenshotFormatField: "png" | "jpeg" | "webp";
        /** @description One path of a self-test. */
        SelftestPart: {
            /**
             * @description What was found: the echo's comparison on the http path, the
             *     sidecar's findings per page on the browser path. Null for a path
             *     that was skipped.
             */
            findings: components["schemas"]["AnyJson"];
            /**
             * Format: uuid
             * @description The stored run.
             */
            run?: string | null;
            /**
             * Format: double
             * @description The score of a browser self-test, from 0 to 100.
             */
            score?: number | null;
            /** @description Why the path was not tested, when it was skipped. */
            skipped?: string | null;
            /**
             * @description `passed`, `failed`, `inconclusive` (the test site could not be
             *     read), or `skipped`.
             */
            verdict: string;
        };
        /** @description What a self-test is to cover. */
        SelftestRequest: {
            /**
             * @description Also use the echo and load the detection pages that others run
             *     (`selftest.http.third_party_url`,
             *     `selftest.browser.third_party_pages`). They see the address of the
             *     persona's proxy. Never done on a timer, only when asked for here.
             */
            third_party?: boolean;
        };
        /** @description What a self-test of a persona found. */
        SelftestView: {
            /** @description The browser path. */
            browser: components["schemas"]["SelftestPart"];
            /** @description The http path. */
            http: components["schemas"]["SelftestPart"];
            /** @description Whether every path that was tested passed, and at least one was. */
            passed: boolean;
            /**
             * @description The persona after the test: a failure cools it, a pass ends a
             *     cooling that waited for one.
             */
            persona: components["schemas"]["PersonaAdminView"];
        };
        /** @description What a new session is to be. */
        SessionRequest: {
            /**
             * @description A host the session is for: the persona is one its policy allows.
             * @example example.org
             */
            domain?: string | null;
            /**
             * @description The id: 1 to 64 characters of `[A-Za-z0-9_.-]`, unique for this
             *     key. Left out: the server makes one.
             * @example proj-a-42
             */
            id?: string | null;
            persona?: components["schemas"]["PersonaHints"] | null;
        };
        /** @description A session. */
        SessionView: {
            /**
             * Format: date-time
             * @description When the session was opened.
             */
            created_at: string;
            /**
             * Format: date-time
             * @description When it ends unless it is used before: ten idle minutes, and six
             *     hours after it was opened at the latest (`sessions.idle_ttl`,
             *     `sessions.max_lifetime`).
             */
            expires_at: string;
            /** @description The id; `session_id` of a fetch. */
            id: string;
            /**
             * Format: date-time
             * @description When it last served a request.
             */
            last_used_at: string;
            /** @description The persona every request of the session is made by. */
            persona: components["schemas"]["PersonaView"];
        };
        /**
         * @description Time per phase of the last attempt, in milliseconds; null for a phase
         *     that did not happen on the path taken.
         */
        TimingView: {
            /**
             * Format: int64
             * @description TCP to the proxy and the tunnel (http path).
             */
            connect_ms?: number | null;
            /**
             * Format: int64
             * @description To `DOMContentLoaded` (browser).
             */
            dom_content_loaded_ms?: number | null;
            /**
             * Format: int64
             * @description To the load event (browser).
             */
            load_ms?: number | null;
            /**
             * Format: int64
             * @description The TLS handshake (http path).
             */
            tls_ms?: number | null;
            /**
             * Format: int64
             * @description The whole attempt: on a browser render this includes the time the
             *     persona spent on the page.
             */
            total_ms: number;
            /**
             * Format: int64
             * @description To the first byte of the response.
             */
            ttfb_ms?: number | null;
        };
        /** @description What a browser waits for before the page is captured. */
        WaitField: {
            /**
             * Format: int64
             * @description Milliseconds to wait after everything else.
             */
            extra_ms?: number;
            /** @description A CSS selector that must match an element afterwards. */
            selector?: string | null;
            /** @description The lifecycle point. */
            until?: components["schemas"]["WaitUntilField"];
        };
        /**
         * @description The lifecycle point a browser waits for.
         * @enum {string}
         */
        WaitUntilField: "commit" | "dom_content_loaded" | "load" | "network_idle";
        /** @description Which responses of the page's scripts are kept. */
        XhrField: {
            /** @description Media types to keep, for example `application/json`. */
            content_types?: string[];
            /**
             * Format: int64
             * @description The largest body kept per response, in bytes; a longer one is cut.
             */
            max_bytes?: number;
            /**
             * Format: int64
             * @description The most responses kept per page.
             */
            max_responses?: number | null;
        };
        /** @description One response a script of the page fetched. */
        XhrView: {
            /**
             * Format: int64
             * @description Matching responses that were not kept because the page had more
             *     than `xhr.max_responses`.
             */
            dropped: number;
            /**
             * @description The responses that matched the request's filter: each with `url`,
             *     `method`, `kind` (`xhr` or `fetch`), `status`, `content_type`,
             *     `bytes`, `truncated`, and its body as `body` (text, when it is at
             *     most 256 kB) or as `body_artifact` (an artifact's id).
             */
            responses: components["schemas"]["FreeForm"][];
        };
    };
    responses: never;
    parameters: never;
    requestBodies: never;
    headers: never;
    pathItems: never;
}
export type $defs = Record<string, never>;
export interface operations {
    healthz: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The process is alive. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Liveness"];
                };
            };
        };
    };
    metrics: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The Prometheus text format. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "text/plain": string;
                };
            };
            /** @description No key, or not a valid one. */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description The key lacks the `admin` scope. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    readyz: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description Kage can fetch. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ReadyReport"];
                };
            };
            /** @description Kage cannot fetch now; `checks` says why. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ReadyReport"];
                };
            };
        };
    };
    list_audit: {
        parameters: {
            query?: {
                /** @description The `next_cursor` of the page before; left out for the newest. */
                cursor?: string;
                /** @description How many events, at most 200; 50 when left out. */
                limit?: number;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description A page of events. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["AuditPage"];
                };
            };
        };
    };
    list_keys: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The keys. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["KeyList"];
                };
            };
        };
    };
    create_key: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["KeyRequest"];
            };
        };
        responses: {
            /** @description The key and its token. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["KeyCreated"];
                };
            };
            /** @description The name, the scopes or a limit is not acceptable. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    revoke_key: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The key's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The key, disabled. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["KeyView"];
                };
            };
            /** @description `key_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    rotate_key: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The key's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The key and its new token. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["KeyCreated"];
                };
            };
            /** @description `key_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `conflict`: the key is disabled. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    list_personas: {
        parameters: {
            query?: {
                /** @description Only personas in this state: `active`, `cooling` or `retired`. */
                state?: string;
                /**
                 * @description Include retired personas; without `state`, only live ones are
                 *     listed.
                 */
                all?: boolean;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The personas. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PersonaList"];
                };
            };
        };
    };
    get_persona: {
        parameters: {
            query?: {
                /** @description Include the stored fingerprint document. */
                fingerprint?: boolean;
            };
            header?: never;
            path: {
                /** @description The persona's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The persona. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PersonaAdminView"];
                };
            };
            /** @description `persona_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    regenerate_persona: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The persona's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The new persona. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PersonaAdminView"];
                };
            };
            /** @description `persona_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `conflict`: the persona is retired, or its proxy cannot carry traffic. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    retire_persona: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The persona's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The persona, retired. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PersonaAdminView"];
                };
            };
            /** @description `persona_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    selftest_persona: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The persona's id. */
                id: string;
            };
            cookie?: never;
        };
        /** @description Optional; `{}` tests against the operator's own echo and pages. */
        requestBody: {
            content: {
                "application/json": components["schemas"]["SelftestRequest"];
            };
        };
        responses: {
            /** @description What the self-tests found; `passed` says whether they passed. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["SelftestView"];
                };
            };
            /** @description `persona_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `conflict`: the persona is retired, held by other work, or its proxy cannot carry traffic. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    reload_policies: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The policies are in force. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PoliciesView"];
                };
            };
            /** @description `validation_failed`: a policy file is invalid; the last good policies stay in force. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    get_pool: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The pool. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PoolView"];
                };
            };
        };
    };
    list_proxies: {
        parameters: {
            query?: {
                /** @description Include retired proxies. */
                all?: boolean;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The proxies. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ProxyList"];
                };
            };
        };
    };
    check_proxy: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The proxy's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The proxy after the probe. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ProxyAdminView"];
                };
            };
            /** @description `proxy_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `conflict`: the proxy is being replaced or is retired, and is not probed. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    replace_proxy: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The proxy's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description What became of the request. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["ReplacementView"];
                };
            };
            /** @description `proxy_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `conflict`: no provider is configured. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    get_artifact: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The artifact's id, as a result names it (`art_...`). */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The artifact's bytes, in its own media type. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/octet-stream": number[];
                };
            };
            /** @description `artifact_not_found`: no such artifact of this key, or it expired. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    fetch: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["FetchRequest"];
            };
        };
        responses: {
            /** @description The fetch ended; `outcome` says how. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["FetchResult"];
                };
            };
            /** @description The fetch outlasted the wall time and runs on. */
            202: {
                headers: {
                    /** @description Where the result will be. */
                    Location?: string;
                    /** @description Seconds to wait before asking for it. */
                    "Retry-After"?: number;
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["FetchAccepted"];
                };
            };
            /** @description The body is not JSON. */
            400: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description No key, or not a valid one. */
            401: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description The key lacks the `fetch` scope, or `sessions` for `session_id`. */
            403: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `session_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description `session_persona_conflict`. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description The request cannot be made as asked; `code` says why. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description A limit of the key or of the API; `Retry-After` says when. */
            429: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description Nothing can be fetched: no proxy pool, or the server stops. */
            503: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    preview_policy: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description A host name, for example `shop.example.org`. */
                domain: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The policy in force for the host. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PolicyPreview"];
                };
            };
            /** @description Not a host name. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    get_result: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The request's id, as in `X-Request-Id`. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The result. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["FetchResult"];
                };
            };
            /** @description The fetch is still under way. */
            202: {
                headers: {
                    /** @description Seconds to wait before asking again. */
                    "Retry-After"?: number;
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["FetchAccepted"];
                };
            };
            /** @description `result_not_found`: no such request of this key, or it expired. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    open_session: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SessionRequest"];
            };
        };
        responses: {
            /** @description The session was open already. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["SessionView"];
                };
            };
            /** @description A new session; its persona is new to this id. */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["SessionView"];
                };
            };
            /** @description `no_matching_persona`, or `session_persona_conflict` for an open session whose persona the request excludes. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
            /** @description The id or a hint is not well formed. */
            422: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    get_session: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The session's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The session. */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["SessionView"];
                };
            };
            /** @description `session_not_found`: never opened, or ended. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
    end_session: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                /** @description The session's id. */
                id: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description The session ended. */
            204: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description `session_not_found`. */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/problem+json": components["schemas"]["Problem"];
                };
            };
        };
    };
}
