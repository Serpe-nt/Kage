import { z } from 'zod';

import { SeedSchema } from './personas.js';
import {
  BehaviorReportSchema,
  CaptureOptionsSchema,
  MAX_NAVIGATE_TIMEOUT_MS,
  PageResultSchema,
  PdfOptionsSchema,
  ScreenshotOptionsSchema,
  httpUrl,
} from './navigate.js';

/**
 * The v1 flow DSL (spec §10.5), run by the flow executor of engine-core on
 * both engines. A step with `human` true (the default) is done through the
 * mouse and keyboard of the context's display by the behavior engine; with
 * `human` false it is done by the driver, which a page can tell from a
 * person's input.
 */

const selector = (description: string) => z.string().min(1).max(1000).describe(description);

const human = z
  .boolean()
  .optional()
  .describe('Perform the step the way a person would (mouse path, key cadence); default true.');

const stepTimeout = z
  .number()
  .int()
  .min(100)
  .max(MAX_NAVIGATE_TIMEOUT_MS)
  .optional()
  .describe('Budget for this step in milliseconds; default 30000.');

const step = <const Name extends string, Shape extends z.ZodRawShape>(
  name: Name,
  id: string,
  description: string,
  shape: Shape,
) =>
  z
    .strictObject({
      action: z.literal(name).describe(`Always \`${name}\`.`),
      ...shape,
      human,
      timeout_ms: stepTimeout,
    })
    .meta({ id, description });

export const GotoActionSchema = step('goto', 'GotoAction', 'Load a URL.', {
  url: httpUrl('The page to load.'),
});
export const ClickActionSchema = step('click', 'ClickAction', 'Click an element.', {
  selector: selector('CSS selector of the element.'),
});
export const TypeActionSchema = step('type', 'TypeAction', 'Type text into an element.', {
  selector: selector('CSS selector of the input.'),
  text: z.string().max(10_000).describe('The text to type.'),
});
export const PressActionSchema = step('press', 'PressAction', 'Press one key.', {
  key: z
    .string()
    .min(1)
    .max(40)
    .describe('The key, as `KeyboardEvent.key` names it, for example `Enter`.'),
});
export const ScrollActionSchema = step('scroll', 'ScrollAction', 'Scroll the page.', {
  depth: z
    .number()
    .min(0)
    .max(1)
    .describe('How far down to end, as a share of the document height.'),
});
export const WaitForActionSchema = step(
  'wait_for',
  'WaitForAction',
  'Wait for one condition: exactly one of the four fields is set.',
  {
    selector: z
      .string()
      .min(1)
      .max(1000)
      .nullable()
      .optional()
      .describe('Wait until this CSS selector matches.'),
    url: z
      .string()
      .max(2000)
      .nullable()
      .optional()
      .describe('Wait until the URL matches this glob.'),
    network_idle: z
      .boolean()
      .nullable()
      .optional()
      .describe('Wait until no request ran for 500 ms.'),
    ms: z.number().int().min(0).max(120_000).nullable().optional().describe('Wait this long.'),
  },
);
export const SelectActionSchema = step(
  'select',
  'SelectAction',
  'Choose an option of a select element.',
  {
    selector: selector('CSS selector of the select element.'),
    value: z.string().max(1000).describe('The option value.'),
  },
);
export const HoverActionSchema = step('hover', 'HoverAction', 'Move the pointer over an element.', {
  selector: selector('CSS selector of the element.'),
});
export const BackActionSchema = step('back', 'BackAction', 'Go back one history entry.', {});
export const ExtractActionSchema = step(
  'extract',
  'ExtractAction',
  'Capture the page at this point of the flow.',
  { capture: CaptureOptionsSchema.describe('What to capture.') },
);
export const ScreenshotActionSchema = step('screenshot', 'ScreenshotAction', 'Take a screenshot.', {
  options: ScreenshotOptionsSchema.describe('How to take it.'),
});
export const PdfActionSchema = step('pdf', 'PdfAction', 'Print a PDF.', {
  options: PdfOptionsSchema.describe('How to print it.'),
});
export const AssertActionSchema = step(
  'assert',
  'AssertAction',
  'Fail the flow unless a condition holds: exactly one of the two fields is set.',
  {
    selector: z
      .string()
      .min(1)
      .max(1000)
      .nullable()
      .optional()
      .describe('The CSS selector must match an element.'),
    text_contains: z
      .string()
      .max(2000)
      .nullable()
      .optional()
      .describe('The page text must contain this.'),
  },
);

/** One step of a flow. */
export const FlowActionSchema = z
  .discriminatedUnion('action', [
    GotoActionSchema,
    ClickActionSchema,
    TypeActionSchema,
    PressActionSchema,
    ScrollActionSchema,
    WaitForActionSchema,
    SelectActionSchema,
    HoverActionSchema,
    BackActionSchema,
    ExtractActionSchema,
    ScreenshotActionSchema,
    PdfActionSchema,
    AssertActionSchema,
  ])
  .meta({ id: 'FlowAction', description: 'One step of a flow, chosen by `action`.' });
export type FlowAction = z.infer<typeof FlowActionSchema>;

/** Longest median a caller may give the dwell of a paced flow. */
export const MAX_PACE_DWELL_MS = 120_000;
/** Highest floor a caller may put under the dwells of a paced flow: the cap of a persona's own dwell. */
export const MAX_PACE_MIN_DWELL_MS = 45_000;

