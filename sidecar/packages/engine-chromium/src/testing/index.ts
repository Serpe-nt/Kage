/**
 * Test support of the Chromium engine. The fixture site launcher and the
 * local authenticated proxy are shared with the Firefox engine and live in
 * engine-core; they are re-exported here for the tools of this package.
 */
export { startFixtureSite, startTestProxy } from '@kage/engine-core/testing';
export type { FixtureRequest, FixtureSite, TestProxy } from '@kage/engine-core/testing';
