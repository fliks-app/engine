import createClient from 'openapi-fetch';
import type { paths } from './schema';

export type { components, paths } from './schema';

/** Typed control-plane client; every request carries the bearer token. */
export function createEngineClient(baseUrl: string, token: string) {
  return createClient<paths>({
    baseUrl,
    headers: { Authorization: `Bearer ${token}` },
  });
}
