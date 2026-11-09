import type { RedirectHop } from '@kage/contract';

/** A proxy's request for credentials, which is no answer of the site. */
const PROXY_CHALLENGE = 407;

/** The most redirects of one document request that are kept. */
const MAX_HOPS = 50;

/** A request for a document as Chrome's debugger announces it (`Network.requestWillBeSent`). */
export interface DocumentRequested {
  readonly requestId: string;
  readonly frameId?: string;
  readonly type?: string;
  readonly request: { readonly url: string };
  /** Present when the request follows a redirect it was answered with. */
  readonly redirectResponse?: unknown;
}

/** The head of an answer as the network saw it (`Network.responseReceivedExtraInfo`). */
export interface AnswerSeen {
  readonly requestId: string;
  readonly statusCode?: number;
  readonly headers: Readonly<Record<string, string>>;
  /**
   * The head as it came over the wire, its field lines in the order sent;
   * the debugger has none for HTTP/2 and HTTP/3.
   */
  readonly headersText?: string;
}

/** An answer with a document (`Network.responseReceived`): the response the tab shows. */
export interface DocumentAnswered {
  readonly requestId: string;
  readonly type?: string;
  readonly response: {
    readonly url: string;
    readonly status: number;
    /** One value per name; Chrome joins repeated lines with a line feed. */
    readonly headers: Readonly<Record<string, string>>;
  };
}

/** A document's response as the tab shows it. */
export interface ShownAnswer {
  readonly status: number;
  readonly headers: { name: string; value: string }[];
}

interface Latest {
  readonly id: string;
  /** The address of the request and of every redirect it followed, in order. */
  readonly urls: string[];
  /** The redirects it was answered with, in order. */
  readonly answers: { status: number; location: string }[];
  /** Whether a document came of it. */
  answered: boolean;
  /** The status of the latest answer the network gave it (`Network.responseReceivedExtraInfo`). */
  wireStatus: number | null;
  /** The field lines of that answer in the order they came, where the debugger had its text. */
  wireLines: { name: string; value: string }[] | null;
  /** The field lines of that answer as the debugger's header object holds them, sorted by name. */
  wireHeaders: { name: string; value: string }[];
  /** The response the tab shows, as the debugger announced it with the document. */
  shown: DocumentAnswered['response'] | null;
}

const withoutFragment = (url: string) => url.split('#')[0] ?? '';

/** A header object of the debugger as header lines, a line per value. */
function headerLines(headers: Readonly<Record<string, string>>): { name: string; value: string }[] {
  return Object.entries(headers).flatMap(([name, joined]) =>
    joined.split('\n').map((value) => ({ name, value })),
  );
}

/**
 * The field lines of a head as the debugger quotes it from the wire
 * (`headersText`): the status line first, then one line per field in the
 * order sent, a repeated name on lines of its own. A line folded onto the
 * next (obsolete, RFC 9112 §5.2) is one value, its fold a space. `null`
 * where the text holds no status line.
 */
export function wireLines(text: string): { name: string; value: string }[] | null {
  const [status, ...rest] = text.split(/\r?\n/);
  if (!status?.startsWith('HTTP/')) {
    return null;
  }
  const lines: { name: string; value: string }[] = [];
  for (const line of rest) {
    if (line === '') {
      break;
    }
    const previous = lines[lines.length - 1];
    if (/^[ \t]/.test(line)) {
      if (previous !== undefined) {
        previous.value = `${previous.value} ${line.trim()}`.trim();
      }
      continue;
    }
    const colon = line.indexOf(':');
    if (colon <= 0) {
      continue;
    }
    lines.push({ name: line.slice(0, colon).trim(), value: line.slice(colon + 1).trim() });
  }
  return lines;
}

/**
 * The main frame's latest request for a document, as Chrome's debugger saw
 * it (F167). Chrome refuses by itself to follow a redirect to `file:`,
 * `data:`, `ftp:` or `javascript:` and tells the driver only that the
 * navigation failed (`net::ERR_UNSAFE_REDIRECT`, `net::ERR_ABORTED`): the
 * answer with its `Location` reaches the debugger alone, and this keeps it,
 * so that the capture reports the refused hop and the chain check in Kage
 * judges it. Nothing is asked of the page.
 */
export class DocumentRequests {
  private latest: Latest | null = null;

  /**
   * @param mainFrame the id of the tab's main frame, once it is known; a
   *   document request of another frame (an iframe's) is not the tab's.
   */
  constructor(private mainFrame: string | null = null) {}

  /** The id of the tab's main frame, when it was looked up after the tab opened. */
  knowMainFrame(id: string): void {
    this.mainFrame = id;
  }

  requested(event: DocumentRequested): void {
    if (event.type !== 'Document') {
      return;
    }
    if (this.mainFrame !== null && event.frameId !== this.mainFrame) {
      return;
    }
    const latest = this.latest;
    if (event.redirectResponse !== undefined && latest !== null && latest.id === event.requestId) {
      if (latest.urls.length < MAX_HOPS + 1) {
        latest.urls.push(event.request.url);
      }
      return;
    }
    this.latest = {
      id: event.requestId,
      urls: [event.request.url],
      answers: [],
      answered: false,
      wireStatus: null,
      wireLines: null,
      wireHeaders: [],
      shown: null,
    };
  }