/** How a flow paces itself (spec §10.5). */
export const FlowPaceSchema = z
  .strictObject({
    dwell_ms: z
      .number()
      .int()
      .min(0)
      .max(MAX_PACE_DWELL_MS)
      .optional()
      .describe(
        "Median of the time the persona stays on a page after arriving on it, before the next step. Each dwell is drawn from a log-normal with this median whose 95th percentile is twice the median. Absent: the persona's own human dwell (log-normal, median about 6 s, at most 45 s).",
      ),
    min_dwell_ms: z
      .number()
      .int()
      .min(0)
      .max(MAX_PACE_MIN_DWELL_MS)
      .optional()
      .describe(
        'No dwell is shorter than this; default 0. A shorter draw is drawn again, never raised to the floor, so that dwells do not pile up at it.',
      ),
    wander: z
      .boolean()
      .optional()
      .describe(
        "Whether the persona's idle hand wanders while it dwells on a page it arrived on; default true. False: the dwell sends no input at all, the hand rests. The thinking time before a step is the same either way. Since 1.5.0.",
      ),
  })
  .meta({
    id: 'FlowPace',
    description:
      "How a flow paces itself. Present, even empty: after every step that arrived on a page (a `goto`; a `click`, `press` or `back` after which a new document or another address was seen) the persona dwells there, its idle hand wandering unless `wander` is false, before the next step. And before every step other than the five that only read (`wait_for`, `assert`, `extract`, `screenshot`, `pdf`) it takes a thinking time of its own (log-normal, median 0.4 to 0.9 s by persona, 80 ms to 5 s), unless it has just dwelled or the step is the flow's first. So no two actions are a constant time apart and none follows a read at machine speed. The reading steps are never delayed and send no input. A click or a key of the driver (`human: false`) is then watched for a navigation as a person's is (1.2 s). Every time is drawn from the stream of the flow's seed, so the same flow replays. A pause never outlasts the flow's budget less a reserve for the captures; it counts in the `duration_ms` and `paced_ms` of its step, not against the step's `timeout_ms`. Absent: the flow runs its steps one right after the other.",
  });
export type FlowPace = z.infer<typeof FlowPaceSchema>;

/** How the human steps of a flow are performed. */
export const FlowBehaviorSchema = z
  .strictObject({
    seed: SeedSchema.optional().describe(
      "Seed of this flow. With the persona's own seed it decides every movement, key and pause; absent: the sidecar draws one and returns it.",
    ),
    typos: z
      .boolean()
      .optional()
      .describe('Now and then type a neighbouring key, notice, delete and retype; default false.'),
    pace: FlowPaceSchema.optional().describe(
      'Makes the flow pace itself like a person: a dwell on every page it arrives on, a thinking time between actions. Absent: no pacing.',
    ),
  })
  .meta({
    id: 'FlowBehavior',
    description: 'How the human steps of a flow are performed, and whether the flow paces itself.',
  });
export type FlowBehavior = z.infer<typeof FlowBehaviorSchema>;

/** Body of `POST /v1/contexts/{id}/flow`. */
export const FlowRequestSchema = z
  .strictObject({
    actions: z.array(FlowActionSchema).min(1).max(100).describe('The steps, run in order.'),
    capture: CaptureOptionsSchema.describe('What the final page result captures.'),
    timeout_ms: z
      .number()
      .int()
      .min(1000)
      .max(MAX_NAVIGATE_TIMEOUT_MS)
      .optional()
      .describe('Budget for the whole flow; default 180000.'),
    behavior: FlowBehaviorSchema.optional().describe('How the human steps are performed.'),
  })
  .meta({ id: 'FlowRequest', description: 'A scripted navigation flow.' });
export type FlowRequest = z.infer<typeof FlowRequestSchema>;

/** One line of the flow's NDJSON stream: a finished step. */
export const FlowStepEventSchema = z
  .strictObject({
    type: z.literal('step').describe('Always `step`.'),
    index: z.number().int().nonnegative().describe('Position of the step in `actions`.'),
    action: z.string().describe("The step's action name."),
    ok: z.boolean().describe('Whether the step succeeded.'),
    duration_ms: z
      .number()
      .int()
      .nonnegative()
      .describe('How long the step took, its pacing included.'),
    paced_ms: z
      .number()
      .int()
      .nonnegative()
      .nullable()
      .describe(
        'The part of `duration_ms` that was pacing: the thinking time before the step and the dwell after it. Null when the flow is not paced (`behavior.pace` absent).',
      ),
    error: z.string().nullable().describe('Why the step failed; null when it did not.'),
    capture: PageResultSchema.nullable().describe(
      'What an `extract`, `screenshot` or `pdf` step captured; null otherwise.',
    ),
  })
  .meta({ id: 'FlowStepEvent', description: 'A finished step of a flow.' });

/** The last line of the flow's NDJSON stream. */
export const FlowFinalEventSchema = z
  .strictObject({
    type: z.literal('final').describe('Always `final`.'),
    ok: z.boolean().describe('Whether every step succeeded.'),
    result: PageResultSchema.describe('The page as the flow left it.'),
    behavior: BehaviorReportSchema.nullable().describe(
      'What the behavior engine did over the whole flow; null when no step was a human one and the flow was not paced. Its `dwell_ms` is the sum of the dwells of a paced flow, 0 otherwise.',
    ),
  })
  .meta({ id: 'FlowFinalEvent', description: 'The end of a flow.' });

/** One line of the NDJSON stream `POST /v1/contexts/{id}/flow` answers with. */
export const FlowEventSchema = z
  .discriminatedUnion('type', [FlowStepEventSchema, FlowFinalEventSchema])
  .meta({ id: 'FlowEvent', description: "One line of a flow's NDJSON stream, chosen by `type`." });
export type FlowEvent = z.infer<typeof FlowEventSchema>;
