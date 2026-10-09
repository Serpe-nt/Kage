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

/** How the human steps of a flow are performed. */
export const FlowBehaviorSchema = z
  .strictObject({
    seed: SeedSchema.optional().describe(
      'Seed of this flow. With the persona\'s own seed it decides every movement and key; absent: the sidecar draws one and returns it.',
    ),
    typos: z
      .boolean()
      .optional()
      .describe('Now and then type a neighbouring key, notice, delete and retype; default false.'),
  })
  .meta({ id: 'FlowBehavior', description: 'How the human steps of a flow are performed.' });
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
    duration_ms: z.number().int().nonnegative().describe('How long the step took.'),
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
      'What the behavior engine did over the whole flow; null when no step was a human one.',
    ),
  })
  .meta({ id: 'FlowFinalEvent', description: 'The end of a flow.' });

/** One line of the NDJSON stream `POST /v1/contexts/{id}/flow` answers with. */
export const FlowEventSchema = z
  .discriminatedUnion('type', [FlowStepEventSchema, FlowFinalEventSchema])
  .meta({ id: 'FlowEvent', description: "One line of a flow's NDJSON stream, chosen by `type`." });
export type FlowEvent = z.infer<typeof FlowEventSchema>;
