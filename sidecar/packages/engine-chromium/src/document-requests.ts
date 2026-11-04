import type { RedirectHop } from '@kage/contract';

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
}

/** An answer with a document (`Network.responseReceived`). */
export interface DocumentAnswered {
  readonly requestId: string;
  readonly type?: string;
  readonly response: { readonly status: number };
}

interface Latest {
  readonly id: string;
  /** The address of the request and of every redirect it followed, in order. */
  readonly urls: string[];
  /** The redirects it was answered with, in order. */
  readonly answers: { status: number; location: string }[];
  /** Whether a document came of it. */
  answered: boolean;
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
    };
  }

  seen(event: AnswerSeen): void {
    const latest = this.latest;
    const status = event.statusCode ?? 0;
    if (latest?.id !== event.requestId || status < 300 || status > 399) {
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
    if (latest?.id === event.requestId && event.type === 'Document') {
      latest.answered = true;
    }
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