  seen(event: AnswerSeen): void {
    const latest = this.latest;
    const status = event.statusCode ?? 0;
    // A proxy's `407` asks for the persona's credentials: no answer of the
    // site, and the browser answers it and asks again.
    if (latest?.id !== event.requestId || status === PROXY_CHALLENGE) {
      return;
    }
    latest.wireStatus = status === 0 ? null : status;
    latest.wireLines = event.headersText === undefined ? null : wireLines(event.headersText);
    latest.wireHeaders = headerLines(event.headers);
    if (status < 300 || status > 399) {
      return;
    }
    const location = Object.entries(event.headers).find(
      ([name]) => name.toLowerCase() === 'location',
    )?.[1];
    if (location !== undefined && latest.answers.length < MAX_HOPS) {
      latest.answers.push({ status, location });
    }
  }

  answered(event: DocumentAnswered): void {
    const latest = this.latest;
    if (
      latest?.id === event.requestId &&
      event.type === 'Document' &&
      event.response.status !== PROXY_CHALLENGE
    ) {
      latest.answered = true;
      latest.shown = event.response;
    }
  }

  /**
   * Whether an answer to the main frame's document request is a file to
   * save (`Content-Disposition: attachment`): the tab shows no page of it.
   * Over plain `http` Chrome holds such a download for the person to allow
   * and the driver hears of no download at all, so this answer is all that
   * says the link led to a file (F120).
   */
  savesFile(event: DocumentAnswered): boolean {
    if (this.latest?.id !== event.requestId || event.type !== 'Document') {
      return false;
    }
    return Object.entries(event.response.headers).some(
      ([name, value]) =>
        name.toLowerCase() === 'content-disposition' && /^\s*attachment\b/i.test(value),
    );
  }

  /**
   * The head the tab shows for the document at `url`, where Chrome
   * revalidated a page it kept and the network answered `304` (F178): the
   * kept response, updated by the 304's fields, as the debugger announced
   * it with the document. The driver gives the wire's header lines and
   * the kept status there. Its lines come in the order of the debugger's
   * header object, not the site's: the head's text it quotes is the 304's
   * own, so the order the page was once sent in is not known here (QA
   * round four, b85-02). `null` for any other document.
   */
  revalidated(url: string): ShownAnswer | null {
    const shown = this.latest?.shown ?? null;
    if (
      shown === null ||
      this.latest?.wireStatus !== 304 ||
      shown.status === 304 ||
      withoutFragment(shown.url) !== withoutFragment(url)
    ) {
      return null;
    }
    return { status: shown.status, headers: headerLines(shown.headers) };
  }

  /**
   * The head of the document at `url` that the network answered with
   * `status`, its field lines in the order the site sent them (QA round
   * three, browser-04): the driver gives Chrome's lines sorted by name, as
   * the debugger's header object holds them. `null` where the debugger had
   * no text of the head (HTTP/2, HTTP/3, a page from the cache) or the
   * document is another.
   */
  asSent(url: string, status: number): ShownAnswer | null {
    const latest = this.latest;
    const last = latest?.urls[latest.urls.length - 1];
    if (
      latest === null ||
      !latest.answered ||
      latest.wireLines === null ||
      latest.wireStatus !== status ||
      last === undefined ||
      withoutFragment(last) !== withoutFragment(url)
    ) {
      return null;
    }
    return { status, headers: latest.wireLines.map((line) => ({ ...line })) };
  }

  /**
   * The wire's `304` the latest document request got at `url`, its last
   * address, where no kept page came of it (QA round three, class-02):
   * Chrome breaks a load off at a `304` it kept no page for, as at a
   * `204`, and tells its driver only that the load was aborted. The
   * status and the 304's own fields, in the order sent where the debugger
   * quoted them; `null` for any other answer.
   */
  notModified(url: string): ShownAnswer | null {
    const latest = this.latest;
    const last = latest?.urls[latest.urls.length - 1];
    if (
      latest === null ||
      // A kept page brought up to date is {@link DocumentRequests.revalidated}'s.
      (latest.shown !== null && latest.shown.status !== 304) ||
      latest.wireStatus !== 304 ||
      last === undefined ||
      withoutFragment(last) !== withoutFragment(url)
    ) {
      return null;
    }
    const lines = latest.wireLines ?? latest.wireHeaders;
    return { status: 304, headers: lines.map((line) => ({ ...line })) };
  }

  /**
   * The status of the error answer the latest document request got at
   * `url`, its last address (F204): Chrome shows its own page in place of
   * an empty error answer and tells its driver only that the load failed.
   * `null` when the request is another, or its answer was no error.
   */
  replacedStatus(url: string): number | null {
    const latest = this.latest;
    const last = latest?.urls[latest.urls.length - 1];
    if (latest === null || last === undefined || withoutFragment(last) !== withoutFragment(url)) {
      return null;
    }
    const status = latest.shown?.status ?? latest.wireStatus ?? 0;
    return status >= 400 ? status : null;
  }

  /**
   * The redirects the latest document request was answered with when no
   * document came of it, each with the address it answered; empty when a
   * document came, or when none was asked for.
   */
  unanswered(): RedirectHop[] {
    const latest = this.latest;
    if (latest === null || latest.answered) {
      return [];
    }
    const last = latest.urls[latest.urls.length - 1] ?? '';
    return latest.answers.map((answer, index) => ({
      url: latest.urls[index] ?? last,
      status: answer.status,
      location: answer.location,
    }));
  }
}
